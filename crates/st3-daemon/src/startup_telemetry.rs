//! Bounded daemon startup spans, closed before the long-lived servers run.
use opentelemetry::{KeyValue, metrics::Histogram};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::registry::LookupSpan as _;

/// Update the deferred OTel builder before the span is entered or closed.
fn span_times(span: &tracing::Span, started: SystemTime, ended: Option<SystemTime>) {
    span.with_subscriber(|(id, subscriber)| {
        let Some(registry) = subscriber.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        let Some(span) = registry.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(data) = extensions.get_mut::<tracing_opentelemetry::OtelData>() {
            data.builder.start_time = Some(started);
            data.builder.end_time = ended;
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Phase {
    NodeIdentity,
    InstallHooks,
    OpenStore,
    JudgeClaims,
    ValidateReplicationBacklog,
    ApplyReplicationRepairs,
    SettleRuns,
    ProjectReplicationBacklog,
    InitializeRuntime,
    StartServices,
    BindListeners,
    Other,
    Total,
}
impl Phase {
    const ALL: [Self; 13] = [
        Self::NodeIdentity,
        Self::InstallHooks,
        Self::OpenStore,
        Self::JudgeClaims,
        Self::ValidateReplicationBacklog,
        Self::ApplyReplicationRepairs,
        Self::SettleRuns,
        Self::ProjectReplicationBacklog,
        Self::InitializeRuntime,
        Self::StartServices,
        Self::BindListeners,
        Self::Other,
        Self::Total,
    ];
    const fn name(self) -> &'static str {
        match self {
            Self::NodeIdentity => "node_identity",
            Self::InstallHooks => "install_hooks",
            Self::OpenStore => "open_store",
            Self::JudgeClaims => "judge_claims",
            Self::ValidateReplicationBacklog => "validate_replication_backlog",
            Self::ApplyReplicationRepairs => "apply_replication_repairs",
            Self::SettleRuns => "settle_runs",
            Self::ProjectReplicationBacklog => "project_replication_backlog",
            Self::InitializeRuntime => "initialize_runtime",
            Self::StartServices => "start_services",
            Self::BindListeners => "bind_listeners",
            Self::Other => "other",
            Self::Total => "total",
        }
    }
}
impl From<&str> for Phase {
    fn from(name: &str) -> Self {
        match name {
            "node-identity" => Self::NodeIdentity,
            "install-hooks" => Self::InstallHooks,
            "open-store" => Self::OpenStore,
            "judge-claims" => Self::JudgeClaims,
            "validate-replication-backlog" => Self::ValidateReplicationBacklog,
            "apply-replication-repairs" => Self::ApplyReplicationRepairs,
            "settle-runs" => Self::SettleRuns,
            "project-replication-backlog" => Self::ProjectReplicationBacklog,
            "initialize-runtime" => Self::InitializeRuntime,
            "start-services" => Self::StartServices,
            "bind-listeners" => Self::BindListeners,
            _ => Self::Other,
        }
    }
}
static ATTRIBUTES: LazyLock<[[KeyValue; 1]; Phase::ALL.len()]> =
    LazyLock::new(|| Phase::ALL.map(|phase| [KeyValue::new("phase", phase.name())]));
static DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("st3.startup")
        .f64_histogram("st.startup.duration")
        .with_unit("s")
        .with_boundaries(vec![
            0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
            600.0, 1200.0, 1800.0,
        ])
        .build()
});
struct TimedSpan {
    span: tracing::Span,
    started: Option<Instant>,
    phase: Phase,
}
impl Drop for TimedSpan {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            DURATION.record(
                started.elapsed().as_secs_f64(),
                &ATTRIBUTES[self.phase as usize],
            );
        }
    }
}
struct State {
    root: Option<TimedSpan>,
    phase: Option<TimedSpan>,
}
/// No entered guard crosses an await or leaks startup context into spawned services.
pub struct StartupTelemetry(Mutex<State>);
impl Default for StartupTelemetry {
    fn default() -> Self {
        Self::begin()
    }
}
impl StartupTelemetry {
    pub fn begin() -> Self {
        Self::begin_at(SystemTime::now(), Instant::now())
    }
    /// Identity recovery precedes telemetry initialization but belongs to this root.
    pub fn begin_at(started: SystemTime, monotonic_started: Instant) -> Self {
        let span = if crate::otel::export_enabled() {
            let span = tracing::info_span!(parent: None, "st.startup",
                "span.label" = "startup", "st.startup.outcome" = tracing::field::Empty);
            span.set_parent(opentelemetry::Context::new());
            span_times(&span, started, None);
            span
        } else {
            tracing::Span::none()
        };
        Self(Mutex::new(State {
            root: Some(TimedSpan {
                span,
                started: crate::otel::metrics_enabled().then_some(monotonic_started),
                phase: Phase::Total,
            }),
            phase: None,
        }))
    }
    /// Export the already-completed recovery without running it again.
    pub fn node_identity(
        &self,
        started: SystemTime,
        ended: SystemTime,
        duration: Duration,
        failed: bool,
    ) {
        self.phase("node-identity");
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut phase) = state.phase.take() {
            span_times(&phase.span, started, Some(ended));
            if failed {
                phase.span
                    .set_status(opentelemetry::trace::Status::error("startup failed"));
            }
            phase.started = None;
            if crate::otel::metrics_enabled() {
                DURATION.record(duration.as_secs_f64(), &ATTRIBUTES[phase.phase as usize]);
            }
        }
    }
    pub fn phase(&self, name: &str) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.phase.take();
        let Some(root) = &state.root else { return };
        let phase = Phase::from(name);
        let span = if crate::otel::export_enabled() {
            tracing::info_span!(parent: &root.span, "st.startup.phase",
                "span.label" = phase.name(), "st.startup.phase" = phase.name(),
                "st.startup.claims_processed" = tracing::field::Empty,
                "st.startup.full_replay" = tracing::field::Empty)
        } else {
            tracing::Span::none()
        };
        if phase == Phase::ProjectReplicationBacklog {
            span.record("st.startup.claims_processed", 0_i64);
            span.record("st.startup.full_replay", false);
        }
        state.phase = Some(TimedSpan {
            span,
            started: crate::otel::metrics_enabled().then(Instant::now),
            phase,
        });
    }
    /// Keep the opt-in profiler task intact while entering the corresponding phase.
    pub fn task<T>(&self, name: &'static str, task: impl FnOnce() -> T) -> T {
        let span = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase
            .as_ref()
            .map(|phase| phase.span.clone())
            .unwrap_or_else(tracing::Span::none);
        span.in_scope(|| crate::profile::task(name, task))
    }
    pub fn progress(&self, progress: smallclaims::store::ProjectionProgress) {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(phase) = &state.phase {
            if progress.phase.starts_with("full-replay") {
                phase.span.record("st.startup.full_replay", true);
            }
            if let Some(processed) = progress.processed {
                phase.span.record(
                    "st.startup.claims_processed",
                    i64::try_from(processed).unwrap_or(i64::MAX),
                );
            }
        }
    }
    pub fn serving(&self) {
        self.finish(true);
    }
    fn finish(&self, serving: bool) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !serving && let Some(phase) = &state.phase {
            phase.span
                .set_status(opentelemetry::trace::Status::error("startup failed"));
        }
        state.phase.take();
        if let Some(root) = state.root.take() {
            root.span.record(
                "st.startup.outcome",
                if serving { "serving" } else { "failed" },
            );
            if !serving {
                root.span
                    .set_status(opentelemetry::trace::Status::error("startup failed"));
            }
        }
    }
}
impl Drop for StartupTelemetry {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_call_site_phases_have_closed_labels() {
        let main = include_str!("main.rs");
        let phases: Vec<_> = main
            .split("startup_telemetry.phase(\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(phases.len(), 10);
        for name in phases {
            assert_ne!(Phase::from(name), Phase::Other, "unmapped phase: {name}");
        }
        assert_eq!(Phase::from("unknown"), Phase::Other);
        assert_eq!(Phase::from("node-identity"), Phase::NodeIdentity);
    }

