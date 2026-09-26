use std::fmt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use pty_core::client::tty::{FdWriter, is_tty};
use pty_core::client::{
    AttachParams, CURSOR_TO_BOTTOM, ClientIo, Reconnect, RouteRefusedError, TERMINAL_SANITIZE,
    attach,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

#[cfg(test)]
use crate::model::ApiResponse;
use crate::model::{ApiErrorResponse, AttachRequest, Attachment};

#[derive(Clone, Debug)]
pub enum Endpoint {
    Unix(PathBuf),
    Http(String),
}

impl Endpoint {
    pub fn parse(value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        if value.starts_with("http://") || value.starts_with("https://") {
            Self::Http(value.trim_end_matches('/').into())
        } else if let Some(path) = value.strip_prefix("unix://") {
            Self::Unix(PathBuf::from(format!("/{path}").replace("//", "/")))
        } else {
            Self::Unix(PathBuf::from(value))
        }
    }
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    http: reqwest::Client,
    deadlines: ClientDeadlines,
    person: Option<String>,
}

#[derive(Clone, Copy)]
struct ClientDeadlines {
    connect: Duration,
    request: Duration,
    bulk: Duration,
    event: Duration,
    terminal_handshake: Duration,
}

impl Default for ClientDeadlines {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            request: Duration::from_secs(15),
            bulk: Duration::from_secs(120),
            event: Duration::from_secs(35),
            terminal_handshake: Duration::from_secs(10),
        }
    }
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        let deadlines = ClientDeadlines::default();
        Self {
            endpoint,
            http: reqwest::Client::builder()
                .connect_timeout(deadlines.connect)
                .build()
                .expect("the st3 HTTP client configuration is valid"),
            deadlines,
            person: None,
        }
    }

    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::new(Endpoint::Unix(path.into()))
    }

    /// Name the concrete human authority carried over the trusted Unix boundary.
    /// Ordinary [`Client::unix`] sessions intentionally remain read-only.
    pub fn unix_as(path: impl Into<PathBuf>, person: impl Into<String>) -> Result<Self> {
        let person = person.into();
        anyhow::ensure!(
            person.starts_with("person/")
                && person.matches('/').count() == 1
                && !person.chars().any(char::is_whitespace),
            "Unix person authority must be one concrete `person/<id>` subject"
        );
        let mut client = Self::unix(path);
        client.person = Some(person);
        Ok(client)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let first_page = path.starts_with("/v1/client/") && !request_has_page_cursor(path);
        for attempt in 0..3 {
            match self.request::<(), T>("GET", path, None).await {
                Ok(value) => return Ok(value),
                Err(error)
                    if first_page
                        && attempt < 2
                        && error
                            .downcast_ref::<ApiResponseError>()
                            .is_some_and(|error| error.code == "page-cursor-expired") =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the bounded first-page retry loop always returns")
    }

    pub async fn post<I: Serialize, O: DeserializeOwned>(&self, path: &str, body: &I) -> Result<O> {
        self.request("POST", path, Some(body)).await
    }

    pub async fn request<I: Serialize, O: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&I>,
    ) -> Result<O> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let deadline = request_deadline(path, self.deadlines);
        let response = match &self.endpoint {
            Endpoint::Unix(socket) => {
                unix_request(
                    socket,
                    method,
                    path,
                    &bytes,
                    self.deadlines.connect,
                    deadline,
                    self.person.as_deref(),
                )
                .await?
            }
            Endpoint::Http(base) => {
                let started = tokio::time::Instant::now();
                let url = format!("{base}{path}");
                let mut request = match method {
                    "GET" => self.http.get(&url),
                    "POST" => self.http.post(&url).body(bytes),
                    other => anyhow::bail!("unsupported HTTP method {other}"),
                }
                .header("content-type", "application/json")
                .header("connection", "close");
                if let Some(person) = self.person.as_deref() {
                    request = request.header("x-st3-person", person);
                }
                let endpoint = url.clone();
                let response = tokio::time::timeout(deadline, request.send())
                    .await
                    .map_err(|_| deadline_error(&endpoint, "request", deadline))?
                    .with_context(|| format!("connect to the st3 API endpoint {endpoint}"))?;
                let status = response.status();
                let remaining = deadline.saturating_sub(started.elapsed());
                let bytes = tokio::time::timeout(remaining, response.bytes())
                    .await
                    .map_err(|_| deadline_error(&endpoint, "response", deadline))??
                    .to_vec();
                if !status.is_success() {
                    return Err(api_error(status.as_u16(), &bytes));
                }
                bytes
            }
        };
        decode_api_response(&response)
    }

    pub async fn proxy_terminal(&self, name: &str, path: &str) -> Result<i32> {
        let stream = self.open_terminal_bridge(path).await?;
        proxy_stream_with_io(name, stream, None, ClientIo::default()).await
    }

    /// Keep one interactive terminal attachment alive while the local gateway restarts.
    /// Every reconnect obtains a fresh one-use capability and remains fenced to the original
    /// terminal incarnation.
    pub async fn proxy_terminal_resilient(
        &self,
        subject: &str,
        attachment: &Attachment,
    ) -> Result<i32> {
        self.proxy_terminal_resilient_with_io(subject, attachment, ClientIo::default())
            .await
    }

    async fn proxy_terminal_resilient_with_io(
        &self,
        subject: &str,
        attachment: &Attachment,
        io: ClientIo,
    ) -> Result<i32> {
        let expected_incarnation = attachment
            .incarnation_id
            .clone()
            .context("the terminal attachment has no incarnation fence")?;
        let initial = self
            .open_terminal_bridge(&attachment.websocket_path)
            .await?;
        let client = self.clone();
        let subject = subject.to_owned();
        let name = attachment.runtime_id.clone();
        let handle = tokio::runtime::Handle::current();
        let reconnect: Reconnect = Box::new(move || {
            let next: Attachment = match handle.block_on(client.post(
                &format!("/v1/sessions/attach/{}", urlencoding::encode(&subject)),
                &AttachRequest::default(),
            )) {
                Ok(attachment) => attachment,
                Err(error) if terminal_reconnect_is_refused(&error) => {
                    return Err(RouteRefusedError(error.to_string()));
                }
                Err(_) => return Ok(None),
            };
            if next.incarnation_id.as_deref() != Some(expected_incarnation.as_str()) {
                return Err(RouteRefusedError(format!(
                    "terminal `{subject}` changed incarnation"
                )));
            }
            match handle.block_on(client.open_terminal_bridge(&next.websocket_path)) {
                Ok(stream) => Ok(Some(stream)),
                Err(error) if terminal_reconnect_is_refused(&error) => {
                    Err(RouteRefusedError(error.to_string()))
                }
                Err(_) => Ok(None),
            }
        });
        proxy_stream_with_io(&name, initial, Some(reconnect), io).await
    }

    async fn open_terminal_bridge(&self, path: &str) -> Result<StdUnixStream> {
        match &self.endpoint {
            Endpoint::Unix(socket) => {
                let endpoint = socket.display().to_string();
                let stream = tokio::time::timeout(
                    self.deadlines.connect,
                    tokio::net::UnixStream::connect(socket),
                )
                .await
                .map_err(|_| deadline_error(&endpoint, "connect", self.deadlines.connect))?
                .with_context(|| format!("connect to the st3 API at {endpoint}"))?;
                let request = terminal_request(&format!("ws://localhost{path}"))?;
                let (websocket, _) = tokio::time::timeout(
                    self.deadlines.terminal_handshake,
                    tokio_tungstenite::client_async(request, stream),
                )
                .await
                .map_err(|_| {
                    deadline_error(
                        &endpoint,
                        "terminal WebSocket handshake",
                        self.deadlines.terminal_handshake,
                    )
                })??;
                spawn_terminal_bridge(websocket)
            }
            Endpoint::Http(base) => {
                let base = base
                    .strip_prefix("http://")
                    .map(|value| format!("ws://{value}"))
                    .or_else(|| {
                        base.strip_prefix("https://")
                            .map(|value| format!("wss://{value}"))
                    })
                    .context("a terminal endpoint must use http or https")?;
                let endpoint = format!("{base}{path}");
                let request = terminal_request(&endpoint)?;
                let (websocket, _) = tokio::time::timeout(
                    self.deadlines.terminal_handshake,
                    tokio_tungstenite::connect_async(request),
                )
                .await
                .map_err(|_| {
                    deadline_error(
                        &endpoint,
                        "terminal WebSocket handshake",
                        self.deadlines.terminal_handshake,
                    )
                })??;
                spawn_terminal_bridge(websocket)
            }
        }
    }
}

