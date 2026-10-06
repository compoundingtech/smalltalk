use std::fmt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use pty_client::tty::{FdWriter, is_tty};
use pty_client::{
    AttachParams, CURSOR_TO_BOTTOM, ClientIo, Reconnect, RouteRefusedError, TERMINAL_SANITIZE,
    attach,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

#[cfg(test)]
use crate::model::ApiResponse;
use crate::model::{
    ApiErrorResponse, AttachRequest, Attachment, LocalTerminal, MessageSendReceipt,
    MessageSendRequest,
};

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
    outage_wait: Duration,
    announce_outage_wait: bool,
}

/// Where a request stood when the daemon went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutagePhase {
    /// The daemon accepted no connection, so nothing reached it.
    Connect,
    /// The daemon closed the connection before it answered, so the request may have applied.
    Response,
}

/// The st daemon could not be reached, typically because it is restarting.
///
/// Every caller sees this one error for an outage, so the CLI, the native drivers, and the
/// channels can say the same plain thing and decide how to wait. It never tells anyone to start
/// the daemon: agents cannot do that, and during a deploy the service manager already is.
#[derive(Clone, Debug)]
pub struct DaemonUnreachable {
    endpoint: String,
    reason: String,
    phase: OutagePhase,
    waited: Option<Duration>,
}

impl DaemonUnreachable {
    fn connect(endpoint: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            reason: reason.into(),
            phase: OutagePhase::Connect,
            waited: None,
        }
    }

    fn response(endpoint: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            reason: reason.into(),
            phase: OutagePhase::Response,
            waited: None,
        }
    }

    fn connect_io(endpoint: impl Into<String>, error: &std::io::Error) -> Self {
        let reason = match error.kind() {
            std::io::ErrorKind::NotFound => "its socket does not exist".to_owned(),
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_owned(),
            _ => error.to_string(),
        };
        Self::connect(endpoint, reason)
    }

    fn after(mut self, waited: Duration) -> Self {
        self.waited = Some(waited);
        self
    }

    pub fn phase(&self) -> OutagePhase {
        self.phase
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// A short present-tense summary without advice, for callers that say what they do next.
    pub fn summary(&self) -> String {
        match self.phase {
            OutagePhase::Connect => format!(
                "the st daemon at {} is not reachable ({}); it may be restarting",
                self.endpoint, self.reason
            ),
            OutagePhase::Response => format!(
                "the st daemon at {} closed the connection before it answered ({}); it may be restarting, and the request may or may not have been applied",
                self.endpoint, self.reason
            ),
        }
    }
}

impl fmt::Display for DaemonUnreachable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(waited) = self.waited else {
            return formatter.write_str(&self.summary());
        };
        let seconds = waited.as_secs_f64().round() as u64;
        match self.phase {
            OutagePhase::Connect => write!(
                formatter,
                "the st daemon at {} was not reachable for {seconds}s ({}); it may be restarting or stopped. Nothing was sent; run the command again once the daemon is back",
                self.endpoint, self.reason
            ),
            OutagePhase::Response => write!(
                formatter,
                "the st daemon at {} stopped answering for {seconds}s ({}); it may be restarting or stopped. The last request may or may not have been applied; check its result once the daemon is back",
                self.endpoint, self.reason
            ),
        }
    }
}

impl std::error::Error for DaemonUnreachable {}

/// The daemon outage in this error's chain, if any.
pub fn daemon_unreachable(error: &anyhow::Error) -> Option<&DaemonUnreachable> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DaemonUnreachable>())
}