    #[test]
    fn startup_identity_preserves_measured_timestamps_and_failure() {
        let started = SystemTime::now() - Duration::from_secs(2);
        let ended = started + Duration::from_millis(50);
        for failed in [false, true] {
            let spans = crate::otel::capture_test_spans(|| {
                let startup = StartupTelemetry::begin_at(started, Instant::now());
                startup.node_identity(started, ended, Duration::from_millis(50), failed);
                if !failed {
                    startup.serving();
                }
            });
            let root = spans.iter().find(|span| span.name == "st.startup").unwrap();
            let identity = spans.iter().find(|span| span.name == "st.startup.phase").unwrap();
            assert_eq!(root.start_time, started);
            assert_eq!(identity.start_time, started);
            assert_eq!(identity.end_time, ended);
            assert_eq!(identity.parent_span_id, root.span_context.span_id());
            assert!(identity.attributes.contains(&KeyValue::new("st.startup.phase", "node_identity")));
            assert_eq!(matches!(identity.status, opentelemetry::trace::Status::Error { .. }), failed);
            assert_eq!(matches!(root.status, opentelemetry::trace::Status::Error { .. }), failed);
        }
    }

    #[test]
    fn startup_failure_exports_error_and_serving_closes_root() {
        for serving in [false, true] {
            let spans = crate::otel::capture_test_spans(|| {
                let startup = StartupTelemetry::begin();
                startup.phase("open-store");
                if serving {
                    startup.serving();
                }
            });
            let root = spans.iter().find(|span| span.name == "st.startup").unwrap();
            assert_eq!(root.parent_span_id, opentelemetry::trace::SpanId::INVALID);
            assert!(root.attributes.contains(&KeyValue::new(
                "st.startup.outcome",
                if serving { "serving" } else { "failed" }
            )));
            assert_eq!(
                matches!(root.status, opentelemetry::trace::Status::Error { .. }),
                !serving
            );
            let phase = spans
                .iter()
                .find(|span| span.name == "st.startup.phase")
                .unwrap();
            assert_eq!(phase.parent_span_id, root.span_context.span_id());
            assert_eq!(
                matches!(phase.status, opentelemetry::trace::Status::Error { .. }),
                !serving
            );
        }
    }
}
