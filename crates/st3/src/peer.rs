use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hmac::{Hmac, Mac as _};
use notify::Watcher as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::client::Client;
use crate::config::{Config, PeerConfig};
use crate::fleet::{Acceptance, FleetView, MemberKey, Refusal, Sender, verify_signature};
use crate::model::{
    ApiResponse, ReplicaEnvelopeId, ReplicationExchange, ReplicationExportRequest,
    ReplicationExportResponse, ReplicationInventory, ReplicationPeerFailureRequest,
    ReplicationReceiveRequest, ReplicationReceiveResponse,
};
#[cfg(test)]
use crate::store::Store;

const PROTOCOL: &str = "st3-replication-v1";
const EXCHANGE_PATH: &str = "/v1/peer/exchange";
const REPLICATION_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_WAKE_COALESCE: Duration = Duration::from_secs(1);
const CLIENT_READ_PATH: &str = "/v1/peer/client-read";
const MAX_CLIENT_READ_BYTES: usize = 1_048_576;
const HEADER_FLEET: &str = "x-st3-fleet";
const HEADER_NODE: &str = "x-st3-node";
const HEADER_BODY: &str = "x-st3-body-sha256";
const HEADER_SIGNATURE: &str = "x-st3-signature";
const HEADER_REQUEST: &str = "x-st3-request-digest";
const HEADER_MEMBER_KEY: &str = "x-st3-member-key";
const HEADER_MEMBER_SIGNATURE: &str = "x-st3-member-signature";
const MEMBER_SIGNATURE_DOMAIN: &str = "st3-member-v1";
pub(crate) const MAX_EXCHANGE_BYTES: usize = 64 * 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct FleetAuth {
    fleet_id: String,
    secret: Arc<Vec<u8>>,
    /// This member's key. Set, every request and response also carries a member signature.
    member: Option<Arc<MemberKey>>,
}

impl FleetAuth {
    pub fn load(fleet_id: &str, path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("inspect the fleet secret {}", path.display()))?;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "the fleet secret must not grant group or other permissions"
        );
        let bytes =
            fs::read(path).with_context(|| format!("read the fleet secret {}", path.display()))?;
        let hexadecimal = std::str::from_utf8(&bytes)
            .ok()
            .map(str::trim)
            .filter(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        let secret = match hexadecimal {
            Some(value) => hex::decode(value).context("decode the hexadecimal fleet secret")?,
            None => bytes,
        };
        anyhow::ensure!(secret.len() == 32, "the fleet secret must contain 32 bytes");
        Ok(Self {
            fleet_id: fleet_id.into(),
            secret: Arc::new(secret),
            member: None,
        })
    }

    /// Sign every request and response with this member key as well.
    pub fn with_member_key(mut self, member: Option<Arc<MemberKey>>) -> Self {
        self.member = member;
        self
    }

    pub fn member_key(&self) -> Option<&str> {
        self.member.as_deref().map(MemberKey::public)
    }

    #[cfg(test)]
    fn test(fleet_id: &str, secret: &[u8]) -> Self {
        Self {
            fleet_id: fleet_id.into(),
            secret: Arc::new(secret.to_vec()),
            member: None,
        }
    }

    fn canonical(
        &self,
        method: &str,
        path: &str,
        node: &str,
        body_digest: &str,
        request_digest: Option<&str>,
    ) -> String {
        format!(
            "{PROTOCOL}\n{method}\n{path}\n{}\n{node}\n{body_digest}\n{}",
            self.fleet_id,
            request_digest.unwrap_or_default()
        )
    }

    fn member_message(canonical: &str) -> Vec<u8> {
        format!("{MEMBER_SIGNATURE_DOMAIN}\n{canonical}").into_bytes()
    }

    fn add_member_signature(&self, headers: &mut HeaderMap, canonical: &str) -> Result<()> {
        if let Some(member) = &self.member {
            headers.insert(HEADER_MEMBER_KEY, HeaderValue::from_str(member.public())?);
            headers.insert(
                HEADER_MEMBER_SIGNATURE,
                HeaderValue::from_str(&member.sign(&Self::member_message(canonical)))?,
            );
        }
        Ok(())
    }

    pub fn fleet_id(&self) -> &str {
        &self.fleet_id
    }

    fn body_digest(body: &[u8]) -> String {
        hex::encode(Sha256::digest(body))
    }

    fn signature(
        &self,
        method: &str,
        path: &str,
        node: &str,
        body_digest: &str,
        request_digest: Option<&str>,
    ) -> String {
        let canonical = self.canonical(method, path, node, body_digest, request_digest);
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a secret of any length");
        mac.update(canonical.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn request_headers(&self, node: &str, body: &[u8]) -> Result<HeaderMap> {
        self.request_headers_for(EXCHANGE_PATH, node, body)
    }

    fn request_headers_for(&self, path: &str, node: &str, body: &[u8]) -> Result<HeaderMap> {
        let digest = Self::body_digest(body);
        let signature = self.signature("POST", path, node, &digest, None);
        let mut headers = headers(&self.fleet_id, node, &digest, &signature, None)?;
        self.add_member_signature(
            &mut headers,
            &self.canonical("POST", path, node, &digest, None),
        )?;
        Ok(headers)
    }

    fn response_headers_for(
        &self,
        path: &str,
        node: &str,
        body: &[u8],
        request_digest: &str,
    ) -> Result<HeaderMap> {
        let digest = Self::body_digest(body);
        let signature = self.signature("RESPONSE", path, node, &digest, Some(request_digest));
        let mut headers = headers(
            &self.fleet_id,
            node,
            &digest,
            &signature,
            Some(request_digest),
        )?;
        self.add_member_signature(
            &mut headers,
            &self.canonical("RESPONSE", path, node, &digest, Some(request_digest)),
        )?;
        Ok(headers)
    }

    fn verify(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        expected_node: Option<&str>,
        request_digest: Option<&str>,
    ) -> Result<String> {
        self.verify_sender(headers, method, path, body, expected_node, request_digest)
            .map(|sender| sender.name)
    }

    /// Check the fleet HMAC, then read the optional member key and check its signature.
    fn verify_sender(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        expected_node: Option<&str>,
        request_digest: Option<&str>,
    ) -> Result<Sender> {
        let field = |name: &str| -> Result<&str> {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .with_context(|| format!("the signed message has no valid {name} header"))
        };
        anyhow::ensure!(
            field(HEADER_FLEET)? == self.fleet_id,
            "the fleet ID does not match"
        );
        let node = field(HEADER_NODE)?;
        if let Some(expected) = expected_node {
            anyhow::ensure!(
                node == expected,
                "the signed node does not match the configured peer"
            );
        }
        let digest = Self::body_digest(body);
        anyhow::ensure!(
            field(HEADER_BODY)? == digest,
            "the signed body digest does not match"
        );
        if let Some(expected) = request_digest {
            anyhow::ensure!(
                field(HEADER_REQUEST)? == expected,
                "the response does not bind to this request"
            );
        }
        let signature = hex::decode(field(HEADER_SIGNATURE)?)
            .context("the replication signature is not hexadecimal")?;
        let canonical = self.canonical(method, path, node, &digest, request_digest);
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a secret of any length");
        mac.update(canonical.as_bytes());
        mac.verify_slice(&signature)
            .context("the replication signature does not match")?;
        let optional = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let member_key = optional(HEADER_MEMBER_KEY);
        let member_signature_valid = match (&member_key, optional(HEADER_MEMBER_SIGNATURE)) {
            (Some(key), Some(signature)) => {
                verify_signature(key, &Self::member_message(&canonical), &signature)
            }
            _ => false,
        };
        Ok(Sender {
            name: node.into(),
            member_key,
            member_signature_valid,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ClientReadOperation {
    Timeline {
        session_id: String,
        limit: usize,
        cursor: Option<String>,
    },
    TerminalScreen {
        terminal_id: String,
    },
    TerminalControl {
        action_id: String,
        idempotency_key: String,
        action_type: String,
        terminal_id: String,
        runtime_incarnation: String,
        expected_sequence: u64,
        parameters: serde_json::Value,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReadRequest {
    pub authority_actor: String,
    pub request: ClientReadOperation,
}

#[derive(Debug)]
pub struct ClientReadRejected {
    pub code: String,
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ClientReadRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "owner rejected client read ({}): {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for ClientReadRejected {}

/// A paired gateway uses this for bounded owner-local client operations. The peer worker
/// authenticates both ends and the owner daemon rechecks the requested resource and fences.
#[derive(Clone)]
pub struct ClientRelay {
    node: String,
    peers: Vec<PeerConfig>,
    auth: FleetAuth,
    http: reqwest::Client,
}

impl ClientRelay {
    pub fn has_peer(&self, host_id: &str) -> bool {
        host_id
            .strip_prefix("host/")
            .is_some_and(|name| self.peers.iter().any(|peer| peer.name == name))
    }

    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        let (Some(fleet), Some(secret)) = (
            config.fleet_id.as_deref(),
            config.shared_secret_file.as_deref(),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            node: config.node.clone(),
            peers: config.peers.clone(),
            auth: FleetAuth::load(fleet, secret)?,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(15))
                .build()?,
        }))
    }

    pub async fn read(
        &self,
        host_id: &str,
        request: &ClientReadRequest,
    ) -> Result<serde_json::Value> {
        let name = host_id
            .strip_prefix("host/")
            .context("the owner host ID is invalid")?;
        let peer = self
            .peers
            .iter()
            .find(|peer| peer.name == name)
            .with_context(|| format!("owner `{host_id}` is not a configured peer"))?;
        let body = serde_json::to_vec(request)?;
        anyhow::ensure!(
            body.len() <= 16_384,
            "the client read request exceeds its bound"
        );
        let digest = FleetAuth::body_digest(&body);
        let headers = self
            .auth
            .request_headers_for(CLIENT_READ_PATH, &self.node, &body)?;
        let mut response = self
            .http
            .post(format!(
                "{}{}",
                peer.url.trim_end_matches('/'),
                CLIENT_READ_PATH
            ))
            .headers(headers)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let response_headers = response.headers().clone();
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_CLIENT_READ_BYTES as u64),
            "the peer client read response exceeds its bound"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_CLIENT_READ_BYTES,
                "the peer client read response exceeds its bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        self.auth.verify(
            &response_headers,
            "RESPONSE",
            CLIENT_READ_PATH,
            &bytes,
            Some(name),
            Some(&digest),
        )?;
        let envelope: ApiResponse<serde_json::Value> = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            envelope.api_version == "st3.v1",
            "the peer client read protocol differs"
        );
        if !status.is_success() {
            return Err(ClientReadRejected {
                code: envelope.value["code"]
                    .as_str()
                    .unwrap_or("remote-unavailable")
                    .into(),
                status: status.as_u16(),
                message: envelope.value["message"]
                    .as_str()
                    .unwrap_or("owner read failed")
                    .into(),
            }
            .into());
        }
        Ok(envelope.value)
    }
}

fn headers(
    fleet: &str,
    node: &str,
    digest: &str,
    signature: &str,
    request_digest: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (HEADER_FLEET, fleet),
        (HEADER_NODE, node),
        (HEADER_BODY, digest),
        (HEADER_SIGNATURE, signature),
    ] {
        headers.insert(name, HeaderValue::from_str(value)?);
    }
    if let Some(request_digest) = request_digest {
        headers.insert(HEADER_REQUEST, HeaderValue::from_str(request_digest)?);
    }
    Ok(headers)
}

