//! Frame successful JSON without decoding a second owned document or encoding its payload.
//! Ordinary routes call this on their existing handler worker after its reader loan ends.
use super::*;
use axum::body::{Bytes, HttpBody};
use serde::de::{MapAccess, SeqAccess, Visitor};
use std::convert::Infallible;

#[derive(Clone)]
pub struct Context {
    pub state: AppState,
    pub client_request: bool,
    pub observation_report: bool,
    pub request_id: String,
    pub client_snapshot: Option<ClientSnapshot>,
    pub server: Option<tracing::Span>,
    pub roster: bool,
}

#[derive(Clone)]
pub struct Metrics {
    pub elapsed: Duration,
    pub bytes: usize,
}

pub fn is_json(response: &Response) -> bool {
    response.status() != StatusCode::SWITCHING_PROTOCOLS
        && response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
}

impl Context {
    pub async fn wrap(&self, response: Response) -> Result<Response, ApiError> {
        self.wrap_inner(response, None).await
    }

    pub async fn wrap_owned(
        &self,
        response: Response,
        cancelled: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<Response, ApiError> {
        self.wrap_inner(response, Some(cancelled)).await
    }

    async fn wrap_inner(
        &self,
        response: Response,
        cancelled: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<Response, ApiError> {
        if !is_json(&response) {
            return Ok(response);
        }
        smallclaims::read_budget::check().map_err(ApiError::bad)?;
        let started = Instant::now();
        let mut diagnostic = crate::relay_trace::span(crate::relay_trace::Phase::Envelope);
        let mut status = response.status();
        let (mut parts, body) = response.into_parts();
        let snapshot = parts
            .extensions
            .remove::<ClientSnapshot>()
            .or_else(|| self.client_snapshot.clone());
        let assistant_read = parts.extensions.remove::<client_v0::assistant::ReadReceipt>();
        // Retain the envelope's existing request deadline while awaiting body frames.
        // Drop the actual collection future on expiry, including on forwarded reads.
        // No reader loan surrounds this wait or the synchronous codec below.
        let collect = async {
            if let Some(budget) = smallclaims::read_budget::current() {
                match tokio::time::timeout(budget.remaining(), to_bytes(body, usize::MAX)).await {
                    Ok(bytes) => Ok(bytes),
                    Err(_) => {
                        budget.cancel();
                        Err(ApiError::bad(budget.check().unwrap_err()))
                    }
                }
            } else {
                Ok(to_bytes(body, usize::MAX).await)
            }
        };
        let bytes = if let Some(cancelled) = cancelled {
            tokio::select! {
                biased;
                _ = cancelled => return Err(ApiError::bad(St3Error::new(
                    "read-deadline", "the request ended before its response body was collected",
                ))),
                result = collect => result?,
            }
        } else {
            collect.await?
        };
        smallclaims::read_budget::check().map_err(ApiError::bad)?;
        let serialize = || self.encode(status, snapshot, bytes);
        let mut encoded = match self.server.as_ref().filter(|_| self.roster) {
            Some(server) => {
                let _server = server.enter();
                crate::otel::roster_scope(|| crate::otel::roster_phase("serialization", serialize))
            }
            None => serialize(),
        };
        smallclaims::read_budget::check().map_err(ApiError::bad)?;
        if status.is_success() && let Some(receipt) = assistant_read {
            let within_budget = encoded.len <= receipt.max_response_bytes;
            let framed = to_bytes(encoded.body, usize::MAX).await
                .map_err(ApiError::internal)?;
            let authorization = client_v0::assistant::finish_read(
                &self.state, &receipt, &framed, within_budget,
            ).await;
            if !within_budget || authorization.is_err() {
                let error = authorization.err().unwrap_or_else(|| ApiError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    code: "response-budget-exceeded".into(),
                    message: "the entire assistant response envelope exceeds its approved byte budget".into(),
                    details: Box::default(),
                });
                status = error.status;
                parts.status = error.status;
                encoded = self.encode(error.status, None, Ok(serde_json::to_vec(
                    &json!({"code":error.code,"message":error.message}),
                ).map_err(ApiError::internal)?.into()));
            } else {
                encoded.body = Body::from(framed);
            }
        }
        if let Some(trace) = crate::relay_trace::current() {
            trace.response(&self.request_id);
        }
        diagnostic.finish(if status.is_success() {
            crate::relay_trace::Outcome::Completed
        } else {
            crate::relay_trace::Outcome::Failed
        });
        if let Some(server) = &self.server
            && let Some(rows) = encoded.rows
        {
            server.record("st.page.rows", rows as i64);
            server.record("st.page.bytes", encoded.len as i64);
        }
        // Preserve the old Full body's exact size hint and header policy. The HTTP
        // transport sets lengths as appropriate for HEAD/no-content responses.
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        parts.extensions.insert(Metrics {
            elapsed: started.elapsed(),
            bytes: encoded.len,
        });
        Ok(Response::from_parts(parts, encoded.body))
    }

    fn encode(
        &self,
        status: StatusCode,
        snapshot: Option<ClientSnapshot>,
        bytes: Result<Bytes, axum::Error>,
    ) -> Encoded {
        if status.is_success() {
            let (raw, rows) = match bytes {
                Ok(bytes) => match serde_json::from_slice::<Checked>(&bytes) {
                    // Visit with serde_json's normal depth/number/string checks, retaining
                    // only an item count. No Value tree and no second payload encoding.
                    Ok(checked) => (bytes, checked.page_rows()),
                    Err(error) => (error_value("invalid-server-json", error).into(), None),
                },
                Err(error) => (error_value("response-read-failed", error).into(), None),
            };
            let metadata = if self.observation_report {
                json!({"api_version": CLIENT_API_VERSION, "request_id": self.request_id})
            } else if self.client_request {
                json!({
                    "api_version": CLIENT_API_VERSION,
                    "request_id": self.request_id,
                    "snapshot": snapshot.expect("a successful client request has a snapshot"),
                })
            } else {
                json!({
                    "api_version": "st3.v1",
                    "request_id": self.request_id,
                    "snapshot_host": self.state.node,
                    "store_index": self.state.store.index().unwrap_or_default(),
                })
            };

            return frame(metadata, raw, rows);
        }
        // Error normalization inspects code/message/details. Keep its previous contract.
        let raw = match bytes {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|error| {
                json!({
                    "code": "invalid-server-json", "message": error.to_string(),
                })
            }),
            Err(error) => json!({"code": "response-read-failed", "message": error.to_string()}),
        };
        let mut envelope = if self.client_request {
            client_error_envelope(status, &raw, &self.request_id)
        } else {
            json!({
                "api_version": "st3.v1", "request_id": self.request_id,
                "snapshot_host": self.state.node,
                "store_index": self.state.store.index().unwrap_or_default(),
                "code": raw.get("code").and_then(Value::as_str).unwrap_or("request-failed"),
                "message": raw.get("message").and_then(Value::as_str).unwrap_or("the request failed"),
                "details": raw.get("details").cloned().unwrap_or_else(|| json!({})),
            })
        };
        if !self.client_request
            && let Some(applied) = raw.get("applied")
        {
            envelope["applied"] = applied.clone();
        }
        let bytes = serde_json::to_vec(&envelope).unwrap_or_else(|_| b"{}".to_vec());
        Encoded {
            len: bytes.len(),
            body: Body::from(bytes),
            rows: None,
        }
    }
}

