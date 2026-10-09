//! One explicitly selected relay diagnostic, off by default. No request bodies or error text.
//!
//! Gateway/owner Unix read: ST3_RELAY_TRACE_PATH (exact path and query) + ST3_RELAY_TRACE_ACTOR.
//! ST3_RELAY_TRACE_CALLER defaults to "st3 conversations timeline"; the only other choice
//! is "st3 replication-worker", for the owner Unix request. Its existing response UUID
//! joins the owner trace to the peer OwnerCall. No client-provided correlation header is used.
//! Forwarding daemon: ST3_RELAY_TRACE_FORWARD_DIGEST selects the existing raw body digest
//! only for a Unix timeline POST from the kernel-named "st3 replication-worker". This
//! is local transport trust, not signature verification in that daemon; peer verification
//! remains at the worker. The identical digest joins these phases without a wire addition.
//! Peer: ST3_RELAY_TRACE_DIGEST (existing authenticated body SHA256) + ST3_RELAY_TRACE_PEER.
//! Each process consumes its selector once, including on refusal/cancellation. Configure only
//! one mode. Adding st3::relay_diagnostic=debug to the existing RUST_LOG filter enables
//! the existing private stderr sink;
//! the existing INFO+ OTLP filter excludes these DEBUG events. No exporter is installed here.
//! Activation is a separate operational decision. Identical authenticated bodies have identical
//! digests: that binding is not a globally unique request token. Retain epochs/attempt ordinals.

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

const RECORDS: u16 = 96; // Includes the reserved terminal record.
const TARGET: &str = "st3::relay_diagnostic";
static SELECTOR: OnceLock<Option<Selector>> = OnceLock::new();

enum Match {
    Forward {
        digest: String,
    },
    Gateway {
        path: String,
        actor: String,
        caller: String,
    },
    Peer {
        digest: String,
        sender: String,
    },
}

struct Selector {
    selected: Match,
    used: AtomicBool,
}

impl Selector {
    #[cfg(test)]
    fn parse(
        path: Option<String>,
        actor: Option<String>,
        digest: Option<String>,
        sender: Option<String>,
        caller: Option<String>,
    ) -> Option<Self> {
        Self::parse_forward(path, actor, digest, sender, caller, None)
    }

    fn parse_forward(
        path: Option<String>,
        actor: Option<String>,
        digest: Option<String>,
        sender: Option<String>,
        caller: Option<String>,
        forward: Option<String>,
    ) -> Option<Self> {
        let selected = match (path, actor, digest, sender, caller, forward) {
            (Some(path), Some(actor), None, None, caller, None)
                if path.len() <= 1024
                    && path.is_ascii()
                    && path.split('?').next().is_some_and(|p| {
                        p.starts_with("/v1/client/sessions/") && p.ends_with("/timeline")
                    })
                    && actor.len() <= 256
                    && (actor.starts_with("person/") || actor.starts_with("agent/"))
                    && caller.as_deref().is_none_or(|caller| {
                        matches!(
                            caller,
                            "st3 conversations timeline" | "st3 replication-worker"
                        )
                    }) =>
            {
                Match::Gateway {
                    path,
                    actor,
                    caller: caller.unwrap_or_else(|| "st3 conversations timeline".into()),
                }
            }
            (None, None, Some(digest), Some(sender), None, None)
                if valid_digest(&digest) && !sender.is_empty() && sender.len() <= 128 =>
            {
                Match::Peer { digest, sender }
            }
            (None, None, None, None, None, Some(digest)) if valid_digest(&digest) => {
                Match::Forward { digest }
            }
            _ => return None,
        };
        Some(Self {
            selected,
            used: AtomicBool::new(false),
        })
    }