/// A PTY session accepts in its own process, so this only bounds a session that stopped
/// accepting; it never waits for the st daemon.
const LOCAL_TERMINAL_CONNECT: Duration = Duration::from_secs(3);

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
                .expect("the st HTTP client configuration is valid"),
            deadlines,
            person: None,
            outage_wait: Duration::ZERO,
            announce_outage_wait: false,
        }
    }

    /// Keep retrying for up to `wait` while the daemon is unreachable. Only requests that are
    /// safe to repeat are retried: any request that never reached the daemon, and reads.
    /// With `announce`, one line on stderr says what is happening while it waits.
    pub fn with_outage_wait(mut self, wait: Duration, announce: bool) -> Self {
        self.outage_wait = wait;
        self.announce_outage_wait = announce;
        self
    }

    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::new(Endpoint::Unix(path.into()))
    }

    /// The daemon's Unix socket, when this client talks to one.
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        match &self.endpoint {
            Endpoint::Unix(path) => Some(path),
            Endpoint::Http(_) => None,
        }
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

    /// Send a message, retrying one unanswered request with the same idempotency key and body.
    /// The receipt says whether the key had already sent it. If both attempts go unanswered the
    /// error is a [`MessageSendUnconfirmed`]: keep this request for a later retry, and ask
    /// [`Client::sent_message`] whether it landed.
    pub async fn send_message(&self, request: &MessageSendRequest) -> Result<MessageSendReceipt> {
        anyhow::ensure!(
            !request.idempotency_key.trim().is_empty(),
            "a message send needs a nonempty idempotency key"
        );
        for attempt in 0..2 {
            match self
                .post::<_, MessageSendReceipt>("/v1/messages", request)
                .await
            {
                Ok(mut receipt) => {
                    // An older daemon answers with the bare message.
                    if receipt.idempotency_key.is_empty() {
                        receipt.idempotency_key = request.idempotency_key.clone();
                    }
                    return Ok(receipt);
                }
                Err(error) if daemon_did_not_answer(&error) => {
                    if attempt == 0 {
                        continue;
                    }
                    return Err(MessageSendUnconfirmed {
                        idempotency_key: request.idempotency_key.clone(),
                        reason: error.to_string(),
                    }
                    .into());
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the bounded message retry loop always returns")
    }

    /// The message a send's idempotency key landed as, or `None` when nothing was sent with it.
    /// A daemon from before this lookup fails it with a bare 404.
    pub async fn sent_message(&self, idempotency_key: &str) -> Result<Option<MessageSendReceipt>> {
        match self
            .get(&format!(
                "/v1/messages/by-key?key={}",
                urlencoding::encode(idempotency_key)
            ))
            .await
        {
            Ok(receipt) => Ok(Some(receipt)),
            Err(error) if api_error_code(&error) == Some("message-not-sent") => Ok(None),
            Err(error) => Err(error),
        }
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
        let started = tokio::time::Instant::now();
        let mut pause = Duration::from_millis(100);
        let mut announced = false;
        loop {
            let error = match self.request_once(method, path, &bytes).await {
                Ok(response) => return decode_api_response(&response),
                Err(error) => error,
            };
            let Some(outage) = daemon_unreachable(&error) else {
                return Err(error);
            };
            let repeatable = outage.phase() == OutagePhase::Connect || method == "GET";
            let waited = started.elapsed();
            if !repeatable || self.outage_wait.is_zero() {
                return Err(error);
            }
            if waited >= self.outage_wait {
                return Err(outage.clone().after(waited).into());
            }
            if self.announce_outage_wait && !announced {
                eprintln!(
                    "st: {}; retrying for up to {}s",
                    outage.summary(),
                    self.outage_wait.as_secs()
                );
                announced = true;
            }
            tokio::time::sleep(jittered(pause).min(self.outage_wait - waited)).await;
            pause = (pause * 2).min(Duration::from_secs(1));
        }
    }

    async fn request_once(&self, method: &str, path: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        let deadline = request_deadline(path, self.deadlines);
        let response = match &self.endpoint {
            Endpoint::Unix(socket) => {
                unix_request(
                    socket,
                    method,
                    path,
                    bytes,
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
                    "POST" => self.http.post(&url).body(bytes.to_vec()),
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
                    .map_err(|error| http_send_error(base, &endpoint, error))?;
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
        Ok(response)
    }

    /// The running terminal `subject` when this client's daemon runs on this host and owns it,
    /// read without any graph write. `None` leaves the attach to the WebSocket: an HTTP endpoint
    /// can be another host, and a daemon from before direct attachment has no such route.
    pub async fn local_terminal(&self, subject: &str) -> Result<Option<LocalTerminal>> {
        if !matches!(self.endpoint, Endpoint::Unix(_)) {
            return Ok(None);
        }
        match self
            .get(&format!(
                "/v1/sessions/local-terminal/{}",
                urlencoding::encode(subject)
            ))
            .await
        {
            Ok(terminal) => Ok(Some(terminal)),
            // A missing subject fails the WebSocket attach with the same answer.
            Err(error) if http_status(&error) == Some(404) => Ok(None),
            Err(error) => Err(error),
        }
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
        // The attach loop already retries while the gateway restarts. One reconnect must neither
        // wait for the daemon nor print into the attached screen.
        let client = self.clone().with_outage_wait(Duration::ZERO, false);
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

    pub async fn open_mailbox(
        &self,
        fence: &crate::mailbox::Fence,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>> {
        let Endpoint::Unix(path) = &self.endpoint else {
            anyhow::bail!("native mailbox subscriptions require a local Unix socket");
        };
        let stream = tokio::net::UnixStream::connect(path).await?;
        let url = format!(
            "ws://localhost/v1/mailbox?subject={}&incarnation={}&component={}&epoch={}&token={}",
            urlencoding::encode(&fence.subject),
            urlencoding::encode(&fence.incarnation),
            urlencoding::encode(&fence.component),
            fence.epoch,
            urlencoding::encode(&fence.token)
        );
        let (socket, _) = tokio::time::timeout(
            self.deadlines.terminal_handshake,
            tokio_tungstenite::client_async(url, stream),
        )
        .await??;
        Ok(socket)
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
                .map_err(|_| connect_deadline_error(&endpoint, self.deadlines.connect))?
                .map_err(|error| DaemonUnreachable::connect_io(&endpoint, &error))?;
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

/// Attach this terminal straight to a PTY session on this host. Nothing goes through the st
/// daemon, so a busy daemon cannot stall or end the attachment. It never starts or restarts the
/// session: when the session ends, the attachment ends.
pub async fn attach_local_terminal(terminal: &LocalTerminal) -> Result<i32> {
    attach_local_terminal_with_io(terminal, ClientIo::default()).await
}

async fn attach_local_terminal_with_io(terminal: &LocalTerminal, io: ClientIo) -> Result<i32> {
    let stream = open_local_terminal(terminal).await?;
    proxy_stream_with_io(&terminal.runtime_id, stream, None, io).await
}

/// Connect to the terminal's PTY socket and prove it serves the incarnation the graph selected
/// before sending anything: the kernel names the process serving the socket, which must be the
/// incarnation's PTY daemon, and the registry must record the incarnation's start time. A socket
/// path proves nothing alone, since a replacement session binds the same path.
pub(crate) async fn open_local_terminal(terminal: &LocalTerminal) -> Result<StdUnixStream> {
    let (stream, peer) =
        connect_pty_session(&terminal.pty_root, &terminal.runtime_id, &terminal.subject).await?;
    let created_at = pty_core::registry::read_metadata_in(&terminal.pty_root, &terminal.runtime_id)
        .map(|metadata| metadata.created_at)
        .with_context(|| {
            format!(
                "terminal `{}` has no PTY record in {}",
                terminal.subject,
                terminal.pty_root.display()
            )
        })?;
    let incarnation = format!("{peer}:{created_at}");
    anyhow::ensure!(
        incarnation == terminal.incarnation_id,
        "terminal `{}` changed incarnation: st selected `{}`, but its PTY is `{incarnation}`",
        terminal.subject,
        terminal.incarnation_id
    );
    Ok(stream)
}

/// Connect to PTY session `runtime_id` under `pty_root`, returning the stream and the pid the
/// kernel reports for the process serving it. Nothing is sent.
async fn connect_pty_session(
    pty_root: &Path,
    runtime_id: &str,
    subject: &str,
) -> Result<(StdUnixStream, i32)> {
    let socket = pty_root.join(format!("{runtime_id}.sock"));
    let stream = tokio::time::timeout(
        LOCAL_TERMINAL_CONNECT,
        tokio::net::UnixStream::connect(&socket),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "terminal `{subject}` did not accept a connection at {} within {} ms",
            socket.display(),
            LOCAL_TERMINAL_CONNECT.as_millis()
        )
    })?
    .with_context(|| format!("connect to terminal `{subject}` at {}", socket.display()))?
    .into_std()?;
    stream.set_nonblocking(false)?;
    let peer = pty_core::unix_peer::credentials(&stream).with_context(|| {
        format!(
            "identify the process serving terminal `{subject}` at {}",
            socket.display()
        )
    })?;
    Ok((stream, peer.pid))
}

/// Attach this terminal straight to a PTY session that only the PTY registry named, for when the
/// st daemon cannot say which incarnation it selected. The kernel must still name the session's
/// live PTY daemon, as the registry records it, as the process serving the socket, so a session
/// replaced after the registry was read receives nothing. It never starts or restarts anything.
pub async fn attach_unconsulted_terminal(
    pty_root: &Path,
    subject: &str,
    session: &TaggedPtySession,
) -> Result<i32> {
    attach_unconsulted_terminal_with_io(pty_root, subject, session, ClientIo::default()).await
}

async fn attach_unconsulted_terminal_with_io(
    pty_root: &Path,
    subject: &str,
    session: &TaggedPtySession,
    io: ClientIo,
) -> Result<i32> {
    let (stream, peer) = connect_pty_session(pty_root, &session.runtime_id, subject).await?;
    anyhow::ensure!(
        peer == session.pid,
        "PTY session `{}` of `{subject}` changed: its registry names PTY daemon {}, but process {peer} serves it",
        session.runtime_id,
        session.pid
    );
    proxy_stream_with_io(&session.runtime_id, stream, None, io).await
}

/// A running PTY session under a PTY root that is tagged as one st subject's terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaggedPtySession {
    pub runtime_id: String,
    pub created_at: String,
    /// The session's live PTY daemon, as the registry records it.
    pub pid: i32,
}

/// The running PTY sessions under `pty_root` tagged as `subject`'s, newest first, read from the
/// PTY registry alone. Only the daemon knows which incarnation it selected; without it, the
/// newest session is the best guess.
pub fn tagged_pty_sessions(pty_root: &Path, subject: &str) -> Vec<TaggedPtySession> {
    let mut sessions: Vec<_> =
        pty_core::registry::list_sessions_in(pty_root, &pty_core::registry::ListOptions::default())
            .into_iter()
            .filter(pty_core::registry::SessionInfo::is_running)
            .filter_map(|session| {
                let pid = session.pid?;
                let metadata = session.metadata?;
                let tagged = metadata.tags.as_ref()?.get("st3.subject")? == subject;
                tagged.then_some(TaggedPtySession {
                    runtime_id: session.name,
                    created_at: metadata.created_at,
                    pid,
                })
            })
            .collect();
    sessions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    sessions
}

/// Whether an API call failed because the requested subject does not exist on this host.
pub fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiResponseError>()
        .is_some_and(|error| error.status == 404 || error.code == "not-found")
}

/// The HTTP status an API call failed with, including an answer without an error body.
pub fn http_status(error: &anyhow::Error) -> Option<u16> {
    error
        .downcast_ref::<ApiResponseError>()
        .map(|error| error.status)
        .or_else(|| {
            error
                .downcast_ref::<UnexpectedResponse>()
                .map(|error| error.status)
        })
}

/// The code of the API error that an API call failed with, if it failed with one.
pub fn api_error_code(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<ApiResponseError>()
        .map(|error| error.code.as_str())
}

/// The status, code, message, and details of the API error that an API call failed with, if it
/// did.
pub fn api_error_parts(
    error: &anyhow::Error,
) -> Option<(u16, &str, &str, &serde_json::Map<String, serde_json::Value>)> {
    error.downcast_ref::<ApiResponseError>().map(|error| {
        (
            error.status,
            error.code.as_str(),
            error.message.as_str(),
            &error.details,
        )
    })
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
    details: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Display for ApiResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "st API returned {} {}: {}",
            self.status, self.code, self.message
        )
    }
}

impl std::error::Error for ApiResponseError {}

/// A failed answer without an st error body, such as an unknown route on an older daemon.
#[derive(Debug)]
struct UnexpectedResponse {
    status: u16,
    body: String,
}

impl fmt::Display for UnexpectedResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "st API returned {}: {}", self.status, self.body)
    }
}

