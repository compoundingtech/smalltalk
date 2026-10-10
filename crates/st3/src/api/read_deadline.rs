//! Server read deadlines include queueing, authentication, blocking work and the envelope.
//! Cancellation is cooperative: SQLite reads use progress handlers and Rust folds check
//! the budget between items. It does not preempt an individual filesystem call or parse.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::sync::Arc;

thread_local! {
    static STORE: RefCell<Option<Arc<super::Store>>> = const { RefCell::new(None) };
    // The router runs its handler on a blocking worker already. Submitting its synchronous
    // store work again wastes a second pool slot and can wait behind unrelated blocking work.
    static IN_HANDLER: Cell<bool> = const { Cell::new(false) };
}

fn with_handler<T>(work: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IN_HANDLER.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(IN_HANDLER.with(|slot| slot.replace(true)));
    work()
}

fn with_store<T>(store: Option<Arc<super::Store>>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<super::Store>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STORE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = STORE.with(|slot| slot.replace(store));
    let _restore = Restore(previous);
    work()
}

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use smallclaims::read_budget::{self, ReadBudget};

use super::{ApiError, AppState, ClientTransportBoundary};

const ORDINARY: Duration = Duration::from_secs(15);
const MAX_WAIT_MS: u64 = 30_000;
#[cfg(test)]
const EVENT: Duration = Duration::from_secs(45);
const BULK: Duration = Duration::from_secs(120);

/// Only explicit read operations are cancellable. Writes and their acknowledgement
/// keep their existing semantics; a WebSocket's deadline ends when its upgrade completes.
fn deadline(request: &Request<Body>) -> Option<Duration> {
    let path = request.uri().path();
    let forwarded = path == crate::peer::CLIENT_READ_FORWARD_PATH;
    let export = path == "/v1/internal/replication/export";
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && !(request.method() == Method::POST && (forwarded || export))
    {
        return None;
    }
    let query = request.uri().query().unwrap_or("");
    let parameter = |key: &str| {
        query
            .split('&')
            .filter_map(|part| part.split_once('='))
            .find_map(|(name, value)| {
                let name = urlencoding::decode(name).ok()?;
                if name == key {
                    urlencoding::decode(value).ok()
                } else {
                    None
                }
            })
    };
    let wait_ms = if (path == "/v1/events" && parameter("wait").as_deref() != Some("false"))
        || (path == "/v1/events/page" && parameter("wait").as_deref() == Some("true"))
    {
        Some(
            parameter("timeout_ms")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(MAX_WAIT_MS)
                .clamp(10, MAX_WAIT_MS),
        )
    } else if path == "/v1/client/events"
        || (path.starts_with("/v1/client/conversations/") && path.ends_with("/changes"))
        || (path.starts_with("/v1/client/terminals/")
            && path.ends_with("/screen")
            && parameter("after").is_some())
    {
        Some(
            parameter("wait_ms")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
                .min(MAX_WAIT_MS),
        )
    } else if path.starts_with("/v1/sessions/logs/") && parameter("wait").as_deref() == Some("true")
    {
        Some(MAX_WAIT_MS)
    } else {
        None
    };
    if let Some(wait_ms) = wait_ms {
        return Some(ORDINARY + Duration::from_millis(wait_ms));
    }
    if export
        || path.starts_with("/v1/internal/replication/checkpoint")
        || path == "/v1/checkpoint/plan"
        || path == "/v1/checkpoint/status"
        || path == "/v1/doctor"
    {
        Some(BULK)
    } else {
        Some(ORDINARY)
    }
}

fn long_poll_route(route: &str) -> bool {
    route == crate::peer::CLIENT_READ_FORWARD_PATH
        || route == "/v1/events"
        || route == "/v1/events/page"
        || route == "/v1/client/events"
        || (route.starts_with("/v1/client/conversations/") && route.ends_with("/changes"))
        || (route.starts_with("/v1/client/terminals/") && route.ends_with("/screen"))
        || route.starts_with("/v1/sessions/logs/")
}

/// Install thread-local context only while this future is polled. It never leaks across
/// an await onto another task on the runtime worker. Dropping the request signals workers.
struct Budgeted<F> {
    future: Pin<Box<F>>,
    budget: ReadBudget,
    store: Arc<super::Store>,
    completed: bool,
}

impl<F: Future> Future for Budgeted<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let budget = self.budget.clone();
        let store = self.store.clone();
        let poll = with_store(Some(store), || {
            read_budget::with(Some(budget), || self.future.as_mut().poll(cx))
        });
        if poll.is_ready() {
            self.completed = true;
        }
        poll
    }
}

impl<F> Drop for Budgeted<F> {
    fn drop(&mut self) {
        if !self.completed {
            self.budget.cancel();
        }
    }
}

fn forwarded_deadline(operation: &crate::peer::ClientReadOperation) -> Option<Duration> {
    // This transport also carries terminal actions: cancelling their acknowledgement
    // would turn a completed mutation into an apparently failed read.
    use crate::peer::ClientReadOperation;
    match operation {
        ClientReadOperation::TerminalControl { .. } => None,
        ClientReadOperation::ConversationChanges { .. }
        | ClientReadOperation::TerminalScreenChange { .. } => Some(ORDINARY + operation.wait()),
        ClientReadOperation::ConversationContent { .. }
        | ClientReadOperation::Timeline { .. }
        | ClientReadOperation::TerminalScreen { .. }
        | ClientReadOperation::SeatSnapshot { .. }
        | ClientReadOperation::AgentWorkspace { .. }
        | ClientReadOperation::Blob { .. }
        | ClientReadOperation::Messages { .. } => Some(ORDINARY),
    }
}

fn timeout_response(state: &AppState, path: &str) -> Response {
    construct_timeout_response(|| {
        let error = json!({"code":"read-deadline", "message":"the read exceeded its server deadline", "details":{}});
        let request_id = super::new_request_id();
        if let Some(trace) = crate::relay_trace::current() {
            trace.response(&request_id);
        }
        let value = if path.starts_with("/v1/client/") {
            super::client_error_envelope(StatusCode::GATEWAY_TIMEOUT, &error, &request_id)
        } else {
            json!({"api_version":"st3.v1", "request_id":request_id,
            "snapshot_host":state.node,
            "store_index":state.store.index().unwrap_or_default(),
            "code":"read-deadline", "message":"the read exceeded its server deadline", "details":{}})
        };
        (StatusCode::GATEWAY_TIMEOUT, Json(value)).into_response()
    })
}

/// The deadline has fired, but the response does not exist until construction returns.
/// Keep the existing construction (including its Store read) inside this interval. A
/// panic during construction is a panic terminal, never a prematurely completed timeout.
fn construct_timeout_response(construct: impl FnOnce() -> Response) -> Response {
    use crate::relay_trace::{Outcome, Phase};
    let trace = crate::relay_trace::current();
    let _root = trace.as_ref().map(crate::relay_trace::Trace::guard);
    crate::relay_trace::span(Phase::Deadline).finish(Outcome::TimedOut);
    let response = crate::relay_trace::work(Phase::TimeoutEnvelope, construct);
    if let Some(trace) = trace {
        trace.finish(Outcome::TimedOut);
    }
    response
}

#[track_caller]
fn body_timeout_response(state: &AppState) -> Response {
    // Static route and phase only: never include the forwarded JSON or credentials.
    eprintln!(
        "st3: read cancelled route={:?} phase=request-body callsite={} deadline_elapsed=true",
        crate::peer::CLIENT_READ_FORWARD_PATH,
        std::panic::Location::caller(),
    );
    timeout_response(state, crate::peer::CLIENT_READ_FORWARD_PATH)
}

pub(super) async fn envelope(
    state: (AppState, ClientTransportBoundary),
    request: Request<Body>,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let caller = request
        .extensions()
        .get::<crate::profile::Caller>()
        .map(|caller| caller.0.as_ref())
        .unwrap_or("");
    let trace = crate::relay_trace::gateway(
        request
            .uri()
            .path_and_query()
            .map_or("", |path| path.as_str()),
        request
            .headers()
            .get(super::client_v0::LOCAL_PERSON_HEADER)
            .and_then(|value| value.to_str().ok()),
        caller,
        request.method() == Method::GET && matches!(state.1, ClientTransportBoundary::Unix),
        request.extensions().get::<crate::relay_trace::Connection>(),
    );
    let _root = trace.as_ref().map(crate::relay_trace::Trace::guard);
    let response = crate::relay_trace::scope(trace.clone(), async {
        let mut span = crate::relay_trace::span(crate::relay_trace::Phase::Request);
        let response = envelope_inner(state, request, next, started).await;
        span.finish(if response.status().is_success() {
            crate::relay_trace::Outcome::Completed
        } else {
            crate::relay_trace::Outcome::Failed
        });
        response
    })
    .await;
    if let Some(trace) = trace {
        trace.finish(if response.status().is_success() {
            crate::relay_trace::Outcome::Completed
        } else {
            crate::relay_trace::Outcome::Failed
        });
    }
    response
}