fn terminal_reconnect_is_refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiResponseError>()
        .is_some_and(|error| {
            matches!(
                error.code.as_str(),
                "not-found" | "stale-incarnation" | "runtime-not-local" | "unsupported-capability"
            )
        })
}

fn request_has_page_cursor(path: &str) -> bool {
    path.split_once('?').is_some_and(|(_, query)| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .any(|(name, _)| name == "cursor")
    })
}

#[derive(Debug)]
struct ApiResponseError {
    status: u16,
    code: String,
    message: String,
}

impl fmt::Display for ApiResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "st3 API returned {} {}: {}",
            self.status, self.code, self.message
        )
    }
}

impl std::error::Error for ApiResponseError {}

fn terminal_request(url: &str) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "st3.terminal.v1"
            .parse()
            .expect("the protocol header is valid"),
    );
    Ok(request)
}

#[cfg(test)]
async fn proxy_websocket_with_io<S>(
    name: &str,
    websocket: tokio_tungstenite::WebSocketStream<S>,
    io: ClientIo,
) -> Result<i32>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (client_stream, bridge_stream) = StdUnixStream::pair()?;
    bridge_stream.set_nonblocking(true)?;
    let bridge_stream = tokio::net::UnixStream::from_std(bridge_stream)?;
    let bridge = tokio::spawn(bridge_terminal_protocol(websocket, bridge_stream));
    let name = name.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        let outcome = attach(AttachParams::new(&name, client_stream), &io);
        sanitize_interactive_terminal(io);
        outcome
    })
    .await
    .context("join the terminal client")?;
    bridge.abort();
    Ok(outcome.exit_code())
}