    fn claim(&self, matches: bool) -> bool {
        matches
            && self
                .used
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    fn gateway(&self, path: &str, actor: Option<&str>, caller: &str) -> bool {
        self.claim(matches!(&self.selected, Match::Gateway { path: expected, actor: expected_actor, caller: expected_caller }
            if expected == path && actor == Some(expected_actor.as_str())
                && caller == expected_caller))
    }

    fn peer(&self, digest: &str, sender: &str) -> bool {
        self.claim(
            matches!(&self.selected, Match::Peer { digest: expected, sender: expected_sender }
            if expected == digest && expected_sender == sender),
        )
    }

    fn forwarding(
        &self,
        digest: impl FnOnce() -> String,
        caller: &str,
        unix: bool,
    ) -> Option<String> {
        let Match::Forward { digest: expected } = &self.selected else {
            return None;
        };
        if !unix || caller != "st3 replication-worker" || self.used.load(Ordering::Relaxed) {
            return None;
        }
        let actual = digest();
        self.claim(actual == *expected).then_some(actual)
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Router construction only: no lazy configuration/cache initialization in a read.
pub(crate) fn init() {
    SELECTOR.get_or_init(|| {
        Selector::parse_forward(
            std::env::var("ST3_RELAY_TRACE_PATH").ok(),
            std::env::var("ST3_RELAY_TRACE_ACTOR").ok(),
            std::env::var("ST3_RELAY_TRACE_DIGEST").ok(),
            std::env::var("ST3_RELAY_TRACE_PEER").ok(),
            std::env::var("ST3_RELAY_TRACE_CALLER").ok(),
            std::env::var("ST3_RELAY_TRACE_FORWARD_DIGEST").ok(),
        )
    });
}

pub(crate) fn gateway(
    path: &str,
    actor: Option<&str>,
    caller: &str,
    unix_get: bool,
    connection: Option<&Connection>,
) -> Option<Trace> {
    let selector = SELECTOR.get()?.as_ref()?;
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return None;
    }
    (unix_get && selector.gateway(path, actor, caller)).then(|| {
        let trace = Trace::new(format!("request/{}", crate::api::new_request_id()));
        if let Some(connection) = connection {
            connection.record(&trace);
        }
        trace
    })
}

/// Selection uses the original, already bounded body and existing Unix caller trust.
/// It creates no signature/authority fact; only the worker verifies the peer's signature.
pub(crate) fn forwarded(
    body: &[u8],
    caller: &str,
    unix: bool,
    connection: Option<&Connection>,
) -> Option<Trace> {
    let selector = SELECTOR.get()?.as_ref()?;
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return None;
    }
    let actual = selector.forwarding(|| crate::peer::FleetAuth::body_digest(body), caller, unix)?;
    let trace = Trace::new(actual);
    if let Some(connection) = connection {
        connection.record(&trace);
    }
    Some(trace)
}

/// Selected-mode connection clocks carry no content, writes, events or heap allocation.
/// These precede request selection and are connection scope (including possible reuse),
/// not a claimed per-request queue interval. The trace records them only if that request wins.
#[derive(Clone, Copy)]
pub(crate) struct Connection {
    accepted: Instant,
    dispatched: Option<Instant>,
    queued: Option<Instant>,
    started: Option<Instant>,
    finished: Option<Instant>,
}
impl Connection {
    pub(crate) fn capture() -> Option<Self> {
        let selector = SELECTOR.get()?.as_ref()?;
        (matches!(
            selector.selected,
            Match::Gateway { .. } | Match::Forward { .. }
        ) && !selector.used.load(Ordering::Relaxed)
            && tracing::enabled!(target: TARGET, tracing::Level::DEBUG))
        .then(|| Self {
            accepted: Instant::now(),
            dispatched: None,
            queued: None,
            started: None,
            finished: None,
        })
    }
    pub(crate) fn dispatched(&mut self) {
        self.dispatched = Some(Instant::now());
    }
    pub(crate) fn queued(&mut self) {
        self.queued = Some(Instant::now());
    }
    pub(crate) fn started(&mut self) {
        self.started = Some(Instant::now());
    }
    pub(crate) fn finished(&mut self) {
        self.finished = Some(Instant::now());
    }
    fn record(&self, trace: &Trace) {
        let duration = |from: Instant, to: Instant| {
            u64::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u64::MAX)
        };
        for (phase, from, to) in [
            (
                Phase::ConnectionDispatch,
                Some(self.accepted),
                self.dispatched,
            ),
            (Phase::CallerQueue, self.queued, self.started),
            (Phase::CallerPreparation, self.started, self.finished),
            (
                Phase::ConnectionAge,
                Some(self.accepted),
                Some(Instant::now()),
            ),
        ] {
            if let (Some(from), Some(to)) = (from, to) {
                trace.0.emit(
                    phase,
                    "preceding-connection",
                    duration(from, to),
                    (0, "unix"),
                    "",
                    0,
                );
            }
        }
    }
}