async fn envelope_inner(
    state: (AppState, ClientTransportBoundary),
    mut request: Request<Body>,
    next: Next,
    started: std::time::Instant,
) -> Response {
    let mut duration = deadline(&request);
    let mut forwarding_trace = None;
    if request.method() == Method::POST
        && request.uri().path() == crate::peer::CLIENT_READ_FORWARD_PATH
    {
        // The operation is in a bounded JSON body, not the URI. Include receiving
        // it in the same budget, then return identical bytes to the normal extractor.
        let (parts, body) = request.into_parts();
        let bytes = match tokio::time::timeout(ORDINARY, axum::body::to_bytes(body, 16_384)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                use std::error::Error;
                let status = if error
                    .source()
                    .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
                {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                };
                return status.into_response();
            }
            Err(_) => return body_timeout_response(&state.0),
        };
        if let Ok(forwarded) = serde_json::from_slice::<crate::peer::ClientReadRequest>(&bytes) {
            duration = forwarded_deadline(&forwarded.request);
            if matches!(
                &forwarded.request,
                crate::peer::ClientReadOperation::Timeline { .. }
            ) {
                let caller = parts
                    .extensions
                    .get::<crate::profile::Caller>()
                    .map(|caller| caller.0.as_ref())
                    .unwrap_or("");
                forwarding_trace = crate::relay_trace::forwarded(
                    &bytes,
                    caller,
                    matches!(state.1, ClientTransportBoundary::Unix),
                    parts.extensions.get::<crate::relay_trace::Connection>(),
                );
            }
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }
    if let Some(trace) = forwarding_trace {
        let _root = trace.guard();
        let response = crate::relay_trace::scope(
            Some(trace.clone()),
            envelope_work(state, request, next, started, duration),
        )
        .await;
        trace.finish(if response.status().is_success() {
            crate::relay_trace::Outcome::Completed
        } else {
            crate::relay_trace::Outcome::Failed
        });
        response
    } else {
        envelope_work(state, request, next, started, duration).await
    }
}

async fn envelope_work(
    state: (AppState, ClientTransportBoundary),
    request: Request<Body>,
    next: Next,
    started: std::time::Instant,
    duration: Option<Duration>,
) -> Response {
    let Some(duration) = duration.map(|limit| limit.saturating_sub(started.elapsed())) else {
        return super::response_envelope_unbounded(axum::extract::State(state), request, next)
            .await;
    };
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|route| route.as_str())
        .unwrap_or("/unmatched")
        .to_owned();
    let budget = ReadBudget::new(route, duration);
    let timeout_state = state.0.clone();
    let work = Budgeted {
        completed: false,
        budget: budget.clone(),
        store: timeout_state.store.clone(),
        future: Box::pin(super::response_envelope_unbounded(
            axum::extract::State(state),
            request,
            next,
        )),
    };
    match tokio::time::timeout(duration, work).await {
        Ok(response) if !budget.expired() => response,
        _ => {
            budget.cancel();
            let _ = budget.check();
            timeout_response(&timeout_state, &path)
        }
    }
}

#[derive(Debug)]
pub(super) enum WorkError {
    Join(tokio::task::JoinError),
    Panic,
    Deadline(crate::model::St3Error),
    Store(crate::model::St3Error),
}

impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Join(error) => error.fmt(f),
            Self::Panic => f.write_str("blocking work panicked"),
            Self::Deadline(error) | Self::Store(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for WorkError {}

fn admission_error(error: anyhow::Error) -> WorkError {
    let typed = smallclaims::error::typed(error);
    if typed.code == "read-deadline" {
        WorkError::Deadline(typed)
    } else {
        WorkError::Store(typed)
    }
}

/// Admit and loan the handler's reader freshly for each poll of its future. Admission is
/// asynchronous and happens before any store lock; a poll that stays pending — an event
/// wait, a WebSocket upgrade, or a window/roster permit — returns its loan and permit, so
/// waits hold no reader on either runtime and cannot starve other read workers. Long-poll
/// handlers and unbudgeted upgrades never take this loan: their actual queries use their
/// own explicit scopes.
pub(super) async fn handler<F>(future: F) -> Response
where
    F: Future<Output = Response>,
{
    let admission_budget = read_budget::current();
    let scoped_store = if admission_budget
        .as_ref()
        .is_some_and(|budget| !long_poll_route(budget.route()))
    {
        STORE.with(|slot| slot.borrow().clone())
    } else {
        None
    };
    let Some(store) = scoped_store else {
        return future.await;
    };
    let mut future = std::pin::pin!(future);
    let mut admission = std::pin::pin!(store.readers.admit_read(admission_budget.clone()));
    let mut diagnostic_admission = None;
    std::future::poll_fn(|cx| {
        if smallclaims::sqlite::thread_holds_reader() {
            if store.readers.has_request_reader() {
                return future.as_mut().poll(cx);
            }
            return store
                .readers
                .request_read(|| future.as_mut().poll(cx))
                .unwrap_or_else(|error| Poll::Ready(ApiError::internal(error).into_response()));
        }
        let admission_span = diagnostic_admission.get_or_insert_with(|| {
            crate::relay_trace::span(crate::relay_trace::Phase::HandlerReaderAdmission)
        });
        let admitted = match admission.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(permit)) => {
                admission_span.finish(crate::relay_trace::Outcome::Completed);
                diagnostic_admission = None;
                permit
            }
            Poll::Ready(Err(error)) => {
                admission_span.finish(crate::relay_trace::Outcome::Failed);
                return Poll::Ready(ApiError::internal(error).into_response());
            }
        };
        // Reset the stack-pinned admission future for the next handler poll. The current
        // poll's loan publishes its returned connection before its permit wakes a waiter.
        admission.set(store.readers.admit_read(admission_budget.clone()));
        store
            .readers
            .request_read_with_permit(admitted, || future.as_mut().poll(cx))
            .unwrap_or_else(|error| Poll::Ready(ApiError::internal(error).into_response()))
    })
    .await
}

/// One store-reading worker started outside the request envelope, typically after a
/// WebSocket upgrade. It takes the same reader admission as an HTTP read worker, but
/// keeps the upgraded stream's existing policy: no read budget or deadline is imposed,
/// and dropping the returned future releases a still-pending admission. Same-thread
/// nested callers run inline: same-pool loans are reused, and another pool is tried without waiting.
pub(super) async fn store_read<T, F>(store: &Arc<super::Store>, work: F) -> Result<T, WorkError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if smallclaims::sqlite::thread_holds_reader() {
        return store.readers.request_read(work).map_err(admission_error);
    }
    let permit = store
        .readers
        .admit_read(None)
        .await
        .map_err(|error| WorkError::Store(smallclaims::error::typed(error)))?;
    let store = store.clone();
    let run = move || {
        store
            .readers
            .request_read_with_permit(permit, work)
            .map_err(|error| WorkError::Store(smallclaims::error::typed(error)))
    };
    tokio::task::spawn_blocking(run)
        .await
        .map_err(WorkError::Join)?
}

/// Propagate the read budget to every nested blocking task in the API. This wrapper has
/// no effect on writes or on stream work started after the request's upgrade completes.
#[track_caller]
pub(super) fn spawn_blocking<F, T>(work: F) -> impl Future<Output = Result<T, WorkError>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn(work, true)
}

#[track_caller]
pub(super) fn spawn_handler<F, T>(work: F) -> impl Future<Output = Result<T, WorkError>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn(move || with_handler(work), false)
}

