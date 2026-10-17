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

/// Mirror bounded profiler sections without exporting per-subject isolate operations.
pub struct Phase {
    _profile: Option<smallclaims::profile::Span>,
    span: Option<opentelemetry::global::BoxedSpan>,
    members: Option<[MemberTotals; 2]>,
}

#[derive(Default)]
struct MemberTotals {
    count: u64,
    total: std::time::Duration,
    max: std::time::Duration,
}

pub struct Member<'a> {
    _profile: Option<smallclaims::profile::Span>,
    phase: &'a mut Phase,
    started: Option<Instant>,
    stop: bool,
}

impl Drop for Member<'_> {
    fn drop(&mut self) {
        if let (Some(started), Some(members)) = (self.started, &mut self.phase.members) {
            let elapsed = started.elapsed();
            let totals = &mut members[usize::from(!self.stop)];
            totals.count += 1;
            totals.total += elapsed;
            totals.max = totals.max.max(elapsed);
        }
    }
}

pub fn phase(scope: &str) -> Phase {
    use opentelemetry::trace::Tracer as _;
    let profile = smallclaims::profile::span(scope);
    let name = match scope {
        "pass/changes" => "st.reconcile.changes",
        "pass/gate-runners" => "st.reconcile.gate-runners",
        "pass/desired" => "st.reconcile.desired",
        "pass/pty-snapshot" => "st.reconcile.pty-snapshot",
        "pass/render" => "st.reconcile.render",
        "pass/unreadable-members" => "st.reconcile.unreadable-members",
        "pass/members" => "st.reconcile.members",
        "pass/work-messages" => "st.reconcile.work-messages",
        "stage/intake" => "st.reconcile.stage.intake",
        "stage/observers" => "st.reconcile.stage.observers",
        "stage/schedules" => "st.reconcile.stage.schedules",
        "stage/scheduled-work" => "st.reconcile.stage.scheduled-work",
        "stage/subscriptions" => "st.reconcile.stage.subscriptions",
        "stage/provider-capacity-retries" => "st.reconcile.stage.provider-capacity-retries",
        "stage/person-asks" => "st.reconcile.stage.person-asks",
        "stage/disk-space" => "st.reconcile.stage.disk-space",
        "stage/faults" => "st.reconcile.stage.faults",
        "stage/github-watches" => "st.reconcile.stage.github-watches",
        "stage/missions" => "st.reconcile.stage.missions",
        "stage/subagents" => "st.reconcile.stage.subagents",
        "fault_snapshot" => "st.reconcile.fault_snapshot",
        _ => return Phase { _profile: profile, span: None, members: None },
    };
    let span = smallclaims::sqlite::telemetry::phase_spans_enabled().then(|| {
        let tracer = opentelemetry::global::tracer("st3");
        tracer.span_builder(name)
            .with_attributes([KeyValue::new("span.label", name.strip_prefix("st.reconcile.").unwrap_or(name))])
            .start_with_context(&tracer, &tracing::Span::current().context())
    });
    let members = (scope == "pass/members" && span.is_some()).then(Default::default);
    Phase { _profile: profile, span, members }
}