/// One monotonic clock while a peer selector is pending. No event is emitted before trust.
pub(crate) fn peer_authentication_clock() -> Option<Instant> {
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return None;
    }
    #[cfg(test)]
    if let Ok(pending) = TEST_SELECTOR.try_with(|selector| {
        matches!(selector.selected, Match::Peer { .. }) && !selector.used.load(Ordering::Relaxed)
    }) {
        return pending.then(Instant::now);
    }
    let selector = SELECTOR.get()?.as_ref()?;
    (matches!(selector.selected, Match::Peer { .. }) && !selector.used.load(Ordering::Relaxed))
        .then(Instant::now)
}

/// Call only after existing signature verification AND fleet acceptance succeeded.
pub(crate) fn authenticated_peer(digest: &str, sender: &str) -> Option<Trace> {
    #[cfg(test)]
    if let Ok(selected) = TEST_SELECTOR.try_with(|selector| {
        tracing::enabled!(target: TARGET, tracing::Level::DEBUG) && selector.peer(digest, sender)
    }) {
        return selected.then(|| Trace::new(digest.into()));
    }
    let selector = SELECTOR.get()?.as_ref()?;
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return None;
    }
    selector
        .peer(digest, sender)
        .then(|| Trace::new(digest.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Panicked,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Phase {
    Request,
    ConnectionDispatch,
    CallerQueue,
    CallerPreparation,
    ConnectionAge,
    AdmissionQueue,
    Authenticate,
    Snapshot,
    HandlerQueue,
    Handler,
    BlockingQueue,
    BlockingWork,
    Session,
    Owner,
    Route,
    Attempt,
    FabricDial,
    RequestBinding,
    PeerSelection,
    FabricTarget,
    ResponseBinding,
    Headers,
    Body,
    Verify,
    Decode,
    OwnerCall,
    ForwardCall,
    Envelope,
}

struct Inner {
    id: String, // UUID response ID or exact SHA256, at most 64 bytes.
    started: Instant,
    records: AtomicU16,
    omitted: AtomicU16,
    terminal: AtomicBool,
}

impl Inner {
    fn emit(
        &self,
        phase: Phase,
        state: &str,
        elapsed_us: u64,
        attempt: (u8, &str),
        binding: &str,
        http_status: u16,
    ) {
        let (attempt, transport) = attempt;
        let Ok(sequence) = self
            .records
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < RECORDS - 1).then_some(n + 1)
            })
        else {
            let _ = self
                .omitted
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(1))
                });
            return;
        };
        tracing::debug!(target: TARGET, correlation_id = %self.id, pid = std::process::id(),
            phase = ?phase, state, elapsed_us, since_request_us = micros(self.started),
            sequence, attempt, binding, transport, http_status, after_terminal = self.terminal.load(Ordering::Relaxed));
    }

    fn finish(&self, outcome: Outcome) {
        if self.terminal.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::debug!(target: TARGET, correlation_id = %self.id, pid = std::process::id(),
            phase = "Terminal", state = ?outcome, elapsed_us = micros(self.started),
            records_at_terminal = self.records.load(Ordering::Relaxed).saturating_add(1),
            omitted = self.omitted.load(Ordering::Relaxed));
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.finish(if std::thread::panicking() {
            Outcome::Panicked
        } else {
            Outcome::Cancelled
        });
    }
}

fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone)]
pub(crate) struct Trace(Arc<Inner>, u8, &'static str);

impl Trace {
    fn new(id: String) -> Self {
        assert!(id.len() <= 64);
        Self(
            Arc::new(Inner {
                id,
                started: Instant::now(),
                records: AtomicU16::new(0),
                omitted: AtomicU16::new(0),
                terminal: AtomicBool::new(false),
            }),
            0,
            "",
        )
    }

    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn finish(&self, outcome: Outcome) {
        self.0.finish(outcome);
    }

    pub(crate) fn guard(&self) -> Root {
        Root(self.clone())
    }

    pub(crate) fn attempt(&self, attempt: u8, transport: &'static str) -> Self {
        Self(self.0.clone(), attempt, transport)
    }

    pub(crate) fn selected_peer(&self, name: &str) {
        self.routing_identity(Phase::PeerSelection, "peer", name);
    }

    pub(crate) fn fabric_target(&self, node: &str, protocol: &str) {
        self.routing_identity(Phase::FabricTarget, "node", node);
        self.routing_identity(Phase::FabricTarget, "protocol", protocol);
    }

    fn routing_identity(&self, phase: Phase, kind: &'static str, value: &str) {
        // Only bounded routing labels, never a URL (userinfo/query), path or raw body.
        let safe = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        self.0.emit(
            phase,
            if safe { kind } else { "identity-not-recorded" },
            0,
            (self.1, self.2),
            if safe { value } else { "" },
            0,
        );
    }

    pub(crate) fn authenticated_after(&self, elapsed: std::time::Duration) {
        let elapsed = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.0.emit(
            Phase::Authenticate,
            "completed",
            elapsed,
            (self.1, self.2),
            "",
            0,
        );
    }

    pub(crate) fn http_status(&self, status: u16) {
        self.0
            .emit(Phase::Headers, "status", 0, (self.1, self.2), "", status);
    }

    pub(crate) fn bind(&self, digest: &str) {
        if valid_digest(digest) {
            self.0.emit(
                Phase::RequestBinding,
                "bound",
                0,
                (self.1, self.2),
                digest,
                0,
            );
        }
    }

    pub(crate) fn response(&self, id: &str) {
        // A peer-controlled field is never arbitrary log text. Only the existing UUID form.
        let uuid = id.strip_prefix("request/").unwrap_or(id);
        if id.len() <= 64 && uuid::Uuid::parse_str(uuid).is_ok() {
            self.0.emit(
                Phase::ResponseBinding,
                "response-id",
                0,
                (self.1, "unix"),
                id,
                0,
            );
        }
    }

    pub(crate) fn span(&self, phase: Phase) -> Span {
        self.0.emit(phase, "started", 0, (self.1, self.2), "", 0);
        Span {
            trace: Some(self.clone()),
            phase,
            started: Some(Instant::now()),
            done: false,
        }
    }
}

/// The owning request's drop records cancellation even if a blocking child is still alive.
pub(crate) struct Root(Trace);
impl Drop for Root {
    fn drop(&mut self) {
        self.0.finish(if std::thread::panicking() {
            Outcome::Panicked
        } else {
            Outcome::Cancelled
        });
    }
}

tokio::task_local! { static ASYNC_TRACE: Option<Trace>; }
thread_local! { static BLOCKING_TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) }; }

pub(crate) fn current() -> Option<Trace> {
    match ASYNC_TRACE.try_with(Clone::clone) {
        Ok(trace) => trace, // Explicit None masks any enclosing blocking context.
        Err(_) => BLOCKING_TRACE.with(|slot| slot.borrow().clone()),
    }
}

pub(crate) async fn scope<T>(trace: Option<Trace>, work: impl Future<Output = T>) -> T {
    if trace.is_none()
        && ASYNC_TRACE.try_with(|_| ()).is_err()
        && BLOCKING_TRACE.with(|slot| slot.borrow().is_none())
    {
        // Default-off path: do not install a task-local frame on every request poll.
        work.await
    } else {
        ASYNC_TRACE.scope(trace, work).await
    }
}

pub(crate) fn blocking<T>(trace: Option<Trace>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Trace>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BLOCKING_TRACE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(BLOCKING_TRACE.with(|slot| slot.replace(trace)));
    work()
}

pub(crate) struct Span {
    trace: Option<Trace>,
    phase: Phase,
    started: Option<Instant>,
    done: bool,
}

pub(crate) fn span(phase: Phase) -> Span {
    current().map_or(
        Span {
            trace: None,
            phase,
            started: None,
            done: false,
        },
        |trace| trace.span(phase),
    )
}