impl std::error::Error for UnexpectedResponse {}

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

/// Run the interactive attach client over a socket that already speaks the PTY session protocol.
pub async fn attach_socket_with_io(name: &str, stream: StdUnixStream, io: ClientIo) -> Result<i32> {
    proxy_stream_with_io(name, stream, None, io).await
}

pub(crate) async fn proxy_stream_with_io(
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
    .map_err(|_| connect_deadline_error(&endpoint, connect_deadline))?
    .map_err(|error| DaemonUnreachable::connect_io(&endpoint, &error))?;
    let remaining = deadline.saturating_sub(started.elapsed());
    let response = tokio::time::timeout(remaining, async {
        let person_header = person
            .map(|person| format!("X-St3-Person: {person}\r\n"))
            .unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{person_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        // A handler may answer without consuming its body. Keep small bodies in the same
        // write as their headers to avoid a separate body write racing Connection: close.
        request.extend_from_slice(body);
        stream.write_all(&request).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok::<_, std::io::Error>(response)
    })
    .await
    .map_err(|_| deadline_error(&endpoint, "request and response", deadline))?
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof => {
            anyhow::Error::from(DaemonUnreachable::response(&endpoint, error.to_string()))
        }
        _ => anyhow::Error::from(error),
    })?;
    // A daemon that exits between accepting and answering closes the socket without a byte.
    if response.is_empty() {
        return Err(DaemonUnreachable::response(&endpoint, "no response").into());
    }
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("the st API returned an incomplete HTTP response")?;
    let header = std::str::from_utf8(&response[..header_end])?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("the st API returned an invalid HTTP status")?;
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
            "the st API returned an incomplete HTTP response: expected {length} body bytes, received {}",
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
        || path.starts_with("/v1/internal/replication/checkpoint")
        || path.starts_with(crate::peer::CLIENT_READ_FORWARD_PATH)
    {
        // A forwarded client read is bounded by the relay's own per-hop timeouts.
        deadlines.bulk
    } else if path.starts_with("/v1/internal/replication/heal/") {
        // A heal can replay the graph from nothing, 41 seconds on a 2 GB store.
        Duration::from_secs(10 * 60)
    } else if path.starts_with("/v1/checkpoint/plan") || path.starts_with("/v1/checkpoint/status")
    {
        // A dry run copies the store and replays it twice; status reads what is sealed.
        Duration::from_secs(30 * 60)
    } else if path.starts_with("/v1/events?") && path.contains("wait=true") {
        deadlines.event
    } else {
        deadlines.request
    }
}

