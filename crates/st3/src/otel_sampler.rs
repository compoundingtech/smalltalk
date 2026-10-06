//! Bounded, process-local tail sampling; propagation retains the head decision.
//!
//! Verified against opentelemetry_sdk 0.30.0: trace/tracer.rs:261-275 builds a
//! recording span for RecordOnly, and trace/span.rs:200-237 calls on_end without
//! a sampled-bit gate. tracing-opentelemetry 0.31.0 src/tracer.rs:79-91 consults
//! this sampler during pre-sampling; :137-141 propagates RecordOnly as sampled=0.
//! Thus unsampled spans can be buffered without lying to downstream services.
//! At export only, kept spans acquire sampled=1 (the SDK SimpleSpanProcessor
//! filters unsampled data at trace/span_processor.rs:136-139; BatchSpanProcessor
//! accepts it at :516-525). This never changes the live propagation context.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use opentelemetry::trace::{
    Link, SamplingDecision, SamplingResult, Span as _, SpanContext, SpanId, SpanKind, Status,
    TraceContextExt, TraceId,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Sampler, ShouldSample, Span, SpanData, SpanProcessor};
use parking_lot::{Mutex, MutexGuard};

const RECENT_DECISION_CAP: usize = 4096;
const DECISION_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct TailSamplingConfig {
    pub slow_threshold: Duration,
    pub ratio: f64,
    pub per_trace_span_cap: usize,
    pub max_buffered_spans: usize,
    pub parent_keep_per_sec: u32,
}

impl Default for TailSamplingConfig {
    fn default() -> Self {
        Self {
            slow_threshold: Duration::from_secs(1),
            ratio: 0.01,
            per_trace_span_cap: 512,
            max_buffered_spans: 20_000,
            parent_keep_per_sec: 20,
        }
    }
}

#[derive(Clone, Debug)]
struct HeadSampler {
    ratio: Sampler,
}

pub fn head_sampler(config: &TailSamplingConfig) -> impl ShouldSample + Clone + 'static {
    HeadSampler {
        ratio: Sampler::TraceIdRatioBased(config.ratio),
    }
}

impl ShouldSample for HeadSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let mut result =
            self.ratio
                .should_sample(parent_context, trace_id, name, span_kind, attributes, links);
        let parent_decision = parent_context.and_then(|cx| {
            let parent = cx.span();
            let context = parent.span_context();
            if !context.is_valid() {
                None
            } else if context.is_sampled() {
                Some(SamplingDecision::RecordAndSample)
            } else if !context.is_remote() {
                Some(SamplingDecision::RecordOnly)
            } else {
                None
            }
        });
        if let Some(decision) = parent_decision {
            result.decision = decision;
        } else if result.decision == SamplingDecision::Drop {
            result.decision = SamplingDecision::RecordOnly;
        }
        result
    }
}

/// Span counts, except parent_keep_throttled which counts local roots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TailSamplingStats {
    pub kept: u64,
    pub dropped: u64,
    pub overflow_dropped: u64,
    pub parent_keep_throttled: u64,
}

#[derive(Debug)]
struct PendingTrace {
    root: SpanId,
    head_keep: bool,
    error: bool,
    order: u64,
    spans: Vec<SpanData>,
}

#[derive(Debug)]
struct RecentDecision {
    keep: bool,
    decided_at: Instant,
    order: u64,
}

#[derive(Debug)]
struct State {
    pending: HashMap<TraceId, PendingTrace>,
    pending_order: BTreeMap<u64, TraceId>,
    recent: HashMap<TraceId, RecentDecision>,
    recent_order: BTreeMap<u64, TraceId>,
    sequence: u64,
    buffered: usize,
    tokens: f64,
    last_refill: Instant,
    shutdown: bool,
    stats: TailSamplingStats,
}

impl State {
    fn next_order(&mut self) -> u64 {
        let order = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        order
    }