impl Span {
    pub(crate) fn finish(&mut self, outcome: Outcome) {
        if self.done {
            return;
        }
        self.done = true;
        if let (Some(trace), Some(started)) = (&self.trace, self.started) {
            trace.0.emit(
                self.phase,
                match outcome {
                    Outcome::Completed => "completed",
                    Outcome::Failed => "failed",
                    Outcome::TimedOut => "timed-out",
                    Outcome::Cancelled => "cancelled",
                    Outcome::Panicked => "panicked",
                },
                micros(started),
                (trace.1, trace.2),
                "",
                0,
            );
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.finish(if std::thread::panicking() {
            Outcome::Panicked
        } else {
            Outcome::Cancelled
        });
    }
}

pub(crate) fn work<T>(phase: Phase, work: impl FnOnce() -> T) -> T {
    let mut span = span(phase);
    let value = work();
    span.finish(Outcome::Completed);
    value
}

pub(crate) fn result<T, E>(phase: Phase, work: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let mut span = span(phase);
    let value = work();
    span.finish(if value.is_ok() {
        Outcome::Completed
    } else {
        Outcome::Failed
    });
    value
}

pub(crate) async fn future<T, E>(
    phase: Phase,
    work: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let mut span = span(phase);
    let value = work.await;
    span.finish(if value.is_ok() {
        Outcome::Completed
    } else {
        Outcome::Failed
    });
    value
}

#[cfg(test)]
tokio::task_local! { static TEST_SELECTOR: Arc<Selector>; }

/// Test-only per-task selector: never changes process environment or production enablement.
#[cfg(test)]
pub(crate) async fn test_peer_selector<T>(
    digest: String,
    sender: String,
    work: impl Future<Output = T>,
) -> T {
    let selector = Selector::parse(None, None, Some(digest), Some(sender), None).unwrap();
    TEST_SELECTOR.scope(Arc::new(selector), work).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

    #[derive(Clone, Default)]
    pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    struct Fields(BTreeMap<String, String>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            if event.metadata().target() != TARGET {
                return;
            }
            assert_eq!(*event.metadata().level(), tracing::Level::DEBUG);
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
    impl Capture {
        pub(crate) fn dispatch(&self) -> tracing::Dispatch {
            tracing::Dispatch::new(tracing_subscriber::registry().with(self.clone()))
        }
        pub(crate) fn events(&self) -> Vec<BTreeMap<String, String>> {
            self.0.lock().unwrap().clone()
        }
    }
    fn trace() -> Trace {
        Trace::new(format!("request/{}", uuid::Uuid::nil()))
    }

    #[test]
    fn selectors_are_exact_bounded_and_consumed_only_once() {
        let path = "/v1/client/sessions/agent%2Ftest/timeline?limit=100";
        let selector = Selector::parse(
            Some(path.into()),
            Some("person/test".into()),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!selector.gateway(path, Some("person/other"), "st3 conversations timeline"));
        assert!(!selector.gateway(path, Some("person/test"), "st3 replication-worker"));
        assert!(!selector.gateway(
            &format!("{path}&cursor=other"),
            Some("person/test"),
            "st3 conversations timeline"
        ));
        assert!(selector.gateway(path, Some("person/test"), "st3 conversations timeline"));
        assert!(!selector.gateway(path, Some("person/test"), "st3 conversations timeline"));
        assert!(
            Selector::parse(
                Some("x".repeat(1025)),
                Some("person/test".into()),
                None,
                None,
                None
            )
            .is_none()
        );
        assert!(
            Selector::parse(
                Some(path.into()),
                Some("person/test".into()),
                None,
                None,
                Some("arbitrary caller".into())
            )
            .is_none()
        );
        assert!(
            Selector::parse(
                None,
                None,
                Some("A".repeat(64)),
                Some("source".into()),
                None
            )
            .is_none()
        );
        assert!(
            Selector::parse(
                Some(path.into()),
                Some("person/test".into()),
                Some("a".repeat(64)),
                Some("source".into()),
                None
            )
            .is_none()
        );
        let selector = Arc::new(
            Selector::parse(
                None,
                None,
                Some("a".repeat(64)),
                Some("source".into()),
                None,
            )
            .unwrap(),
        );
        assert!(!selector.peer(&"b".repeat(64), "source"));
        assert!(!selector.peer(&"a".repeat(64), "other"));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let selector = selector.clone();
                std::thread::spawn(move || selector.peer(&"a".repeat(64), "source"))
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|worker| worker.join().unwrap().then_some(()))
                .count(),
            1
        );
    }