fn error_value(code: &str, error: impl std::fmt::Display) -> Vec<u8> {
    serde_json::to_vec(&json!({"code": code, "message": error.to_string()}))
        .expect("an error code and message serialize")
}

struct Encoded {
    body: Body,
    len: usize,
    rows: Option<usize>,
}

fn frame(metadata: Value, raw: Bytes, rows: Option<usize>) -> Encoded {
    let mut prefix = serde_json::to_vec(&metadata).expect("envelope metadata serializes");
    assert_eq!(prefix.pop(), Some(b'}'));
    prefix.extend_from_slice(b",\"value\":");
    let suffix = Bytes::from_static(b"}");
    let len = prefix.len() + raw.len() + suffix.len();
    let frames = [Bytes::from(prefix), raw, suffix].into_iter();
    Encoded {
        body: Body::new(Frames {
            frames,
            remaining: len,
        }),
        len,
        rows,
    }
}

// Exact byte length like Full, without joining the payload to the framing bytes.
struct Frames {
    frames: std::array::IntoIter<Bytes, 3>,
    remaining: usize,
}
impl HttpBody for Frames {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        std::task::Poll::Ready(self.frames.next().map(|bytes| {
            self.remaining -= bytes.len();
            Ok(hyper::body::Frame::data(bytes))
        }))
    }
    fn is_end_stream(&self) -> bool {
        self.frames.len() == 0
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.remaining as u64)
    }
}