    fn remember(&mut self, trace_id: TraceId, keep: bool, now: Instant) {
        if let Some(previous) = self.recent.remove(&trace_id) {
            self.recent_order.remove(&previous.order);
        }
        while self.recent.len() >= RECENT_DECISION_CAP {
            if let Some((_, oldest)) = self.recent_order.pop_first() {
                self.recent.remove(&oldest);
            }
        }
        let order = self.next_order();
        self.recent.insert(
            trace_id,
            RecentDecision {
                keep,
                decided_at: now,
                order,
            },
        );
        self.recent_order.insert(order, trace_id);
    }

    fn recent_decision(&mut self, trace_id: TraceId, now: Instant) -> Option<bool> {
        let decision = self.recent.get(&trace_id)?;
        let keep = decision.keep;
        let order = decision.order;
        let expired = now.duration_since(decision.decided_at) >= DECISION_GRACE;
        self.recent_order.remove(&order);
        if expired {
            self.recent.remove(&trace_id);
            return None;
        }
        let new_order = self.next_order();
        self.recent.get_mut(&trace_id)?.order = new_order;
        self.recent_order.insert(new_order, trace_id);
        Some(keep)
    }

    fn evict_oldest(&mut self, now: Instant) {
        if let Some((_, trace_id)) = self.pending_order.pop_first()
            && let Some(trace) = self.pending.remove(&trace_id)
        {
            let count = trace.spans.len();
            self.buffered -= count;
            self.stats.dropped += count as u64;
            self.stats.overflow_dropped += count as u64;
            self.remember(trace_id, false, now);
        }
    }

    fn parent_token(&mut self, rate: u32, now: Instant) -> bool {
        let capacity = f64::from(rate);
        self.tokens = (self.tokens + now.duration_since(self.last_refill).as_secs_f64() * capacity)
            .min(capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.stats.parent_keep_throttled += 1;
            false
        }
    }
}

#[derive(Debug)]
pub struct LocalRootTailSampler<P: SpanProcessor> {
    inner: P,
    config: TailSamplingConfig,
    state: Mutex<State>,
}

impl<P: SpanProcessor> LocalRootTailSampler<P> {
    pub fn new(inner: P, config: TailSamplingConfig) -> Self {
        let state = State {
            pending: HashMap::new(),
            pending_order: BTreeMap::new(),
            recent: HashMap::new(),
            recent_order: BTreeMap::new(),
            sequence: 0,
            buffered: 0,
            tokens: f64::from(config.parent_keep_per_sec),
            last_refill: Instant::now(),
            shutdown: false,
            stats: TailSamplingStats::default(),
        };
        Self {
            inner,
            config,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    pub fn stats(&self) -> TailSamplingStats {
        self.lock().stats
    }

    fn export(&self, mut span: SpanData) {
        let context = &span.span_context;
        span.span_context = SpanContext::new(
            context.trace_id(),
            context.span_id(),
            context.trace_flags().with_sampled(true),
            context.is_remote(),
            context.trace_state().clone(),
        );
        self.inner.on_end(span);
    }
}

impl<P: SpanProcessor> SpanProcessor for LocalRootTailSampler<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        let parent = cx.span();
        let parent_context = parent.span_context();
        if !span.is_recording() || (parent_context.is_valid() && !parent_context.is_remote()) {
            return;
        }
        let context = span.span_context();
        let now = Instant::now();
        let mut state = self.lock();
        if state.shutdown || state.pending.contains_key(&context.trace_id()) {
            return;
        }
        // Bound unfinished-root metadata too: a span need not ever end.
        while !state.pending.is_empty()
            && state.pending.len() >= self.config.max_buffered_spans.max(1)
        {
            state.evict_oldest(now);
        }
        let remote_sampled = parent_context.is_valid() && parent_context.is_sampled();
        let head_keep = if remote_sampled {
            state.parent_token(self.config.parent_keep_per_sec, now)
                || matches!(
                    Sampler::TraceIdRatioBased(self.config.ratio)
                        .should_sample(None, context.trace_id(), "", &SpanKind::Internal, &[], &[],)
                        .decision,
                    SamplingDecision::RecordAndSample
                )
        } else {
            context.is_sampled()
        };
        // A new local root supersedes a cached decision for the same distributed trace.
        if let Some(previous) = state.recent.remove(&context.trace_id()) {
            state.recent_order.remove(&previous.order);
        }
        let order = state.next_order();
        state.pending_order.insert(order, context.trace_id());
        state.pending.insert(
            context.trace_id(),
            PendingTrace {
                root: context.span_id(),
                head_keep,
                error: false,
                order,
                spans: Vec::new(),
            },
        );
    }

