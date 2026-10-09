//! Admission telemetry stays on the caller's existing span; only cold builds have roots.

use super::*;
use opentelemetry::trace::TraceContextExt as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[derive(Default)]
pub(crate) struct AdmissionState {
    next_id: u64,
    holder: Option<Arc<Holder>>,
}

enum Blocked {
    Known {
        id: u64,
        class: &'static str,
        context: opentelemetry::trace::SpanContext,
        age: std::time::Duration,
    },
    // Tokio can reserve a released permit for a queued future not yet polled again.
    // It exposes neither that future nor an acquired holder to identify here.
    Unknown,
}

enum Blocker {
    Known { holder: Arc<Holder>, age: std::time::Duration },
    Unknown,
}

struct Holder {
    id: u64,
    class: &'static str,
    acquired: std::time::Instant,
    waited: std::time::Duration,
    blocked: Option<Blocked>,
    span: tracing::Span,
    context: Mutex<opentelemetry::Context>,
    rebuilds: Mutex<Vec<opentelemetry::Context>>,
}

pub(crate) struct AdmissionGuard {
    // Snapshot, publication, and release all serialize with the actual mutex poll.
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    runtime: Arc<runtime::SmalltalkRuntime>,
    holder: Option<Arc<Holder>>,
    waited: std::time::Duration,
}

pub(super) struct Rebuild {
    context: opentelemetry::Context,
    _entered: opentelemetry::ContextGuard,
    admitted: bool,
    _scope: Option<crate::otel::RosterScopeGuard>,
}

impl Drop for Rebuild {
    fn drop(&mut self) {
        if !self.admitted {
            self.context.span().end();
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(holder) = &self.holder {
            let elapsed = holder.acquired.elapsed().as_secs_f64() * 1000.0;
            holder.span.set_attribute("st.roster.admission.hold_ms", elapsed);
            for context in holder.rebuilds.lock().expect("roster rebuild contexts poisoned").drain(..) {
                context.span().set_attribute(opentelemetry::KeyValue::new("st.roster.admission.hold_ms", elapsed));
                context.span().end();
            }
        }
        let mut state = self.runtime.agent_resources_admission_state.lock().expect("roster admission state poisoned");
        if self.holder.as_ref().is_some_and(|holder|
            state.holder.as_ref().is_some_and(|active| active.id == holder.id))
        {
            state.holder = None;
        }
        // Release while still holding metadata: a successor cannot acquire before the
        // old holder is cleared, or acquire without publishing its own identity.
        drop(self.guard.take());
    }
}

impl AdmissionState {
    fn register(
        &mut self,
        class: &'static str,
        acquired: std::time::Instant,
        waited: std::time::Duration,
        blocked: Option<Blocker>,
        span: Option<&tracing::Span>,
    ) -> Option<Arc<Holder>> {
        let holder = span.map(|span| {
            self.next_id += 1;
            Arc::new(Holder {
                id: self.next_id, class, acquired, waited,
                blocked: blocked.map(|blocked| match blocked {
                    Blocker::Known { holder, age } => Blocked::Known {
                        id: holder.id, class: holder.class, age,
                        context: holder.context.lock().expect("roster holder context poisoned")
                            .span().span_context().clone(),
                    },
                    Blocker::Unknown => Blocked::Unknown,
                }),
                context: Mutex::new(span.context()), span: span.clone(),
                rebuilds: Mutex::new(Vec::new()),
            })
        });
        self.holder = holder.clone();
        holder
    }
}

impl AdmissionGuard {
    pub(crate) fn waited(&self) -> std::time::Duration {
        self.waited
    }

    fn record_admission(&self, class: &'static str) {
        let Some(holder) = &self.holder else { return };
        let waited = self.waited;
        let span = &holder.span;
        span.set_attribute("st.roster.admission.wait_ms", waited.as_secs_f64() * 1000.0);
        span.set_attribute("st.roster.admission.waiter_class", class);
        span.set_attribute("st.roster.admission.id", holder.id as i64);
        match &holder.blocked {
            Some(Blocked::Known { id, class: holder_class, context, age }) => {
                span.set_attribute("st.roster.admission.holder_class", *holder_class);
                span.set_attribute("st.roster.admission.holder_id", *id as i64);
                span.set_attribute("st.roster.admission.holder_age_ms", age.as_secs_f64() * 1000.0);
                if context.is_valid() {
                    span.add_link_with_attributes(context.clone(), vec![
                        opentelemetry::KeyValue::new("st.roster.admission.holder_id", *id as i64),
                    ]);
                }
                if waited >= std::time::Duration::from_millis(1) {
                    span.context().span().add_event("st.roster.admission.wait", vec![
                        opentelemetry::KeyValue::new("st.roster.admission.holder_id", *id as i64),
                        opentelemetry::KeyValue::new("st.roster.admission.holder_class", *holder_class),
                        opentelemetry::KeyValue::new("st.roster.admission.waiter_class", class),
                        opentelemetry::KeyValue::new("st.roster.admission.wait_ms", waited.as_secs_f64() * 1000.0),
                    ]);
                }
            }
            Some(Blocked::Unknown) => span.set_attribute("st.roster.admission.holder_class", "unknown"),
            None => {}
        }
    }
}

impl Store {
    /// Never waits; multi-gate readers release unrelated guards before queuing on a miss.
    pub(crate) fn try_admit_agent_resources(&self, class: &'static str) -> Option<AdmissionGuard> {
        debug_assert!(matches!(class, "http" | "ws" | "refresher" | "other"));
        let span = crate::otel::export_enabled().then(tracing::Span::current);
        let mut state = self.smalltalk.agent_resources_admission_state.lock()
            .expect("roster admission state poisoned");
        let guard = self.smalltalk.agent_resources_admission.clone().try_lock_owned().ok()?;
        let waited = std::time::Duration::ZERO;
        let holder = state.register(class, std::time::Instant::now(), waited, None, span.as_ref());
        let admission = AdmissionGuard {
            guard: Some(guard), runtime: Arc::clone(&self.smalltalk), holder, waited,
        };
        drop(state);
        admission.record_admission(class);
        Some(admission)
    }