/// Half to one and a half times `pause`. Every seat and command waiting out one daemon restart
/// retries on the same doubling schedule; without a random share they would all reach the daemon
/// in the same instant it starts to listen.
fn jittered(pause: Duration) -> Duration {
    let mut share = [0_u8; 1];
    if getrandom::fill(&mut share).is_err() {
        return pause;
    }
    pause.mul_f64(0.5 + f64::from(share[0]) / 255.0)
}

/// A connect that times out never delivered the request, so it is an outage, not a slow answer.
fn connect_deadline_error(endpoint: &str, deadline: Duration) -> anyhow::Error {
    DaemonUnreachable::connect(
        endpoint,
        format!(
            "it accepted no connection within {} ms",
            deadline.as_millis()
        ),
    )
    .into()
}

fn http_send_error(base: &str, url: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_connect() {
        let reason = std::error::Error::source(&error)
            .and_then(|source| {
                let mut cause = Some(source);
                while let Some(current) = cause {
                    if let Some(io) = current.downcast_ref::<std::io::Error>() {
                        return Some(DaemonUnreachable::connect_io(base, io).reason);
                    }
                    cause = current.source();
                }
                None
            })
            .unwrap_or_else(|| error.to_string());
        return DaemonUnreachable::connect(base, reason).into();
    }
    anyhow::Error::from(error).context(format!("send the request to the st API at {url}"))
}

