//! Bounded local-root operations and metrics for the independently running sync worker.
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram},
    trace::Span as _,
};

use super::worker::{PeerOverloaded, RemovedFromFleet};

static EXPORT_ENABLED: AtomicBool = AtomicBool::new(false);
static METRICS_ENABLED: AtomicBool = AtomicBool::new(false);
static TRACER: LazyLock<opentelemetry::global::BoxedTracer> =
    LazyLock::new(|| opentelemetry::global::tracer("smallclaims.sync"));

/// Arm the independent signal gates after installing the process's providers.
/// The unset path never constructs spans, instruments, peer labels, or clocks.
pub fn init(export_enabled: bool, metrics_enabled: bool) {
    if export_enabled {
        LazyLock::force(&TRACER);
    }
    if metrics_enabled {
        LazyLock::force(&METRICS);
    }
    EXPORT_ENABLED.store(export_enabled, Ordering::Relaxed);
    METRICS_ENABLED.store(metrics_enabled, Ordering::Relaxed);
}

struct Instruments {
    rounds: Counter<u64>,
    round_duration: Histogram<f64>,
    batch_size: Histogram<u64>,
    errors: Counter<u64>,
}
static METRICS: LazyLock<Instruments> = LazyLock::new(|| {
    let meter = opentelemetry::global::meter("smallclaims.replication");
    Instruments {
        rounds: meter
            .u64_counter("st.replication.rounds")
            .with_unit("{round}")
            .build(),
        round_duration: meter
            .f64_histogram("st.replication.round.duration")
            .with_unit("s")
            .build(),
        batch_size: meter
            .u64_histogram("st.replication.batch.size")
            .with_unit("{envelope}")
            .with_boundaries(vec![1., 2., 4., 8., 16., 32., 64., 128., 256., 512., 1024.])
            .build(),
        errors: meter
            .u64_counter("st.replication.errors")
            .with_unit("{error}")
            .build(),
    }
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ErrorReason {
    Down,
    Overloaded,
    AuthFailed,
    Refused,
    Removed,
    Timeout,
    Invalid,
    Other,
}
impl ErrorReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Overloaded => "overloaded",
            Self::AuthFailed => "auth_failed",
            Self::Refused => "refused",
            Self::Removed => "removed",
            Self::Timeout => "timeout",
            Self::Invalid => "invalid",
            Self::Other => "other",
        }
    }
}

fn error_reason(error: &anyhow::Error) -> ErrorReason {
    if error.is::<RemovedFromFleet>() {
        return ErrorReason::Removed;
    }
    if error.is::<PeerOverloaded>() {
        return ErrorReason::Overloaded;
    }
    if error.is::<crate::fleet::transport::FabricGrantRefusal>() {
        return ErrorReason::Refused;
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return if error.is_timeout() {
            ErrorReason::Timeout
        } else {
            ErrorReason::Down
        };
    }
    if error.is::<serde_json::Error>() {
        return ErrorReason::Invalid;
    }
    // Match the existing dialer's authentication classification, without exporting prose.
    let message = error.to_string();
    // Existing durable worker failure/phase spellings, not arbitrary error prose.
    match message.as_str() {
        "down" => return ErrorReason::Down,
        "overloaded" | "overload" => return ErrorReason::Overloaded,
        "auth-failed" => return ErrorReason::AuthFailed,
        "refused" => return ErrorReason::Refused,
        "member-removed" | "member-left" => return ErrorReason::Removed,
        _ => {}
    }
    if message.contains("signature")
        || message.contains("fleet")
        || message.contains("member authentication")
    {
        ErrorReason::AuthFailed
    } else if message == "the peer API version differs"
        || message.contains("inflate the exchange body")
        || message.starts_with("the exchange body inflates past ")
    {
        ErrorReason::Invalid
    } else {
        ErrorReason::Other
    }
}

pub(super) fn record_error(peer: &str, reason: ErrorReason) {
    if METRICS_ENABLED.load(Ordering::Relaxed) {
        METRICS.errors.add(
            1,
            &[
                KeyValue::new("peer", Arc::<str>::from(peer)),
                KeyValue::new("reason", reason.as_str()),
            ],
        );
    }
}

fn root_span<T: opentelemetry::trace::Tracer>(tracer: &T, peer: Arc<str>) -> T::Span {
    // An explicit empty context detaches even from an attached ambient OTel span.
    let mut span =
        tracer.start_with_context("st.replication.round", &opentelemetry::Context::new());
    span.set_attribute(KeyValue::new("span.label", "round"));
    span.set_attribute(KeyValue::new("st.replication.peer", peer));
    span
}