#[track_caller]
fn spawn<F, T>(work: F, query: bool) -> impl Future<Output = Result<T, WorkError>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let budget = read_budget::current().map(|parent| {
        if query && long_poll_route(parent.route()) {
            parent.child(ORDINARY)
        } else {
            parent.child(parent.remaining())
        }
    });
    let worker_budget = budget.clone();
    let diagnostic = crate::relay_trace::current();
    let diagnostic_admission = diagnostic.clone();
    let diagnostic_dispatch = diagnostic.clone();
    let store = STORE.with(|slot| slot.borrow().clone());
    let multithread = matches!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread
    );
    let reentrant = store
        .as_ref()
        .is_some_and(|store| store.readers.has_request_reader());
    let cross_pool =
        query && store.is_some() && !reentrant && smallclaims::sqlite::thread_holds_reader();
    // Handlers never own a whole-request reader: `handler` admits per poll, and their
    // nested queries take query admission themselves. Only explicit query work admits here.
    let loan = budget.is_some() && query;
    let admission_store = store.clone().filter(|_| loan);
    let admission_budget = budget.clone();
    // Capture cancellation before executing an inline operation, including a panic.
    struct Cancel(Option<ReadBudget>);
    impl Drop for Cancel {
        fn drop(&mut self) {
            if let Some(budget) = &self.0 {
                budget.cancel();
            }
        }
    }
    let cancel = Cancel(budget.clone());
    let profile = query.then(crate::profile::current).flatten();
    let queued = profile.as_ref().map(|op| op.wall_span("blocking/queue"));
    let run = move |permit: Option<smallclaims::sqlite::ReadPermit>,
                    mut diagnostic_queue: Option<crate::relay_trace::Span>| {
        if let Some(span) = &mut diagnostic_queue {
            span.finish(crate::relay_trace::Outcome::Completed);
        }
        drop(queued);
        let _work = profile.as_ref().map(|op| op.wall_span("blocking/work"));
        crate::relay_trace::blocking(diagnostic, || {
            crate::relay_trace::result(crate::relay_trace::Phase::BlockingWork, || {
                with_store(store.clone(), || {
                    read_budget::with(worker_budget.clone(), || {
                        if let Some(budget) = &worker_budget {
                            budget.check().map_err(WorkError::Deadline)?;
                        }
                        let result = match (&store, permit) {
                            (Some(store), Some(permit)) => store
                                .readers
                                .request_read_with_permit(permit, work)
                                .map_err(|error| {
                                    WorkError::Store(smallclaims::error::typed(error))
                                })?,
                            (Some(store), None) if loan => {
                                store.readers.request_read(work).map_err(|error| {
                                    WorkError::Store(smallclaims::error::typed(error))
                                })?
                            }
                            _ => work(),
                        };
                        if let Some(budget) = &worker_budget {
                            budget.check().map_err(WorkError::Deadline)?;
                        }
                        Ok(result)
                    })
                })
            })
        })
    };
    let start_queue = move || {
        diagnostic_dispatch
            .as_ref()
            .map(|trace| trace.span(crate::relay_trace::Phase::BlockingQueue))
    };
    enum Task<T> {
        Inline(Result<T, WorkError>),
        Spawned(tokio::task::JoinHandle<Result<T, WorkError>>),
    }
    // The marker is installed only by spawn_handler, never on an async runtime worker.
    // Handlers loan their reader per poll, so nested wrappers on that thread reuse the
    // loan inline; a spawned child admits its own permit without holding the parent's.
    // Both paths retain the same read budget, reader lease and committed mutation result,
    // and each call keeps its panic boundary for best-effort callers and error cleanup.
    let inline = cross_pool || (multithread && (IN_HANDLER.with(Cell::get) || reentrant));
    let task = if inline {
        // Leave the handler's block_on context while executing synchronous callbacks. Some
        // callbacks enter a runtime themselves (for example a forwarded conversation read).
        // A current-thread handler instead releases its poll-scoped loan before a child runs.
        Task::Inline(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if multithread {
                    tokio::task::block_in_place(|| run(None, start_queue()))
                } else {
                    // A different pool cannot queue while a synchronous outer loan is held.
                    run(None, start_queue())
                }
            }))
            .unwrap_or(Err(WorkError::Panic)),
        )
    } else if let Some(store) = admission_store {
        // Start eagerly, as before: dropping even an unpolled waiter cancels queued/running
        // work. Admission itself is asynchronous and happens before opening a connection or
        // entering the blocking callback's snapshot, writer, cache or roster locks.
        Task::Spawned(tokio::spawn(async move {
            let mut admission_span = diagnostic_admission
                .as_ref()
                .map(|trace| trace.span(crate::relay_trace::Phase::ReaderAdmission));
            let admitted = store
                .readers
                .admit_read(admission_budget)
                .await
                .map_err(admission_error);
            if let Some(span) = &mut admission_span {
                span.finish(match &admitted {
                    Ok(_) => crate::relay_trace::Outcome::Completed,
                    Err(WorkError::Deadline(_)) => crate::relay_trace::Outcome::TimedOut,
                    Err(_) => crate::relay_trace::Outcome::Failed,
                });
            }
            let permit = admitted?;
            let diagnostic_queue = start_queue();
            tokio::task::spawn_blocking(move || run(Some(permit), diagnostic_queue))
                .await
                .map_err(WorkError::Join)?
        }))
    } else {
        let diagnostic_queue = start_queue();
        Task::Spawned(tokio::task::spawn_blocking(move || {
            run(None, diagnostic_queue)
        }))
    };
    async move {
        let _cancel = cancel;
        let completed = async move {
            match task {
                Task::Inline(result) => result,
                Task::Spawned(task) => task.await.map_err(WorkError::Join)?,
            }
        };
        if let Some(budget) = budget {
            let result = match tokio::time::timeout(budget.remaining(), completed).await {
                Ok(result) => result,
                Err(_) => {
                    budget.cancel();
                    Err(WorkError::Deadline(budget.check().unwrap_err()))
                }
            };
            // A completed worker may have finished just before its parent was cancelled.
            // Do not deliver that stale success after cancellation of the awaiting read.
            budget.check().map_err(WorkError::Deadline)?;
            result
        } else {
            completed.await
        }
    }
}

/// One synchronous, explicitly read-only query. Long-poll waiting and WebSocket
/// lifetime do not become this query's deadline; stream reads get a fresh budget.
#[track_caller]
pub(super) fn query<T>(
    store: &super::Store,
    route: &'static str,
    work: impl FnOnce() -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let budget = read_budget::current()
        .map(|parent| parent.child(ORDINARY))
        .unwrap_or_else(|| ReadBudget::new(route, ORDINARY));
    read_budget::with(Some(budget.clone()), || {
        budget.check().map_err(ApiError::bad)?;
        let result = store
            .readers
            .request_read(work)
            .map_err(ApiError::internal)?;
        budget.check().map_err(ApiError::bad)?;
        result
    })
}