fn deadline_error(endpoint: &str, phase: &str, deadline: Duration) -> anyhow::Error {
    DaemonDeadline {
        endpoint: endpoint.to_owned(),
        phase: phase.to_owned(),
        deadline,
    }
    .into()
}

/// The daemon took a request but did not answer it in time.
#[derive(Debug)]
struct DaemonDeadline {
    endpoint: String,
    phase: String,
    deadline: Duration,
}

impl fmt::Display for DaemonDeadline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "st API endpoint `{}` exceeded the {} limit of {} ms; the service may be busy or unavailable, retry the command",
            self.endpoint,
            self.phase,
            self.deadline.as_millis()
        )
    }
}

impl std::error::Error for DaemonDeadline {}

/// A message send st never answered, so it may or may not have been sent. The same request with
/// the same key sends it at most once.
#[derive(Debug)]
pub struct MessageSendUnconfirmed {
    pub idempotency_key: String,
    reason: String,
}

impl MessageSendUnconfirmed {
    /// Why the last attempt went unanswered.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for MessageSendUnconfirmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "st did not answer; the message may have been sent. Retry with the same recipient, sender, body and options, and --idempotency-key {:?}. Delivery is unconfirmed: {}",
            self.idempotency_key, self.reason
        )
    }
}

impl std::error::Error for MessageSendUnconfirmed {}

/// Whether an API call failed because the daemon did not answer: it was unreachable, or it took
/// the request and ran out of time. A refusal is an answer.
pub fn daemon_did_not_answer(error: &anyhow::Error) -> bool {
    daemon_unreachable(error).is_some()
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<DaemonDeadline>().is_some())
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
            details: error.details,
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
                details: value
                    .get("details")
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
            }
            .into();
        }
        return anyhow::anyhow!("st API returned {status}: {message}");
    }
    UnexpectedResponse {
        status,
        body: String::from_utf8_lossy(bytes).trim().to_owned(),
    }
    .into()
}