impl Phase {
    pub fn member(&mut self, stop: bool) -> Member<'_> {
        let profile = smallclaims::profile::span(if stop { "pass/member stop" } else { "pass/member live" });
        let started = self.members.as_ref().map(|_| Instant::now());
        Member { _profile: profile, phase: self, started, stop }
    }

    pub fn items(&mut self, count: usize) {
        use opentelemetry::trace::Span as _;
        if let Some(span) = &mut self.span {
            span.set_attribute(KeyValue::new("st.reconcile.items", i64::try_from(count).unwrap_or(i64::MAX)));
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        use opentelemetry::trace::Span as _;
        if let Some(span) = &mut self.span {
            if let Some(members) = &self.members {
                for (totals, keys) in members.iter().zip([
                    ["st.reconcile.members.stop.count", "st.reconcile.members.stop.total_ms", "st.reconcile.members.stop.max_ms"],
                    ["st.reconcile.members.live.count", "st.reconcile.members.live.total_ms", "st.reconcile.members.live.max_ms"],
                ]) {
                    span.set_attribute(KeyValue::new(keys[0], i64::try_from(totals.count).unwrap_or(i64::MAX)));
                    span.set_attribute(KeyValue::new(keys[1], totals.total.as_secs_f64() * 1000.));
                    span.set_attribute(KeyValue::new(keys[2], totals.max.as_secs_f64() * 1000.));
                }
            }
            span.end();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum WakeCause {
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
    pub const ALL: [Self; 17] = [
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
    pub const fn legacy_str(self) -> &'static str {
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
    pub const fn otel_str(self) -> &'static str {
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
    observation_drops: Counter<u64>,
    duration: Histogram<f64>,
    passes_per_wake: Histogram<u64>,
}
static METRICS: LazyLock<Instruments> = LazyLock::new(|| {
    let meter = opentelemetry::global::meter("st3.reconcile");
    Instruments {
        wakes: meter
            .u64_counter("st.reconcile.wakes")
            .with_unit("{wake}")
            .build(),
        observation_drops: meter
            .u64_counter("st.reconcile.runtime_observation.dropped")
            .with_unit("{observation}")
            .build(),
        duration: meter
            .f64_histogram("st.reconcile.pass.duration")
            .with_unit("s")
            .build(),
        passes_per_wake: meter
            .u64_histogram("st.reconcile.passes_per_wake")
            .with_unit("{pass}")
            .with_boundaries(vec![1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0])
            .build(),
    }
});
pub fn init() {
    LazyLock::force(&ATTRIBUTES);
    LazyLock::force(&TASKS);
    LazyLock::force(&METRICS);
    smallclaims::fifo::init(&opentelemetry::global::meter("st3.fifo"));
}
/// Notify coalesces wakes: the next pass carries the last recorded cause, not a request parent.
pub fn record_wake(cause: WakeCause, detail: Option<&str>) {
    smallclaims::performance::record_wake(cause.legacy_str(), detail);
    PENDING.store(cause as usize, Ordering::Relaxed);
    if crate::otel::metrics_enabled() {
        METRICS.wakes.add(1, &ATTRIBUTES[cause as usize]);
    }
}
/// A rejected physical capture is visible without unbounded subject metric labels.
pub fn record_runtime_observation_drop() {
    if crate::otel::metrics_enabled() {
        METRICS.observation_drops.add(1, &[]);
    }
}

pub fn take_wake() -> WakeCause {
    WakeCause::ALL[PENDING.swap(WakeCause::Other as usize, Ordering::Relaxed)]
}

/// Count completed passes in one outer wake, including a wake that reaches the continuation cap.
pub fn record_wake_passes(passes: usize, cause: WakeCause) {
    if crate::otel::metrics_enabled() {
        METRICS.passes_per_wake.record(passes as u64, &ATTRIBUTES[cause as usize]);
    }
}

/// Fault isolation lets the pass continue, but its root must still be retained as an error.
pub fn record_error() {
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

pub struct Operation {
    started: Option<Instant>,
    task: usize,
    pub span: tracing::Span,
}
impl Operation {
    pub fn new(deadline: bool, cause: WakeCause) -> Self {
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
    pub fn error(&self) {
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
    fn member_timings_aggregate_without_member_spans() {
        let mut phase = Phase { _profile: None, span: None, members: Some(Default::default()) };
        drop(phase.member(true));
        drop(phase.member(false));
        drop(phase.member(false));
        let members = phase.members.as_ref().unwrap();
        assert_eq!(members[0].count, 1);
        assert_eq!(members[1].count, 2);
        assert_eq!(members[0].total, members[0].max);
        assert!(members[1].total >= members[1].max);
        assert!(phase.span.is_none());
    }

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
            "api",
            "replication_receive",
            "deadline",
            "timer_restart",
            "timer_step_timeout",
            "timer_resume_verification",
            "timer_gate_timeout",
            "timer_gate_recheck",
            "timer_gate",
            "timer_gate_poll",
            "timer_llm_gate",
            "file_watch",
            "reconciler",
            "recorder_receipts",
            "startup",
            "continuation",
            "other",
        ];
        for (index, cause) in WakeCause::ALL.into_iter().enumerate() {
            assert_eq!(cause as usize, index);
            assert_eq!(WakeCause::from(cause.legacy_str()), cause);
            assert_eq!(cause.otel_str(), otel_values[index]);
            assert_eq!(
                ATTRIBUTES[index][0],
                KeyValue::new("cause", otel_values[index])
            );
        }
    }
}