pub(super) fn error(error: &WorkError) -> Option<ApiError> {
    match error {
        WorkError::Deadline(_) => Some(ApiError::bad(crate::model::St3Error::new(
            "read-deadline",
            "the read exceeded its deadline or was cancelled",
        ))),
        WorkError::Store(error) => Some(ApiError::bad(crate::model::St3Error {
            code: error.code,
            message: error.message.clone(),
            details: error.details.clone(),
        })),
        WorkError::Join(_) | WorkError::Panic => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bounded_state(root: &std::path::Path, limit: usize) -> AppState {
        smallclaims::sqlite::with_read_limit_for_test(limit, || super::super::tests::state(root))
    }

    #[test]
    fn selected_reader_admission_precedes_blocking_dispatch_without_holding_a_reader() {
        use crate::relay_trace::{self, Outcome};
        for handler_case in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = bounded_state(root.path(), 1);
            let store = state.store.clone();
            let capture = relay_trace::tests::Capture::default();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tracing::dispatcher::with_default(&capture.dispatch(), || {
                let trace = relay_trace::tests::trace();
                with_store(Some(store.clone()), || {
                    read_budget::with(Some(ReadBudget::new("/phase-control", ORDINARY)), || {
                        runtime.block_on(relay_trace::scope(Some(trace.clone()), async {
                            // Occupy only the permit, not a connection/snapshot or a worker.
                            let held = store.readers.admit_read(None).await.unwrap();
                            let reading = store.clone();
                            let mut work = Box::pin(async move {
                                if handler_case {
                                    let response = handler(async {
                                        assert!(reading.readers.has_request_reader());
                                        StatusCode::OK.into_response()
                                    })
                                    .await;
                                    u64::from(response.status().as_u16())
                                } else {
                                    spawn_blocking(move || {
                                        assert!(reading.readers.has_request_reader());
                                        reading
                                            .readers
                                            .get()
                                            .query_row("SELECT 42", [], |row| row.get::<_, u64>(0))
                                            .unwrap()
                                    })
                                    .await
                                    .unwrap()
                                }
                            });
                            assert!(futures_util::poll!(work.as_mut()).is_pending());
                            let phase = if handler_case {
                                "HandlerReaderAdmission"
                            } else {
                                "ReaderAdmission"
                            };
                            let observed = tokio::time::timeout(Duration::from_secs(5), async {
                                loop {
                                    if capture.events().iter().any(|e| e["phase"] == phase) {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await;
                            let waiting = capture.events();
                            // Always release before assertions/joins, including a missed event.
                            drop(held);
                            let answer = work.await;
                            observed.expect("the selected admission was not observed");
                            assert_eq!(answer, if handler_case { 200 } else { 42 });
                            assert!(
                                !waiting
                                    .iter()
                                    .any(|e| { e["phase"] == phase && e["state"] == "completed" })
                            );
                            assert!(!waiting.iter().any(|e| e["phase"] == "BlockingQueue"));
                            let events = capture.events();
                            let admitted = events
                                .iter()
                                .position(|e| e["phase"] == phase && e["state"] == "completed")
                                .unwrap();
                            if !handler_case {
                                let dispatched = events
                                    .iter()
                                    .position(|e| {
                                        e["phase"] == "BlockingQueue" && e["state"] == "started"
                                    })
                                    .unwrap();
                                assert!(admitted < dispatched);
                            }
                        }));
                    })
                });
                trace.finish(Outcome::Completed);
            });
            assert_eq!(store.readers.usage().open, store.readers.usage().idle);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sixty_api_clients_share_the_configured_reader_bound() {
        use axum::{Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        const CLIENTS: usize = 60;
        const LIMIT: usize = 32;
        let root = tempfile::tempdir().unwrap();
        let state = bounded_state(root.path(), LIMIT);
        let store = state.store.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = tokio::sync::watch::channel(false);
        let app = Router::new()
            .route("/v1/client/admission-probe", get({
                let (active, peak) = (active.clone(), peak.clone());
                move || {
                    let (store, active, peak, entered, mut released) = (
                        store.clone(), active.clone(), peak.clone(), entered.clone(), released.clone(),
                    );
                    async move {
                        let value = spawn_blocking(move || {
                            assert!(store.readers.has_request_reader());
                            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            entered.send(()).unwrap();
                            tokio::runtime::Handle::current()
                                .block_on(released.wait_for(|released| *released))
                                .unwrap();
                            let value = store.readers.get().query_row(
                                "SELECT COUNT(*) FROM claims", [], |row| row.get::<_, u64>(0),
                            ).unwrap();
                            active.fetch_sub(1, Ordering::SeqCst);
                            value
                        }).await.unwrap();
                        Json(json!({"claims": value}))
                    }
                }
            }))
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let start = Arc::new(tokio::sync::Barrier::new(CLIENTS + 1));
        let mut clients = Vec::new();
        for _ in 0..CLIENTS {
            let (app, start) = (app.clone(), start.clone());
            clients.push(tokio::spawn(async move {
                start.wait().await;
                app.oneshot(request(Method::GET, "/v1/client/admission-probe")).await.unwrap()
            }));
        }
        start.wait().await;
        let saturated = tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..LIMIT {
                entries.recv().await.unwrap();
            }
        }).await;
        // Release even if saturation failed, so no blocked callbacks survive an assertion.
        release.send(true).unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            for client in clients {
                assert_eq!(client.await.unwrap().status(), StatusCode::OK);
            }
        }).await;
        saturated.expect("the admitted batch did not start");
        completed.expect("the API burst did not complete");
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), LIMIT);
        assert!(state.store.readers.usage().peak <= LIMIT);
        assert_eq!(state.store.readers.usage().open, state.store.readers.usage().idle);
    }

    #[tokio::test]
    async fn current_thread_handlers_release_their_parent_loan_before_a_nested_worker() {
        use axum::{Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = bounded_state(root.path(), 1);
        let store = state.store.clone();
        let app = Router::new()
            .route("/v1/nested", get(move || {
                let store = store.clone();
                async move {
                    assert!(IN_HANDLER.with(Cell::get));
                    assert!(store.readers.has_request_reader());
                    let value = spawn_blocking(move || {
                        assert!(store.readers.has_request_reader());
                        // Keep the existing current-thread callback runtime contract.
                        tokio::runtime::Handle::current().block_on(async {});
                        store.readers.get().query_row("SELECT 42", [], |row| row.get::<_, u64>(0))
                            .unwrap()
                    }).await.unwrap();
                    Json(json!({"answer": value}))
                }
            }))
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let response = tokio::time::timeout(
            Duration::from_secs(5), app.oneshot(request(Method::GET, "/v1/nested")),
        ).await.expect("a current-thread nested worker deadlocked").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["value"]["answer"], 42);
        assert_eq!(state.store.readers.usage().peak, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_event_long_poll_releases_its_reader_while_waiting_at_bound_one() {
        use axum::{Router, extract::{Query, State}, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = bounded_state(root.path(), 1);
        let (waiting, mut waits) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route("/v1/events/page", get({
                let state = state.clone();
                move || {
                    let (state, waiting) = (state.clone(), waiting.clone());
                    async move {
                        let work = super::super::events_page(State(state.clone()), Query(
                            super::super::EventQuery {
                                after: 0, subject: None, owner_run: None, wait: Some(true),
                                timeout_ms: Some(30_000), limit: Some(10),
                            },
                        ));
                        let mut work = std::pin::pin!(work);
                        let mut notified = false;
                        std::future::poll_fn(|cx| {
                            let poll = work.as_mut().poll(cx);
                            // Nested queries execute inline on this multithread handler.
                            // Its first Pending is the real event-notification wait.
                            if poll.is_pending() && !notified {
                                assert!(!state.store.readers.has_request_reader());
                                waiting.send(()).unwrap();
                                notified = true;
                            }
                            poll
                        }).await
                    }
                }
            }))
            .route("/v1/probe", get({
                let store = state.store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        assert!(store.readers.has_request_reader());
                        let value = store.readers.get()
                            .query_row("SELECT 42", [], |row| row.get::<_, u64>(0)).unwrap();
                        Json(json!({"answer": value}))
                    }
                }
            }))
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let poll_app = app.clone();
        let poll = tokio::spawn(async move {
            poll_app.oneshot(request(Method::GET, "/v1/events/page?wait=true")).await.unwrap()
        });
        tokio::time::timeout(Duration::from_secs(5), waits.recv()).await
            .expect("the event handler did not reach its wait").unwrap();
        let unrelated = tokio::time::timeout(
            Duration::from_secs(5), app.oneshot(request(Method::GET, "/v1/probe")),
        ).await;
        state.store.append_claim(&crate::model::ClaimInput {
            subject: "agent/admission-wake".into(), kind: "runtime.observed".into(), actor: None,
            fields: serde_json::from_value(json!({
                "status": "running", "runtime_id": "admission-wake", "incarnation_id": "one",
            })).unwrap(), evidence: Vec::new(),
            expected_subject: None, idempotency_key: None,
        }).unwrap();
        super::super::signal_visible_change(&state);
        let response = tokio::time::timeout(Duration::from_secs(5), poll).await
            .expect("the event did not wake the long poll").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(unrelated.expect("the waiting poll retained admission").unwrap().status(), StatusCode::OK);
        assert_eq!(state.store.readers.usage().peak, 1);
        assert_eq!(state.store.readers.usage().idle, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgraded_socket_reads_take_admission_and_reentrant_loans_inline() {
        let root = tempfile::tempdir().unwrap();
        let state = bounded_state(root.path(), 1);
        let store = state.store.clone();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, mut release_rx) = tokio::sync::watch::channel(false);
        let holder = store.clone();
        let first = tokio::spawn(async move {
            let admission_store = holder.clone();
            store_read(&admission_store, move || {
            started.send(()).unwrap();
            tokio::runtime::Handle::current()
                .block_on(release_rx.wait_for(|released| *released))
                .unwrap();
            // A nested socket read on this worker reuses the loan without a second permit.
            let nested = tokio::runtime::Handle::current()
                .block_on(store_read(&holder, || 5))
                .unwrap();
            assert_eq!(nested, 5);
            assert_eq!(holder.readers.usage().open, 1);
            7
            }).await
        });
        started_rx.await.unwrap();
        assert_eq!(store.readers.usage().open, 1);
        let queued_store = store.clone();
        let queued = tokio::spawn(async move { store_read(&queued_store, || 9).await });
        tokio::task::yield_now().await;
        // The queued read holds no reader while waiting for admission.
        assert_eq!(store.readers.usage().open, 1);
        release_tx.send(true).unwrap();
        let results = tokio::time::timeout(Duration::from_secs(5), async {
            (first.await.unwrap().unwrap(), queued.await.unwrap().unwrap())
        })
        .await
        .expect("upgraded-socket reads did not finish");
        assert_eq!(results, (7, 9));
        assert_eq!(store.readers.usage().peak, 1);
        assert_eq!(store.readers.usage().idle, 1);
    }

    #[test]
    fn opposite_cross_pool_async_queries_do_not_queue_under_outer_loans() {
        for multithread in [false, true] {
            let mut builder = if multithread {
                tokio::runtime::Builder::new_multi_thread()
            } else {
                tokio::runtime::Builder::new_current_thread()
            };
            builder.enable_all();
            if multithread {
                builder.worker_threads(2);
            }
            let runtime = builder.build().unwrap();
            let first = Arc::new(smallclaims::sqlite::with_read_limit_for_test(1, ||
                super::super::Store::open_memory("first")).unwrap());
            let second = Arc::new(smallclaims::sqlite::with_read_limit_for_test(1, ||
                super::super::Store::open_memory("second")).unwrap());
            let held = Arc::new(std::sync::Barrier::new(2));
            let (finished, completed) = std::sync::mpsc::channel();
            let workers = [(first.clone(), second.clone()), (second.clone(), first.clone())]
                .map(|(outer, inner)| {
                let held = held.clone();
                let finished = finished.clone();
                let handle = runtime.handle().clone();
                std::thread::spawn(move || {
                    let result = outer.readers.request_read(|| {
                        held.wait();
                        let query_store = inner.clone();
                        let value = handle.block_on(store_read(&inner, move ||
                            query_store.readers.get().query_row("SELECT 7", [], |row| row.get::<_, u64>(0))
                        )).unwrap().unwrap();
                        assert_eq!(value, 7);
                        // Both outer loans still own their sole permits for the budgeted
                        // query helper as well as the unbudgeted upgraded-stream helper.
                        held.wait();
                        let query_store = inner.clone();
                        let value = with_store(Some(inner.clone()), || read_budget::with(
                            Some(ReadBudget::new("/cross-pool", ORDINARY)),
                            || handle.block_on(async move {
                                spawn_blocking(move ||
                                    query_store.readers.get().query_row("SELECT 9", [], |row| row.get::<_, u64>(0))
                                ).await
                            }),
                        )).unwrap().unwrap();
                        assert_eq!(value, 9);
                    });
                    finished.send(result).unwrap();
                })
            });
            drop(finished);
            for _ in 0..2 {
                completed.recv_timeout(Duration::from_secs(10))
                    .expect("a nested cross-pool API query waited on an outer loan").unwrap();
            }
            for worker in workers {
                worker.join().unwrap();
            }
            assert!(first.readers.try_admit_read().is_some());
            assert!(second.readers.try_admit_read().is_some());
        }
    }

    /// An ordinary (non-long-poll) route that stays pending holds no reader: its poll
    /// permit and loan drop at the await, so the only configured slot stays usable.
    async fn ordinary_pending_holds_no_reader(nested_worker: bool) {
        use axum::{Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = bounded_state(root.path(), 1);
        let store = state.store.clone();
        let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let app = Router::new()
            .route("/v1/client/ordinary", get(move || {
                let (store, entered_tx, mut release_rx) = (store.clone(), entered_tx.clone(), release_rx.clone());
                async move {
                    assert!(IN_HANDLER.with(Cell::get));
                    assert!(store.readers.has_request_reader());
                    entered_tx.send(()).unwrap();
                    // The whole-request await: no reader may survive into it.
                    release_rx.changed().await.unwrap();
                    if nested_worker {
                        let answer = spawn_blocking(move || {
                            assert!(store.readers.has_request_reader());
                            store.readers.get()
                                .query_row("SELECT 7", [], |row| row.get::<_, u64>(0)).unwrap()
                        }).await.unwrap();
                        return Json(json!({"answer": answer}));
                    }
                    Json(json!({"answer": 1}))
                }
            }))
            .route("/v1/probe", get({
                let store = state.store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        assert!(store.readers.has_request_reader());
                        let value = store.readers.get()
                            .query_row("SELECT 42", [], |row| row.get::<_, u64>(0)).unwrap();
                        Json(json!({"answer": value}))
                    }
                }
            }))
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let pending_app = app.clone();
        let pending = tokio::spawn(async move {
            pending_app.oneshot(request(Method::GET, "/v1/client/ordinary")).await.unwrap()
        });
        tokio::time::timeout(Duration::from_secs(5), entered.recv()).await
            .expect("the ordinary handler did not start").unwrap();
        // The pending wait released the whole-request reader: the loan's connection is
        // back in the pool and the unrelated read takes the same single slot.
        let unrelated = tokio::time::timeout(
            Duration::from_secs(5), app.oneshot(request(Method::GET, "/v1/probe")),
        ).await;
        release_tx.send(true).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), pending).await
            .expect("the ordinary handler did not resume").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let expected = if nested_worker { 7 } else { 1 };
        assert_eq!(body["value"]["answer"], expected);
        assert_eq!(unrelated.expect("the pending ordinary handler held its reader")
            .unwrap().status(), StatusCode::OK);
        assert_eq!(state.store.readers.usage().peak, 1);
        assert_eq!(state.store.readers.usage().idle, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_ordinary_handler_holds_no_reader_on_a_multithread_runtime() {
        ordinary_pending_holds_no_reader(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pending_ordinary_handler_holds_no_reader_and_keeps_nested_workers_on_a_current_thread_runtime() {
        ordinary_pending_holds_no_reader(true).await;
    }

    #[test]
    fn timeout_terminal_waits_for_response_construction_and_survives_late_child_completion() {
        use crate::relay_trace::{self, Outcome, Phase};
        let capture = relay_trace::tests::Capture::default();
        let dispatch = capture.dispatch();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let request_id = uuid::Uuid::nil().to_string();
        let expected_id = request_id.clone();
        let worker = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let trace = relay_trace::tests::trace();
                let mut child = trace.span(Phase::BlockingWork);
                relay_trace::blocking(Some(trace.clone()), || {
                    let response = construct_timeout_response(|| {
                        trace.response(&request_id);
                        entered.send(()).unwrap();
                        // Control an unfinished construction without a Store or a timed sleep.
                        released.recv().unwrap();
                        (
                            StatusCode::GATEWAY_TIMEOUT,
                            [("x-test-response-id", request_id)],
                            "unchanged response body",
                        )
                            .into_response()
                    });
                    child.finish(Outcome::Completed);
                    trace.finish(Outcome::Completed);
                    response
                })
            })
        });
        waiting.recv().unwrap();
        let during_construction = capture.events();
        // Release and join before assertions so a failing control cannot strand a worker.
        release.send(()).unwrap();
        let response = worker.join().unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(response.headers()["x-test-response-id"], expected_id);
        assert!(
            during_construction
                .iter()
                .any(|e| { e["phase"] == "Deadline" && e["state"] == "timed-out" })
        );
        assert!(
            during_construction
                .iter()
                .any(|e| e["phase"] == "ResponseBinding")
        );
        assert!(
            during_construction
                .iter()
                .any(|e| e["phase"] == "TimeoutEnvelope")
        );
        assert!(!during_construction.iter().any(|e| e["phase"] == "Terminal"));

        let events = capture.events();
        let binding = events
            .iter()
            .position(|e| e["phase"] == "ResponseBinding")
            .unwrap();
        let construction = events
            .iter()
            .position(|e| e["phase"] == "TimeoutEnvelope" && e["state"] == "completed")
            .unwrap();
        let terminal = events
            .iter()
            .position(|e| e["phase"] == "Terminal")
            .unwrap();
        assert!(binding < construction && construction < terminal);
        assert_eq!(events[terminal]["state"], "TimedOut");
        assert_eq!(
            events.iter().filter(|e| e["phase"] == "Terminal").count(),
            1
        );
        assert!(events[terminal + 1..].iter().any(|e| {
            e["phase"] == "BlockingWork"
                && e["state"] == "completed"
                && e["after_terminal"] == "true"
        }));
    }

    #[test]
    fn aborted_timeout_construction_records_panic_instead_of_timeout_completion() {
        use crate::relay_trace;
        let capture = relay_trace::tests::Capture::default();
        tracing::dispatcher::with_default(&capture.dispatch(), || {
            let trace = relay_trace::tests::trace();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                relay_trace::blocking(Some(trace), || {
                    construct_timeout_response(|| panic!("private construction failure"))
                })
            }));
            assert!(result.is_err());
            assert!(relay_trace::current().is_none());
        });
        let events = capture.events();
        assert!(
            events
                .iter()
                .any(|e| { e["phase"] == "TimeoutEnvelope" && e["state"] == "panicked" })
        );
        assert_eq!(events.last().unwrap()["phase"], "Terminal");
        assert_eq!(events.last().unwrap()["state"], "Panicked");
        assert_eq!(
            events.iter().filter(|e| e["phase"] == "Terminal").count(),
            1
        );
        assert!(!format!("{events:?}").contains("private construction failure"));
    }

    #[test]
    fn a_handler_write_ack_does_not_wait_for_a_second_blocking_pool_slot() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            super::super::Store::open(&directory.path().join("claims.sqlite3"), "blocking-test")
                .unwrap(),
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (entered, occupied) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let released_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
            });
            occupied.await.unwrap();
            // One pool slot is busy with unrelated work; the handler uses the other.
            // Release it independently so the old nested-submission control fails cleanly,
            // rather than deadlocking or leaving a background worker after an assertion.
            let release_flag = released_flag.clone();
            let releaser = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(350));
                release_flag.store(true, Ordering::SeqCst);
                release.send(()).unwrap();
            });
            let writing = store.clone();
            let result = spawn_handler(move || {
                tokio::runtime::Handle::current().block_on(super::super::blocking_action(move || {
                    // A synchronous callback may need to wait for an async service itself.
                    tokio::runtime::Handle::current().block_on(async {});
                    writing.connection.batched(|transaction| {
                        transaction.execute(
                            "INSERT INTO meta(key,value) VALUES('blocking-ack','committed')", [],
                        ).map_err(|error| crate::model::St3Error::new("internal", error.to_string()))?;
                        Ok(())
                    }).map_err(|error| crate::model::St3Error::new("internal", error))?
                }))
            }).await;
            let acknowledged_before_release = !released_flag.load(Ordering::SeqCst);
            releaser.join().unwrap();
            blocker.await.unwrap();
            result.unwrap().unwrap();
            let value: String = store.readers.get().query_row(
                "SELECT value FROM meta WHERE key='blocking-ack'", [], |row| row.get(0),
            ).unwrap();
            assert_eq!(value, "committed");
            assert!(acknowledged_before_release, "write ACK waited for the unrelated pool slot");
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_inline_read_keeps_its_deadline_without_cancelling_a_later_write() {
        let result = spawn_handler(|| {
            let handle = tokio::runtime::Handle::current();
            let read = read_budget::with(Some(ReadBudget::new("/expired", Duration::ZERO)), || {
                spawn_blocking(|| panic!("an expired read must not run"))
            });
            assert!(matches!(handle.block_on(read), Err(WorkError::Deadline(_))));
            handle.block_on(spawn_blocking(|| 42))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn inline_panics_keep_the_nested_error_boundary_and_handler_cleanup() {
        let result = spawn_handler(|| {
            let handle = tokio::runtime::Handle::current();
            let failure = handle.block_on(spawn_blocking(|| panic!("nested callback")));
            assert!(matches!(failure, Err(WorkError::Panic)));
            assert!(error(failure.as_ref().unwrap_err()).is_none());
            // This code must still execute, just like the old JoinError path.
            handle.block_on(spawn_blocking(|| 42))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn real_envelope_keeps_best_effort_nested_panics_out_of_the_screen_response() {
        use axum::{Json, Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let app = Router::new()
            .route(
                "/v1/client/terminal/test/screen",
                get(|| async {
                    assert!(IN_HANDLER.with(Cell::get));
                    // terminal_facts uses this same .await.ok().flatten() contract.
                    let facts: Option<serde_json::Value> =
                        spawn_blocking(|| -> Option<serde_json::Value> {
                            panic!("best effort stats callback");
                        })
                        .await
                        .ok()
                        .flatten();
                    Json(serde_json::json!({"screen": "available", "facts": facts}))
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let response = app
            .oneshot(request(Method::GET, "/v1/client/terminal/test/screen"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(envelope["value"]["screen"], "available");
        assert!(envelope["value"]["facts"].is_null());
    }

    fn request(method: Method, path: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn deadlines_classify_reads_without_cancelling_writes_or_shortening_event_waits() {
        assert_eq!(
            deadline(&request(Method::GET, "/v1/mission-runs?root=r")),
            Some(ORDINARY)
        );
        assert_eq!(
            deadline(&request(Method::HEAD, "/v1/client/agents")),
            Some(ORDINARY)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/events?wait=true")),
            Some(EVENT)
        );
        assert_eq!(deadline(&request(Method::GET, "/v1/events")), Some(EVENT));
        assert_eq!(
            deadline(&request(
                Method::GET,
                "/v1/client/events?wait%5fms=%33%30%30%30%30"
            )),
            Some(EVENT)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/sessions/logs/seat?wait=%74rue")),
            Some(EVENT)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/events?wait=%66alse")),
            Some(ORDINARY)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/events?wait=false")),
            Some(ORDINARY)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/checkpoint/plan")),
            Some(BULK)
        );
        assert_eq!(
            deadline(&request(Method::POST, "/v1/internal/replication/export")),
            Some(BULK)
        );
        assert_eq!(deadline(&request(Method::POST, "/v1/messages/send")), None);
        assert_eq!(
            deadline(&request(Method::POST, "/v1/internal/replication/receive")),
            None
        );
    }

    #[test]
    fn forwarded_reads_use_operation_budgets_and_terminal_actions_keep_acknowledgements() {
        use crate::peer::ClientReadOperation;
        let read = ClientReadOperation::Timeline {
            session_id: "invented".into(),
            limit: 20,
            cursor: None,
        };
        assert_eq!(
            deadline(&request(
                Method::POST,
                crate::peer::CLIENT_READ_FORWARD_PATH
            )),
            Some(ORDINARY)
        );
        assert_eq!(forwarded_deadline(&read), Some(ORDINARY));
        for wait_ms in [0, 100, 10_000, u64::MAX] {
            let read = ClientReadOperation::ConversationChanges {
                session_id: "invented".into(),
                after: None,
                wait_ms,
            };
            assert_eq!(
                forwarded_deadline(&read),
                Some(ORDINARY + Duration::from_millis(wait_ms.min(10_000)))
            );
            let screen = ClientReadOperation::TerminalScreenChange {
                terminal_id: "invented".into(),
                after_revision: "before".into(),
                wait_ms,
                facts: false,
            };
            assert_eq!(forwarded_deadline(&screen), forwarded_deadline(&read));
        }
        let action = ClientReadOperation::TerminalControl {
            action_id: "invented".into(),
            idempotency_key: "invented".into(),
            action_type: "close".into(),
            terminal_id: "invented".into(),
            runtime_incarnation: "invented".into(),
            expected_sequence: 0,
            parameters: json!({}),
        };
        assert_eq!(forwarded_deadline(&action), None);
        // Heal-next is a mutating protocol step (readmission/replay), not an ordinary read.
        assert_eq!(
            deadline(&request(Method::POST, "/v1/internal/replication/heal/next")),
            None
        );
    }

    #[tokio::test]
    async fn forwarded_long_poll_queries_cannot_spend_the_envelopes_wait_budget() {
        let parent = ReadBudget::new(
            crate::peer::CLIENT_READ_FORWARD_PATH,
            ORDINARY + Duration::from_secs(10),
        );
        let work = read_budget::with(Some(parent.clone()), || {
            spawn_blocking(|| read_budget::current().unwrap().remaining())
        });
        assert!(work.await.unwrap() <= ORDINARY);
        assert!(parent.remaining() > ORDINARY);
        assert!(!parent.expired());
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_long_poll_keeps_its_allowed_wait_in_the_envelope() {
        use axum::{Router, middleware::from_fn_with_state, routing::post};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let signals = Arc::new(std::sync::Mutex::new(Some((started_tx, finish_rx))));
        let app = Router::new()
            .route(
                crate::peer::CLIENT_READ_FORWARD_PATH,
                post(move || {
                    let signals = signals.clone();
                    async move {
                        let (started, finish) = signals.lock().unwrap().take().unwrap();
                        let _ = started.send(());
                        finish.await.unwrap();
                        Json(json!({"items": []}))
                    }
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let request = Request::builder()
            .method(Method::POST)
            .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&crate::peer::ClientReadRequest {
                    authority_actor: "person/fixture".into(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::ConversationChanges {
                        session_id: "invented".into(),
                        after: None,
                        wait_ms: 10_000,
                    },
                })
                .unwrap(),
            ))
            .unwrap();
        let response = tokio::spawn(app.oneshot(request));
        started_rx.await.unwrap();
        tokio::time::advance(ORDINARY + Duration::from_secs(1)).await;
        assert!(!response.is_finished());
        finish_tx.send(()).unwrap();
        assert_eq!(response.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_read_timeout_drops_the_actual_handler_future() {
        use axum::{Router, middleware::from_fn_with_state, routing::post};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let signals = Arc::new(std::sync::Mutex::new(Some((started_tx, dropped_tx))));
        let app = Router::new()
            .route(
                crate::peer::CLIENT_READ_FORWARD_PATH,
                post(move || {
                    let signals = signals.clone();
                    async move {
                        struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);
                        impl Drop for Dropped {
                            fn drop(&mut self) {
                                if let Some(tx) = self.0.take() {
                                    let _ = tx.send(());
                                }
                            }
                        }
                        let (started, dropped) = signals.lock().unwrap().take().unwrap();
                        let _guard = Dropped(Some(dropped));
                        let _ = started.send(());
                        std::future::pending::<Response>().await
                    }
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let request = Request::builder()
            .method(Method::POST)
            .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&crate::peer::ClientReadRequest {
                    authority_actor: "person/fixture".into(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::Timeline {
                        session_id: "invented".into(),
                        limit: 20,
                        cursor: None,
                    },
                })
                .unwrap(),
            ))
            .unwrap();
        let response = tokio::spawn(app.oneshot(request));
        started_rx.await.unwrap();
        tokio::time::advance(ORDINARY + Duration::from_secs(1)).await;
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_terminal_action_is_not_cut_by_the_read_deadline() {
        use axum::{Router, middleware::from_fn_with_state, routing::post};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let signals = Arc::new(std::sync::Mutex::new(Some((started_tx, finish_rx))));
        let app = Router::new()
            .route(
                crate::peer::CLIENT_READ_FORWARD_PATH,
                post(move || {
                    let signals = signals.clone();
                    async move {
                        let (started, finish) = signals.lock().unwrap().take().unwrap();
                        let _ = started.send(());
                        finish.await.unwrap();
                        Json(json!({"acknowledged": true}))
                    }
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let request = Request::builder()
            .method(Method::POST)
            .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&crate::peer::ClientReadRequest {
                    authority_actor: "person/fixture".into(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::TerminalControl {
                        action_id: "invented".into(),
                        idempotency_key: "invented".into(),
                        action_type: "close".into(),
                        terminal_id: "invented".into(),
                        runtime_incarnation: "invented".into(),
                        expected_sequence: 0,
                        parameters: json!({}),
                    },
                })
                .unwrap(),
            ))
            .unwrap();
        let response = tokio::spawn(app.oneshot(request));
        started_rx.await.unwrap();
        tokio::time::advance(ORDINARY + Duration::from_secs(1)).await;
        assert!(!response.is_finished());
        finish_tx.send(()).unwrap();
        assert_eq!(response.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    fn forwarded_app(state: AppState) -> axum::Router {
        use axum::{
            Router, extract::DefaultBodyLimit, middleware::from_fn_with_state, routing::post,
        };
        Router::new()
            .route(
                crate::peer::CLIENT_READ_FORWARD_PATH,
                post(super::super::forward_client_read).layer(DefaultBodyLimit::max(16_384)),
            )
            .with_state(state.clone())
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ))
    }

    #[tokio::test]
    async fn forwarded_real_handler_rejects_oversized_and_malformed_bodies() {
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let app = forwarded_app(super::super::tests::state(root.path()));
        for (body, expected) in [
            ("x".repeat(16_385), StatusCode::PAYLOAD_TOO_LARGE),
            ("{".into(), StatusCode::BAD_REQUEST),
        ] {
            let request = Request::builder()
                .method(Method::POST)
                .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn forwarded_real_handler_preserves_long_poll_wait_through_a_signed_peer() {
        use crate::peer::{
            ClientReadOperation, ClientReadRequest, ClientReadRoute, ClientRelay, FleetAuth,
        };
        use axum::{Router, routing::post};
        use std::os::unix::fs::PermissionsExt;
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("fleet-secret");
        std::fs::write(&secret, [5_u8; 32]).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = FleetAuth::test("fleet-test", &[5; 32]);
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let seen = Arc::new(std::sync::Mutex::new(Some(seen_tx)));
        let peer = Router::new().route(
            "/v1/peer/client-read",
            post(move |body: axum::body::Bytes| {
                let auth = auth.clone();
                let seen = seen.clone();
                async move {
                    let received: ClientReadRequest = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().take().unwrap().send(received).unwrap();
                    // Allowed owner wait plus ordinary work exceeds the 15-second query
                    // budget, but fits in the forwarding envelope's 25-second budget.
                    tokio::time::sleep(ORDINARY + Duration::from_secs(1)).await;
                    let answer = serde_json::to_vec(&crate::model::ApiResponse {
                        api_version: "st3.v1".into(),
                        request_id: "invented".into(),
                        snapshot_host: "far".into(),
                        store_index: 0,
                        value: json!({"items": []}),
                    })
                    .unwrap();
                    let headers = auth
                        .response_headers_for(
                            "/v1/peer/client-read",
                            "far",
                            &answer,
                            &FleetAuth::body_digest(&body),
                        )
                        .unwrap();
                    let mut response = (StatusCode::OK, answer).into_response();
                    response.headers_mut().insert(
                        "content-type",
                        axum::http::HeaderValue::from_static("application/json"),
                    );
                    response.headers_mut().extend(headers);
                    response
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, peer).await.unwrap() });
        let mut state = super::super::tests::state(root.path());
        state.node = "middle".into();
        state.client_relay = ClientRelay::from_config(&crate::config::Config {
            node: "middle".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![crate::config::PeerConfig {
                name: "far".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&ClientReadRequest {
                    authority_actor: "person/fixture".into(),
                    relay: Some(ClientReadRoute {
                        target: "host/far".into(),
                        path: vec!["near".into()],
                        hops_left: 3,
                    }),
                    request: ClientReadOperation::ConversationChanges {
                        session_id: "invented".into(),
                        after: None,
                        wait_ms: 10_000,
                    },
                })
                .unwrap(),
            ))
            .unwrap();
        let started = std::time::Instant::now();
        let response = forwarded_app(state).oneshot(request).await.unwrap();
        server.abort();
        let _ = server.await;
        let seen = seen_rx.await.unwrap();
        assert_eq!(seen.authority_actor, "person/fixture");
        assert!(
            seen.relay.is_none(),
            "the last hop reaches the owner directly"
        );
        assert!(matches!(
            seen.request,
            ClientReadOperation::ConversationChanges {
                wait_ms: 10_000,
                ..
            }
        ));
        assert!(started.elapsed() >= ORDINARY);
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(answer["value"]["items"], json!([]));
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_request_body_wait_has_the_ordinary_deadline() {
        use axum::{Router, middleware::from_fn_with_state, routing::post};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let app = Router::new()
            .route(
                crate::peer::CLIENT_READ_FORWARD_PATH,
                post(|| async { StatusCode::OK }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let stream = futures_util::stream::once(async move {
            let _ = started_tx.send(());
            std::future::pending::<Result<axum::body::Bytes, std::io::Error>>().await
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri(crate::peer::CLIENT_READ_FORWARD_PATH)
            .body(Body::from_stream(stream))
            .unwrap();
        let response = tokio::spawn(app.oneshot(request));
        started_rx.await.unwrap();
        tokio::time::advance(ORDINARY + Duration::from_secs(1)).await;
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn synchronous_stream_queries_have_fresh_budgets_and_long_polls_have_query_children() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        assert!(read_budget::current().is_none());
        query(&state.store, "/stream/conversation", || {
            let current = read_budget::current().unwrap();
            assert_eq!(current.route(), "/stream/conversation");
            assert!(current.remaining() <= ORDINARY);
            assert!(state.store.readers.get().is_autocommit());
            Ok(())
        })
        .unwrap();
        assert!(read_budget::current().is_none());
        let poll = ReadBudget::new("/v1/client/events", EVENT);
        read_budget::with(Some(poll.clone()), || {
            query(&state.store, "/unused-fallback", || {
                let current = read_budget::current().unwrap();
                assert_eq!(current.route(), poll.route());
                assert!(current.remaining() <= ORDINARY);
                Ok(())
            })
            .unwrap();
            assert!(read_budget::current().unwrap().remaining() > ORDINARY);
        });
        assert!(read_budget::current().is_none());
    }

    #[test]
    fn synchronous_queries_refuse_expired_work_and_reject_late_success() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let ran = AtomicUsize::new(0);
        let expired = ReadBudget::new("/v1/client/events", Duration::ZERO);
        let error = read_budget::with(Some(expired), || {
            query(&state.store, "/events", || {
                ran.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        })
        .unwrap_err();
        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(ran.load(Ordering::Relaxed), 0);
        let parent = ReadBudget::new("/v1/client/events", ORDINARY);
        let error = read_budget::with(Some(parent.clone()), || {
            query(&state.store, "/events", || {
                parent.cancel();
                Ok(())
            })
        })
        .unwrap_err();
        assert_eq!(error.code, "read-deadline");
        assert!(state.store.readers.get().is_autocommit());
    }

    #[tokio::test]
    async fn an_ordinary_completed_http_read_is_not_cancelled_by_future_drop() {
        use axum::{Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let app = Router::new()
            .route(
                "/v1/invented-read",
                get(|| async { Json(json!({"items": []})) }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let response = app
            .oneshot(request(Method::GET, "/v1/invented-read"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn idle_long_poll_keeps_its_normal_empty_answer(path: &'static str, route: &'static str) {
        use axum::{Router, middleware::from_fn_with_state, routing::get};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let app = Router::new()
            .route(
                route,
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(MAX_WAIT_MS)).await;
                    Json(json!({ "items": [] }))
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let response = app.oneshot(request(Method::GET, path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let value = body.get("value").unwrap_or(&body);
        assert_eq!(value["items"], json!([]), "{body}");
    }

    #[tokio::test]
    async fn an_idle_conversation_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer(
            "/v1/client/conversations/test/changes?wait_ms=30000",
            "/v1/client/conversations/{id}/changes",
        )
        .await;
    }
    #[tokio::test]
    async fn an_idle_client_event_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer(
            "/v1/client/events?wait_ms=30000",
            "/v1/client/events",
        )
        .await;
    }
    #[tokio::test]
    async fn an_idle_screen_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer(
            "/v1/client/terminals/test/screen?after=cursor&wait_ms=30000",
            "/v1/client/terminals/{id}/screen",
        )
        .await;
    }
    #[tokio::test]
    async fn an_idle_log_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer(
            "/v1/sessions/logs/test?wait=true",
            "/v1/sessions/logs/{*subject}",
        )
        .await;
    }
    #[tokio::test]
    async fn an_idle_legacy_event_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer("/v1/events?timeout_ms=30000", "/v1/events")
            .await;
    }

    #[tokio::test]
    async fn an_idle_paged_event_poll_is_not_cut_at_the_ordinary_deadline() {
        idle_long_poll_keeps_its_normal_empty_answer(
            "/v1/events/page?wait=true&timeout_ms=30000",
            "/v1/events/page",
        )
        .await;
    }

    #[test]
    fn paged_event_deadlines_follow_the_handlers_opt_in_wait() {
        for path in [
            "/v1/events/page",
            "/v1/events/page?wait=false&timeout_ms=30000",
            "/v1/events/page?wait=%66alse&timeout_ms=30000",
        ] {
            assert_eq!(deadline(&request(Method::GET, path)), Some(ORDINARY));
        }
        for path in [
            "/v1/events/page?wait=true",
            "/v1/events/page?wait=%74rue&timeout_ms=999999",
        ] {
            assert_eq!(deadline(&request(Method::GET, path)), Some(EVENT));
        }
        assert_eq!(
            deadline(&request(
                Method::GET,
                "/v1/events/page?wait=true&timeout_ms=40"
            )),
            Some(ORDINARY + Duration::from_millis(40))
        );
        assert_eq!(
            deadline(&request(
                Method::GET,
                "/v1/events/page?wait=true&timeout_ms=0"
            )),
            Some(ORDINARY + Duration::from_millis(10))
        );
        assert!(long_poll_route("/v1/events/page"));
    }
    #[test]
    fn wait_budgets_follow_handler_defaults_caps_and_nonwaiting_requests() {
        for path in [
            "/v1/client/events",
            "/v1/client/conversations/test/changes",
            "/v1/client/terminals/test/screen?after=cursor",
            "/v1/sessions/logs/test?wait=false",
        ] {
            assert_eq!(deadline(&request(Method::GET, path)), Some(ORDINARY));
        }
        assert_eq!(
            deadline(&request(Method::GET, "/v1/client/events?wait_ms=999999")),
            Some(EVENT)
        );
        assert_eq!(
            deadline(&request(Method::GET, "/v1/events?timeout_ms=1234")),
            Some(ORDINARY + Duration::from_millis(1234))
        );
        assert_eq!(deadline(&request(Method::GET, "/v1/doctor")), Some(BULK));
    }

    #[tokio::test]
    async fn completed_nested_work_does_not_cancel_its_parent_or_next_query() {
        let parent = ReadBudget::new("/ordinary", ORDINARY);
        let first = read_budget::with(Some(parent.clone()), || spawn_blocking(|| 7));
        assert_eq!(first.await.unwrap(), 7);
        assert!(!parent.expired());
        let next = read_budget::with(Some(parent.clone()), || spawn_blocking(|| 9));
        assert_eq!(next.await.unwrap(), 9);
        assert!(!parent.expired());
    }

    #[tokio::test]
    async fn dropping_an_unpolled_worker_future_cancels_the_actual_cpu_loop() {
        let parent = ReadBudget::new("/cpu", ORDINARY);
        let items = Arc::new(AtomicUsize::new(0));
        let count = items.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let future = read_budget::with(Some(parent.clone()), || {
            spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                let result = (|| {
                    for _ in 0..1_000_000 {
                        read_budget::check()?;
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<_, crate::model::St3Error>(())
                })();
                let _ = done_tx.send(result);
            })
        });
        started_rx.await.unwrap();
        drop(future);
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().code, "read-deadline");
        assert_eq!(items.load(Ordering::Relaxed), 0);
        assert!(!parent.expired());
    }

    #[tokio::test]
    async fn dropping_the_request_cancels_a_worker_queued_before_it_starts() {
        let parent = ReadBudget::new("/queued", ORDINARY);
        let future = read_budget::with(Some(parent.clone()), || spawn_blocking(|| 1));
        parent.cancel();
        assert!(matches!(future.await, Err(WorkError::Deadline(_))));
    }

    #[tokio::test]
    async fn an_expired_budget_returns_a_typed_deadline_not_an_internal_error() {
        let parent = ReadBudget::new("/expired", Duration::ZERO);
        let future = read_budget::with(Some(parent), || spawn_blocking(|| 1));
        let error = ApiError::internal(future.await.unwrap_err());
        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.code, "read-deadline");
    }
    #[tokio::test]
    async fn a_running_cpu_reduction_stops_after_its_waiter_is_dropped() {
        let parent = ReadBudget::new("/cpu-running", ORDINARY);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let future = read_budget::with(Some(parent.clone()), || {
            spawn_blocking(move || {
                let mut items = 1usize;
                let _ = started_tx.send(());
                let result = (|| {
                    loop {
                        read_budget::check()?;
                        items = items.wrapping_add(1);
                        std::hint::black_box(items);
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, crate::model::St3Error>(())
                })();
                let _ = done_tx.send((items, result));
            })
        });
        started_rx.await.unwrap();
        drop(future);
        let (items, result) = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(items > 0);
        assert_eq!(result.unwrap_err().code, "read-deadline");
        assert!(!parent.expired());
    }

    #[tokio::test]
    async fn event_query_work_gets_its_own_shorter_deadline() {
        let parent = ReadBudget::new("/v1/events", EVENT);
        let work = read_budget::with(Some(parent.clone()), || {
            spawn_blocking(|| read_budget::current().unwrap().remaining())
        });
        assert!(work.await.unwrap() <= ORDINARY);
        assert!(!parent.expired());
    }

    #[test]
    fn a_cancelled_nested_snapshot_is_ended_before_the_request_reader_is_reused() {
        let store = Arc::new(super::super::Store::open_memory("deadline-test").unwrap());
        let budget = ReadBudget::new("/snapshot", ORDINARY);
        let result = read_budget::with(Some(budget.clone()), || {
            store.readers.request_read(|| {
                store.read_snapshot(|cut| {
                    store.read_snapshot(|nested| {
                        assert_eq!(cut, nested);
                        budget.cancel();
                        let _ = store
                            .readers
                            .get()
                            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
                        Ok(())
                    })
                })
            })
        });
        assert!(result.unwrap().is_err());
        let reader = store.readers.get();
        assert!(reader.is_autocommit());
        assert_eq!(
            reader
                .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(reader);
        // Only reader hooks were changed: the writer remains usable after cancellation.
        store
            .connection
            .write()
            .execute_batch("CREATE TABLE cancellation_writer_control(value)")
            .unwrap();
    }
    #[test]
    fn different_store_snapshot_nesting_restores_the_outer_cut_and_loan() {
        let first = Arc::new(super::super::Store::open_memory("first").unwrap());
        let second = Arc::new(super::super::Store::open_memory("second").unwrap());
        let budget = ReadBudget::new("/nested-stores", ORDINARY);
        read_budget::with(Some(budget), || {
            first.readers.request_read(|| {
                first.read_snapshot(|outer| {
                    second.readers.request_read(|| {
                        second.read_snapshot(|_| {
                            // Membership is per pool: the inner pin is on top while the
                            // outer store's pin stays registered below it.
                            assert!(second.readers.has_pinned_reader());
                            assert!(first.readers.has_pinned_reader());
                            Ok(())
                        })
                    })??;
                    // The inner pin popped; only the outer store's pin remains.
                    assert!(first.readers.has_pinned_reader());
                    assert!(!second.readers.has_pinned_reader());
                    first.read_snapshot(|again| {
                        assert_eq!(again, outer);
                        Ok(())
                    })
                })
            })
        })
        .unwrap()
        .unwrap();
        assert!(!first.readers.has_pinned_reader());
        assert!(!second.readers.has_pinned_reader());
        assert!(first.readers.get().is_autocommit());
        assert!(second.readers.get().is_autocommit());
    }
}
