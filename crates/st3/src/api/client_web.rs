//! Optional same-origin bundle and paired-client relays. Authentication stays in the client API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Extension, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::{ApiError, AppState};

const DEVICE_COOKIE: &str = "st3_device";
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);
const USAGE_MAX_BYTES: usize = 2 * 1024 * 1024;
const OTLP_MAX_BYTES: usize = 4 * 1024 * 1024;
const QUOTA_TTL: Duration = Duration::from_secs(5);
const HISTORY_TTL: Duration = Duration::from_secs(60);

/// Relay bodies are upstream contracts, not client API response envelopes.
#[derive(Clone, Copy)]
pub(super) struct RelayResponse;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientWebConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_dir: Option<PathBuf>,
    #[serde(default = "default_mount")]
    pub mount: String,
    /// Collector base URL; exports are sent to its `/v1/traces` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
}

fn default_mount() -> String {
    "/app".into()
}

impl Default for ClientWebConfig {
    fn default() -> Self {
        Self {
            static_dir: None,
            mount: default_mount(),
            otlp_endpoint: None,
        }
    }
}

impl ClientWebConfig {
    pub fn validate(&self) -> Result<()> {
        let mount = &self.mount;
        anyhow::ensure!(
            mount.starts_with('/')
                && mount.len() > 1
                && !mount.ends_with('/')
                && mount != "/v1"
                && !mount.starts_with("/v1/")
                && mount[1..].split('/').all(|part| {
                    !part.is_empty()
                        && part != "."
                        && part != ".."
                        && !part.contains(['\\', '%', '?', '#'])
                        && !part.chars().any(char::is_control)
                }),
            "client_web.mount must be an absolute path like `/app` outside `/v1`"
        );
        if let Some(endpoint) = &self.otlp_endpoint {
            validate_endpoint(endpoint, "client_web.otlp_endpoint")?;
        }
        Ok(())
    }
}

/// The independent `[usage]` upstream, available even when no bundle is configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    pub endpoint: String,
}

impl UsageConfig {
    pub fn validate(&self) -> Result<()> {
        validate_endpoint(&self.endpoint, "usage.endpoint")
    }
}

fn validate_endpoint(endpoint: &str, field: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint).with_context(|| format!("parse {field}"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{field} must be an HTTP(S) base URL without a query or fragment"
    );
    Ok(())
}

#[derive(Clone)]
struct UsageAnswer {
    body: Bytes,
    fetched: Instant,
}

#[derive(Clone, Debug)]
struct RelayFailure {
    status: StatusCode,
    message: &'static str,
}

impl RelayFailure {
    fn api_error(&self) -> ApiError {
        relay_error(self.status, "remote-unavailable", self.message)
    }
}

type UsageRead = Shared<BoxFuture<'static, std::result::Result<UsageAnswer, RelayFailure>>>;

pub struct ClientWeb {
    config: ClientWebConfig,
    bundle: Option<PathBuf>,
    usage: Option<UsageConfig>,
    http: reqwest::Client,
    usage_reads: Mutex<HashMap<String, UsageRead>>,
    node: String,
}

impl ClientWeb {
    pub fn new(config: ClientWebConfig, state: &AppState) -> Result<Arc<Self>> {
        Self::new_with_usage(Some(config), None, state)
    }

    pub fn new_with_usage(
        config: Option<ClientWebConfig>,
        usage: Option<UsageConfig>,
        state: &AppState,
    ) -> Result<Arc<Self>> {
        Self::build(config.unwrap_or_default(), usage, state.node.clone())
    }

    fn build(
        config: ClientWebConfig,
        usage: Option<UsageConfig>,
        node: String,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        if let Some(usage) = &usage {
            usage.validate()?;
        }
        let bundle = config
            .static_dir
            .as_ref()
            .map(|dir| {
                let root = std::fs::canonicalize(dir).context("resolve client_web.static_dir")?;
                anyhow::ensure!(root.is_dir(), "client_web.static_dir must be a directory");
                let index = std::fs::canonicalize(root.join("index.html"))
                    .context("client_web.static_dir must contain index.html")?;
                anyhow::ensure!(
                    index.starts_with(&root) && index.is_file(),
                    "bundle index must stay inside static_dir"
                );
                Ok::<_, anyhow::Error>(root)
            })
            .transpose()?;
        Ok(Arc::new(Self {
            config,
            bundle,
            usage,
            node,
            http: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            usage_reads: Mutex::new(HashMap::new()),
        }))
    }