#[derive(Clone)]
struct PeerState {
    backend: PeerBackend,
    node: String,
    auth: FleetAuth,
    fleet: FleetContext,
    main_socket: PathBuf,
    outbound_notify: watch::Sender<u64>,
}

/// What this worker knows about the fleet: the membership view, its config peers, and whether
/// it still accepts HMAC-only exchanges from config peers.
#[derive(Clone)]
struct FleetContext {
    view: Arc<std::sync::RwLock<FleetView>>,
    config_peers: BTreeSet<String>,
    /// True on a node without `fleet.toml`, or with `legacy_peers = true`.
    legacy: bool,
    own_key: Option<String>,
    /// Keys this node trusts before membership arrives: its pinned anchor and its sponsor.
    bootstrap_keys: BTreeSet<String>,
    /// Set once a member refused this node with a signed refusal naming its own key.
    removed: Arc<std::sync::atomic::AtomicBool>,
    state_dir: Option<PathBuf>,
}

impl FleetContext {
    /// A node without membership: config peers only, as before.
    #[cfg(test)]
    fn legacy(config_peers: BTreeSet<String>) -> Self {
        Self {
            view: Arc::default(),
            config_peers,
            legacy: true,
            own_key: None,
            bootstrap_keys: BTreeSet::new(),
            removed: Arc::default(),
            state_dir: None,
        }
    }

    fn accept(&self, sender: &Sender) -> Result<Acceptance, Refusal> {
        let view = self.view.read().expect("fleet view lock poisoned");
        // Before membership arrives, a new member knows only its anchor and its sponsor.
        if sender.member_signature_valid
            && view.members.iter().all(|member| member.name != sender.name)
            && sender
                .member_key
                .as_ref()
                .is_some_and(|key| self.bootstrap_keys.contains(key))
        {
            return Ok(Acceptance::Member);
        }
        crate::fleet::accept(
            &view,
            sender,
            self.config_peers.contains(&sender.name),
            self.legacy,
        )
    }