fn spawn_terminal_bridge<S>(
    websocket: tokio_tungstenite::WebSocketStream<S>,
) -> Result<StdUnixStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (client_stream, bridge_stream) = StdUnixStream::pair()?;
    bridge_stream.set_nonblocking(true)?;
    let bridge_stream = tokio::net::UnixStream::from_std(bridge_stream)?;
    tokio::spawn(async move {
        let _ = bridge_terminal_protocol(websocket, bridge_stream).await;
    });
    Ok(client_stream)
}

async fn proxy_stream_with_io(
    name: &str,
    stream: StdUnixStream,
    reconnect: Option<Reconnect>,
    io: ClientIo,
) -> Result<i32> {
    let name = name.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut params = AttachParams::new(&name, stream);
        params.reconnect = reconnect;
        let outcome = attach(params, &io);
        sanitize_interactive_terminal(io);
        outcome
    })
    .await
    .context("join the terminal client")?;
    Ok(outcome.exit_code())
}

fn sanitize_interactive_terminal(io: ClientIo) {
    if is_tty(io.stdout) {
        use std::io::Write as _;
        let mut output = FdWriter(io.stdout);
        let _ = output.write_all(TERMINAL_SANITIZE.as_bytes());
        // TERMINAL_SANITIZE leaves the alternate screen. Repeating that cleanup after the
        // underlying attach client returns can restore a saved cursor in the middle of the old
        // main-screen content. Always reposition afterwards so the caller's prompt starts below
        // the attached session, matching the standalone pty client's detach behavior.
        let _ = output.write_all(CURSOR_TO_BOTTOM.as_bytes());
        let _ = output.write_all(b"\r\n");
    }
}