    #[test]
    fn forwarding_selection_preserves_local_trust_and_skips_hash_work_when_ineligible() {
        let expected = "a".repeat(64);
        let selector =
            Selector::parse_forward(None, None, None, None, None, Some(expected.clone())).unwrap();
        assert!(
            selector
                .forwarding(
                    || panic!("must not hash TCP"),
                    "st3 replication-worker",
                    false
                )
                .is_none()
        );
        assert!(
            selector
                .forwarding(
                    || panic!("must not hash other caller"),
                    "st3 conversations timeline",
                    true
                )
                .is_none()
        );
        assert!(
            selector
                .forwarding(|| "b".repeat(64), "st3 replication-worker", true)
                .is_none()
        );
        assert_eq!(
            selector.forwarding(|| expected.clone(), "st3 replication-worker", true),
            Some(expected)
        );
        assert!(
            selector
                .forwarding(
                    || panic!("must not hash a consumed selector"),
                    "st3 replication-worker",
                    true
                )
                .is_none()
        );
        assert!(
            Selector::parse_forward(
                Some("/v1/client/sessions/a/timeline".into()),
                Some("person/test".into()),
                None,
                None,
                None,
                Some("a".repeat(64))
            )
            .is_none()
        );
    }

    #[test]
    fn disabled_hooks_allocate_no_trace_timer_or_records_and_preserve_values() {
        let capture = Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            blocking(None, || {
                let mut span = span(Phase::Owner);
                assert!(span.trace.is_none() && span.started.is_none());
                span.finish(Outcome::Completed);
                assert_eq!(work(Phase::Session, || 42), 42);
                assert_eq!(
                    result(Phase::Verify, || Err::<(), _>("secret failure")),
                    Err("secret failure")
                );
            })
        });
        assert!(capture.events().is_empty());
    }

    #[test]
    fn records_are_capped_with_a_reserved_terminal_and_no_content_fields() {
        let capture = Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            let trace = trace();
            trace.bind("not a digest: credential");
            trace.response("not a UUID: private body");
            assert!(capture.events().is_empty());
            for _ in 0..1000 {
                trace
                    .attempt(255, "fabric")
                    .span(Phase::Headers)
                    .finish(Outcome::Completed);
            }
            trace.bind("not a digest: credential");
            trace.response("not a UUID: private body");
            trace.finish(Outcome::Failed);
            trace.finish(Outcome::Completed);
            let events = capture.events();
            assert_eq!(events.len(), usize::from(RECORDS));
            assert_eq!(events.last().unwrap()["state"], "Failed");
            assert_eq!(
                events.iter().filter(|e| e["phase"] == "Terminal").count(),
                1
            );
            assert!(events.last().unwrap()["omitted"].parse::<u16>().unwrap() > 0);
            for event in events {
                assert!(event.len() <= 12);
                assert!(event.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() <= 512);
                assert!(event.keys().all(|key| !matches!(
                    key.as_str(),
                    "body" | "actor" | "path" | "error" | "credentials"
                )));
            }
        });
    }

    #[test]
    fn preceding_connection_work_is_separate_and_response_binding_uses_only_uuid() {
        let capture = Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            let mut connection = Connection {
                accepted: Instant::now(),
                dispatched: None,
                queued: None,
                started: None,
                finished: None,
            };
            connection.dispatched();
            connection.queued();
            connection.started();
            connection.finished();
            let trace = trace();
            connection.record(&trace);
            trace.selected_peer("owner-node");
            trace.fabric_target("owner-node", "st2");
            trace.selected_peer("private credential with spaces");
            trace.fabric_target(&"x".repeat(1000), "http://user:credential@host");
            trace.response("request/00000000-0000-0000-0000-000000000000");
            trace.bind(&"a".repeat(64));
            trace.finish(Outcome::Completed);
        });
        let events = capture.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["state"] == "preceding-connection")
                .count(),
            4
        );
        assert!(
            events
                .iter()
                .any(|event| event["phase"] == "CallerPreparation")
        );
        assert!(
            events
                .iter()
                .any(|event| event["phase"] == "ResponseBinding")
        );
        assert!(
            events
                .iter()
                .any(|event| event["phase"] == "RequestBinding")
        );
        assert_eq!(events.last().unwrap()["state"], "Completed");
        assert!(events.iter().any(|event| {
            event
                .get("binding")
                .is_some_and(|binding| binding == "owner-node")
        }));
        assert!(
            events
                .iter()
                .any(|event| event["state"] == "identity-not-recorded")
        );
        assert!(!format!("{events:?}").contains("credential"));
        assert!(events.iter().all(|event| {
            event
                .get("binding")
                .is_none_or(|binding| binding.len() <= 64)
        }));
    }

    #[test]
    fn cancellation_is_terminal_before_a_retained_blocking_child_finishes() {
        let capture = Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            let trace = trace();
            let root = trace.guard();
            let child = trace.span(Phase::BlockingWork);
            drop(root);
            drop(child);
            trace.finish(Outcome::Completed);
        });
        let events = capture.events();
        assert_eq!(events[1]["phase"], "Terminal");
        assert_eq!(events[1]["state"], "Cancelled");
        assert_eq!(events[2]["state"], "cancelled");
        assert_eq!(events[2]["after_terminal"], "true");
        assert_eq!(
            events.iter().filter(|e| e["phase"] == "Terminal").count(),
            1
        );
    }

    #[test]
    fn panic_restores_blocking_context_and_records_panic_without_its_text() {
        let capture = Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            let trace = trace();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                blocking(Some(trace.clone()), || {
                    let _root = trace.guard();
                    let _span = span(Phase::Handler);
                    panic!("private panic text");
                })
            }));
            assert!(outcome.is_err());
            assert!(current().is_none());
        });
        let events = capture.events();
        assert!(events.iter().any(|e| e["state"] == "panicked"));
        assert_eq!(events.last().unwrap()["state"], "Panicked");
        assert!(!format!("{events:?}").contains("private panic text"));
    }

    #[tokio::test]
    async fn dropped_future_is_cancelled_and_errors_are_not_reported_as_timeout() {
        use tracing::instrument::WithSubscriber as _;
        let capture = Capture::default();
        async {
            let selected = trace();
            let mut waiting = Box::pin(scope(Some(selected.clone()), async {
                let _root = selected.guard();
                future::<(), ()>(Phase::Headers, std::future::pending()).await
            }));
            assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            drop(waiting);
            scope(Some(trace()), async {
                assert_eq!(
                    future(Phase::Verify, async { Err::<(), _>("signed refusal") }).await,
                    Err("signed refusal")
                );
                let mut span = span(Phase::Headers);
                span.finish(Outcome::TimedOut);
            })
            .await;
        }
        .with_subscriber(capture.dispatch())
        .await;
        let events = capture.events();
        assert!(
            events
                .iter()
                .any(|e| e["phase"] == "Headers" && e["state"] == "cancelled")
        );
        assert!(
            events
                .iter()
                .any(|e| e["phase"] == "Verify" && e["state"] == "failed")
        );
        assert!(events.iter().any(|e| e["state"] == "timed-out"));
        assert!(!format!("{events:?}").contains("signed refusal"));
    }

    #[test]
    fn an_unselected_async_request_cannot_inherit_a_blocking_parent_trace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        blocking(Some(trace()), || {
            assert!(current().is_some());
            runtime.block_on(scope(None, async {
                assert!(current().is_none());
            }));
            assert!(current().is_some());
        });
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn async_contexts_are_isolated_and_blocking_transfer_restores_the_worker() {
        let a = trace();
        let b = Trace::new("a".repeat(64));
        let ((), ()) = tokio::join!(
            scope(Some(a.clone()), async {
                tokio::task::yield_now().await;
                assert_eq!(current().unwrap().id(), a.id());
                let captured = current();
                tokio::task::spawn_blocking(move || {
                    blocking(captured, || assert!(current().is_some()));
                    assert!(current().is_none());
                })
                .await
                .unwrap();
            }),
            scope(Some(b.clone()), async {
                tokio::task::yield_now().await;
                assert_eq!(current().unwrap().id(), b.id());
                scope(None, async {
                    assert!(current().is_none());
                })
                .await;
                assert_eq!(current().unwrap().id(), b.id());
            }),
        );
        assert!(current().is_none());
    }
}