    fn on_end(&self, span: SpanData) {
        let trace_id = span.span_context.trace_id();
        let now = Instant::now();
        let kept = {
            let mut state = self.lock();
            if state.shutdown {
                state.stats.dropped += 1;
                return;
            }
            if !state.pending.contains_key(&trace_id) {
                if state.recent_decision(trace_id, now) == Some(true) {
                    state.stats.kept += 1;
                    drop(state);
                    self.export(span);
                } else {
                    state.stats.dropped += 1;
                }
                return;
            }
            let trace = state
                .pending
                .get_mut(&trace_id)
                .expect("pending trace exists");
            let is_root = trace.root == span.span_context.span_id();
            trace.error |= matches!(span.status, Status::Error { .. });
            let slow = is_root
                && span
                    .end_time
                    .duration_since(span.start_time)
                    .is_ok_and(|duration| duration > self.config.slow_threshold);
            let keep = trace.head_keep || trace.error || slow;
            let at_cap = trace.spans.len() >= self.config.per_trace_span_cap;
            if !at_cap {
                trace.spans.push(span);
                state.buffered += 1;
            } else {
                state.stats.dropped += 1;
                state.stats.overflow_dropped += 1;
            }
            // Enforce the global limit even when the current root is ending.
            while state.buffered > self.config.max_buffered_spans {
                state.evict_oldest(now);
            }
            if !is_root {
                return;
            }
            let Some(trace) = state.pending.remove(&trace_id) else {
                return; // It was just evicted; its cached decision is drop.
            };
            state.pending_order.remove(&trace.order);
            state.buffered -= trace.spans.len();
            state.remember(trace_id, keep, now);
            if keep {
                state.stats.kept += trace.spans.len() as u64;
                trace.spans
            } else {
                state.stats.dropped += trace.spans.len() as u64;
                Vec::new()
            }
        };
        for span in kept {
            self.export(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        // Pending roots are undecided; flushing must not invent their outcome.
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        {
            let mut state = self.lock();
            state.shutdown = true;
            state.stats.dropped += state.buffered as u64;
            state.pending.clear();
            state.pending_order.clear();
            state.recent.clear();
            state.recent_order.clear();
            state.buffered = 0;
        }
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{TraceFlags, TraceState, Tracer, TracerProvider};
    use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
    use std::sync::Arc;
    use std::time::SystemTime;

    // The workspace does not enable the SDK's `testing` feature.
    #[derive(Clone, Debug, Default)]
    struct RecordingProcessor(Arc<Mutex<Vec<SpanData>>>);

    impl SpanProcessor for RecordingProcessor {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, span: SpanData) {
            self.0.lock().push(span);
        }
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct SharedSampler(Arc<LocalRootTailSampler<RecordingProcessor>>);

    impl SpanProcessor for SharedSampler {
        fn on_start(&self, span: &mut Span, cx: &Context) {
            self.0.on_start(span, cx);
        }
        fn on_end(&self, span: SpanData) {
            self.0.on_end(span);
        }
        fn force_flush(&self) -> OTelSdkResult {
            self.0.force_flush()
        }
        fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
            self.0.shutdown_with_timeout(timeout)
        }
    }

    struct Harness {
        _provider: SdkTracerProvider,
        tracer: SdkTracer,
        sampler: Arc<LocalRootTailSampler<RecordingProcessor>>,
        output: RecordingProcessor,
    }

    impl Harness {
        fn new(config: TailSamplingConfig) -> Self {
            let output = RecordingProcessor::default();
            let sampler = Arc::new(LocalRootTailSampler::new(output.clone(), config.clone()));
            let provider = SdkTracerProvider::builder()
                .with_sampler(head_sampler(&config))
                .with_span_processor(SharedSampler(sampler.clone()))
                .build();
            let tracer = provider.tracer("tail-sampler-test");
            Self {
                _provider: provider,
                tracer,
                sampler,
                output,
            }
        }

        fn root(&self, id: u128, parent: &Context) -> Span {
            self.tracer
                .span_builder("root")
                .with_trace_id(TraceId::from(id))
                .with_start_time(SystemTime::UNIX_EPOCH)
                .start_with_context(&self.tracer, parent)
        }

        fn child(&self, parent: &Context) -> Span {
            self.tracer
                .span_builder("child")
                .with_start_time(SystemTime::UNIX_EPOCH)
                .start_with_context(&self.tracer, parent)
        }

        fn names(&self) -> Vec<String> {
            self.output
                .0
                .lock()
                .iter()
                .map(|span| span.name.to_string())
                .collect()
        }
    }

    fn config() -> TailSamplingConfig {
        TailSamplingConfig {
            ratio: 0.0,
            ..TailSamplingConfig::default()
        }
    }

    fn end(mut span: Span, millis: u64) {
        span.end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(millis));
    }

    #[test]
    fn fast_unsampled_trace_is_recorded_but_dropped() {
        let h = Harness::new(config());
        let root = h.root(1, &Context::new());
        assert!(root.is_recording());
        assert!(!root.span_context().is_sampled());
        end(root, 1000); // Threshold is strictly greater than one second.
        assert!(h.names().is_empty());
        assert_eq!(h.sampler.stats().dropped, 1);
    }

    #[test]
    fn slow_root_keeps_buffered_children() {
        let h = Harness::new(config());
        let parent = Context::new().with_span(h.root(2, &Context::new()));
        end(h.child(&parent), 10);
        assert!(h.names().is_empty());
        parent
            .span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(1001));
        assert_eq!(h.names(), ["child", "root"]);
        assert!(
            h.output
                .0
                .lock()
                .iter()
                .all(|span| span.span_context.is_sampled())
        );
        assert!(!parent.span().span_context().is_sampled());
    }