// Validate with the same deserialize_any path as Value, including finite-number,
// UTF-8 and recursion checks (RawValue/IgnoredAny skip some of those checks). Borrowed
// strings are discarded immediately; escaped strings use the deserializer's scratch
// buffer. Space is bounded by nesting and the largest escaped string, not a DOM.
// Count items in this same traversal for OTel; duplicate keys retain last-key-wins.
enum Checked {
    Array(usize),
    Object(Option<usize>),
    Other,
}
impl Checked {
    fn page_rows(self) -> Option<usize> {
        match self {
            Self::Object(rows) => rows,
            _ => None,
        }
    }
}
impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Check;
        impl<'de> Visitor<'de> for Check {
            type Value = Checked;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any valid JSON value")
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Checked, S::Error> {
                let mut count = 0;
                while seq.next_element::<Checked>()?.is_some() {
                    count += 1;
                }
                Ok(Checked::Array(count))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Checked, M::Error> {
                let mut rows = None;
                while let Some(ItemsKey(items)) = map.next_key::<ItemsKey>()? {
                    let value = map.next_value::<Checked>()?;
                    if items {
                        rows = match value {
                            Checked::Array(n) => Some(n),
                            _ => None,
                        };
                    }
                }
                Ok(Checked::Object(rows))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Checked, E> {
                Ok(Checked::Other)
            }
        }
        d.deserialize_any(Check)
    }
}
struct ItemsKey(bool);
impl<'de> Deserialize<'de> for ItemsKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Key;
        impl Visitor<'_> for Key {
            type Value = ItemsKey;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string key")
            }
            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<ItemsKey, E> {
                Ok(ItemsKey(key == "items"))
            }
        }
        d.deserialize_str(Key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn context(root: &std::path::Path, client: bool, observation: bool) -> Context {
        Context {
            state: super::super::tests::state(root),
            client_request: client,
            observation_report: observation,
            request_id: "request/control".into(),
            client_snapshot: client.then(snapshot),
            server: None,
            roster: false,
        }
    }
    fn snapshot() -> ClientSnapshot {
        ClientSnapshot {
            id: "snapshot/admission".into(),
            host_id: "host/node".into(),
            store_index: 7,
            projection_version: CLIENT_PROJECTION_VERSION.into(),
            created_at: "2026-01-01".into(),
            published_at: None,
        }
    }
    fn response(status: StatusCode, bytes: impl Into<Body>) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("x-control", "retained")
            .header("content-length", "999999")
            .body(bytes.into())
            .unwrap()
    }

    #[tokio::test]
    async fn successful_large_json_reuses_the_original_payload_frame() {
        let root = tempfile::tempdir().unwrap();
        let raw = Bytes::from(format!(
            " {{\"content\":\"{}\",\"escaped\":\"\\u0061\",\"number\":1.000e+0}} \n",
            "x".repeat(4 * 1024 * 1024)
        ));
        let pointer = raw.as_ptr();
        let raw_len = raw.len();
        let wrapped = context(root.path(), false, false)
            .wrap(response(StatusCode::OK, raw.clone()))
            .await
            .unwrap();
        assert!(!wrapped.headers().contains_key("content-length"));
        let length = wrapped.body().size_hint().exact().unwrap() as usize;
        assert_eq!(wrapped.headers()["x-control"], "retained");
        let mut body = wrapped.into_body();
        let prefix = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(
            body.size_hint().exact(),
            Some((length - prefix.len()) as u64)
        );
        let payload = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let suffix = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(
            payload.as_ptr(),
            pointer,
            "the payload must reuse the handler's byte allocation"
        );
        assert_eq!(
            payload, raw,
            "escapes, whitespace and number text pass through verbatim"
        );
        assert_eq!(length, prefix.len() + raw_len + suffix.len());
        assert!(body.frame().await.is_none());
        let full = [prefix.as_ref(), payload.as_ref(), suffix.as_ref()].concat();
        let envelope: Value = serde_json::from_slice(&full).unwrap();
        assert_eq!(
            envelope["value"],
            serde_json::from_slice::<Value>(&raw).unwrap()
        );
        assert_eq!(envelope["snapshot_host"], "node");
    }

    #[tokio::test]
    async fn metadata_errors_snapshots_and_non_json_keep_their_contracts() {
        let root = tempfile::tempdir().unwrap();
        for (client, observation) in [(false, false), (true, false), (true, true)] {
            let cx = context(root.path(), client, observation);
            let mut selected = snapshot();
            selected.id = "snapshot/selected".into();
            selected.store_index = 42;
            let mut reply = response(
                StatusCode::CREATED,
                Bytes::from_static(b"{\"items\":[1,2]}"),
            );
            reply.extensions_mut().insert(selected.clone());
            let wrapped = cx.wrap(reply).await.unwrap();
            assert_eq!(wrapped.status(), StatusCode::CREATED);
            let body = to_bytes(wrapped.into_body(), usize::MAX).await.unwrap();
            let envelope: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(envelope["request_id"], "request/control");
            assert_eq!(envelope["value"], json!({"items":[1,2]}));
            if client && !observation {
                assert_eq!(
                    envelope["snapshot"],
                    serde_json::to_value(selected).unwrap()
                );
            } else {
                assert!(envelope.get("snapshot").is_none());
            }
            for status in [StatusCode::BAD_REQUEST, StatusCode::SERVICE_UNAVAILABLE] {
                let raw = json!({"code":"remote-unavailable", "message":"peer absent", "details":{"x":1}});
                let wrapped = cx
                    .wrap(response(status, serde_json::to_vec(&raw).unwrap()))
                    .await
                    .unwrap();
                let got: Value = serde_json::from_slice(
                    &to_bytes(wrapped.into_body(), usize::MAX).await.unwrap(),
                )
                .unwrap();
                let expected = if client {
                    client_error_envelope(status, &raw, &cx.request_id)
                } else {
                    json!({"api_version":"st3.v1", "request_id":cx.request_id,
                        "snapshot_host":cx.state.node, "store_index":cx.state.store.index().unwrap(),
                        "code":raw["code"], "message":raw["message"], "details":raw["details"]})
                };
                assert_eq!(got, expected);
            }
        }
        for status in [StatusCode::OK, StatusCode::SWITCHING_PROTOCOLS] {
            let reply = Response::builder()
                .status(status)
                .header("content-type", "text/plain")
                .header("content-length", "5")
                .body(Body::from("plain"))
                .unwrap();
            let wrapped = context(root.path(), false, false)
                .wrap(reply)
                .await
                .unwrap();
            assert_eq!(wrapped.headers()["content-length"], "5");
            assert!(wrapped.extensions().get::<Metrics>().is_none());
            assert_eq!(to_bytes(wrapped.into_body(), 100).await.unwrap(), "plain");
        }
    }

    #[tokio::test]
    async fn malformed_json_and_read_failures_keep_old_error_values() {
        let root = tempfile::tempdir().unwrap();
        let cx = context(root.path(), false, false);
        let mut corpus: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"{x}".to_vec(),
            b"1e9999".to_vec(),
            b"[1,]".to_vec(),
            b"{} trailing".to_vec(),
            b"{\"a\":\"\xff\"}".to_vec(),
            b"\"\\uD800\"".to_vec(),
        ];
        corpus.push(format!("{}0{}", "[".repeat(150), "]".repeat(150)).into_bytes());
        for raw in corpus {
            let old_error = serde_json::from_slice::<Value>(&raw)
                .unwrap_err()
                .to_string();
            assert_eq!(
                serde_json::from_slice::<Checked>(&raw)
                    .err()
                    .unwrap()
                    .to_string(),
                old_error
            );
            let wrapped = cx.wrap(response(StatusCode::OK, raw)).await.unwrap();
            let got: Value =
                serde_json::from_slice(&to_bytes(wrapped.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(
                got["value"],
                json!({"code":"invalid-server-json", "message":old_error})
            );
        }
        let failing = || {
            Body::from_stream(futures_util::stream::iter([Err::<Bytes, _>(
                std::io::Error::other("body failed"),
            )]))
        };
        let old_error = to_bytes(failing(), usize::MAX)
            .await
            .unwrap_err()
            .to_string();
        let wrapped = cx.wrap(response(StatusCode::OK, failing())).await.unwrap();
        let got: Value =
            serde_json::from_slice(&to_bytes(wrapped.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            got["value"],
            json!({"code":"response-read-failed", "message":old_error})
        );
        for raw in [
            b"{\"items\":[1,2]}".as_slice(),
            b"{\"items\":[1],\"items\":false}",
            b"{\"it\\u0065ms\":[{},null,[]]}",
            b"{\"other\":{\"items\":[1]}}",
            b"null",
        ] {
            let old: Value = serde_json::from_slice(raw).unwrap();
            let expected = old.get("items").and_then(Value::as_array).map(Vec::len);
            assert_eq!(
                serde_json::from_slice::<Checked>(raw).unwrap().page_rows(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn body_collection_on_the_handler_worker_leaves_health_free_and_holds_no_reader() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let store = state.store.clone();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(Mutex::new(gate));
        let expired = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(
                "/codec-control",
                get({
                    let expired = expired.clone();
                    move || {
                        let (entered, gate, expired, store) = (
                            entered.clone(),
                            gate.clone(),
                            expired.clone(),
                            store.clone(),
                        );
                        async move {
                            let mut done = false;
                            let body =
                                Body::from_stream(futures_util::stream::poll_fn(move |_| {
                                    if done {
                                        return std::task::Poll::Ready(None);
                                    }
                                    done = true;
                                    assert!(
                                        !smallclaims::sqlite::thread_holds_reader(),
                                        "body framing must not hold the handler reader"
                                    );
                                    assert!(!store.readers.has_request_reader());
                                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                                    // A bounded failure guard prevents a broken acceptor path hanging the suite.
                                    if gate
                                        .lock()
                                        .unwrap()
                                        .recv_timeout(Duration::from_secs(3))
                                        .is_err()
                                    {
                                        expired.store(true, Ordering::SeqCst);
                                    }
                                    std::task::Poll::Ready(Some(Ok::<_, Infallible>(
                                        Bytes::from_static(b"{\"ok\":true}"),
                                    )))
                                }));
                            response(StatusCode::OK, body)
                        }
                    }
                }),
            )
            .route("/v1/health", get(|| async { Json(json!({"ok":true})) }))
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let slow = tokio::spawn(
            app.clone().oneshot(
                Request::builder()
                    .uri("/codec-control")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        waiting.await.unwrap();
        let healthy = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let released_after_health = !expired.load(Ordering::SeqCst);
        let _ = release.send(());
        let wrapped = slow.await.unwrap().unwrap();
        assert!(
            released_after_health,
            "the health request must finish before the body gate is released"
        );
        assert_eq!(healthy.status(), StatusCode::OK);
        assert_eq!(wrapped.status(), StatusCode::OK);
        let got: Value =
            serde_json::from_slice(&to_bytes(wrapped.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(got["value"], json!({"ok":true}));
    }

    #[tokio::test]
    async fn pending_body_is_dropped_at_the_existing_worker_read_deadline() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let cx = context(root.path(), false, false);
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
            let _ = &guard;
            std::task::Poll::Pending::<Option<Result<Bytes, Infallible>>>
        }));
        let budget =
            smallclaims::read_budget::ReadBudget::new("/codec-control", Duration::from_millis(50));
        let runtime = tokio::runtime::Handle::current();
        let work = smallclaims::read_budget::with(Some(budget.clone()), || {
            read_deadline::spawn_handler(None, None, move || {
                runtime.block_on(cx.wrap(response(StatusCode::OK, body)))
            })
        });
        assert!(work.await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the actual body future must be dropped, not only its outer waiter");
        assert!(budget.expired());
    }

    #[tokio::test]
    async fn dropping_the_request_drops_pending_body_without_waiting_for_its_deadline() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let dropped = Arc::new(AtomicBool::new(false));
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let app = Router::new()
            .route(
                "/v1/pending-body",
                get({
                    let dropped = dropped.clone();
                    move || {
                        let guard = Dropped(dropped.clone());
                        let entered = entered.clone();
                        async move {
                            let body =
                                Body::from_stream(futures_util::stream::poll_fn(move |_| {
                                    let _ = &guard;
                                    if let Some(entered) = entered.lock().unwrap().take() {
                                        let _ = entered.send(());
                                    }
                                    std::task::Poll::Pending::<Option<Result<Bytes, Infallible>>>
                                }));
                            response(StatusCode::OK, body)
                        }
                    }
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let pending = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .uri("/v1/pending-body")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the request must promptly drop the actual body, before its 15s deadline");
    }

    #[tokio::test]
    async fn large_framed_response_has_exact_length_over_the_real_unix_http_transport() {
        use http_body_util::Empty;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let state = super::super::tests::state(root.path());
        let raw = Bytes::from(format!(
            " {{\"content\":\"{}\",\"escaped\":\"\\u0061\"}} ",
            "x".repeat(4 * 1024 * 1024)
        ));
        let app = Router::new()
            .route(
                "/v1/socket-framing",
                get({
                    let raw = raw.clone();
                    move || {
                        let raw = raw.clone();
                        async move { response(StatusCode::OK, raw) }
                    }
                }),
            )
            .layer(from_fn_with_state(
                (state, ClientTransportBoundary::Unix),
                super::super::response_envelope,
            ));
        let (ready, ready_wait) = tokio::sync::oneshot::channel();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            // The production Unix/HTTP server, with fixture-only ancestry disabled.
            super::super::serve_unix_with_ancestor_ready(
                &server_socket,
                None,
                app,
                false,
                |_| None,
                || {
                    let _ = ready.send(());
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), ready_wait)
            .await
            .unwrap()
            .unwrap();
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let connection = tokio::spawn(connection);
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let reply = sender
                .send_request(
                    Request::builder()
                        .uri("/v1/socket-framing")
                        .header("host", "local")
                        .header("connection", "close")
                        .body(Empty::<Bytes>::new())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = reply.status();
            let headers = reply.headers().clone();
            let body = reply.into_body().collect().await.unwrap().to_bytes();
            (status, headers, body)
        })
        .await;
        // Close both sides before any assertion can leave an accept/connection task behind.
        drop(sender);
        connection.abort();
        let _ = connection.await;
        server.abort();
        let _ = server.await;
        let (status, headers, body) = result.expect("the complete large HTTP body must arrive");
        assert_eq!(status, StatusCode::OK);
        assert!(!headers.contains_key("transfer-encoding"), "{headers:?}");
        assert_eq!(
            headers["content-length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            body.len()
        );
        assert_eq!(headers["x-control"], "retained");
        let marker = b"\"value\":";
        let start = body
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap()
            + marker.len();
        assert_eq!(
            &body[start..body.len() - 1],
            raw.as_ref(),
            "the complete original payload must reach the real socket unchanged"
        );
    }
}