    fn is_removed(&self) -> bool {
        self.removed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record that a member refused this node for good, so it stops dialing and `st3 doctor`
    /// can say what happened.
    fn mark_removed(&self, reported_by: &str, code: &str) {
        self.removed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(state_dir) = &self.state_dir
            && let Ok(Some(mut file)) = crate::config::FleetFile::load(state_dir)
            && file.removed.is_none()
        {
            file.removed = Some(crate::config::FleetRemoval {
                reported_by: reported_by.into(),
                code: code.into(),
            });
            let _ = file.save(state_dir);
        }
    }
}

/// A signed refusal from a member that names this node's own key as ended.
#[derive(Debug)]
struct RemovedFromFleet {
    code: String,
}

impl std::fmt::Display for RemovedFromFleet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "this node is no longer a member of the fleet ({})",
            self.code
        )
    }
}

impl std::error::Error for RemovedFromFleet {}

#[derive(Clone)]
enum PeerBackend {
    Main(Client),
    #[cfg(test)]
    Local(Arc<Store>),
}

impl PeerBackend {
    async fn export(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        match self {
            Self::Main(client) => {
                client
                    .post(
                        "/v1/internal/replication/export",
                        &ReplicationExportRequest {
                            fleet_id: fleet_id.to_owned(),
                            inventory: inventory.clone(),
                            summary_only,
                            signature_requests: signature_requests.to_vec(),
                        },
                    )
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => {
                let exchange = if summary_only {
                    store.export_replication_summary(fleet_id)?
                } else {
                    store.export_replication_exchange_answering(
                        fleet_id,
                        inventory,
                        signature_requests,
                    )?
                };
                Ok(ReplicationExportResponse {
                    exchange,
                    store_index: store.index()?,
                })
            }
        }
    }

    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
    ) -> Result<ReplicationReceiveResponse> {
        match self {
            Self::Main(client) => {
                client
                    .post(
                        "/v1/internal/replication/receive",
                        &ReplicationReceiveRequest {
                            peer: peer.to_owned(),
                            fleet_id: fleet_id.to_owned(),
                            exchange: exchange.clone(),
                        },
                    )
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => {
                let receipt = store
                    .receive_replication_exchange(peer, fleet_id, exchange)
                    .map_err(anyhow::Error::msg)?;
                store.record_transport_observation(peer, "up", None, None)?;
                let admission = store.validate_replication_backlog()?;
                let repairs = store.apply_replication_repairs()?;
                let projected = store.project_replication_backlog()?;
                Ok(ReplicationReceiveResponse {
                    receipt,
                    changed: projected && (admission.changed || repairs != 0),
                    store_index: store.index()?,
                })
            }
        }
    }

    async fn fleet_view(&self) -> Result<FleetView> {
        match self {
            Self::Main(client) => client.get("/v1/internal/fleet/membership").await,
            #[cfg(test)]
            Self::Local(store) => store.fleet_view(),
        }
    }

    async fn record_failure(&self, peer: &str, status: &str, error: &str) -> Result<()> {
        match self {
            Self::Main(client) => {
                let _: serde_json::Value = client
                    .post(
                        "/v1/internal/replication/peer-failure",
                        &ReplicationPeerFailureRequest {
                            peer: peer.to_owned(),
                            status: status.to_owned(),
                            error: error.to_owned(),
                        },
                    )
                    .await?;
                Ok(())
            }
            #[cfg(test)]
            Self::Local(store) => {
                store.record_peer_failure(peer, status, error)?;
                store.record_transport_observation(peer, status, Some(error), None)
            }
        }
    }
}

pub async fn run_worker(config: Config) -> Result<()> {
    config.validate()?;
    let fleet_id = config
        .fleet_id
        .as_deref()
        .context("the replication worker needs fleet_id")?;
    let secret_file = config
        .shared_secret_file
        .as_deref()
        .context("the replication worker needs shared_secret_file")?;
    let member_key = match &config.fleet {
        Some(file) => Some(Arc::new(MemberKey::load(
            &file.node_key_path(&config.state_dir),
        )?)),
        None => None,
    };
    let auth = FleetAuth::load(fleet_id, secret_file)?.with_member_key(member_key.clone());
    let fleet = FleetContext {
        view: Arc::default(),
        config_peers: config.peers.iter().map(|peer| peer.name.clone()).collect(),
        legacy: config.fleet.as_ref().is_none_or(|file| file.legacy_peers),
        own_key: member_key.as_ref().map(|key| key.public().to_owned()),
        bootstrap_keys: config
            .fleet
            .iter()
            .flat_map(|file| [file.anchor_key.clone(), file.sponsor_key.clone()])
            .flatten()
            .collect(),
        removed: Arc::new(std::sync::atomic::AtomicBool::new(
            config
                .fleet
                .as_ref()
                .is_some_and(|file| file.removed.is_some()),
        )),
        state_dir: Some(config.state_dir.clone()),
    };
    wait_for_main_daemon(&config.socket).await;
    let backend = PeerBackend::Main(Client::unix(config.socket.clone()));
    let (notify, _notify_receiver) = watch::channel(0_u64);
    let wake_file = config.state_dir.join("replication.wake");
    if !wake_file.exists() {
        fs::write(&wake_file, b"worker-start\n")?;
    }
    let watcher_notify = notify.clone();
    let mut database_watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                watcher_notify.send_modify(|generation| *generation = generation.saturating_add(1));
            }
        })?;
    database_watcher.watch(&wake_file, notify::RecursiveMode::NonRecursive)?;
    refresh_fleet_view(&backend, &fleet).await;
    tokio::spawn(keep_fleet_view_current(
        backend.clone(),
        fleet.clone(),
        notify.subscribe(),
    ));
    let state = PeerState {
        backend: backend.clone(),
        node: config.node.clone(),
        auth: auth.clone(),
        fleet: fleet.clone(),
        main_socket: config.socket.clone(),
        outbound_notify: notify.clone(),
    };
    start_outbound(
        backend,
        config.node.clone(),
        config.peers,
        auth,
        fleet,
        config.socket,
        notify,
    );
    // A dial-out member accepts no connections.
    let Some(address) = config.peer_listen.as_deref() else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind the replication listener at {address}"))?;
    let app = peer_router(state);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn refresh_fleet_view(backend: &PeerBackend, fleet: &FleetContext) {
    if let Ok(view) = backend.fleet_view().await {
        *fleet.view.write().expect("fleet view lock poisoned") = view;
    }
}