    pub(crate) async fn admit_agent_resources(&self, class: &'static str) -> AdmissionGuard {
        self.admit_agent_resources_before_enqueue(class, || {}).await
    }

    // The no-op production closure is optimized away; fixtures use the boundary to
    // force a handoff after this waiter starts but before its actual mutex enqueue.
    pub(crate) async fn admit_agent_resources_before_enqueue(
        &self,
        class: &'static str,
        before_enqueue: impl FnOnce(),
    ) -> AdmissionGuard {
        use std::task::Poll;

        debug_assert!(matches!(class, "http" | "ws" | "refresher" | "other"));
        let started = std::time::Instant::now();
        let recording = crate::otel::export_enabled();
        let span = recording.then(tracing::Span::current);
        let mut blocked = None;
        before_enqueue();
        // Budget exhaustion must not produce a false contended sample before Tokio
        // actually enqueues this acquisition. Only the mutex future is unconstrained.
        let mut acquire = std::pin::pin!(tokio::task::unconstrained(
            self.smalltalk.agent_resources_admission.clone().lock_owned()));
        let (admission, waited) = std::future::poll_fn(|cx| {
            let mut state = self.smalltalk.agent_resources_admission_state.lock()
                .expect("roster admission state poisoned");
            match acquire.as_mut().poll(cx) {
                Poll::Pending => {
                    // Freeze the blocker at the first contended poll, not before enqueue
                    // and not a different holder observed later while this waiter queues.
                    if recording && blocked.is_none() {
                        blocked = Some(state.holder.as_ref().map_or(Blocker::Unknown, |holder|
                            Blocker::Known { holder: Arc::clone(holder), age: holder.acquired.elapsed() }));
                    }
                    Poll::Pending
                }
                Poll::Ready(guard) => {
                    let acquired = std::time::Instant::now();
                    let waited = acquired.duration_since(started);
                    let holder = state.register(class, acquired, waited, blocked.take(), span.as_ref());
                    Poll::Ready((AdmissionGuard {
                        guard: Some(guard), runtime: Arc::clone(&self.smalltalk), holder, waited,
                    }, waited))
                }
            }
        }).await;
        crate::performance::record_request("roster/admission-wait", None, waited);
        admission.record_admission(class);
        admission
    }

    /// A refresher has no request span: its cold root becomes the admission holder identity.
    pub(super) fn bind_roster_rebuild(&self, context: &opentelemetry::Context) -> bool {
        let state = self.smalltalk.agent_resources_admission_state.lock().expect("roster admission state poisoned");
        let Some(holder) = &state.holder else { return false };
        context.span().set_attributes([
            opentelemetry::KeyValue::new("st.roster.admission.id", holder.id as i64),
            opentelemetry::KeyValue::new("st.roster.admission.holder_class", holder.class),
            opentelemetry::KeyValue::new("st.roster.admission.waiter_class", holder.class),
            opentelemetry::KeyValue::new("st.roster.admission.wait_ms", holder.waited.as_secs_f64() * 1000.0),
        ]);
        match &holder.blocked {
            Some(Blocked::Known { id, class, context: blocked_context, age }) => {
                context.span().set_attributes([
                    opentelemetry::KeyValue::new("st.roster.admission.holder_id", *id as i64),
                    opentelemetry::KeyValue::new("st.roster.admission.holder_class", *class),
                    opentelemetry::KeyValue::new("st.roster.admission.holder_age_ms", age.as_secs_f64() * 1000.0),
                ]);
                if blocked_context.is_valid() {
                    context.span().add_link(blocked_context.clone(), vec![
                        opentelemetry::KeyValue::new("st.roster.admission.holder_id", *id as i64),
                    ]);
                }
            }
            Some(Blocked::Unknown) => context.span().set_attribute(
                opentelemetry::KeyValue::new("st.roster.admission.holder_class", "unknown")),
            None => {}
        }
        let mut holder_context = holder.context.lock().expect("roster holder context poisoned");
        if !holder_context.span().span_context().is_valid() {
            *holder_context = context.clone();
        }
        holder.rebuilds.lock().expect("roster rebuild contexts poisoned").push(context.clone());
        true
    }

    pub(super) fn start_roster_rebuild(&self, reason: &'static str, index: u64) -> Option<Rebuild> {
        if !crate::otel::export_enabled() {
            return None;
        }
        crate::otel::roster_attribute("st.roster.invalidation", reason);
        let span = crate::otel::stage_root("st.roster.rebuild", "cold", None)?;
        let scope = crate::otel::ensure_roster_scope();
        let context = opentelemetry::Context::new().with_span(span);
        crate::otel::copy_roster_attributes(&context);
        context.span().set_attributes([
            opentelemetry::KeyValue::new("st.roster.mode", "cold"),
            opentelemetry::KeyValue::new("st.roster.invalidation", reason),
            opentelemetry::KeyValue::new("st.roster.path", "exact_cold"),
            opentelemetry::KeyValue::new("st.roster.fresh_requested", false),
            opentelemetry::KeyValue::new("st.roster.requested_cut", index as i64),
        ]);
        let admitted = self.bind_roster_rebuild(&context);
        let entered = context.clone().attach();
        Some(Rebuild { context, _entered: entered, admitted, _scope: scope })
    }
}