    pub fn mount(&self) -> &str {
        &self.config.mount
    }

    pub fn bundle(&self) -> Option<PathBuf> {
        self.bundle.clone()
    }

    async fn usage_read(
        &self,
        url: String,
        ttl: Duration,
    ) -> std::result::Result<Bytes, RelayFailure> {
        let read = {
            let mut reads = self.usage_reads.lock().await;
            // Completed errors never become cached answers. Pending futures remain shareable even
            // if their first caller disconnects; the next caller drives the same fetch to completion.
            reads.retain(|_, read| match read.peek() {
                None => true,
                Some(Ok(answer)) => answer.fetched.elapsed() < HISTORY_TTL,
                Some(Err(_)) => false,
            });
            if reads
                .get(&url)
                .and_then(Shared::peek)
                .is_some_and(|answer| {
                    answer
                        .as_ref()
                        .is_ok_and(|answer| answer.fetched.elapsed() >= ttl)
                })
            {
                reads.remove(&url);
            }
            reads
                .entry(url.clone())
                .or_insert_with(|| {
                    let http = self.http.clone();
                    async move {
                        let upstream = http
                            .get(url)
                            .header("accept", "application/json")
                            .send()
                            .await
                            .map_err(|_| RelayFailure {
                                status: StatusCode::SERVICE_UNAVAILABLE,
                                message: "the usage upstream is unavailable",
                            })?;
                        if !upstream.status().is_success() {
                            return Err(RelayFailure {
                                status: StatusCode::BAD_GATEWAY,
                                message: "the usage upstream rejected the read",
                            });
                        }
                        let body = upstream.bytes().await.map_err(|_| RelayFailure {
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            message: "the usage upstream response failed",
                        })?;
                        if body.len() > USAGE_MAX_BYTES {
                            return Err(RelayFailure {
                                status: StatusCode::BAD_GATEWAY,
                                message: "the usage upstream response is too large",
                            });
                        }
                        Ok(UsageAnswer {
                            body,
                            fetched: Instant::now(),
                        })
                    }
                    .boxed()
                    .shared()
                })
                .clone()
        };
        read.await.map(|answer| answer.body)
    }
}

pub fn wrap(app: Router, web: Arc<ClientWeb>) -> Router {
    app.layer(axum::middleware::from_fn_with_state(
        web.clone(),
        static_or_next,
    ))
    .layer(Extension(web))
}

async fn static_or_next(
    State(web): State<Arc<ClientWeb>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // With no bundle installed the API's existing routing remains untouched.
    if web.bundle.is_none() {
        return next.run(request).await;
    }
    let path = request.uri().path();
    let mount = web.mount();
    let relative = path
        .strip_prefix(mount)
        .and_then(|rest| rest.strip_prefix('/'));
    if path != "/" && path != mount && relative.is_none() {
        return next.run(request).await;
    }
    let head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !head {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "the web client is read-only",
        )
            .into_response();
    }
    if path == "/" || path == mount {
        return (StatusCode::FOUND, [(LOCATION, format!("{mount}/"))]).into_response();
    }
    serve_static(&web, relative.unwrap_or_default(), head).await
}