async fn bridge_terminal_protocol<S>(
    websocket: tokio_tungstenite::WebSocketStream<S>,
    stream: tokio::net::UnixStream,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut writer, mut reader) = websocket.split();
    let (mut input, mut output) = stream.into_split();
    let mut bytes = vec![0_u8; 65_536];
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        tokio::select! {
            read = input.read(&mut bytes) => {
                let count = read?;
                if count == 0 {
                    writer.close().await?;
                    break;
                }
                writer.send(tokio_tungstenite::tungstenite::Message::Binary(bytes[..count].to_vec().into())).await?;
            }
            message = reader.next() => {
                match message {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                        output.write_all(&bytes).await?;
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        output.write_all(text.as_bytes()).await?;
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                }
            }
        }
    }
    Ok(())
}

async fn unix_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: &[u8],
    connect_deadline: Duration,
    deadline: Duration,
    person: Option<&str>,
) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let started = tokio::time::Instant::now();
    let endpoint = socket.display().to_string();
    let mut stream = tokio::time::timeout(
        connect_deadline.min(deadline),
        tokio::net::UnixStream::connect(socket),
    )
    .await
    .map_err(|_| deadline_error(&endpoint, "connect", connect_deadline))?
    .with_context(|| {
        format!(
            "connect to the st3 API at {}; run `st3 up` first",
            socket.display()
        )
    })?;
    let remaining = deadline.saturating_sub(started.elapsed());
    let response = tokio::time::timeout(remaining, async {
        let person_header = person
            .map(|person| format!("X-St3-Person: {person}\r\n"))
            .unwrap_or_default();
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{person_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        stream.write_all(body).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok::<_, std::io::Error>(response)
    })
    .await
    .map_err(|_| deadline_error(&endpoint, "request and response", deadline))??;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("the st3 API returned an incomplete HTTP response")?;
    let header = std::str::from_utf8(&response[..header_end])?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("the st3 API returned an invalid HTTP status")?;
    let mut body = response[(header_end + 4)..].to_vec();
    if header
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        body = decode_chunked(&body)?;
    } else if let Some(length) = header.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
    }) {
        anyhow::ensure!(
            body.len() >= length,
            "the st3 API returned an incomplete HTTP response: expected {length} body bytes, received {}",
            body.len()
        );
        body.truncate(length);
    }
    if !(200..300).contains(&status) {
        return Err(api_error(status, &body));
    }
    Ok(body)
}

fn request_deadline(path: &str, deadlines: ClientDeadlines) -> Duration {
    if path.starts_with("/v1/internal/replication/export")
        || path.starts_with("/v1/internal/replication/receive")
    {
        deadlines.bulk
    } else if path.starts_with("/v1/events?") && path.contains("wait=true") {
        deadlines.event
    } else {
        deadlines.request
    }
}

fn deadline_error(endpoint: &str, phase: &str, deadline: Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "st3 API endpoint `{endpoint}` exceeded the {phase} limit of {} ms; the service may be busy or unavailable, retry the command",
        deadline.as_millis()
    )
}

fn decode_chunked(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = 0;
    let mut output = Vec::new();
    loop {
        let end = bytes[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|position| cursor + position)
            .context("invalid chunked API response")?;
        let length = usize::from_str_radix(std::str::from_utf8(&bytes[cursor..end])?.trim(), 16)?;
        cursor = end + 2;
        if length == 0 {
            break;
        }
        anyhow::ensure!(
            cursor + length <= bytes.len(),
            "truncated chunked API response"
        );
        output.extend_from_slice(&bytes[cursor..cursor + length]);
        cursor += length + 2;
    }
    Ok(output)
}