struct Recording {
    span: Option<opentelemetry::global::BoxedSpan>,
    // The span and metric labels share the peer bytes; only metrics need a clock.
    metrics: Option<(KeyValue, Instant)>,
}
impl Recording {
    fn new(peer: &str, export: bool, metrics: bool) -> Option<Self> {
        if !export && !metrics {
            return None;
        }
        let peer = Arc::<str>::from(peer);
        let span = export.then(|| root_span(&*TRACER, peer.clone()));
        Some(Self {
            span,
            metrics: metrics.then(|| (KeyValue::new("peer", peer), Instant::now())),
        })
    }
    fn error(&self, reason: ErrorReason) {
        if let Some((peer, _)) = &self.metrics {
            METRICS
                .errors
                .add(1, &[peer.clone(), KeyValue::new("reason", reason.as_str())]);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoundOutcome {
    Moved,
    InSync,
    Failed,
    Cancelled,
}
impl RoundOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Moved => "moved",
            Self::InSync => "in_sync",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

pub(super) struct Round {
    recording: Option<Recording>,
    moved: u64,
}
impl Round {
    pub(super) fn new(peer: &str) -> Self {
        Self {
            recording: Recording::new(
                peer,
                EXPORT_ENABLED.load(Ordering::Relaxed),
                METRICS_ENABLED.load(Ordering::Relaxed),
            ),
            moved: 0,
        }
    }
    pub(super) fn pull(&mut self, size: usize) {
        self.batch(size, "pull", true);
    }
    pub(super) fn push(&mut self, size: usize, moved: bool) {
        self.batch(size, "push", moved);
    }
    fn batch(&mut self, size: usize, direction: &'static str, moved: bool) {
        let Some(recording) = &self.recording else {
            return;
        };
        if moved {
            self.moved = self.moved.saturating_add(size as u64);
        }
        if let Some((peer, _)) = &recording.metrics {
            METRICS.batch_size.record(
                size as u64,
                &[peer.clone(), KeyValue::new("direction", direction)],
            );
        }
    }
    pub(super) fn finish(&mut self, result: &anyhow::Result<bool>) {
        let (outcome, error) = match result {
            Ok(true) => (RoundOutcome::Moved, None),
            Ok(false) => (RoundOutcome::InSync, None),
            Err(error) => (RoundOutcome::Failed, Some(error)),
        };
        self.finalize(outcome, error);
    }
    fn finalize(&mut self, outcome: RoundOutcome, error: Option<&anyhow::Error>) {
        // Taking the recording makes explicit completion and Drop mutually exclusive.
        let Some(mut recording) = self.recording.take() else {
            return;
        };
        if outcome == RoundOutcome::Failed {
            if let Some(span) = &mut recording.span {
                span.set_status(opentelemetry::trace::Status::error(
                    "replication round failed",
                ));
            }
            if let Some(error) = error
                && recording.metrics.is_some()
            {
                recording.error(error_reason(error));
            }
        }
        let outcome = outcome.as_str();
        if let Some(span) = &mut recording.span {
            span.set_attribute(KeyValue::new("st.replication.outcome", outcome));
            span.set_attribute(KeyValue::new(
                "st.replication.moved_envelopes",
                i64::try_from(self.moved).unwrap_or(i64::MAX),
            ));
            span.end();
        }
        if let Some((peer, started)) = &recording.metrics {
            let attributes = [peer.clone(), KeyValue::new("outcome", outcome)];
            METRICS.rounds.add(1, &attributes);
            METRICS
                .round_duration
                .record(started.elapsed().as_secs_f64(), &attributes);
        }
    }
}
impl Drop for Round {
    fn drop(&mut self) {
        self.finalize(RoundOutcome::Cancelled, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Tracer as _, TracerProvider as _};

    #[test]
    fn existing_failure_kinds_have_closed_reasons() {
        for (kind, expected) in [
            ("down", ErrorReason::Down),
            ("overloaded", ErrorReason::Overloaded),
            ("overload", ErrorReason::Overloaded),
            ("auth-failed", ErrorReason::AuthFailed),
            ("refused", ErrorReason::Refused),
            ("member-removed", ErrorReason::Removed),
            ("member-left", ErrorReason::Removed),
        ] {
            assert_eq!(error_reason(&anyhow::anyhow!("{kind}")), expected, "{kind}");
            assert_ne!(expected, ErrorReason::Other);
        }
        assert_eq!(
            error_reason(&anyhow::anyhow!("unknown")),
            ErrorReason::Other
        );
        assert_eq!(
            error_reason(
                &PeerOverloaded {
                    retry_after: std::time::Duration::from_secs(5)
                }
                .into()
            ),
            ErrorReason::Overloaded
        );
        assert_eq!(
            error_reason(
                &RemovedFromFleet {
                    code: "member-left".into(),
                    message: String::new()
                }
                .into()
            ),
            ErrorReason::Removed
        );
        let invalid = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(error_reason(&invalid.into()), ErrorReason::Invalid);
        assert_eq!(
            error_reason(&anyhow::anyhow!("the peer API version differs")),
            ErrorReason::Invalid
        );
        // An inflate refusal that names its path and size is still an invalid body.
        assert_eq!(
            error_reason(&anyhow::anyhow!(
                "the exchange body inflates past 67108864 bytes (the answer to \
                 `/v1/peer/exchange` from peer invented-peer, 912345 compressed bytes)"
            )),
            ErrorReason::Invalid
        );
        assert_eq!(
            error_reason(&anyhow::anyhow!("bad signature")),
            ErrorReason::AuthFailed
        );
        assert_eq!(
            error_reason(&anyhow::anyhow!("unclassified")),
            ErrorReason::Other
        );
    }

    #[test]
    fn round_is_detached_from_attached_ambient_context() {
        use opentelemetry::trace::TraceContextExt as _;
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer =
            opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("replication.test")));
        {
            let ambient = tracer.start("ambient");
            let _context = opentelemetry::Context::current_with_span(ambient).attach();
            let mut round = Round {
                recording: Some(Recording {
                    span: Some(root_span(&tracer, Arc::from("cedar"))),
                    metrics: None,
                }),
                moved: 0,
            };
            round.finish(&Ok(false));
        }
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let round = spans
            .iter()
            .find(|span| span.name == "st.replication.round")
            .unwrap();
        let ambient = spans.iter().find(|span| span.name == "ambient").unwrap();
        assert_eq!(round.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        assert_ne!(
            round.span_context.trace_id(),
            ambient.span_context.trace_id()
        );
        assert!(
            round
                .attributes
                .iter()
                .any(
                    |attribute| attribute.key.as_str() == "st.replication.outcome"
                        && attribute.value.as_str() == "in_sync"
                )
        );
    }

    #[test]
    fn operation_outcomes_are_closed_and_include_cancellation() {
        assert_eq!(
            [
                RoundOutcome::Moved,
                RoundOutcome::InSync,
                RoundOutcome::Failed,
                RoundOutcome::Cancelled
            ]
            .map(RoundOutcome::as_str),
            ["moved", "in_sync", "failed", "cancelled"],
        );
    }

    fn capture_guard_spans(
        work: impl FnOnce(&opentelemetry::global::BoxedTracer),
    ) -> Vec<opentelemetry_sdk::trace::SpanData> {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer =
            opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("replication.test")));
        work(&tracer);
        provider.force_flush().unwrap();
        exporter.get_finished_spans().unwrap()
    }

    fn test_recording(tracer: &opentelemetry::global::BoxedTracer) -> Option<Recording> {
        Some(Recording {
            span: Some(root_span(tracer, Arc::from("cedar"))),
            metrics: None,
        })
    }

    #[test]
    fn dropping_unfinished_guards_exports_cancelled_without_error() {
        let spans = capture_guard_spans(|tracer| {
            drop(Round {
                recording: test_recording(tracer),
                moved: 3,
            });
        });
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "st.replication.round");
        assert_eq!(span.status, opentelemetry::trace::Status::Unset);
        assert!(
            span.attributes
                .iter()
                .any(
                    |attribute| attribute.key.as_str() == "st.replication.outcome"
                        && attribute.value.as_str() == "cancelled"
                )
        );
    }

    #[test]
    fn explicit_completion_and_drop_finalize_each_guard_once() {
        let spans = capture_guard_spans(|tracer| {
            let mut round = Round {
                recording: test_recording(tracer),
                moved: 0,
            };
            round.finish(&Ok(false));
            assert!(round.recording.is_none());
            round.finish(&Err(anyhow::anyhow!("already finished")));
        });
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "st.replication.round");
        assert_eq!(span.status, opentelemetry::trace::Status::Unset);
        assert!(
            span.attributes
                .iter()
                .any(
                    |attribute| attribute.key.as_str() == "st.replication.outcome"
                        && attribute.value.as_str() == "in_sync"
                )
        );
    }

    #[test]
    fn disabled_operation_has_no_recording() {
        assert!(Recording::new("cedar", false, false).is_none());
    }
}