async fn serve_static(web: &ClientWeb, relative: &str, head: bool) -> Response {
    let Some(root) = &web.bundle else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(decoded) = urlencoding::decode(relative) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut candidate = root.clone();
    let mut last = "";
    for part in decoded.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." || part.contains(['\\', '\0']) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        candidate.push(part);
        last = part;
        // Check existing ancestors too: a missing route below an escaping symlink must not
        // silently become a successful SPA response.
        match tokio::fs::canonicalize(&candidate).await {
            Ok(path) if !path.starts_with(root) => return StatusCode::NOT_FOUND.into_response(),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return StatusCode::NOT_FOUND.into_response();
            }
            _ => {}
        }
    }
    let file = match tokio::fs::metadata(&candidate).await {
        Ok(meta) if meta.is_file() => candidate,
        Ok(meta) if meta.is_dir() => candidate.join("index.html"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !last.contains('.') => {
            root.join("index.html")
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let file = match tokio::fs::canonicalize(file).await {
        Ok(file) if file.starts_with(root) => file,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let (length, mut response) = if head {
        let meta = match tokio::fs::metadata(&file).await {
            Ok(meta) if meta.is_file() => meta,
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        (meta.len(), StatusCode::OK.into_response())
    } else {
        let bytes = match tokio::fs::read(&file).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        (bytes.len() as u64, bytes.into_response())
    };
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type(&file)));
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(length));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(super) fn device_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == DEVICE_COOKIE)
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

pub(super) fn device_cookie_header(credential: &str, max_age: u64) -> String {
    format!(
        "{DEVICE_COOKIE}={credential}; Path=/v1/client; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict"
    )
}

fn runtime(web: Option<Extension<Arc<ClientWeb>>>) -> Result<Arc<ClientWeb>, ApiError> {
    web.map(|Extension(web)| web)
        .ok_or_else(|| ApiError::not_found("this gateway has no browser relays"))
}

/// Allowlisted quota read; no caller-supplied upstream path or filter is accepted.
pub(super) async fn usage_quota(
    web: Option<Extension<Arc<ClientWeb>>>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    if query.is_some_and(|query| !query.is_empty()) {
        return Err(relay_error(
            StatusCode::BAD_REQUEST,
            "validation-failed",
            "quota accepts no query parameters",
        ));
    }
    usage_response(runtime(web)?, None).await
}

/// Allowlisted account history with the lens filter pinned to daily account totals.
pub(super) async fn usage_history(
    web: Option<Extension<Arc<ClientWeb>>>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let mut params = reqwest::Url::parse("http://localhost/").map_err(ApiError::internal)?;
    params.set_query(query.as_deref());
    let mut pairs = params.query_pairs();
    let account = match (pairs.next(), pairs.next()) {
        (Some((key, account)), None) if key == "account" && !account.is_empty() => {
            account.into_owned()
        }
        _ => {
            return Err(relay_error(
                StatusCode::BAD_REQUEST,
                "validation-failed",
                "expected exactly ?account=<ledgerAccountId>",
            ));
        }
    };
    usage_response(runtime(web)?, Some(&account)).await
}

async fn usage_response(web: Arc<ClientWeb>, account: Option<&str>) -> Result<Response, ApiError> {
    let usage = web
        .usage
        .as_ref()
        .ok_or_else(|| ApiError::not_found("this gateway has no usage relay"))?;
    let path = if account.is_some() {
        "api/v1/lens/usage_over_time"
    } else {
        "api/v1/quota"
    };
    let mut url = reqwest::Url::parse(&format!("{}/{path}", usage.endpoint.trim_end_matches('/')))
        .map_err(ApiError::internal)?;
    if let Some(account) = account {
        url.query_pairs_mut().extend_pairs([
            ("window", "all"),
            ("group_by", "account"),
            ("bucket", "day"),
            ("group", account),
        ]);
    }
    let body = web
        .usage_read(
            url.into(),
            if account.is_some() {
                HISTORY_TTL
            } else {
                QUOTA_TTL
            },
        )
        .await
        .map_err(|error| error.api_error())?;
    let mut response = (
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response();
    response.extensions_mut().insert(RelayResponse);
    Ok(response)
}

/// Pass through JSON exports without changing client resource identity or span attributes.
pub(super) async fn telemetry_traces(
    web: Option<Extension<Arc<ClientWeb>>>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let web = runtime(web)?;
    let endpoint = web
        .config
        .otlp_endpoint
        .as_ref()
        .ok_or_else(|| ApiError::not_found("this gateway has no telemetry relay"))?;
    if !request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Err(relay_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "validation-failed",
            "the telemetry relay accepts OTLP/HTTP JSON",
        ));
    }
    let body = to_bytes(request.into_body(), OTLP_MAX_BYTES)
        .await
        .map_err(|_| {
            relay_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "validation-failed",
                "the telemetry export is too large",
            )
        })?;
    serde_json::from_slice::<serde::de::IgnoredAny>(&body).map_err(|_| {
        relay_error(
            StatusCode::BAD_REQUEST,
            "validation-failed",
            "the telemetry export is not JSON",
        )
    })?;
    let upstream = web
        .http
        .post(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| {
            relay_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "remote-unavailable",
                "the telemetry collector is unavailable",
            )
        })?;
    let status = upstream.status();
    let body = upstream.bytes().await.map_err(|_| {
        relay_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "remote-unavailable",
            "the telemetry collector response failed",
        )
    })?;
    let mut response = (
        status,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response();
    response.extensions_mut().insert(RelayResponse);
    Ok(response)
}