fn decode_api_response<O: DeserializeOwned>(bytes: &[u8]) -> Result<O> {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(bytes).context("decode the st API response envelope")?;
    let version = envelope
        .get("api_version")
        .and_then(serde_json::Value::as_str)
        .context("the st API response envelope has no api_version")?;
    anyhow::ensure!(
        matches!(version, "st3.v1" | "st3.client.v0"),
        "the st API returned unsupported version {version}"
    );
    let value = envelope
        .as_object_mut()
        .and_then(|envelope| envelope.remove("value"))
        .context("the st API response envelope has no value")?;
    serde_json::from_value(value).context("decode the st API response value")
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
            outage_wait: Duration::ZERO,
            announce_outage_wait: false,
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
    async fn message_retry_after_an_accepted_but_unanswered_send_creates_one_message() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let store = Arc::new(crate::store::Store::open_memory("message-retry").unwrap());
        let state = crate::api::AppState {
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0).0,
            node: "message-retry".into(),
            state_dir: directory.path().into(),
            pty_root: directory.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = crate::api::router(state).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let count = count.clone();
                async move {
                    let response = next.run(request).await;
                    // Acceptance has committed. Lose only the first reply, as under load.
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    response
                }
            },
        ));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let request = MessageSendRequest {
            idempotency_key: "message-retry-accepted-001".into(),
            from: "person/ada".into(),
            to: "agent/example/worker".into(),
            content: "Hello once".into(),
            title: Some("A greeting".into()),
            in_reply_to: None,
            tags: vec!["example".into()],
            attachments: Vec::new(),
        };
        let mut client = fast_client(Endpoint::Unix(socket));
        client.deadlines.request = Duration::from_millis(200);
        let receipt = client.send_message(&request).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // The daemon says the retry found the message the first attempt sent.
        assert!(receipt.already_sent);
        assert_eq!(receipt.idempotency_key, request.idempotency_key);
        let message = receipt.message;
        let stored = store.messages(Some("agent/example/worker"), true).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(message.subject, stored[0].subject);
        assert_eq!(message.created_index, stored[0].created_index);
        // A later command retaining the key receives the same durable result as well.
        let replay = client.send_message(&request).await.unwrap();
        assert!(replay.already_sent);
        assert_eq!(replay.sent_at, receipt.sent_at);
        assert_eq!(replay.message.subject, message.subject);
        assert_eq!(replay.message.created_index, message.created_index);
        let found = client
            .sent_message(&request.idempotency_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.message.subject, message.subject);
        assert_eq!(found.sent_at, receipt.sent_at);
        assert!(client.sent_message("never-sent").await.unwrap().is_none());
        assert_eq!(store.messages(None, true).unwrap().len(), 1);
        // A reused key never silently changes the original message.
        let mut changed = request;
        changed.content = "A different message".into();
        let before = calls.load(Ordering::SeqCst);
        assert!(client.send_message(&changed).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(store.messages(None, true).unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn unanswered_message_retries_are_bounded_and_report_the_reusable_key() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Json(test_envelope(Value::Null))
                }
            }),
        );
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let error = fast_client(Endpoint::Unix(socket))
            .send_message(&MessageSendRequest {
                idempotency_key: "retry-this-key".into(),
                from: "person/ada".into(),
                to: "agent/example/worker".into(),
                content: "Hello".into(),
                title: None,
                in_reply_to: None,
                tags: vec![],
                attachments: Vec::new(),
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(error.contains("may have been sent"), "{error}");
        assert!(
            error.contains("--idempotency-key \"retry-this-key\""),
            "{error}"
        );
        assert!(error.contains("unconfirmed"), "{error}");
        server.abort();
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

    fn assert_plain_outage(error: &anyhow::Error, phase: OutagePhase) {
        let outage = daemon_unreachable(error).expect("the error is a daemon outage");
        assert_eq!(outage.phase(), phase);
        let message = format!("{error:#}");
        assert!(message.contains("st daemon"), "{message}");
        assert!(message.contains("restarting"), "{message}");
        assert!(!message.contains("st up"), "{message}");
    }

    #[tokio::test]
    async fn an_absent_or_refusing_daemon_is_one_plain_outage() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let absent = Client::unix(&socket)
            .get::<Value>("/v1/status")
            .await
            .unwrap_err();
        assert_plain_outage(&absent, OutagePhase::Connect);
        assert!(format!("{absent:#}").contains("socket does not exist"));

        // A daemon that exited leaves its socket file behind until the next one binds it.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let refused = Client::unix(&socket)
            .post::<_, Value>("/v1/claims", &json!({}))
            .await
            .unwrap_err();
        assert_plain_outage(&refused, OutagePhase::Connect);
        assert!(format!("{refused:#}").contains("connection refused"));

        // Hold the port without listening on it: a connection is refused, and no test running
        // at the same time can bind the port and answer in the meantime.
        let reserved = tokio::net::TcpSocket::new_v4().unwrap();
        reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = reserved.local_addr().unwrap();
        let http = fast_client(Endpoint::Http(format!("http://{address}")))
            .get::<Value>("/v1/status")
            .await
            .unwrap_err();
        assert_plain_outage(&http, OutagePhase::Connect);
        drop(reserved);
    }

    #[tokio::test]
    async fn a_request_waits_out_a_daemon_restart() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = std::fs::remove_file(&server_socket);
            crate::api::serve_unix(&server_socket, test_api())
                .await
                .unwrap();
        });
        let client = Client::unix(&socket).with_outage_wait(Duration::from_secs(10), false);
        let post: Value = client
            .post("/v1/test", &json!({"method": "post"}))
            .await
            .unwrap();
        assert_eq!(post, json!({"method": "post"}));
        server.abort();
    }

    /// Clients waiting out one restart retry at different times, each within half to one and a
    /// half of its pause.
    #[test]
    fn restart_retries_are_spread_around_their_pause() {
        let pause = Duration::from_millis(400);
        let pauses = (0..64).map(|_| jittered(pause)).collect::<Vec<_>>();
        assert!(pauses.iter().all(|jittered| {
            (Duration::from_millis(200)..=Duration::from_millis(600)).contains(jittered)
        }));
        assert!(
            pauses
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                > 8
        );
    }

    #[tokio::test]
    async fn an_outage_wait_ends_by_saying_how_long_it_waited_and_that_nothing_was_sent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let started = tokio::time::Instant::now();
        let error = Client::unix(&socket)
            .with_outage_wait(Duration::from_millis(300), false)
            .post::<_, Value>("/v1/claims", &json!({}))
            .await
            .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_plain_outage(&error, OutagePhase::Connect);
        let message = format!("{error:#}");
        assert!(message.contains("was not reachable for 0s"), "{message}");
        assert!(message.contains("Nothing was sent"), "{message}");
    }

    #[tokio::test]
    async fn a_request_the_daemon_dropped_is_repeated_only_when_it_is_a_read() {
        use tokio::io::AsyncReadExt as _;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let server_accepted = accepted.clone();
        // Every connection is read and then closed without an answer, as by a daemon that exits.
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                server_accepted.fetch_add(1, Ordering::SeqCst);
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await;
            }
        });
        let client = Client::unix(&socket).with_outage_wait(Duration::from_millis(500), false);
        let post = client
            .post::<_, Value>("/v1/claims", &json!({}))
            .await
            .unwrap_err();
        assert_plain_outage(&post, OutagePhase::Response);
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "a POST was repeated");

        let get = client.get::<Value>("/v1/status").await.unwrap_err();
        assert_plain_outage(&get, OutagePhase::Response);
        assert!(format!("{get:#}").contains("may or may not have been applied"));
        assert!(
            accepted.load(Ordering::SeqCst) > 2,
            "a GET was not repeated"
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_unix_posts_to_handlers_that_ignore_the_body_are_answered_once() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("st3.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = Router::new().route(
            "/v1/test",
            get(|| async { Json(test_envelope(json!({}))) }).post(move || {
                let calls = handler_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(test_envelope(json!({"answered": true})))
                }
            }),
        );
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        let client = Client::unix(&socket).with_outage_wait(Duration::ZERO, false);
        for _ in 0..100 {
            if client.get::<Value>("/v1/test").await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Legacy preview handlers intentionally ignore their small JSON request body.
        // They may answer and close as soon as Hyper parses the request headers.
        for request in 0..10_000 {
            let answer: Value = client
                .post("/v1/test", &json!({"request": request}))
                .await
                .unwrap_or_else(|error| panic!("request {request}: {error:#}"));
            assert_eq!(answer, json!({"answered": true}));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 10_000);
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
        let named: Value = Client::unix_as(&socket, "person/alex")
            .unwrap()
            .get("/v1/person")
            .await
            .unwrap();
        assert_eq!(named["person"], "person/alex");
        assert!(Client::unix_as(&socket, "person/alex\r\nx-forged: yes").is_err());
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
                                    "message": "the st gateway is restarting",
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

        // Drain the PTY while the client runs. On macOS, restoring termios can wait for
        // pending output to drain; reading only after the client returns deadlocks it.
        let output_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            if let Err(error) = master.read_to_end(&mut output) {
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
            }
            output
        });
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
        let output = output_reader.join().unwrap();
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
        // macOS can discard unread PTY output when the final slave descriptor closes.
        let output_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            if let Err(error) = master.read_to_end(&mut output) {
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
            }
            output
        });
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
        // macOS may set PENDIN while processing the queued PTY input. It is a kernel-owned
        // pending-input bit, not a terminal mode changed by the attachment.
        #[cfg(target_os = "macos")]
        assert_eq!(
            restored.c_lflag & !libc::PENDIN,
            original.c_lflag & !libc::PENDIN
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(restored.c_lflag, original.c_lflag);

        drop(slave);
        let output = output_reader.join().unwrap();
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

    /// A stand-in PTY session at `ROOT/RUNTIME.sock` with its registry record. It answers an
    /// ATTACH with a screen and an exit the way a `pty` daemon does, and returns every byte its
    /// one client sent.
    fn pty_session(
        root: &Path,
        runtime_id: &str,
        created_at: &str,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        use std::io::{Read as _, Write as _};
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(format!("{runtime_id}.json")),
            json!({ "createdAt": created_at }).to_string(),
        )
        .unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(root.join(format!("{runtime_id}.sock")))
                .unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut reader = pty_core::protocol::PacketReader::new();
            let mut bytes = [0_u8; 4096];
            loop {
                let count = stream.read(&mut bytes).unwrap_or(0);
                if count == 0 {
                    return received;
                }
                received.extend_from_slice(&bytes[..count]);
                let packets = reader.feed(&bytes[..count]).unwrap();
                if packets
                    .iter()
                    .any(|packet| packet.type_ == pty_core::protocol::MessageType::Attach)
                {
                    stream
                        .write_all(&pty_core::protocol::encode_screen(b"straight from the pty"))
                        .unwrap();
                    stream
                        .write_all(&pty_core::protocol::encode_exit(0))
                        .unwrap();
                }
            }
        })
    }

    /// Client descriptors without a terminal: no input, and output kept in a file. The files
    /// must outlive the attach.
    fn silent_io() -> (ClientIo, std::fs::File, std::fs::File) {
        use std::os::fd::AsRawFd as _;
        let input = std::fs::File::open("/dev/null").unwrap();
        let output = tempfile::tempfile().unwrap();
        let io = ClientIo {
            stdin: input.as_raw_fd(),
            stdout: output.as_raw_fd(),
            stderr: output.as_raw_fd(),
        };
        (io, input, output)
    }

    #[tokio::test]
    async fn a_local_terminal_attaches_to_its_pty_session_without_the_daemon() {
        use std::io::{Read as _, Seek as _};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("pty");
        let session = pty_session(&root, "worker", "2026-09-29T08:00:00.000Z");
        let terminal = LocalTerminal {
            subject: "agent/worker".into(),
            runtime_id: "worker".into(),
            // The stand-in session is served by this process.
            incarnation_id: format!("{}:2026-09-29T08:00:00.000Z", std::process::id()),
            pty_root: root,
        };
        let (io, _input, mut output) = silent_io();

        let exit = attach_local_terminal_with_io(&terminal, io).await.unwrap();

        assert_eq!(exit, 0);
        let received = session.join().unwrap();
        let packets = pty_core::protocol::PacketReader::new()
            .feed(&received)
            .unwrap();
        assert_eq!(
            packets.first().map(|packet| packet.type_),
            Some(pty_core::protocol::MessageType::Attach),
            "the client must open with ATTACH: {received:?}"
        );
        let mut shown = String::new();
        output.rewind().unwrap();
        output.read_to_string(&mut shown).unwrap();
        assert!(shown.contains("straight from the pty"), "{shown:?}");
    }

    #[tokio::test]
    async fn a_local_attach_sends_nothing_to_a_pty_session_of_another_incarnation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("pty");
        // The session was replaced after st observed it: same path, new start.
        let session = pty_session(&root, "worker", "2026-09-29T09:30:00.000Z");
        let terminal = LocalTerminal {
            subject: "agent/worker".into(),
            runtime_id: "worker".into(),
            incarnation_id: format!("{}:2026-09-29T08:00:00.000Z", std::process::id()),
            pty_root: root,
        };
        let (io, _input, _output) = silent_io();

        let error = attach_local_terminal_with_io(&terminal, io)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("changed incarnation"), "{error}");
        assert!(
            session.join().unwrap().is_empty(),
            "a fenced-out session must receive nothing"
        );
    }

    #[tokio::test]
    async fn an_attach_without_st_reaches_the_pty_session_its_registry_names() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("pty");
        let session = pty_session(&root, "worker", "2026-09-29T08:00:00.000Z");
        let tagged = TaggedPtySession {
            runtime_id: "worker".into(),
            created_at: "2026-09-29T08:00:00.000Z".into(),
            // The stand-in session is served by this process.
            pid: std::process::id() as i32,
        };
        let (io, _input, _output) = silent_io();

        let exit = attach_unconsulted_terminal_with_io(&root, "agent/worker", &tagged, io)
            .await
            .unwrap();

        assert_eq!(exit, 0);
        assert!(!session.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_attach_without_st_sends_nothing_to_a_socket_another_process_serves() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("pty");
        let session = pty_session(&root, "worker", "2026-09-29T08:00:00.000Z");
        // The registry named another PTY daemon; a replacement now serves the same path.
        let tagged = TaggedPtySession {
            runtime_id: "worker".into(),
            created_at: "2026-09-29T08:00:00.000Z".into(),
            pid: std::process::id() as i32 + 1,
        };
        let (io, _input, _output) = silent_io();

        let error = attach_unconsulted_terminal_with_io(&root, "agent/worker", &tagged, io)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("changed"), "{error}");
        assert!(
            session.join().unwrap().is_empty(),
            "a session the registry did not name must receive nothing"
        );
    }

    #[tokio::test]
    async fn only_a_local_daemon_that_knows_the_route_names_a_local_terminal() {
        let remote = Client::new(Endpoint::Http("http://127.0.0.1:9".into()));
        assert!(
            remote
                .local_terminal("agent/worker")
                .await
                .unwrap()
                .is_none(),
            "an HTTP endpoint can be another host, so its terminals use the WebSocket"
        );

        let directory = tempfile::tempdir().unwrap();
        let older = directory.path().join("older.sock");
        let server_socket = older.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, Router::new())
                .await
                .unwrap();
        });
        for _ in 0..100 {
            if older.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            Client::unix(&older)
                .local_terminal("agent/worker")
                .await
                .unwrap()
                .is_none(),
            "a daemon without the route leaves the attach to the WebSocket"
        );
        server.abort();

        let elsewhere = directory.path().join("elsewhere.sock");
        let server_socket = elsewhere.clone();
        let app = Router::new().route(
            "/v1/sessions/local-terminal/{*subject}",
            get(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": "runtime-not-local",
                        "message": "subject `agent/worker` is owned by `host/other`",
                        "details": {}
                    })),
                )
            }),
        );
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        for _ in 0..100 {
            if elsewhere.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let error = Client::unix(&elsewhere)
            .local_terminal("agent/worker")
            .await
            .unwrap_err();
        assert_eq!(api_error_code(&error), Some("runtime-not-local"));
        server.abort();
    }
}