    #[test]
    fn error_child_keeps_fast_root() {
        let h = Harness::new(config());
        let parent = Context::new().with_span(h.root(3, &Context::new()));
        let mut child = h.child(&parent);
        child.set_status(Status::error("failed"));
        end(child, 10);
        parent
            .span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(20));
        assert_eq!(h.names(), ["child", "root"]);
    }

    #[test]
    fn ratio_one_keeps_all() {
        let h = Harness::new(TailSamplingConfig {
            ratio: 1.0,
            ..config()
        });
        let root = h.root(4, &Context::new());
        assert!(root.span_context().is_sampled());
        end(root, 1);
        assert_eq!(h.names(), ["root"]);
    }

    #[test]
    fn sampled_remote_parent_is_throttled() {
        let h = Harness::new(TailSamplingConfig {
            parent_keep_per_sec: 1,
            ..config()
        });
        for id in [5_u128, 6] {
            let parent = Context::new().with_remote_span_context(SpanContext::new(
                TraceId::from(id),
                SpanId::from(42),
                TraceFlags::SAMPLED,
                true,
                TraceState::default(),
            ));
            let root = h.root(id, &parent);
            assert!(root.span_context().is_sampled());
            end(root, 10);
        }
        assert_eq!(h.names(), ["root"]);
        assert_eq!(h.sampler.stats().parent_keep_throttled, 1);
    }

    #[test]
    fn per_trace_cap_counts_overflow_and_preserves_error_decision() {
        let h = Harness::new(TailSamplingConfig {
            per_trace_span_cap: 1,
            ..config()
        });
        let parent = Context::new().with_span(h.root(7, &Context::new()));
        end(h.child(&parent), 10);
        let mut error_child = h.child(&parent);
        error_child.set_status(Status::error("overflow error"));
        end(error_child, 20);
        parent
            .span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(30));
        assert_eq!(h.names(), ["child"]);
        assert_eq!(h.sampler.stats().overflow_dropped, 2);
    }

    #[test]
    fn late_children_follow_both_cached_decisions() {
        let h = Harness::new(config());
        for (id, duration) in [(8_u128, 1001_u64), (9, 10)] {
            let parent = Context::new().with_span(h.root(id, &Context::new()));
            let child = h.child(&parent);
            parent
                .span()
                .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(duration));
            end(child, duration + 1);
        }
        assert_eq!(h.names(), ["root", "child"]);
        assert_eq!(h.sampler.stats().kept, 2);
        assert_eq!(h.sampler.stats().dropped, 2);
    }

    #[test]
    fn global_overflow_drops_oldest_trace_and_its_late_root() {
        let h = Harness::new(TailSamplingConfig {
            max_buffered_spans: 2,
            ratio: 1.0,
            ..config()
        });
        let old = Context::new().with_span(h.root(10, &Context::new()));
        let new = Context::new().with_span(h.root(11, &Context::new()));
        end(h.child(&old), 10);
        end(h.child(&old), 20);
        end(h.child(&new), 30);
        old.span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(40));
        new.span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(40));
        assert_eq!(h.names(), ["child", "root"]);
        assert_eq!(h.sampler.stats().overflow_dropped, 2);
    }

    #[test]
    fn shutdown_discards_undecided_spans() {
        let h = Harness::new(config());
        let parent = Context::new().with_span(h.root(12, &Context::new()));
        end(h.child(&parent), 10);
        h.sampler.shutdown_with_timeout(Duration::ZERO).unwrap();
        parent
            .span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(2));
        assert!(h.names().is_empty());
        assert_eq!(h.sampler.stats().dropped, 2);
    }

    #[test]
    fn ratio_matches_sdk_and_local_unsampled_parent_stays_unsampled() {
        let cfg = TailSamplingConfig {
            ratio: 0.25,
            ..config()
        };
        let head = head_sampler(&cfg);
        let sdk = Sampler::TraceIdRatioBased(cfg.ratio);
        for low in [0_u64, 1, u64::MAX / 4, u64::MAX / 2, u64::MAX] {
            let id = TraceId::from((1_u128 << 64) | u128::from(low));
            let decision = head
                .should_sample(None, id, "", &SpanKind::Internal, &[], &[])
                .decision;
            let expected = sdk
                .should_sample(None, id, "", &SpanKind::Internal, &[], &[])
                .decision;
            assert_eq!(
                decision == SamplingDecision::RecordAndSample,
                expected == SamplingDecision::RecordAndSample
            );
            assert_ne!(decision, SamplingDecision::Drop);
        }
        let parent = Context::new().with_remote_span_context(SpanContext::new(
            TraceId::from(1_u128),
            SpanId::from(1_u64),
            TraceFlags::default(),
            false,
            TraceState::default(),
        ));
        let all = head_sampler(&TailSamplingConfig {
            ratio: 1.0,
            ..config()
        });
        assert_eq!(
            all.should_sample(
                Some(&parent),
                TraceId::from(1_u128),
                "",
                &SpanKind::Internal,
                &[],
                &[]
            )
            .decision,
            SamplingDecision::RecordOnly
        );
    }

    #[test]
    fn token_bucket_refills_without_sleeping() {
        let h = Harness::new(config());
        let mut state = h.sampler.lock();
        let now = state.last_refill;
        state.tokens = 2.0;
        assert!(state.parent_token(2, now));
        assert!(state.parent_token(2, now));
        assert!(!state.parent_token(2, now));
        assert!(state.parent_token(2, now + Duration::from_millis(500)));
        assert!(!state.parent_token(2, now + Duration::from_millis(500)));
        assert_eq!(state.stats.parent_keep_throttled, 2);
    }

    #[test]
    fn expired_or_unknown_decisions_drop_late_children() {
        let h = Harness::new(config());
        let parent = Context::new().with_span(h.root(13, &Context::new()));
        let child = h.child(&parent);
        parent
            .span()
            .end_with_timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(2));
        {
            let mut state = h.sampler.lock();
            let decided_at = state
                .recent
                .get(&TraceId::from(13_u128))
                .unwrap()
                .decided_at;
            assert_eq!(
                state.recent_decision(TraceId::from(13_u128), decided_at + DECISION_GRACE),
                None
            );
        }
        end(child, 2001);
        assert_eq!(h.names(), ["root"]);
        assert_eq!(h.sampler.stats().dropped, 1);
    }
}