fn relay_error(status: StatusCode, code: &str, message: &str) -> ApiError {
    ApiError {
        status,
        code: code.into(),
        message: message.into(),
        details: Box::default(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TraceParent {
    pub(super) trace_id: String,
    pub(super) parent_span_id: String,
    pub(super) sampled: bool,
}

pub(super) fn parse_traceparent(value: &str) -> Option<TraceParent> {
    let mut parts = value.trim().split('-');
    let (version, trace_id, span_id, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let hex = |part: &str, len: usize| {
        part.len() == len
            && part
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    if parts.next().is_some()
        || version != "00"
        || !hex(trace_id, 32)
        || !hex(span_id, 16)
        || !hex(flags, 2)
        || trace_id.bytes().all(|byte| byte == b'0')
        || span_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(TraceParent {
        trace_id: trace_id.into(),
        parent_span_id: span_id.into(),
        sampled: u8::from_str_radix(flags, 16).ok()? & 1 == 1,
    })
}

pub(super) fn traceparent_query(query: Option<&str>) -> Option<TraceParent> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "traceparent")
        .and_then(|(_, value)| parse_traceparent(&urlencoding::decode(value).ok()?))
}

pub(super) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

pub(super) fn record_span(
    web: Option<&Arc<ClientWeb>>,
    parent: &TraceParent,
    name: &'static str,
    started_unix_nanos: u128,
    attributes: &[(&str, &str)],
) {
    let mut span_id = [0_u8; 8];
    if getrandom::fill(&mut span_id).is_err() {
        return;
    }
    let span_id = hex::encode(span_id);
    let ended = unix_nanos();
    tracing::info!(target: "st3::client_web", trace_id = %parent.trace_id,
        parent_span_id = %parent.parent_span_id, span_id = %span_id,
        duration_us = ended.saturating_sub(started_unix_nanos) / 1_000, "{name}");
    let Some(web) = web.filter(|_| parent.sampled) else {
        return;
    };
    let Some(endpoint) = &web.config.otlp_endpoint else {
        return;
    };
    let body = json!({"resourceSpans": [{
        "resource": {"attributes": [string_attribute("service.name", "st3-client-gateway"), string_attribute("host.name", &web.node)]},
        "scopeSpans": [{"scope": {"name": "st3.client_gateway"}, "spans": [{
            "traceId": parent.trace_id, "spanId": span_id, "parentSpanId": parent.parent_span_id,
            "name": name, "kind": 2, "startTimeUnixNano": started_unix_nanos.to_string(),
            "endTimeUnixNano": ended.to_string(), "flags": 1,
            "attributes": attributes.iter().map(|(key, value)| string_attribute(key, value)).collect::<Vec<_>>(),
            "status": {"code": 1},
        }]}],
    }]});
    let request = web
        .http
        .post(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .json(&body);
    tokio::spawn(async move {
        match request.send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(target: "st3::client_web", status = %response.status(), "gateway span export rejected")
            }
            Err(error) => {
                tracing::warn!(target: "st3::client_web", %error, "gateway span export failed")
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;
    use axum::routing::{get, post};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Notify, mpsc};
    use tower::ServiceExt as _;

    struct Server {
        endpoint: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(router: Router) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server { endpoint, task }
    }

    async fn body(response: Response) -> Bytes {
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    }

    fn web(config: ClientWebConfig, endpoint: Option<String>) -> Arc<ClientWeb> {
        ClientWeb::build(
            config,
            endpoint.map(|endpoint| UsageConfig { endpoint }),
            "test-node".into(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn paired_gateway_preserves_raw_relays_through_the_response_boundary() {
        use crate::model::ClaimInput;
        use sha2::{Digest as _, Sha256};
        use std::collections::BTreeMap;

        let quota = " { \"version\": 3, \"quotas\": [] } \n";
        let history = " { \"kind\": \"lens\", \"buckets\": [] } \n";
        let reply = " { \"partialSuccess\": { \"rejectedSpans\": 1 } } \n";
        let (observed, mut exports) = mpsc::unbounded_channel();
        let upstream = server(
            Router::new()
                .route(
                    "/api/v1/quota",
                    get(move || async move { ([(CONTENT_TYPE, "application/json")], quota) }),
                )
                .route(
                    "/api/v1/lens/usage_over_time",
                    get(move || async move { ([(CONTENT_TYPE, "application/json")], history) }),
                )
                .route(
                    "/v1/traces",
                    post(move |bytes: Bytes| {
                        let observed = observed.clone();
                        async move {
                            observed.send(bytes).unwrap();
                            (
                                StatusCode::ACCEPTED,
                                [(CONTENT_TYPE, "application/json")],
                                reply,
                            )
                        }
                    }),
                ),
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let credential = "raw-relay-paired-secret";
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/raw-relay-session".into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    (
                        "credential_hash".into(),
                        json!(hex::encode(Sha256::digest(credential.as_bytes()))),
                    ),
                    (
                        "session_actor".into(),
                        json!("person/alex/session/raw-relay"),
                    ),
                    ("person_id".into(), json!("person/alex")),
                    ("scopes".into(), json!(["read.projections"])),
                    (
                        "expires_at_unix_ms".into(),
                        json!(u64::try_from(unix_nanos() / 1_000_000).unwrap() + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let runtime = ClientWeb::new_with_usage(
            Some(ClientWebConfig {
                otlp_endpoint: Some(upstream.endpoint.clone()),
                ..Default::default()
            }),
            Some(UsageConfig {
                endpoint: upstream.endpoint.clone(),
            }),
            &state,
        )
        .unwrap();
        let app = super::super::fabric_router_with_web(state, runtime);
        for (path, expected) in [
            ("/v1/client/usage/quota", quota),
            (
                "/v1/client/usage/history?account=zai%2Fschickling-j",
                history,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("authorization", format!("Bearer {credential}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body(response).await, expected);
        }
        let export = " { \"resourceSpans\": [] } \n";
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/client/telemetry/traces")
                    .header(COOKIE, format!("st3_device={credential}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(export))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(body(response).await, reply);
        assert_eq!(exports.recv().await.unwrap(), export);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/client/usage/quota")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn static_routes_confine_paths_and_preserve_api_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), "<main>SPA</main>").unwrap();
        std::fs::create_dir(root.path().join("assets")).unwrap();
        std::fs::write(root.path().join("assets/app.js"), "export default 1").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("assets/app.js"),
            root.path().join("inside.js"),
        )
        .unwrap();
        let runtime = web(
            ClientWebConfig {
                static_dir: Some(root.path().into()),
                ..Default::default()
            },
            None,
        );
        let app = wrap(
            Router::new().route("/v1/probe", get(|| async { "API" })),
            runtime,
        );
        for (path, status, expected) in [
            ("/app/", StatusCode::OK, Some("<main>SPA</main>")),
            (
                "/app/missions/one",
                StatusCode::OK,
                Some("<main>SPA</main>"),
            ),
            (
                "/app/assets/app.js",
                StatusCode::OK,
                Some("export default 1"),
            ),
            ("/app/inside.js", StatusCode::OK, Some("export default 1")),
            ("/v1/probe", StatusCode::OK, Some("API")),
            ("/v1/missing", StatusCode::NOT_FOUND, None),
            ("/application", StatusCode::NOT_FOUND, None),
            ("/app/missing.js", StatusCode::NOT_FOUND, None),
            ("/app/escape/secret.txt", StatusCode::NOT_FOUND, None),
            ("/app/escape/missing", StatusCode::NOT_FOUND, None),
            ("/app/%2e%2e/secret.txt", StatusCode::BAD_REQUEST, None),
            (
                "/app/assets%2f..%2findex.html",
                StatusCode::BAD_REQUEST,
                None,
            ),
            ("/app/%5csecret", StatusCode::BAD_REQUEST, None),
            ("/app/%00", StatusCode::BAD_REQUEST, None),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{path}");
            if let Some(expected) = expected {
                assert_eq!(body(response).await, expected, "{path}");
            }
        }
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/app").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], "/app/");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/app/assets/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[CONTENT_LENGTH], "16");
        assert!(body(response).await.is_empty());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/app/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        std::fs::remove_file(root.path().join("index.html")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            root.path().join("index.html"),
        )
        .unwrap();
        assert!(
            ClientWeb::build(
                ClientWebConfig {
                    static_dir: Some(root.path().into()),
                    ..Default::default()
                },
                None,
                "test".into()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn usage_coalesces_reads_expires_and_does_not_cache_errors() {
        let hits = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        let (entered, mut requests) = mpsc::unbounded_channel();
        let upstream = server(Router::new().fallback({
            let hits = hits.clone();
            let failed = failed.clone();
            let gate = gate.clone();
            move || {
                let hits = hits.clone();
                let failed = failed.clone();
                let gate = gate.clone();
                let entered = entered.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    entered.send(()).unwrap();
                    gate.notified().await;
                    if failed.load(Ordering::SeqCst) {
                        (StatusCode::INTERNAL_SERVER_ERROR, "producer failed")
                    } else {
                        (StatusCode::OK, " { \"quota\" : 3 } \n")
                    }
                }
            }
        }))
        .await;
        let runtime = web(Default::default(), Some(upstream.endpoint.clone()));
        let url = format!("{}/api/v1/quota", upstream.endpoint);
        let (first, second) = tokio::join!(
            async {
                let first = runtime.usage_read(url.clone(), QUOTA_TTL);
                let second = runtime.usage_read(url.clone(), QUOTA_TTL);
                tokio::join!(first, second)
            },
            async {
                requests.recv().await.unwrap();
                gate.notify_one();
            }
        )
        .0;
        assert_eq!(first.unwrap(), " { \"quota\" : 3 } \n");
        assert_eq!(second.unwrap(), " { \"quota\" : 3 } \n");
        assert_eq!(
            runtime.usage_read(url.clone(), QUOTA_TTL).await.unwrap(),
            " { \"quota\" : 3 } \n"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        tokio::time::pause();
        tokio::time::advance(QUOTA_TTL).await;
        tokio::time::resume();
        let (answer, ()) = tokio::join!(runtime.usage_read(url.clone(), QUOTA_TTL), async {
            requests.recv().await.unwrap();
            gate.notify_one();
        });
        assert_eq!(answer.unwrap(), " { \"quota\" : 3 } \n");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        failed.store(true, Ordering::SeqCst);
        let ((first, second), ()) = tokio::join!(
            async {
                tokio::join!(
                    runtime.usage_read(url.clone(), Duration::ZERO),
                    runtime.usage_read(url.clone(), Duration::ZERO),
                )
            },
            async {
                requests.recv().await.unwrap();
                gate.notify_one();
            }
        );
        assert_eq!(first.unwrap_err().status, StatusCode::BAD_GATEWAY);
        assert_eq!(second.unwrap_err().status, StatusCode::BAD_GATEWAY);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        let (answer, ()) = tokio::join!(runtime.usage_read(url, QUOTA_TTL), async {
            requests.recv().await.unwrap();
            gate.notify_one();
        });
        assert_eq!(answer.unwrap_err().status, StatusCode::BAD_GATEWAY);
        assert_eq!(hits.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn usage_pins_filters_and_returns_opaque_bytes() {
        let (observed, mut requests) = mpsc::unbounded_channel();
        let upstream = server(Router::new().fallback(move |uri: Uri| {
            let observed = observed.clone();
            async move {
                observed.send(uri.to_string()).unwrap();
                " {\"version\":3,\"unknown_future_field\":true}\n"
            }
        }))
        .await;
        let runtime = web(Default::default(), Some(upstream.endpoint.clone()));
        let response = usage_history(
            Some(Extension(runtime.clone())),
            RawQuery(Some("account=zai%2Fschickling-j%26x".into())),
        )
        .await
        .unwrap();
        assert_eq!(
            body(response).await,
            " {\"version\":3,\"unknown_future_field\":true}\n"
        );
        let uri = requests.recv().await.unwrap();
        let url = reqwest::Url::parse(&format!("{}{}", upstream.endpoint, uri)).unwrap();
        assert_eq!(url.path(), "/api/v1/lens/usage_over_time");
        assert_eq!(
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>(),
            [
                ("window".to_owned(), "all".to_owned()),
                ("group_by".to_owned(), "account".to_owned()),
                ("bucket".to_owned(), "day".to_owned()),
                ("group".to_owned(), "zai/schickling-j&x".to_owned()),
            ]
        );
        usage_quota(Some(Extension(runtime.clone())), RawQuery(None))
            .await
            .unwrap();
        assert_eq!(requests.recv().await.unwrap(), "/api/v1/quota");
        for query in [
            None,
            Some("account="),
            Some("account=a&account=b"),
            Some("account=a&window=all"),
            Some("group=a"),
        ] {
            assert_eq!(
                usage_history(
                    Some(Extension(runtime.clone())),
                    RawQuery(query.map(str::to_owned))
                )
                .await
                .unwrap_err()
                .status,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            usage_quota(
                Some(Extension(runtime)),
                RawQuery(Some("path=/api/v1/root_detail".into()))
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            usage_quota(None, RawQuery(None)).await.unwrap_err().status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn telemetry_preserves_exports_and_collector_responses() {
        let (observed, mut exports) = mpsc::unbounded_channel();
        let collector = server(Router::new().route(
            "/v1/traces",
            post(move |headers: HeaderMap, bytes: Bytes| {
                let observed = observed.clone();
                async move {
                    observed.send((headers, bytes)).unwrap();
                    (
                        StatusCode::ACCEPTED,
                        "{\"partialSuccess\":{\"rejectedSpans\":1}}",
                    )
                }
            }),
        ))
        .await;
        let runtime = web(
            ClientWebConfig {
                otlp_endpoint: Some(collector.endpoint.clone()),
                ..Default::default()
            },
            None,
        );
        let export = " { \"resourceSpans\": [{\"resource\":{\"attributes\":[{\"key\":\"service.name\",\"value\":{\"stringValue\":\"client-owned\"}}]},\"scopeSpans\":[]}] } \n";
        let request = |payload: &str, media: &str| {
            Request::builder()
                .method(Method::POST)
                .header(CONTENT_TYPE, media)
                .body(Body::from(payload.to_owned()))
                .unwrap()
        };
        let response = telemetry_traces(
            Some(Extension(runtime.clone())),
            request(export, "application/json; charset=utf-8"),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            body(response).await,
            "{\"partialSuccess\":{\"rejectedSpans\":1}}"
        );
        let (headers, bytes) = exports.recv().await.unwrap();
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(bytes, export);
        assert_eq!(
            telemetry_traces(
                Some(Extension(runtime.clone())),
                request("not json", "application/json")
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            telemetry_traces(
                Some(Extension(runtime.clone())),
                request(export, "application/x-protobuf")
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let parent =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let started = unix_nanos();
        record_span(
            Some(&runtime),
            &parent,
            "client.subscribe",
            started,
            &[("collection", "missions")],
        );
        let (_, bytes) = exports.recv().await.unwrap();
        let recorded: Value = serde_json::from_slice(&bytes).unwrap();
        let span = &recorded["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], parent.trace_id);
        assert_eq!(span["parentSpanId"], parent.parent_span_id);
        assert_eq!(span["name"], "client.subscribe");
        assert_eq!(span["startTimeUnixNano"], started.to_string());
        assert_eq!(
            span["attributes"][0],
            string_attribute("collection", "missions")
        );
    }

    #[test]
    fn traceparent_parsing_validates_ids_flags_and_query_encoding() {
        let value = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let parent = parse_traceparent(value).unwrap();
        assert!(parent.sampled);
        assert_eq!(parent.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(
            traceparent_query(Some(&format!(
                "other=x&traceparent={}",
                value.replace('-', "%2D")
            ))),
            Some(parent)
        );
        assert!(
            !parse_traceparent(&value.replace("-01", "-02"))
                .unwrap()
                .sampled
        );
        for invalid in [
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-gg",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert_eq!(parse_traceparent(invalid), None, "{invalid}");
        }
        assert_eq!(traceparent_query(None), None);
    }

    #[test]
    fn cookies_select_only_the_device_credential() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("other=wrong"));
        headers.append(
            COOKIE,
            HeaderValue::from_static("more=x; st3_device=paired-secret; last=y"),
        );
        assert_eq!(device_cookie(&headers).as_deref(), Some("paired-secret"));
        headers.insert(COOKIE, HeaderValue::from_static("st3_device="));
        assert_eq!(device_cookie(&headers), None);
        assert_eq!(
            device_cookie_header("secret", 30),
            "st3_device=secret; Path=/v1/client; Max-Age=30; HttpOnly; Secure; SameSite=Strict"
        );
    }
}