fn api_error(status: u16, bytes: &[u8]) -> anyhow::Error {
    if let Ok(error) = serde_json::from_slice::<ApiErrorResponse>(bytes) {
        return ApiResponseError {
            status,
            code: error.code,
            message: error.message,
        }
        .into();
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(message) = value.get("message").and_then(|value| value.as_str())
    {
        if let Some(code) = value.get("code").and_then(|value| value.as_str()) {
            return ApiResponseError {
                status,
                code: code.into(),
                message: message.into(),
            }
            .into();
        }
        return anyhow::anyhow!("st3 API returned {status}: {message}");
    }
    anyhow::anyhow!(
        "st3 API returned {status}: {}",
        String::from_utf8_lossy(bytes).trim()
    )
}

fn decode_api_response<O: DeserializeOwned>(bytes: &[u8]) -> Result<O> {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(bytes).context("decode the st3 API response envelope")?;
    let version = envelope
        .get("api_version")
        .and_then(serde_json::Value::as_str)
        .context("the st3 API response envelope has no api_version")?;
    anyhow::ensure!(
        matches!(version, "st3.v1" | "st3.client.v0"),
        "the st3 API returned unsupported version {version}"
    );
    let value = envelope
        .as_object_mut()
        .and_then(|envelope| envelope.remove("value"))
        .context("the st3 API response envelope has no value")?;
    serde_json::from_value(value).context("decode the st3 API response value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message as AxumWsMessage, WebSocketUpgrade};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse as _;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    fn test_api() -> Router {
        Router::new()
            .route(
                "/v1/test",
                get(|| async { Json(test_envelope(json!({"method": "get"}))) })
                    .post(|Json(body): Json<Value>| async move { Json(test_envelope(body)) }),
            )
            .route(
                "/v1/person",
                get(|headers: HeaderMap| async move {
                    Json(test_envelope(json!({
                        "person": headers
                            .get("x-st3-person")
                            .and_then(|value| value.to_str().ok())
                    })))
                }),
            )
            .route(
                "/v1/client-test",
                get(|| async { Json(test_client_envelope(json!({"method": "client"}))) }),
            )
    }

    fn test_envelope(value: Value) -> ApiResponse<Value> {
        ApiResponse {
            api_version: "st3.v1".into(),
            request_id: "test-request".into(),
            snapshot_host: "test-node".into(),
            store_index: 1,
            value,
        }
    }

    fn test_client_envelope(value: Value) -> Value {
        json!({
            "api_version": "st3.client.v0",
            "request_id": "test-client-request",
            "snapshot": {
                "id": "snapshot/test-node/1/test",
                "host_id": "host/test-node",
                "store_index": 1,
                "projection_version": "client-projection.v0",
                "created_at": "2026-09-21T00:00:00.000Z"
            },
            "value": value
        })
    }

    #[test]
    fn response_decoding_accepts_internal_and_client_envelopes() {
        let internal = serde_json::to_vec(&test_envelope(json!({"kind": "internal"}))).unwrap();
        let client = serde_json::to_vec(&test_client_envelope(json!({"kind": "client"}))).unwrap();

        assert_eq!(
            decode_api_response::<Value>(&internal).unwrap(),
            json!({"kind": "internal"})
        );
        assert_eq!(
            decode_api_response::<Value>(&client).unwrap(),
            json!({"kind": "client"})
        );
    }

    #[test]
    fn response_decoding_rejects_unknown_versions_and_missing_values() {
        let unknown = serde_json::to_vec(&json!({
            "api_version": "st3.future.v9",
            "value": {}
        }))
        .unwrap();
        assert!(
            decode_api_response::<Value>(&unknown)
                .unwrap_err()
                .to_string()
                .contains("unsupported version")
        );

        let missing = serde_json::to_vec(&json!({"api_version": "st3.client.v0"})).unwrap();
        assert!(
            decode_api_response::<Value>(&missing)
                .unwrap_err()
                .to_string()
                .contains("has no value")
        );
    }

    fn fast_client(endpoint: Endpoint) -> Client {
        let deadlines = ClientDeadlines {
            connect: Duration::from_millis(20),
            request: Duration::from_millis(30),
            bulk: Duration::from_millis(40),
            event: Duration::from_millis(50),
            terminal_handshake: Duration::from_millis(30),
        };
        Client {
            endpoint,
            http: reqwest::Client::builder()
                .connect_timeout(deadlines.connect)
                .build()
                .unwrap(),
            deadlines,
            person: None,
        }
    }

    #[test]
    fn request_classes_have_distinct_bounded_deadlines() {
        let deadlines = ClientDeadlines::default();
        assert_eq!(
            request_deadline("/v1/status", deadlines),
            Duration::from_secs(15)
        );
        assert_eq!(
            request_deadline("/v1/events?wait=true&timeout_ms=30000", deadlines),
            Duration::from_secs(35)
        );
        assert_eq!(
            request_deadline("/v1/internal/replication/export", deadlines),
            Duration::from_secs(120)
        );
    }

    #[tokio::test]
    async fn a_stalled_unix_response_fails_with_a_retryable_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let error = fast_client(Endpoint::Unix(socket))
            .get::<Value>("/v1/status")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("request and response"));
        assert!(error.contains("retry the command"));
        server.abort();
    }

    #[tokio::test]
    async fn a_short_unix_response_reports_incomplete_body_before_json_decode() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"api_version\":\"st3.v1\"",
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let error = Client::unix(&socket)
            .get::<Value>("/v1/messages/page")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("incomplete HTTP response"), "{error}");
        assert!(error.contains("expected 100 body bytes"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_stalled_http_response_fails_with_a_retryable_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let error = fast_client(Endpoint::Http(format!("http://{address}")))
            .get::<Value>("/v1/status")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("request"));
        assert!(error.contains("retry the command"));
        server.abort();
    }

    #[tokio::test]
    async fn a_first_page_get_retries_snapshot_churn_without_reusing_a_cursor() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let attempts = Arc::new(AtomicUsize::new(0));
        let route_attempts = attempts.clone();
        let app = Router::new().route(
            "/v1/client/test",
            get(move || {
                let route_attempts = route_attempts.clone();
                async move {
                    if route_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            axum::http::StatusCode::GONE,
                            Json(json!({
                                "api_version": "st3.client.v0",
                                "error_version": "st3.client.error.v0",
                                "request_id": "request/expired",
                                "code": "page-cursor-expired",
                                "message": "the snapshot changed; restart pagination from the first page",
                                "retryable": true,
                                "details": {}
                            })),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            Json(test_client_envelope(json!({"items": []}))),
                        )
                    }
                }
            }),
        );
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let value: Value = Client::unix(&socket).get("/v1/client/test").await.unwrap();
        assert_eq!(value, json!({"items": []}));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[test]
    fn an_explicit_page_cursor_is_never_treated_as_a_first_page() {
        assert!(!request_has_page_cursor("/v1/client/work"));
        assert!(!request_has_page_cursor("/v1/client/work?limit=50"));
        assert!(request_has_page_cursor(
            "/v1/client/work?limit=50&cursor=page%2Fnext"
        ));
    }

    async fn assert_request_transports(client: Client) {
        let get: Value = client.get("/v1/test").await.unwrap();
        assert_eq!(get, json!({"method": "get"}));
        let post: Value = client
            .post("/v1/test", &json!({"method": "post"}))
            .await
            .unwrap();
        assert_eq!(post, json!({"method": "post"}));
        let client_value: Value = client.get("/v1/client-test").await.unwrap();
        assert_eq!(client_value, json!({"method": "client"}));
    }

    #[tokio::test]
    async fn the_unix_transport_completes_real_get_and_post_requests() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, test_api())
                .await
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(socket.exists(), "the Unix API socket did not start");
        assert_request_transports(Client::unix(&socket)).await;
        server.abort();
    }

    #[tokio::test]
    async fn only_unix_as_carries_explicit_person_authority() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, test_api())
                .await
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let ordinary: Value = Client::unix(&socket).get("/v1/person").await.unwrap();
        assert!(ordinary["person"].is_null());
        let named: Value = Client::unix_as(&socket, "person/nathan")
            .unwrap()
            .get("/v1/person")
            .await
            .unwrap();
        assert_eq!(named["person"], "person/nathan");
        assert!(Client::unix_as(&socket, "person/nathan\r\nx-forged: yes").is_err());
        server.abort();
    }

    #[tokio::test]
    async fn the_http_transport_completes_real_get_and_post_requests() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, test_api()).await.unwrap();
        });
        assert_request_transports(Client::new(Endpoint::Http(format!("http://{address}")))).await;
        server.abort();
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn the_unix_terminal_transport_completes_a_real_websocket_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let server_observed = observed.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &Request, mut response: Response| {
                    *server_observed.lock().unwrap() = Some((
                        request.uri().path().to_owned(),
                        request
                            .headers()
                            .get("Sec-WebSocket-Protocol")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned),
                    ));
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", "st3.terminal.v1".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            websocket.close(None).await.unwrap();
        });

        Client::unix(&socket)
            .proxy_terminal("demo", "/v1/pty/agent%2Fdemo/attach")
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            Some((
                "/v1/pty/agent%2Fdemo/attach".into(),
                Some("st3.terminal.v1".into())
            ))
        );
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn a_terminal_attachment_reconnects_across_a_temporary_gateway_restart() {
        use std::fs::File;
        use std::io::Read as _;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let attach_attempts = Arc::new(AtomicUsize::new(0));
        let stream_attempts = Arc::new(AtomicUsize::new(0));
        let route_attach_attempts = attach_attempts.clone();
        let route_stream_attempts = stream_attempts.clone();
        let app = Router::new()
            .route(
                "/v1/sessions/attach/{*subject}",
                post(move || {
                    let attempt = route_attach_attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt == 0 {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({
                                    "code": "gateway-restarting",
                                    "message": "the st3 gateway is restarting",
                                    "details": {}
                                })),
                            )
                                .into_response();
                        }
                        (
                            StatusCode::OK,
                            Json(test_envelope(json!({
                                "subject": "agent/test",
                                "runtime_id": "demo",
                                "incarnation_id": "incarnation/one",
                                "capability": "fresh-capability",
                                "websocket_path": "/terminal",
                                "expires_at_unix_ms": 9999999999999_u64
                            }))),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/terminal",
                get(move |websocket: WebSocketUpgrade| {
                    let attempt = route_stream_attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        websocket.protocols(["st3.terminal.v1"]).on_upgrade(
                            move |mut socket| async move {
                                if attempt == 0 {
                                    let _ = socket.close().await;
                                    return;
                                }
                                let _ = socket
                                    .send(AxumWsMessage::Binary(
                                        pty_core::protocol::encode_screen(b"resumed after restart")
                                            .into(),
                                    ))
                                    .await;
                                let _ = socket
                                    .send(AxumWsMessage::Binary(
                                        pty_core::protocol::encode_exit(0).into(),
                                    ))
                                    .await;
                                // Keep the successful route alive until the attach client consumes
                                // the terminal exit and closes its bridge. Dropping the server side
                                // immediately races the exit frame and can induce a valid third
                                // reconnect before the fixture's result is observed.
                                let _ = socket.recv().await;
                            },
                        )
                    }
                }),
            );
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let attachment = Attachment {
            subject: "agent/test".into(),
            runtime_id: "demo".into(),
            incarnation_id: Some("incarnation/one".into()),
            capability: "initial-capability".into(),
            websocket_path: "/terminal".into(),
            expires_at_unix_ms: u128::MAX,
        };
        let io = ClientIo {
            stdin: slave.as_raw_fd(),
            stdout: slave.as_raw_fd(),
            stderr: slave.as_raw_fd(),
        };

        let exit = Client::unix(&socket)
            .proxy_terminal_resilient_with_io("agent/test", &attachment, io)
            .await
            .unwrap();
        assert_eq!(exit, 0);
        // Under parallel test load, opening the first resumed stream can itself fail
        // transiently. The bridge must retry with another one-use capability rather
        // than promising an exact number of HTTP attaches.
        assert!((2..=4).contains(&attach_attempts.load(Ordering::SeqCst)));
        assert!((2..=4).contains(&stream_attempts.load(Ordering::SeqCst)));

        drop(slave);
        let mut output = Vec::new();
        if let Err(error) = master.read_to_end(&mut output) {
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
        }
        assert!(
            output
                .windows(b"resumed after restart".len())
                .any(|window| window == b"resumed after restart"),
            "the resumed terminal screen was not rendered: {output:?}"
        );
        assert!(
            output
                .windows(b"[reconnecting".len())
                .any(|window| window == b"[reconnecting"),
            "the reconnect state was not rendered: {output:?}"
        );
        server.abort();
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn terminal_socket_eof_restores_and_sanitizes_the_callers_tty() {
        use std::fs::File;
        use std::io::Read as _;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                |_request: &Request, mut response: Response| {
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", "st3.terminal.v1".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            websocket.close(None).await.unwrap();
        });

        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut original) },
            0
        );

        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let request = terminal_request("ws://localhost/v1/terminal-eof").unwrap();
        let (websocket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let io = ClientIo {
            stdin: slave.as_raw_fd(),
            stdout: slave.as_raw_fd(),
            stderr: slave.as_raw_fd(),
        };
        proxy_websocket_with_io("demo", websocket, io)
            .await
            .unwrap();

        let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut restored) },
            0
        );
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cflag, original.c_cflag);
        assert_eq!(restored.c_lflag, original.c_lflag);

        drop(slave);
        let mut output = Vec::new();
        if let Err(error) = master.read_to_end(&mut output) {
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
        }
        assert!(
            output
                .windows(TERMINAL_SANITIZE.len())
                .any(|window| window == TERMINAL_SANITIZE.as_bytes()),
            "terminal sanitizer missing from EOF output: {output:?}"
        );
        let final_sanitize = output
            .windows(TERMINAL_SANITIZE.len())
            .rposition(|window| window == TERMINAL_SANITIZE.as_bytes())
            .expect("the final terminal sanitizer is present");
        let after_sanitize = &output[final_sanitize + TERMINAL_SANITIZE.len()..];
        assert!(
            after_sanitize.starts_with(CURSOR_TO_BOTTOM.as_bytes())
                && after_sanitize.ends_with(b"\n"),
            "terminal cleanup did not return the prompt below stale content: {output:?}"
        );
        server.await.unwrap();
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn the_http_terminal_transport_completes_a_real_websocket_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(None));
        let server_observed = observed.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &Request, mut response: Response| {
                    *server_observed.lock().unwrap() = Some((
                        request.uri().path().to_owned(),
                        request
                            .headers()
                            .get("Sec-WebSocket-Protocol")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned),
                    ));
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", "st3.terminal.v1".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            websocket.close(None).await.unwrap();
        });

        Client::new(Endpoint::Http(format!("http://{address}")))
            .proxy_terminal("demo", "/v1/pty/agent%2Fdemo/attach")
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            Some((
                "/v1/pty/agent%2Fdemo/attach".into(),
                Some("st3.terminal.v1".into())
            ))
        );
    }
}
