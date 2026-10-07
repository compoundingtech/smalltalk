//! Server read deadlines include queueing, authentication, blocking work and the envelope.
//! Cancellation is cooperative: SQLite reads use progress handlers and Rust folds check
//! the budget between items. It does not preempt an individual filesystem call or parse.

use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;

thread_local! {
    static STORE: RefCell<Option<Arc<super::Store>>> = const { RefCell::new(None) };
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
    let error = json!({"code":"read-deadline", "message":"the read exceeded its server deadline", "details":{}});
    let request_id = super::new_request_id();
    let value = if path.starts_with("/v1/client/") {
        super::client_error_envelope(StatusCode::GATEWAY_TIMEOUT, &error, &request_id)
    } else {
        json!({"api_version":"st3.v1", "request_id":request_id,
            "snapshot_host":state.node,
            "store_index":state.store.index().unwrap_or_default(),
            "code":"read-deadline", "message":"the read exceeded its server deadline", "details":{}})
    };
    (StatusCode::GATEWAY_TIMEOUT, Json(value)).into_response()
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
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let mut duration = deadline(&request);
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
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }
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
    Deadline(crate::model::St3Error),
    Store(crate::model::St3Error),
}

impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Join(error) => error.fmt(f),
            Self::Deadline(error) | Self::Store(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for WorkError {}

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
    spawn(work, false)
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
    let store = STORE.with(|slot| slot.borrow().clone());
    let task = tokio::task::spawn_blocking(move || {
        with_store(store.clone(), || {
            read_budget::with(worker_budget.clone(), || {
                if let Some(budget) = &worker_budget {
                    budget.check().map_err(WorkError::Deadline)?;
                }
                let result = if worker_budget.is_some() {
                    match &store {
                        Some(store) => store
                            .readers
                            .request_read(work)
                            .map_err(|error| WorkError::Store(smallclaims::error::typed(error)))?,
                        None => work(),
                    }
                } else {
                    work()
                };
                if let Some(budget) = &worker_budget {
                    budget.check().map_err(WorkError::Deadline)?;
                }
                Ok(result)
            })
        })
    });
    // Capture the guard before returning the future: dropping an unpolled future
    // must cancel the already submitted blocking task as well.
    struct Cancel(Option<ReadBudget>);
    impl Drop for Cancel {
        fn drop(&mut self) {
            if let Some(budget) = &self.0 {
                budget.cancel();
            }
        }
    }
    let cancel = Cancel(budget.clone());
    async move {
        let _cancel = cancel;
        if let Some(budget) = budget {
            let result = match tokio::time::timeout(budget.remaining(), task).await {
                Ok(result) => result.map_err(WorkError::Join)?,
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
            task.await.map_err(WorkError::Join)?
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
        WorkError::Join(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                    authority_actor: "person/invented".into(),
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
                    authority_actor: "person/invented".into(),
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
                    authority_actor: "person/invented".into(),
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
                    authority_actor: "person/invented".into(),
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
        assert_eq!(seen.authority_actor, "person/invented");
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
                    let first_key = first.readers.key();
                    second.readers.request_read(|| {
                        second.read_snapshot(|_| {
                            assert!(
                                smallclaims::sqlite::PINNED_READER.with(|slot| slot
                                    .borrow()
                                    .as_ref()
                                    .unwrap()
                                    .0
                                    == second.readers.key())
                            );
                            Ok(())
                        })
                    })??;
                    assert!(
                        smallclaims::sqlite::PINNED_READER.with(|slot| slot
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .0
                            == first_key)
                    );
                    first.read_snapshot(|again| {
                        assert_eq!(again, outer);
                        Ok(())
                    })
                })
            })
        })
        .unwrap()
        .unwrap();
        assert!(smallclaims::sqlite::PINNED_READER.with(|slot| slot.borrow().is_none()));
        assert!(first.readers.get().is_autocommit());
        assert!(second.readers.get().is_autocommit());
    }
}