/// Reread membership on every graph change and at least every 30 seconds.
async fn keep_fleet_view_current(
    backend: PeerBackend,
    fleet: FleetContext,
    mut notify: watch::Receiver<u64>,
) {
    loop {
        tokio::select! {
            changed = notify.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        refresh_fleet_view(&backend, &fleet).await;
    }
}

fn peer_router(state: PeerState) -> Router {
    Router::new()
        .route(EXCHANGE_PATH, post(receive_exchange))
        .route(
            CLIENT_READ_PATH,
            post(receive_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .layer(DefaultBodyLimit::max(MAX_EXCHANGE_BYTES))
        .with_state(state)
}

async fn receive_client_read(
    State(state): State<PeerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sender =
        match state
            .auth
            .verify_sender(&headers, "POST", CLIENT_READ_PATH, &body, None, None)
        {
            Ok(sender) if state.fleet.accept(&sender).is_ok() => sender.name,
            _ => return (StatusCode::UNAUTHORIZED, "untrusted fleet client read").into_response(),
        };
    let request_digest = FleetAuth::body_digest(&body);
    let result: Result<serde_json::Value> = async {
        anyhow::ensure!(
            body.len() <= 16_384,
            "the client read request exceeds its bound"
        );
        let request: ClientReadRequest = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            request.authority_actor.starts_with("person/")
                && request.authority_actor.matches('/').count() == 1,
            "a fleet client read needs one concrete person"
        );
        let client = st3_client::Client::unix_as(&state.main_socket, &request.authority_actor);
        match request.request {
            ClientReadOperation::Timeline {
                session_id,
                limit,
                cursor,
            } => {
                anyhow::ensure!((1..=200).contains(&limit), "the timeline limit is invalid");
                let value = client
                    .timeline(&session_id, cursor.as_deref(), Some(limit))
                    .await?
                    .value;
                Ok(serde_json::to_value(value)?)
            }
            ClientReadOperation::TerminalScreen { terminal_id } => {
                let value = client.terminal_screen(&terminal_id).await?.value;
                Ok(serde_json::to_value(value)?)
            }
            ClientReadOperation::TerminalControl {
                action_id,
                idempotency_key,
                action_type,
                terminal_id,
                runtime_incarnation,
                expected_sequence,
                parameters,
            } => {
                anyhow::ensure!(
                    matches!(action_type.as_str(), "terminal.input" | "terminal.resize"),
                    "the terminal control action is invalid"
                );
                let screen = client.terminal_screen(&terminal_id).await?;
                if screen.value.runtime_incarnation != runtime_incarnation
                    || screen.value.next_sequence != expected_sequence
                {
                    return Err(ClientReadRejected {
                        code: "stale-fence".into(),
                        status: StatusCode::CONFLICT.as_u16(),
                        message: "the terminal control fence is stale".into(),
                    }
                    .into());
                }
                let fence = st3_client::Fence {
                    snapshot_id: screen.snapshot.id,
                    subject_revisions: Default::default(),
                    mission_generation: None,
                    step_definition: None,
                    attempt: None,
                    readiness_epoch: None,
                    runtime_incarnation: Some(runtime_incarnation),
                    terminal_sequence: Some(screen.value.next_sequence),
                    preview_token: None,
                };
                let result = match action_type.as_str() {
                    "terminal.input" => {
                        let parameters: st3_client::TerminalInputParameters =
                            serde_json::from_value(parameters)?;
                        anyhow::ensure!(
                            parameters.terminal_id == terminal_id,
                            "the terminal control target changed"
                        );
                        client
                            .terminal_input(action_id, idempotency_key, fence, parameters)
                            .await?
                    }
                    "terminal.resize" => {
                        let parameters: st3_client::TerminalResizeParameters =
                            serde_json::from_value(parameters)?;
                        anyhow::ensure!(
                            parameters.terminal_id == terminal_id,
                            "the terminal control target changed"
                        );
                        client
                            .terminal_resize(action_id, idempotency_key, fence, parameters)
                            .await?
                    }
                    _ => unreachable!(),
                };
                Ok(serde_json::to_value(result.value)?)
            }
        }
    }
    .await;
    match result {
        Ok(value)
            if serde_json::to_vec(&value)
                .is_ok_and(|bytes| bytes.len() <= MAX_CLIENT_READ_BYTES - 1024) =>
        {
            signed_response_for(&state, CLIENT_READ_PATH, &request_digest, 0, value)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(_) => signed_error_response_for(
            &state,
            CLIENT_READ_PATH,
            &request_digest,
            0,
            StatusCode::PAYLOAD_TOO_LARGE,
            "the client read response exceeds its bound",
        )
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(error) => {
            let (status, code, message) =
                if let Some(rejected) = error.downcast_ref::<ClientReadRejected>() {
                    (
                        StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::CONFLICT),
                        rejected.code.clone(),
                        rejected.message.clone(),
                    )
                } else {
                    match error.downcast_ref::<st3_client::ClientError>() {
                        Some(st3_client::ClientError::Api(code, message, _)) => {
                            let status = match code {
                                st3_client::ErrorCode::PageCursorExpired
                                | st3_client::ErrorCode::CursorGap => StatusCode::GONE,
                                st3_client::ErrorCode::NotFound => StatusCode::NOT_FOUND,
                                st3_client::ErrorCode::StaleFence => StatusCode::CONFLICT,
                                _ => StatusCode::UNPROCESSABLE_ENTITY,
                            };
                            (
                                status,
                                serde_json::to_value(code)
                                    .ok()
                                    .and_then(|value| value.as_str().map(str::to_owned))
                                    .unwrap_or_else(|| "remote-unavailable".into()),
                                message.clone(),
                            )
                        }
                        _ => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "remote-unavailable".into(),
                            format!("fleet client read from {sender} failed"),
                        ),
                    }
                };
            signed_client_read_failure(&state, &request_digest, status, &code, &message)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn signed_client_read_failure(
    state: &PeerState,
    request_digest: &str,
    status: StatusCode,
    code: &str,
    message: &str,
) -> Result<Response> {
    let envelope = ApiResponse {
        api_version: "st3.v1".into(),
        request_id: uuid::Uuid::now_v7().to_string(),
        snapshot_host: state.node.clone(),
        store_index: 0,
        value: serde_json::json!({"code":code,"message":message}),
    };
    let body = serde_json::to_vec(&envelope)?;
    let headers =
        state
            .auth
            .response_headers_for(CLIENT_READ_PATH, &state.node, &body, request_digest)?;
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response.headers_mut().extend(headers);
    Ok(response)
}

async fn wait_for_main_daemon(socket: &Path) {
    let client = Client::unix(socket);
    loop {
        if client.get::<serde_json::Value>("/v1/health").await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn start_outbound(
    backend: PeerBackend,
    node: String,
    peers: Vec<PeerConfig>,
    auth: FleetAuth,
    fleet: FleetContext,
    main_socket: PathBuf,
    notify: watch::Sender<u64>,
) {
    for peer in peers {
        // Retain the connection pool across both phases and later wakeups for this peer.
        let mut http = replication_http_client();
        let backend = backend.clone();
        let node = node.clone();
        let auth = auth.clone();
        let fleet = fleet.clone();
        let main_socket = main_socket.clone();
        let mut notify = notify.subscribe();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                // A removed node stops dialing; `st3 doctor` says what to do next.
                if fleet.is_removed() {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
                match exchange(&http, &backend, &node, &peer, &auth, &fleet, &main_socket).await {
                    Ok(moved) => {
                        backoff = Duration::from_secs(1);
                        // A busy harness can write several observations while one exchange is
                        // in flight. Keep the first exchange immediate, then coalesce the
                        // resulting wake burst without disabling the 30-second retry path.
                        // The window stays short so a publish is startable on every peer
                        // within seconds.
                        let not_before = tokio::time::Instant::now() + REPLICATION_WAKE_COALESCE;
                        if moved {
                            // One exchange carries a bounded batch. Keep going while envelopes
                            // still move instead of leaving the rest of a backlog to the timer.
                            notify.borrow_and_update();
                        } else {
                            tokio::select! {
                                _ = notify.changed() => {}
                                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                            }
                        }
                        tokio::time::sleep_until(not_before).await;
                    }
                    Err(error) => {
                        if let Some(removed) = error.downcast_ref::<RemovedFromFleet>() {
                            fleet.mark_removed(&peer.name, &removed.code);
                            continue;
                        }
                        // A peer can leave an HTTP stream open without making progress. Once
                        // that exchange times out, discard the pooled connection so the next
                        // attempt opens a fresh stream through the Fabric dial.
                        http = replication_http_client();
                        let status = if error.to_string().contains("signature")
                            || error.to_string().contains("fleet")
                        {
                            "auth-failed"
                        } else {
                            "down"
                        };
                        let _ = backend
                            .record_failure(&peer.name, status, &error.to_string())
                            .await;
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        });
    }
}

fn replication_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(REPLICATION_EXCHANGE_TIMEOUT)
        .build()
        .expect("the replication HTTP client configuration is valid")
}

async fn receive_exchange(
    State(state): State<PeerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sender = match state
        .auth
        .verify_sender(&headers, "POST", EXCHANGE_PATH, &body, None, None)
    {
        Ok(sender) => sender,
        Err(error) => {
            return (
                StatusCode::UNAUTHORIZED,
                format!("replication authentication failed: {error:#}"),
            )
                .into_response();
        }
    };
    let request_digest = FleetAuth::body_digest(&body);
    if let Err(refusal) = state.fleet.accept(&sender) {
        // Signed, so a removed member can trust it and stop dialing.
        return signed_refusal(&state, &request_digest, &refusal)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    let relay = sender.name;
    let result = async {
        let request: ReplicationExchange =
            serde_json::from_slice(&body).context("decode the replication exchange")?;
        let received = state
            .backend
            .receive(&relay, state.auth.fleet_id(), &request)
            .await?;
        if received.changed {
            wake_main(&state.main_socket).await;
            refresh_fleet_view(&state.backend, &state.fleet).await;
        }
        if received.receipt.received != 0 {
            state
                .outbound_notify
                .send_modify(|generation| *generation = generation.saturating_add(1));
        }
        let response = state
            .backend
            .export(
                state.auth.fleet_id(),
                &request.inventory,
                false,
                &request.signature_requests,
            )
            .await?;
        signed_response(
            &state,
            &request_digest,
            response.store_index,
            response.exchange,
        )
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => {
            let message = format!("replication request failed: {error:#}");
            let _ = state.backend.record_failure(&relay, "down", &message).await;
            signed_error_response(
                &state,
                &request_digest,
                0,
                StatusCode::UNPROCESSABLE_ENTITY,
                &message,
            )
            .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, message).into_response())
        }
    }
}

fn signed_refusal(state: &PeerState, request_digest: &str, refusal: &Refusal) -> Result<Response> {
    let envelope = ApiResponse {
        api_version: "st3.v1".into(),
        request_id: uuid::Uuid::now_v7().to_string(),
        snapshot_host: state.node.clone(),
        store_index: 0,
        value: serde_json::json!({
            "code": refusal.code,
            "message": refusal.message,
            "member_key": refusal.member_key,
        }),
    };
    let body = serde_json::to_vec(&envelope)?;
    let headers =
        state
            .auth
            .response_headers_for(EXCHANGE_PATH, &state.node, &body, request_digest)?;
    let status = StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::FORBIDDEN);
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response.headers_mut().extend(headers);
    Ok(response)
}

fn signed_response<T: Serialize>(
    state: &PeerState,
    request_digest: &str,
    store_index: u64,
    value: T,
) -> Result<Response> {
    signed_response_for(state, EXCHANGE_PATH, request_digest, store_index, value)
}

fn signed_response_for<T: Serialize>(
    state: &PeerState,
    path: &str,
    request_digest: &str,
    store_index: u64,
    value: T,
) -> Result<Response> {
    let envelope = ApiResponse {
        api_version: "st3.v1".into(),
        request_id: uuid::Uuid::now_v7().to_string(),
        snapshot_host: state.node.clone(),
        store_index,
        value,
    };
    let body = serde_json::to_vec(&envelope)?;
    let headers = state
        .auth
        .response_headers_for(path, &state.node, &body, request_digest)?;
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response.headers_mut().extend(headers);
    Ok(response)
}

fn signed_error_response(
    state: &PeerState,
    request_digest: &str,
    store_index: u64,
    status: StatusCode,
    message: &str,
) -> Result<Response> {
    signed_error_response_for(
        state,
        EXCHANGE_PATH,
        request_digest,
        store_index,
        status,
        message,
    )
}

fn signed_error_response_for(
    state: &PeerState,
    path: &str,
    request_digest: &str,
    store_index: u64,
    status: StatusCode,
    message: &str,
) -> Result<Response> {
    let envelope = ApiResponse {
        api_version: "st3.v1".into(),
        request_id: uuid::Uuid::now_v7().to_string(),
        snapshot_host: state.node.clone(),
        store_index,
        value: serde_json::json!({
            "code": "replication-request-failed",
            "message": message,
        }),
    };
    let body = serde_json::to_vec(&envelope)?;
    let headers = state
        .auth
        .response_headers_for(path, &state.node, &body, request_digest)?;
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response.headers_mut().extend(headers);
    Ok(response)
}

async fn exchange(
    http: &reqwest::Client,
    backend: &PeerBackend,
    node: &str,
    peer: &PeerConfig,
    auth: &FleetAuth,
    fleet: &FleetContext,
    main_socket: &Path,
) -> Result<bool> {
    let first = backend
        .export(auth.fleet_id(), &ReplicationInventory::default(), true, &[])
        .await?
        .exchange;
    let local_digest = first.inventory.digest.clone();
    let query = ReplicationExchange {
        envelopes: Vec::new(),
        ..first
    };
    let remote = post_signed(http, peer, node, auth, fleet, &query).await?;
    let different = remote.inventory.digest != local_digest;
    let pulled = !remote.envelopes.is_empty();
    let received = backend
        .receive(&peer.name, auth.fleet_id(), &remote)
        .await?;
    if received.changed {
        wake_main(main_socket).await;
    }
    let mut pushed = false;
    let mut pulled_follow_up = false;
    // A follow-up also carries the signatures the peer asked for, even when both sides hold
    // the same envelopes.
    if different || !remote.signature_requests.is_empty() {
        let push = backend
            .export(
                auth.fleet_id(),
                &remote.inventory,
                false,
                &remote.signature_requests,
            )
            .await?
            .exchange;
        pushed = !push.envelopes.is_empty();
        let response = post_signed(http, peer, node, auth, fleet, &push).await?;
        pulled_follow_up = !response.envelopes.is_empty();
        let received = backend
            .receive(&peer.name, auth.fleet_id(), &response)
            .await?;
        if received.changed {
            wake_main(main_socket).await;
        }
    }
    Ok(pulled || pulled_follow_up || pushed)
}

async fn post_signed(
    http: &reqwest::Client,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    exchange: &ReplicationExchange,
) -> Result<ReplicationExchange> {
    let body = serde_json::to_vec(exchange)?;
    let request_digest = FleetAuth::body_digest(&body);
    let headers = auth.request_headers(node, &body)?;
    let endpoint = format!("{}{}", peer.url.trim_end_matches('/'), EXCHANGE_PATH);
    let started = std::time::Instant::now();
    let response = http
        .post(&endpoint)
        .headers(headers)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .with_context(|| {
            format!(
                "replication request to peer {} failed before headers within {} seconds at `{endpoint}`",
                peer.name,
                REPLICATION_EXCHANGE_TIMEOUT.as_secs()
            )
        })?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .with_context(|| {
            format!(
                "replication response body from peer {} failed after {} ms",
                peer.name,
                started.elapsed().as_millis()
            )
        })?
        .to_vec();
    let responder = auth.verify_sender(
        &headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &bytes,
        Some(&peer.name),
        Some(&request_digest),
    )?;
    if let Err(refusal) = fleet.accept(&responder) {
        anyhow::bail!(
            "peer {} failed member authentication ({}): {}",
            peer.name,
            refusal.code,
            refusal.message
        );
    }
    if !status.is_success() {
        // A signed refusal that names this node's own key ends its membership.
        let refusal = serde_json::from_slice::<ApiResponse<serde_json::Value>>(&bytes)
            .ok()
            .map(|response| response.value);
        let code = refusal
            .as_ref()
            .and_then(|value| value["code"].as_str())
            .unwrap_or_default();
        let about_us = refusal
            .as_ref()
            .and_then(|value| value["member_key"].as_str())
            .is_some_and(|key| Some(key) == fleet.own_key.as_deref());
        if matches!(code, "member-removed" | "member-left") && about_us {
            return Err(RemovedFromFleet { code: code.into() }.into());
        }
        anyhow::bail!(
            "peer {} returned {status}: {}",
            peer.name,
            String::from_utf8_lossy(&bytes)
        );
    }
    let response: ApiResponse<ReplicationExchange> =
        serde_json::from_slice(&bytes).context("decode the signed peer response")?;
    anyhow::ensure!(
        response.api_version == "st3.v1",
        "the peer API version differs"
    );
    Ok(response.value)
}

async fn wake_main(socket: &Path) {
    if !socket.exists() {
        return;
    }
    let client = Client::unix(socket.to_path_buf());
    let _ = client
        .post::<_, serde_json::Value>("/v1/internal/replication-wake", &serde_json::json!({}))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClaimInput;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn stalled_replication_http_stream_is_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stalled = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _socket = socket;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let request = replication_http_client()
            .get(format!("http://{address}/stalled"))
            .send();
        let result = tokio::time::timeout(Duration::from_secs(25), request).await;
        stalled.abort();
        assert!(matches!(result, Ok(Err(error)) if error.is_timeout()));
    }

    #[tokio::test]
    async fn signed_client_read_returns_an_owner_local_native_timeline() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("native-home");
        let transcript = home.join(".codex/sessions/2026/09/24/relay.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session_meta","timestamp":"2026-09-24T15:00:00Z","payload":{"id":"relay-native-id","cwd":root.path(),"source":"test"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-09-24T15:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Relay owner answer"}]}}),
        )).unwrap();
        let session = crate::external_sessions::discover_fresh(Some(&home), true)
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.native_id == "relay-native-id")
            .unwrap();
        let socket = root.path().join("st3.sock");
        let main = crate::api::AppState {
            store: Arc::new(Store::open_memory("owner").unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "owner".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: vec!["source".into()],
            client_relay: None,
            native_session_home: Some(home),
            planner_default: crate::model::PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, crate::api::router(main))
                .await
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists());
        let auth = FleetAuth::test("fleet-test", &[4; 32]);
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&socket)),
            node: "owner".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            main_socket: socket,
            outbound_notify: watch::channel(0_u64).0,
        };
        let body = serde_json::to_vec(&ClientReadRequest {
            authority_actor: "person/test".into(),
            request: ClientReadOperation::Timeline {
                session_id: session.id,
                limit: 20,
                cursor: None,
            },
        })
        .unwrap();
        let mut request = Request::builder()
            .method("POST")
            .uri(CLIENT_READ_PATH)
            .body(Body::from(body.clone()))
            .unwrap();
        *request.headers_mut() = auth
            .request_headers_for(CLIENT_READ_PATH, "source", &body)
            .unwrap();
        let response = peer_router(peer.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES)
            .await
            .unwrap();
        auth.verify(
            &headers,
            "RESPONSE",
            CLIENT_READ_PATH,
            &bytes,
            Some("owner"),
            Some(&FleetAuth::body_digest(&body)),
        )
        .unwrap();
        let value: ApiResponse<Value> = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.value["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["body"]["text"] == "Relay owner answer")
        );

        let mut rejected = Request::builder()
            .method("POST")
            .uri(CLIENT_READ_PATH)
            .body(Body::from(body.clone()))
            .unwrap();
        *rejected.headers_mut() = auth
            .request_headers_for(CLIENT_READ_PATH, "unconfigured", &body)
            .unwrap();
        let response = peer_router(peer).oneshot(rejected).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[test]
    fn signed_messages_detect_tampering_and_wrong_fleets() {
        let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[7; 32]);
        let body = br#"{"hello":"fleet"}"#;
        let headers = auth.request_headers("node-a", body).unwrap();
        assert!(
            auth.verify(&headers, "POST", EXCHANGE_PATH, body, Some("node-a"), None)
                .is_ok()
        );
        assert!(
            auth.verify(
                &headers,
                "POST",
                EXCHANGE_PATH,
                br#"{"hello":"other"}"#,
                Some("node-a"),
                None
            )
            .is_err()
        );
        let other = FleetAuth::test("48608b46-bf75-442a-a462-787085dc574e", &[7; 32]);
        assert!(
            other
                .verify(&headers, "POST", EXCHANGE_PATH, body, Some("node-a"), None)
                .is_err()
        );
    }

    #[test]
    fn response_signatures_bind_to_one_request() {
        let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[9; 32]);
        let body = b"response";
        let headers = auth
            .response_headers_for(EXCHANGE_PATH, "node-b", body, "request-a")
            .unwrap();
        assert!(
            auth.verify(
                &headers,
                "RESPONSE",
                EXCHANGE_PATH,
                body,
                Some("node-b"),
                Some("request-a")
            )
            .is_ok()
        );
        assert!(
            auth.verify(
                &headers,
                "RESPONSE",
                EXCHANGE_PATH,
                body,
                Some("node-b"),
                Some("request-b")
            )
            .is_err()
        );
    }

    #[test]
    fn fleet_secret_loading_accepts_private_raw_or_hex_files_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fleet-secret");
        fs::write(&path, [3_u8; 32]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).unwrap();

        fs::write(&path, format!("{}\n", hex::encode([4_u8; 32]))).unwrap();
        FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).is_err());
    }

    #[tokio::test]
    async fn an_authenticated_failure_has_a_request_bound_signature() {
        let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[5; 32]);
        let body = Bytes::from_static(b"not json");
        let request_digest = FleetAuth::body_digest(&body);
        let response = receive_exchange(
            State(PeerState {
                backend: PeerBackend::Local(Arc::new(Store::open_memory("target").unwrap())),
                node: "target".into(),
                auth: auth.clone(),
                fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
                main_socket: PathBuf::from("/no/such/socket"),
                outbound_notify: watch::channel(0_u64).0,
            }),
            auth.request_headers("source", &body).unwrap(),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        auth.verify(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            &bytes,
            Some("target"),
            Some(&request_digest),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn the_peer_route_accepts_an_exchange_above_axums_default_body_limit() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[5; 32]);
        let state = PeerState {
            backend: PeerBackend::Local(Arc::new(Store::open_memory("target").unwrap())),
            node: "target".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            main_socket: PathBuf::from("/no/such/socket"),
            outbound_notify: watch::channel(0_u64).0,
        };
        let exchange = ReplicationExchange {
            peer: "source".into(),
            fleet_id: fleet.into(),
            schema_digest: st3_schema::registry().digest(),
            authority_digest: String::new(),
            graph_digest: String::new(),
            inventory: ReplicationInventory::default(),
            envelopes: Vec::new(),
            signature_requests: Vec::new(),
            signatures: Vec::new(),
        };
        let mut body = serde_json::to_vec(&exchange).unwrap();
        body.resize(2 * 1024 * 1024 + 1, b' ');
        let request_digest = FleetAuth::body_digest(&body);
        let mut request = Request::builder()
            .method("POST")
            .uri(EXCHANGE_PATH)
            .body(Body::from(body.clone()))
            .unwrap();
        request
            .headers_mut()
            .extend(auth.request_headers("source", &body).unwrap());
        let response = peer_router(state).oneshot(request).await.unwrap();
        assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let headers = response.headers().clone();
        let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        auth.verify(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            &response_body,
            Some("target"),
            Some(&request_digest),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn signed_peer_exchange_moves_new_authority_in_both_directions() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[6; 32]);
        let source = Arc::new(Store::open_memory("source").unwrap());
        let target = Arc::new(Store::open_memory("target").unwrap());
        source.bind_fleet(fleet).unwrap();
        target.bind_fleet(fleet).unwrap();
        source
            .append_claim(&ClaimInput {
                subject: "host/source".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("source-up".into()),
            })
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = PeerState {
            backend: PeerBackend::Local(target.clone()),
            node: "target".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            main_socket: PathBuf::from("/no/such/socket"),
            outbound_notify: watch::channel(0_u64).0,
        };
        let connection_ports = Arc::new(Mutex::new(BTreeSet::new()));
        let observed_ports = connection_ports.clone();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .route(EXCHANGE_PATH, post(receive_exchange))
                    .layer(axum::middleware::from_fn(
                        move |request: Request<Body>, next: axum::middleware::Next| {
                            let observed_ports = observed_ports.clone();
                            async move {
                                if let Some(axum::extract::ConnectInfo(address)) = request
                                    .extensions()
                                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                                {
                                    observed_ports.lock().unwrap().insert(address.port());
                                }
                                next.run(request).await
                            }
                        },
                    ))
                    .with_state(state)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        );
        let peer = PeerConfig {
            name: "target".into(),
            url: format!("http://{address}"),
        };
        let http = replication_http_client();
        exchange(
            &http,
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap();
        assert!(
            target
                .latest_claim("host/source", Some("transport.observed"))
                .unwrap()
                .is_some()
        );

        target
            .append_claim(&ClaimInput {
                subject: "host/target".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("target-up".into()),
            })
            .unwrap();
        exchange(
            &http,
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap();
        assert!(
            source
                .latest_claim("host/target", Some("transport.observed"))
                .unwrap()
                .is_some()
        );
        let source_status = source.replication_status(true, Some(fleet), &[]).unwrap();
        let target_status = target.replication_status(true, Some(fleet), &[]).unwrap();
        assert_eq!(
            source_status.authority_digest,
            target_status.authority_digest
        );
        assert_eq!(
            connection_ports.lock().unwrap().len(),
            1,
            "the two-phase exchanges and later wakeup should reuse one TCP connection"
        );
        exchange(
            &replication_http_client(),
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap();
        assert_eq!(
            connection_ports.lock().unwrap().len(),
            2,
            "a fresh client should demonstrate the former extra dial"
        );
        let summary = source.export_replication_summary(fleet).unwrap();
        assert!(summary.inventory.envelopes.is_empty());
        assert!(!summary.inventory.digest.is_empty());
        let converged = target
            .export_replication_exchange(fleet, &summary.inventory)
            .unwrap();
        assert!(converged.inventory.envelopes.is_empty());
        assert!(converged.envelopes.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn the_worker_uses_the_main_daemon_as_the_only_database_writer() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "target").unwrap());
        store.bind_fleet(fleet).unwrap();
        let app = crate::api::router(crate::api::AppState {
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "target".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: std::path::PathBuf::from("pty"),
            fleet_id: Some(fleet.into()),
            configured_peers: vec!["source".into()],
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        });
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix(&server_socket, app).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "the main daemon socket did not start");

        let source = Store::open_memory("source").unwrap();
        source.bind_fleet(fleet).unwrap();
        source
            .append_claim(&ClaimInput {
                subject: "host/source".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("source-up".into()),
            })
            .unwrap();
        let exchange = source
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        let backend = PeerBackend::Main(Client::unix(socket));
        let received = backend.receive("source", fleet, &exchange).await.unwrap();
        assert!(received.changed);
        assert!(received.receipt.received > 0);
        let exported = backend
            .export(fleet, &ReplicationInventory::default(), false, &[])
            .await
            .unwrap();
        assert!(!exported.exchange.inventory.envelopes.is_empty());
        let summary = backend
            .export(fleet, &ReplicationInventory::default(), true, &[])
            .await
            .unwrap();
        assert!(summary.exchange.inventory.envelopes.is_empty());
        assert!(!summary.exchange.inventory.digest.is_empty());
        backend
            .record_failure("source", "down", "test outage")
            .await
            .unwrap();
        let down = store
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap();
        assert_eq!(down.body["fields"]["status"], "down");
        backend.receive("source", fleet, &exchange).await.unwrap();
        let recovered = store
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.body["fields"]["status"], "up");
        server.abort();
    }

    #[tokio::test]
    async fn a_duplicate_inbound_exchange_does_not_wake_outbound_replication() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[7; 32]);
        let source = Store::open_memory("source").unwrap();
        let target = Arc::new(Store::open_memory("target").unwrap());
        source.bind_fleet(fleet).unwrap();
        target.bind_fleet(fleet).unwrap();
        source
            .append_claim(&ClaimInput {
                subject: "host/source".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("source-up".into()),
            })
            .unwrap();
        let exchange = source
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        let body = Bytes::from(serde_json::to_vec(&exchange).unwrap());
        let (outbound_notify, mut outbound_wake) = watch::channel(0_u64);
        let state = PeerState {
            backend: PeerBackend::Local(target),
            node: "target".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            main_socket: PathBuf::from("/no/such/socket"),
            outbound_notify,
        };

        let first = receive_exchange(
            State(state.clone()),
            auth.request_headers("source", &body).unwrap(),
            body.clone(),
        )
        .await;
        assert!(first.status().is_success());
        outbound_wake.changed().await.unwrap();
        let _ = outbound_wake.borrow_and_update();

        let duplicate = receive_exchange(
            State(state.clone()),
            auth.request_headers("source", &body).unwrap(),
            body,
        )
        .await;
        assert!(duplicate.status().is_success());
        assert!(
            !outbound_wake.has_changed().unwrap(),
            "a duplicate receipt must not start a replication echo loop"
        );
    }

    fn fleet_claim(store: &Store, kind: &str, subject: &str, fields: Value) {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn member_context(
        store: &Store,
        own: &MemberKey,
        bootstrap: &[&str],
        config_peers: &[&str],
        legacy: bool,
    ) -> FleetContext {
        FleetContext {
            view: Arc::new(std::sync::RwLock::new(store.fleet_view().unwrap())),
            config_peers: config_peers.iter().map(|peer| (*peer).into()).collect(),
            legacy,
            own_key: Some(own.public().into()),
            bootstrap_keys: bootstrap.iter().map(|key| (*key).into()).collect(),
            removed: Arc::default(),
            state_dir: None,
        }
    }

    #[tokio::test]
    async fn members_exchange_with_signatures_and_a_removed_member_is_refused() {
        let fleet = "3b241101-e2bb-4255-8caf-4136c566a962";
        let secret = [9_u8; 32];
        let anchor = Arc::new(MemberKey::generate().unwrap().0);
        let member = Arc::new(MemberKey::generate().unwrap().0);
        let a = Arc::new(Store::open_memory("a").unwrap());
        let b = Arc::new(Store::open_memory("b").unwrap());
        for (store, key) in [(&a, &anchor), (&b, &member)] {
            store.bind_fleet(fleet).unwrap();
            store.pin_fleet_anchor(anchor.public()).unwrap();
            store.set_member_key(Some(key.clone())).unwrap();
        }
        let admitted = |name: &str, key: &MemberKey, via: &str| {
            serde_json::json!({
                "fleet_id": fleet, "member_key": key.public(), "via": via, "mode": "listening"
            })
            .as_object()
            .map(|fields| {
                fleet_claim(
                    &a,
                    "fleet.member-admitted",
                    &format!("host/{name}"),
                    Value::Object(fields.clone()),
                )
            })
        };
        admitted("a", &anchor, "anchor");
        admitted("b", &member, "invite");

        // a listens as a member, without legacy acceptance.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let a_fleet = member_context(&a, &anchor, &[], &[], false);
        let state = PeerState {
            backend: PeerBackend::Local(a.clone()),
            node: "a".into(),
            auth: FleetAuth::test(fleet, &secret).with_member_key(Some(anchor.clone())),
            fleet: a_fleet.clone(),
            main_socket: PathBuf::from("/no/such/socket"),
            outbound_notify: watch::channel(0_u64).0,
        };
        let server = tokio::spawn(async move {
            axum::serve(listener, peer_router(state)).await.unwrap();
        });
        let peer = PeerConfig {
            name: "a".into(),
            url: format!("http://{address}"),
        };
        let http = replication_http_client();
        let b_auth = FleetAuth::test(fleet, &secret).with_member_key(Some(member.clone()));
        // b knows nothing yet but its anchor, which is enough to trust a's answer.
        let b_fleet = member_context(&b, &member, &[anchor.public()], &[], false);
        exchange(
            &http,
            &PeerBackend::Local(b.clone()),
            "b",
            &peer,
            &b_auth,
            &b_fleet,
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap();
        assert!(matches!(
            b.fleet_membership().unwrap().state("b"),
            crate::fleet::MemberState::Current(_)
        ));

        // Without its member key, b is refused.
        let unsigned = exchange(
            &http,
            &PeerBackend::Local(b.clone()),
            "b",
            &peer,
            &FleetAuth::test(fleet, &secret),
            &b_fleet,
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap_err();
        assert!(
            unsigned.to_string().contains("member-signature-required"),
            "{unsigned:#}"
        );

        // a removes b; b's next exchange gets a signed refusal that names its key.
        fleet_claim(
            &a,
            "fleet.member-removed",
            "host/b",
            serde_json::json!({"member_key": member.public(), "high_water": 100, "reason": "test"}),
        );
        *a_fleet.view.write().unwrap() = a.fleet_view().unwrap();
        *b_fleet.view.write().unwrap() = b.fleet_view().unwrap();
        let refused = exchange(
            &http,
            &PeerBackend::Local(b.clone()),
            "b",
            &peer,
            &b_auth,
            &b_fleet,
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap_err();
        let removed = refused
            .downcast_ref::<RemovedFromFleet>()
            .expect("a signed refusal naming b's key");
        assert_eq!(removed.code, "member-removed");

        // Another machine without a key, posing as b or as an unknown name, is refused too.
        // It accepts a's answers as a legacy config peer, so it sees a's refusal itself.
        let stranger = FleetContext::legacy(BTreeSet::from(["a".into()]));
        for name in ["b", "stranger"] {
            let error = exchange(
                &http,
                &PeerBackend::Local(b.clone()),
                name,
                &peer,
                &FleetAuth::test(fleet, &secret),
                &stranger,
                Path::new("/no/such/socket"),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("403"), "{error:#}");
        }
        server.abort();
    }
}
