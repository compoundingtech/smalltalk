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
use crate::replication::ReplicationHealReport;

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
    heals: Counter<u64>,
    heal_duration: Histogram<f64>,
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
        heals: meter
            .u64_counter("st.replication.heals")
            .with_unit("{heal}")
            .build(),
        heal_duration: meter
            .f64_histogram("st.replication.heal.duration")
            .with_unit("s")
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

fn root_span<T: opentelemetry::trace::Tracer>(tracer: &T, peer: Arc<str>, heal: bool) -> T::Span {
    let (name, label) = if heal {
        ("st.replication.heal", "heal")
    } else {
        ("st.replication.round", "round")
    };
    // An explicit empty context detaches even from an attached ambient OTel span.
    let mut span = tracer.start_with_context(name, &opentelemetry::Context::new());
    span.set_attribute(KeyValue::new("span.label", label));
    span.set_attribute(KeyValue::new("st.replication.peer", peer));
    span
}

struct Recording {
    span: Option<opentelemetry::global::BoxedSpan>,
    // The span and metric labels share the peer bytes; only metrics need a clock.
    metrics: Option<(KeyValue, Instant)>,
}
impl Recording {
    fn new(peer: &str, heal: bool, export: bool, metrics: bool) -> Option<Self> {
        if !export && !metrics {
            return None;
        }
        let peer = Arc::<str>::from(peer);
        let span = export.then(|| root_span(&*TRACER, peer.clone(), heal));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HealOutcome {
    Repaired,
    Unchanged,
    Failed,
    Cancelled,
}
impl HealOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Repaired => "repaired",
            Self::Unchanged => "unchanged",
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
                false,
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
    pub(super) fn finish(&mut self, result: &anyhow::Result<(bool, bool)>) {
        let (outcome, heal_due, error) = match result {
            Ok((moved, heal_due)) => (
                if *moved {
                    RoundOutcome::Moved
                } else {
                    RoundOutcome::InSync
                },
                *heal_due,
                None,
            ),
            Err(error) => (RoundOutcome::Failed, false, Some(error)),
        };
        self.finalize(outcome, heal_due, error);
    }
    fn finalize(&mut self, outcome: RoundOutcome, heal_due: bool, error: Option<&anyhow::Error>) {
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
            span.set_attribute(KeyValue::new("st.replication.heal_due", heal_due));
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
        self.finalize(RoundOutcome::Cancelled, false, None);
    }
}

pub(super) struct Heal {
    recording: Option<Recording>,
    questions: u64,
    outcome: HealOutcome,
    error_recorded: bool,
}
impl Heal {
    pub(super) fn new(peer: &str) -> Self {
        Self {
            recording: Recording::new(
                peer,
                true,
                EXPORT_ENABLED.load(Ordering::Relaxed),
                METRICS_ENABLED.load(Ordering::Relaxed),
            ),
            questions: 0,
            outcome: HealOutcome::Failed,
            error_recorded: false,
        }
    }
    pub(super) fn question(&mut self) {
        if self.recording.is_some() {
            self.questions += 1;
        }
    }
    pub(super) fn error(&mut self, error: &anyhow::Error) {
        if let Some(recording) = &self.recording {
            if !self.error_recorded && recording.metrics.is_some() {
                recording.error(error_reason(error));
            }
            self.error_recorded = true;
        }
    }
    pub(super) fn complete(&mut self, report: &ReplicationHealReport) {
        if self.recording.is_none() {
            return;
        }
        self.outcome = if !report.healed || self.error_recorded {
            HealOutcome::Failed
        } else if report.refetched != 0
            || report.pushed != 0
            || report.replayed
            || report.peer_replayed
        {
            HealOutcome::Repaired
        } else {
            HealOutcome::Unchanged
        };
    }
    pub(super) fn finish(&mut self) {
        self.finalize(self.outcome);
    }
    fn finalize(&mut self, outcome: HealOutcome) {
        let Some(mut recording) = self.recording.take() else {
            return;
        };
        if outcome == HealOutcome::Failed {
            if let Some(span) = &mut recording.span {
                span.set_status(opentelemetry::trace::Status::error(
                    "replication heal failed",
                ));
            }
            if !self.error_recorded {
                recording.error(ErrorReason::Other);
            }
        }
        if let Some(span) = &mut recording.span {
            span.set_attribute(KeyValue::new(
                "st.replication.questions",
                i64::try_from(self.questions).unwrap_or(i64::MAX),
            ));
            span.set_attribute(KeyValue::new("st.replication.outcome", outcome.as_str()));
            span.end();
        }
        if let Some((peer, started)) = &recording.metrics {
            let attributes = [peer.clone(), KeyValue::new("outcome", outcome.as_str())];
            METRICS.heals.add(1, &attributes);
            METRICS
                .heal_duration
                .record(started.elapsed().as_secs_f64(), &attributes);
        }
    }
}
impl Drop for Heal {
    fn drop(&mut self) {
        self.finalize(HealOutcome::Cancelled);
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
    fn heal_outcomes_distinguish_repairs_and_unresolved_graphs() {
        let mut heal = Heal {
            recording: Some(Recording {
                span: None,
                metrics: None,
            }),
            questions: 0,
            outcome: HealOutcome::Failed,
            error_recorded: false,
        };
        let mut report = ReplicationHealReport {
            healed: true,
            ..Default::default()
        };
        heal.complete(&report);
        assert_eq!(heal.outcome, HealOutcome::Unchanged);
        report.refetched = 1;
        heal.complete(&report);
        assert_eq!(heal.outcome, HealOutcome::Repaired);
        report.refetched = 0;
        report.peer_replayed = true;
        heal.complete(&report);
        assert_eq!(heal.outcome, HealOutcome::Repaired);
        report.healed = false;
        heal.complete(&report);
        assert_eq!(heal.outcome, HealOutcome::Failed);
        report.healed = true;
        heal.error(&anyhow::anyhow!("request failed"));
        heal.complete(&report);
        assert_eq!(heal.outcome, HealOutcome::Failed);
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
                    span: Some(root_span(&tracer, Arc::from("cedar"), false)),
                    metrics: None,
                }),
                moved: 0,
            };
            round.finish(&Ok((false, false)));
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
        assert_eq!(
            [
                HealOutcome::Repaired,
                HealOutcome::Unchanged,
                HealOutcome::Failed,
                HealOutcome::Cancelled
            ]
            .map(HealOutcome::as_str),
            ["repaired", "unchanged", "failed", "cancelled"],
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

    fn test_recording(
        tracer: &opentelemetry::global::BoxedTracer,
        heal: bool,
    ) -> Option<Recording> {
        Some(Recording {
            span: Some(root_span(tracer, Arc::from("cedar"), heal)),
            metrics: None,
        })
    }

    #[test]
    fn dropping_unfinished_guards_exports_cancelled_without_error() {
        let spans = capture_guard_spans(|tracer| {
            drop(Round {
                recording: test_recording(tracer, false),
                moved: 3,
            });
            drop(Heal {
                recording: test_recording(tracer, true),
                questions: 2,
                outcome: HealOutcome::Failed,
                error_recorded: false,
            });
        });
        assert_eq!(spans.len(), 2);
        for name in ["st.replication.round", "st.replication.heal"] {
            let matching: Vec<_> = spans.iter().filter(|span| span.name == name).collect();
            assert_eq!(matching.len(), 1);
            let span = matching[0];
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
    }

    #[test]
    fn explicit_completion_and_drop_finalize_each_guard_once() {
        let spans = capture_guard_spans(|tracer| {
            let mut round = Round {
                recording: test_recording(tracer, false),
                moved: 0,
            };
            round.finish(&Ok((false, false)));
            assert!(round.recording.is_none());
            round.finish(&Err(anyhow::anyhow!("already finished")));
            let mut heal = Heal {
                recording: test_recording(tracer, true),
                questions: 1,
                outcome: HealOutcome::Unchanged,
                error_recorded: false,
            };
            heal.finish();
            assert!(heal.recording.is_none());
            heal.finish();
        });
        assert_eq!(spans.len(), 2);
        for (name, outcome) in [
            ("st.replication.round", "in_sync"),
            ("st.replication.heal", "unchanged"),
        ] {
            let span = spans.iter().find(|span| span.name == name).unwrap();
            assert_eq!(span.status, opentelemetry::trace::Status::Unset);
            assert!(
                span.attributes
                    .iter()
                    .any(
                        |attribute| attribute.key.as_str() == "st.replication.outcome"
                            && attribute.value.as_str() == outcome
                    )
            );
        }
    }

    #[test]
    fn disabled_operation_has_no_recording() {
        assert!(Recording::new("cedar", false, false, false).is_none());
        assert!(Recording::new("cedar", true, false, false).is_none());
    }
}
