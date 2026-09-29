use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::SocketAddr;
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
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::client::Client;
use crate::config::{Config, PeerConfig};
use crate::fleet::transport::{
    Fabric, LocalTransports, Route, bindable_tailnet_addresses, default_fabric_protocol,
    local_addresses, resolve_tool, routes_from_endpoints, tailscale_addresses,
};
use crate::fleet::{Acceptance, FleetView, MemberKey, Refusal, Sender, verify_signature};
use crate::model::{
    ApiResponse, ReplicaEnvelopeId, ReplicationExchange, ReplicationExportRequest,
    ReplicationExportResponse, ReplicationHealAnswer, ReplicationHealAnswerRequest,
    ReplicationHealNextRequest, ReplicationHealQuery, ReplicationHealRequest, ReplicationHealStep,
    ReplicationInventory, ReplicationPeerFailureRequest, ReplicationReceiveRequest,
    ReplicationReceiveResponse,
};
use crate::store::Store;
use crate::store::{CheckpointManifest, CheckpointManifestPage, CheckpointManifestRequest};

const PROTOCOL: &str = "st3-replication-v1";
const EXCHANGE_PATH: &str = "/v1/peer/exchange";
/// A page of a checkpoint's manifest, for a node that adopts a checkpoint it did not take part
/// in. Older builds neither serve nor call it.
const CHECKPOINT_PATH: &str = "/v1/peer/checkpoint";
const REPLICATION_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_WAKE_COALESCE: Duration = Duration::from_secs(1);
const CLIENT_READ_PATH: &str = "/v1/peer/client-read";
const HEAL_PATH: &str = "/v1/peer/heal";
/// A heal question can make the peer replay its graph from nothing, which takes 41 seconds on a
/// 2 GB store and longer under load.
const HEAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Questions one heal asks before it gives up until the next.
const HEAL_QUESTION_LIMIT: usize = 48;
const JOIN_PATH: &str = "/v1/fleet/join";
const MAX_JOIN_BYTES: usize = 4096;
const MAX_CLIENT_READ_BYTES: usize = 1_048_576;
/// A relayed long poll must answer well inside the relay's 15-second request timeout.
const CLIENT_READ_MAX_WAIT_MS: u64 = 10_000;
/// How long one hop waits for an owner's answer beyond the read's own long poll.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// The most nodes a client read may be forwarded through on its way to its owner.
pub const CLIENT_READ_MAX_HOPS: u8 = 4;
/// Each node on a relayed read waits this much longer than the node after it, so an answer on its
/// way back is never cut short by an earlier hop giving up first.
const CLIENT_READ_HOP_MARGIN: Duration = Duration::from_secs(10);
/// How long a relay reuses the fleet's observed links before it reads them again.
const CLIENT_READ_LINKS_TTL: Duration = Duration::from_secs(5);
/// The daemon route a replication worker hands a read to when it must forward it.
pub const CLIENT_READ_FORWARD_PATH: &str = "/v1/internal/client-read/forward";
const HEADER_FLEET: &str = "x-st3-fleet";
const HEADER_NODE: &str = "x-st3-node";
const HEADER_BODY: &str = "x-st3-body-sha256";
const HEADER_SIGNATURE: &str = "x-st3-signature";
const HEADER_REQUEST: &str = "x-st3-request-digest";
const HEADER_MEMBER_KEY: &str = "x-st3-member-key";
const HEADER_MEMBER_SIGNATURE: &str = "x-st3-member-signature";
const MEMBER_SIGNATURE_DOMAIN: &str = "st3-member-v1";
pub(crate) const MAX_EXCHANGE_BYTES: usize = 64 * 1024 * 1024;

/// The HTTP content coding for large exchange bodies: zlib-wrapped deflate. A requester asks
/// for it with `Accept-Encoding`, and a peer says with the same header in its answer that it
/// takes it in requests. Signatures cover the uncompressed JSON, so an older build, which
/// neither asks nor says, exchanges plain JSON as before.
const EXCHANGE_ENCODING: &str = "deflate";

/// Bodies smaller than this go uncompressed: a quiet exchange is a few kilobytes, while a page
/// of envelopes is megabytes and deflates to about a third.
const DEFLATE_MIN_BYTES: usize = 64 * 1024;

fn deflate(body: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(body.len() / 3),
        flate2::Compression::fast(),
    );
    encoder.write_all(body)?;
    Ok(encoder.finish()?)
}

/// Inflate an exchange body, refusing one that would expand past `MAX_EXCHANGE_BYTES`.
fn inflate(body: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut inflated = Vec::with_capacity(body.len() * 3);
    flate2::read::ZlibDecoder::new(body)
        .take(MAX_EXCHANGE_BYTES as u64 + 1)
        .read_to_end(&mut inflated)
        .context("inflate the exchange body")?;
    anyhow::ensure!(
        inflated.len() <= MAX_EXCHANGE_BYTES,
        "the exchange body inflates past {MAX_EXCHANGE_BYTES} bytes"
    );
    Ok(inflated)
}

fn deflated(headers: &HeaderMap) -> bool {
    headers
        .get("content-encoding")
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(EXCHANGE_ENCODING.as_bytes()))
}

fn accepts_deflate(headers: &HeaderMap) -> bool {
    headers
        .get_all("accept-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            coding
                .split(';')
                .next()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(EXCHANGE_ENCODING))
        })
}

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

    /// The fleet secret as the hexadecimal text a secret file holds.
    fn secret_hex(&self) -> String {
        hex::encode(self.secret.as_slice())
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

    #[cfg(test)]
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
    ConversationChanges {
        session_id: String,
        after: Option<String>,
        wait_ms: u64,
    },
    Timeline {
        session_id: String,
        limit: usize,
        cursor: Option<String>,
    },
    TerminalScreen {
        terminal_id: String,
    },
    /// Wait on the owner until the screen's revision differs from `after_revision`.
    TerminalScreenChange {
        terminal_id: String,
        after_revision: String,
        wait_ms: u64,
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
    /// The directory this host gives a new agent that names no workspace.
    AgentWorkspace {
        identity: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReadRequest {
    pub authority_actor: String,
    pub request: ClientReadOperation,
    /// Set only while the read travels through nodes that do not own what it reads. A read sent
    /// straight to its owner carries none, so an owner that predates relaying still answers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<ClientReadRoute>,
}

/// Where a relayed client read is going and where it has been.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientReadRoute {
    /// The owner's host, such as `host/owner`.
    pub target: String,
    /// The nodes the read has passed through, starting with the one it began on. The last is
    /// the node that sent it to the receiver.
    pub path: Vec<String>,
    /// How many more nodes may forward it.
    pub hops_left: u8,
}

impl ClientReadOperation {
    /// How long the owner may hold this read open before it answers.
    fn wait(&self) -> Duration {
        match self {
            Self::ConversationChanges { wait_ms, .. }
            | Self::TerminalScreenChange { wait_ms, .. } => {
                Duration::from_millis((*wait_ms).min(CLIENT_READ_MAX_WAIT_MS))
            }
            _ => Duration::ZERO,
        }
    }
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

/// Up links as `(observer, observed)`, and when they were read.
type ObservedLinks = (std::time::Instant, Arc<[(String, String)]>);

/// A paired gateway uses this for bounded owner-local client operations. The peer worker
/// authenticates both ends and the owner daemon rechecks the requested resource and fences.
///
/// An owner this node cannot dial is reached through the peers it can: each node on the way
/// forwards the read to the next, choosing by the links the fleet has observed, and relays the
/// owner's answer back. Every hop checks that its sender is a fleet member, and the owner applies
/// its own grants to the person the read carries.
#[derive(Clone)]
pub struct ClientRelay {
    node: String,
    peers: Vec<PeerConfig>,
    auth: FleetAuth,
    http: reqwest::Client,
    /// The store whose replicated transport observations say which nodes reach which.
    links: Option<Arc<Store>>,
    /// The links last read from that store, and when, so a busy gateway reads them rarely.
    observed: Arc<std::sync::Mutex<Option<ObservedLinks>>>,
}

impl ClientRelay {
    /// Choose routes by the transport observations in this store.
    pub fn with_links(mut self, store: Arc<Store>) -> Self {
        self.links = Some(store);
        self
    }

    /// Whether a read for this host has somewhere to go: the host itself, or a peer that can
    /// carry it on.
    pub fn reaches(&self, host_id: &str) -> bool {
        host_id.strip_prefix("host/").is_some_and(|name| {
            name != self.node
                && (self
                    .peers
                    .iter()
                    .any(|peer| peer.name == name && !peer.url.is_empty())
                    || !self.next_hops(name, &[]).is_empty())
        })
    }

    /// The fleet's observed up links, read again once the last reading is a few seconds old.
    fn observed_links(&self) -> Arc<[(String, String)]> {
        let Some(store) = &self.links else {
            return Arc::from([]);
        };
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((read_at, links)) = observed.as_ref()
            && read_at.elapsed() < CLIENT_READ_LINKS_TTL
        {
            return links.clone();
        }
        let links: Arc<[(String, String)]> = store.transport_links().unwrap_or_default().into();
        *observed = Some((std::time::Instant::now(), links.clone()));
        links
    }

    /// The peers to try, in order, for a read bound for `target`: the target itself when it is a
    /// peer, then the peers with the shortest observed path to it. With no observation of the
    /// target at all, every peer is worth a try. Nodes the read already passed are never chosen.
    fn next_hops(&self, target: &str, visited: &[String]) -> Vec<&PeerConfig> {
        let dialable = self
            .peers
            .iter()
            .filter(|peer| !peer.url.is_empty() && peer.name != self.node)
            .filter(|peer| !visited.contains(&peer.name))
            .collect::<Vec<_>>();
        let links = self.observed_links();
        let names = dialable
            .iter()
            .map(|peer| peer.name.clone())
            .collect::<Vec<_>>();
        client_read_next_hops(&self.node, target, &names, visited, &links)
            .into_iter()
            .filter_map(|name| dialable.iter().copied().find(|peer| peer.name == name))
            .collect()
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
                .build()?,
            links: None,
            observed: Arc::default(),
        }))
    }

    /// Read from the owner of `host_id`, directly or through the peers that can reach it.
    pub async fn read(
        &self,
        host_id: &str,
        request: &ClientReadRequest,
    ) -> Result<serde_json::Value> {
        let target = host_id
            .strip_prefix("host/")
            .context("the owner host ID is invalid")?;
        self.send_toward(
            target,
            request,
            vec![self.node.clone()],
            CLIENT_READ_MAX_HOPS,
        )
        .await
    }

    /// Carry on a read a peer relayed here because this node is on its way to the owner.
    pub async fn forward(&self, request: &ClientReadRequest) -> Result<serde_json::Value> {
        let route = request
            .relay
            .as_ref()
            .context("only a relayed client read can be forwarded")?;
        let target = route
            .target
            .strip_prefix("host/")
            .context("the relayed owner host ID is invalid")?;
        if route.hops_left == 0 || route.path.contains(&self.node) {
            return Err(ClientReadRejected {
                code: "remote-unavailable".into(),
                status: StatusCode::LOOP_DETECTED.as_u16(),
                message: format!("{} cannot carry this read any further", self.node),
            }
            .into());
        }
        let mut path = route.path.clone();
        path.push(self.node.clone());
        self.send_toward(target, request, path, route.hops_left - 1)
            .await
    }

    /// Try each next hop in turn. An owner's own answer, including a refusal such as a stale
    /// fence, ends the attempt; a hop that cannot be reached, or cannot reach further, does not.
    async fn send_toward(
        &self,
        target: &str,
        request: &ClientReadRequest,
        path: Vec<String>,
        hops_left: u8,
    ) -> Result<serde_json::Value> {
        let mut last = None;
        for peer in self.next_hops(target, &path) {
            let relay = (peer.name != target).then(|| ClientReadRoute {
                target: format!("host/{target}"),
                path: path.clone(),
                hops_left,
            });
            if relay.as_ref().is_some_and(|relay| relay.hops_left == 0) {
                continue;
            }
            let outgoing = ClientReadRequest {
                authority_actor: request.authority_actor.clone(),
                request: request.request.clone(),
                relay,
            };
            match self.send(peer, &outgoing).await {
                Ok(value) => return Ok(value),
                Err(error)
                    if error
                        .downcast_ref::<ClientReadRejected>()
                        .is_some_and(|rejected| rejected.code != "remote-unavailable") =>
                {
                    return Err(error);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow::anyhow!("no peer of {} can reach owner host/{target}", self.node)
        }))
    }

    async fn send(
        &self,
        peer: &PeerConfig,
        request: &ClientReadRequest,
    ) -> Result<serde_json::Value> {
        let name = peer.name.as_str();
        // A relayed read may pass through more nodes, each waiting a little less than the last.
        let timeout = CLIENT_READ_TIMEOUT
            + request.request.wait()
            + request.relay.as_ref().map_or(Duration::ZERO, |relay| {
                CLIENT_READ_HOP_MARGIN * (u32::from(relay.hops_left) + 1)
            });
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
            .timeout(timeout)
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

/// Order the dialable peers for a read from `node` bound for `target`. Links are the fleet's
/// observed up transports, `(observer, observed)`, taken as usable either way. The target comes
/// first when it is dialable; then every dialable peer with a path to the target that avoids this
/// node and the nodes already visited, nearest first. When the observations never name the target,
/// every dialable peer is tried, since none can be ruled out.
pub(crate) fn client_read_next_hops(
    node: &str,
    target: &str,
    dialable: &[String],
    visited: &[String],
    links: &[(String, String)],
) -> Vec<String> {
    let blocked = |name: &str| name == node || visited.iter().any(|seen| seen == name);
    let mut neighbours = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (from, to) in links {
        if from != to {
            neighbours.entry(from).or_default().insert(to);
            neighbours.entry(to).or_default().insert(from);
        }
    }
    let mut ordered = dialable
        .iter()
        .filter(|name| name.as_str() == target)
        .cloned()
        .collect::<Vec<_>>();
    if !neighbours.contains_key(target) {
        let mut rest = dialable
            .iter()
            .filter(|name| name.as_str() != target && !blocked(name))
            .cloned()
            .collect::<Vec<_>>();
        rest.sort();
        ordered.extend(rest);
        return ordered;
    }
    // Distances to the target, walking out from it through nodes the read may still visit.
    let mut distance = BTreeMap::from([(target, 0_usize)]);
    let mut frontier = std::collections::VecDeque::from([target]);
    while let Some(current) = frontier.pop_front() {
        let next = distance[current] + 1;
        for &neighbour in neighbours.get(current).into_iter().flatten() {
            if blocked(neighbour) || distance.contains_key(neighbour) {
                continue;
            }
            distance.insert(neighbour, next);
            frontier.push_back(neighbour);
        }
    }
    let mut routed = dialable
        .iter()
        .filter(|name| name.as_str() != target && !blocked(name))
        .filter_map(|name| {
            distance
                .get(name.as_str())
                .map(|hops| (*hops, name.clone()))
        })
        .collect::<Vec<_>>();
    routed.sort();
    ordered.extend(routed.into_iter().map(|(_, name)| name));
    ordered
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
    /// Bumped whenever the view changes, so the dial set follows membership.
    view_changed: watch::Sender<u64>,
    config_peers: BTreeSet<String>,
    /// True on a node without `fleet.toml`, or with `legacy_peers = true`.
    legacy: bool,
    own_key: Option<String>,
    /// Keys this node trusts before membership arrives: its pinned anchor and its sponsor.
    bootstrap_keys: BTreeSet<String>,
    /// Which transports this machine can dial with right now.
    transports: Arc<std::sync::RwLock<LocalTransports>>,
    fabric: Option<Fabric>,
    /// The sponsor and how this node reached it when it joined, dialed until membership names
    /// the sponsor.
    bootstrap: Option<(String, Vec<Route>)>,
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
            view_changed: watch::channel(0).0,
            config_peers,
            legacy: true,
            own_key: None,
            bootstrap_keys: BTreeSet::new(),
            transports: Arc::default(),
            fabric: None,
            bootstrap: None,
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

    /// Hand an exchange to the main daemon. `round_trip` is how long this worker's request that
    /// returned it took, when the exchange is a response.
    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
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
                            round_trip_ms: round_trip.map(|duration| duration.as_millis() as u64),
                        },
                    )
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => {
                if let Some(round_trip) = round_trip {
                    store.record_replication_round_trip(round_trip);
                }
                let receipt = store
                    .receive_replication_exchange_asking(
                        peer,
                        fleet_id,
                        exchange,
                        round_trip.is_some(),
                    )
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

    /// Answer a peer's heal question from this node's claims.
    async fn heal_answer(
        &self,
        peer: &str,
        fleet_id: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        match self {
            Self::Main(client) => {
                client
                    .post(
                        "/v1/internal/replication/heal/answer",
                        &ReplicationHealAnswerRequest {
                            peer: peer.to_owned(),
                            fleet_id: fleet_id.to_owned(),
                            query: query.clone(),
                        },
                    )
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => store.heal_answer(peer, query),
        }
    }

    /// Compare a peer's heal answer with this node's claims and learn what to ask next.
    async fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        match self {
            Self::Main(client) => {
                client
                    .post(
                        "/v1/internal/replication/heal/next",
                        &ReplicationHealNextRequest {
                            peer: peer.to_owned(),
                            answer,
                        },
                    )
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => store.heal_next(peer, answer),
        }
    }

    async fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        match self {
            Self::Main(client) => {
                client
                    .post("/v1/internal/replication/checkpoint", request)
                    .await
            }
            #[cfg(test)]
            Self::Local(store) => store.checkpoint_manifest_page(request),
        }
    }

    async fn publish_endpoints(&self, mode: &str, endpoints: &[Value]) -> Result<()> {
        match self {
            Self::Main(client) => {
                let _: Value = client
                    .post(
                        "/v1/internal/fleet/endpoints",
                        &crate::api::FleetEndpointsRequest {
                            mode: mode.into(),
                            endpoints: endpoints.to_vec(),
                        },
                    )
                    .await?;
                Ok(())
            }
            #[cfg(test)]
            Self::Local(store) => store
                .publish_fleet_endpoints(mode, endpoints, "test")
                .map(|_| ()),
        }
    }

    async fn redeem(&self, request: &crate::fleet::handshake::JoinRequest) -> Result<Value> {
        match self {
            Self::Main(client) => client.post("/v1/internal/fleet/redeem", request).await,
            #[cfg(test)]
            Self::Local(store) => Ok(match store.redeem_fleet_invite(request)? {
                crate::store::FleetRedemption::Closed => serde_json::json!({"status": "closed"}),
                crate::store::FleetRedemption::Refused(reason) => {
                    serde_json::json!({"status": "refused", "reason": reason})
                }
                crate::store::FleetRedemption::Admitted {
                    token,
                    writer_floor,
                    admitted_claim,
                    ..
                } => serde_json::json!({
                    "status": "admitted",
                    "token": hex::encode(token),
                    "writer_floor": writer_floor,
                    "admitted_claim": admitted_claim,
                    "anchor_key": store.fleet_anchor()?,
                    "fleet_id": store.bound_fleet()?,
                    "fabric_protocol": serde_json::Value::Null,
                }),
            }),
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
                if store.record_peer_failure(peer, status, error)? {
                    store.record_transport_observation(peer, status, Some(error), None)?;
                }
                Ok(())
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
    let wants = |transport: &str| {
        config
            .fleet
            .as_ref()
            .is_some_and(|file| file.transports.iter().any(|name| name == transport))
    };
    let tailscale = config
        .fleet
        .as_ref()
        .filter(|_| wants("tailscale"))
        .and_then(|file| resolve_tool(file.tailscale.as_deref(), "tailscale"));
    let fabric = config
        .fleet
        .as_ref()
        .filter(|_| wants("fabric"))
        .and_then(|file| resolve_tool(file.fabric.as_deref(), "fabric"))
        .map(Fabric::new);
    let fleet = FleetContext {
        view: Arc::default(),
        view_changed: watch::channel(0).0,
        transports: Arc::new(std::sync::RwLock::new(LocalTransports {
            tailscale: false,
            fabric: fabric.is_some(),
        })),
        fabric: fabric.clone(),
        bootstrap: config.fleet.as_ref().and_then(|file| {
            let sponsor = file.sponsor.clone()?;
            let routes = file
                .sponsor_routes
                .iter()
                .filter_map(|route| parse_route(route))
                .collect::<Vec<_>>();
            (!routes.is_empty()).then_some((sponsor, routes))
        }),
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
    let fleet_transports = fleet.transports.clone();
    let notify_for_transports = notify.clone();
    for peer in config.peers.iter().filter(|peer| peer.url.is_empty()) {
        let backend = backend.clone();
        let name = peer.name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
                let _ = backend
                    .record_failure(&name, "down", "no recent inbound exchange")
                    .await;
            }
        });
    }
    start_outbound(
        backend.clone(),
        config.node.clone(),
        config.peers,
        auth,
        fleet,
        config.socket,
        notify,
    );
    let endpoints = Endpoints::default();
    let app = peer_router(state);
    let listening = config
        .fleet
        .as_ref()
        .is_none_or(|file| file.mode == crate::config::FleetMode::Listening);
    let listener = match config.peer_listen.as_deref().filter(|_| listening) {
        Some(address) => Some(
            TcpListener::bind(address)
                .await
                .with_context(|| format!("bind the replication listener at {address}"))?,
        ),
        None => None,
    };
    let loopback = listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok());
    if let Some(file) = &config.fleet {
        if let (Some(address), true) = (loopback, file.advertise_loopback) {
            endpoints.set_loopback(Some(address));
        }
        if let Some(tailscale) = tailscale {
            tokio::spawn(keep_tailnet_current(
                tailscale,
                loopback.map(|address| (address.port(), app.clone())),
                endpoints.clone(),
                fleet_transports,
                notify_for_transports,
            ));
        }
        if let (Some(fabric), Some(address)) = (fabric, loopback) {
            let protocol = file
                .fabric_protocol
                .clone()
                .unwrap_or_else(|| default_fabric_protocol(&file.fleet_id));
            tokio::spawn(keep_fabric_exposed(
                fabric,
                protocol,
                address,
                endpoints.clone(),
            ));
        }
        tokio::spawn(keep_endpoints_published(
            backend,
            file.mode.as_str(),
            endpoints.clone(),
        ));
    }
    // A dial-out member accepts no connections.
    let Some(listener) = listener else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    axum::serve(listener, app).await?;
    Ok(())
}

/// The endpoints this member announces, as its transports come up.
#[derive(Clone, Default)]
struct Endpoints {
    set: Arc<std::sync::Mutex<EndpointSet>>,
    changed: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Default, PartialEq)]
struct EndpointSet {
    tailscale: Vec<SocketAddr>,
    fabric: Option<(String, String)>,
    loopback: Option<SocketAddr>,
}

impl Endpoints {
    fn update(&self, change: impl FnOnce(&mut EndpointSet)) {
        let mut set = self.set.lock().expect("endpoint lock poisoned");
        let before = set.clone();
        change(&mut set);
        if *set != before {
            self.changed.notify_one();
        }
    }

    fn set_loopback(&self, address: Option<SocketAddr>) {
        self.update(|set| set.loopback = address);
    }

    fn list(&self) -> Vec<Value> {
        let set = self.set.lock().expect("endpoint lock poisoned");
        let mut endpoints = set
            .tailscale
            .iter()
            .map(|address| {
                serde_json::json!({"transport": "tailscale", "address": address.to_string()})
            })
            .collect::<Vec<_>>();
        if let Some((node, protocol)) = &set.fabric {
            endpoints.push(
                serde_json::json!({"transport": "fabric", "node": node, "protocol": protocol}),
            );
        }
        if let Some(address) = set.loopback {
            endpoints
                .push(serde_json::json!({"transport": "loopback", "address": address.to_string()}));
        }
        endpoints
    }
}

/// Follow this machine's tailnet addresses: dial over Tailscale while it has one, and, for a
/// listening member, bind each new address on the replication port and announce it.
async fn keep_tailnet_current(
    tailscale: PathBuf,
    listen: Option<(u16, Router)>,
    endpoints: Endpoints,
    transports: Arc<std::sync::RwLock<LocalTransports>>,
    notify: watch::Sender<u64>,
) {
    let mut bound = BTreeSet::new();
    loop {
        let bindable = match tailscale_addresses(&tailscale).await {
            Ok(reported) => bindable_tailnet_addresses(&reported, &local_addresses()),
            Err(_) => Vec::new(),
        };
        let up = !bindable.is_empty();
        let was_up = std::mem::replace(
            &mut transports
                .write()
                .expect("transport lock poisoned")
                .tailscale,
            up,
        );
        if up != was_up {
            notify.send_modify(|generation| *generation = generation.wrapping_add(1));
        }
        if let Some((port, app)) = &listen {
            for address in bindable {
                if bound.contains(&address) {
                    continue;
                }
                if let Ok(listener) = TcpListener::bind(SocketAddr::new(address, *port)).await {
                    bound.insert(address);
                    let app = app.clone();
                    tokio::spawn(async move {
                        let _ = axum::serve(listener, app).await;
                    });
                }
            }
            let addresses = bound
                .iter()
                .map(|address| SocketAddr::new(*address, *port))
                .collect::<Vec<_>>();
            endpoints.update(|set| set.tailscale = addresses);
        }
        tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
    }
}

/// Keep the loopback listener exposed through Fabric. The exposure is ephemeral, so it is
/// asserted again every minute and vanishes when Fabric restarts without this worker.
async fn keep_fabric_exposed(
    fabric: Fabric,
    protocol: String,
    address: SocketAddr,
    endpoints: Endpoints,
) {
    loop {
        if fabric.expose(&protocol, &address.to_string()).await.is_ok()
            && let Ok(node) = fabric.id().await
        {
            endpoints.update(|set| set.fabric = Some((node, protocol.clone())));
        }
        tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
    }
}

async fn refresh_fleet_view(backend: &PeerBackend, fleet: &FleetContext) {
    if let Ok(view) = backend.fleet_view().await {
        let mut current = fleet.view.write().expect("fleet view lock poisoned");
        if *current != view {
            *current = view;
            fleet
                .view_changed
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }
}

/// Announce this member's mode and endpoints whenever they change, and again every minute;
/// the daemon writes a claim only when they differ from the last one.
async fn keep_endpoints_published(backend: PeerBackend, mode: &'static str, endpoints: Endpoints) {
    loop {
        let _ = backend.publish_endpoints(mode, &endpoints.list()).await;
        tokio::select! {
            _ = endpoints.changed.notified() => {}
            _ = tokio::time::sleep(worker_interval(Duration::from_secs(60))) => {}
        }
    }
}

/// A route recorded in `fleet.toml`: an `http://` URL or `fabric://NODE_ID/PROTOCOL`.
fn parse_route(route: &str) -> Option<Route> {
    if let Some(rest) = route.strip_prefix("fabric://") {
        let (node, protocol) = rest.split_once('/')?;
        return Some(Route::Fabric {
            node: node.into(),
            protocol: protocol.into(),
        });
    }
    route
        .starts_with("http://")
        .then(|| Route::Http(route.into()))
}

/// Who this node dials, and the loopback URLs that reach each, most preferred first: every
/// current listening member other than itself, and every config peer that has never been a
/// member. A dial-out member is never dialed. A config peer entry for a member is that
/// member's first route from this machine.
fn dial_targets(
    view: &FleetView,
    own: &str,
    config_peers: &[PeerConfig],
    local: LocalTransports,
) -> BTreeMap<String, Vec<Route>> {
    let mut targets = BTreeMap::new();
    for member in &view.members {
        if member.name == own || member.state != "current" || member.mode != "listening" {
            continue;
        }
        let mut routes = config_peers
            .iter()
            .filter(|peer| peer.name == member.name && !peer.url.is_empty())
            .map(|peer| Route::Http(peer.url.clone()))
            .collect::<Vec<_>>();
        routes.extend(routes_from_endpoints(&member.endpoints, local));
        if !routes.is_empty() {
            targets.insert(member.name.clone(), routes);
        }
    }
    for peer in config_peers {
        let known = view.members.iter().any(|member| member.name == peer.name)
            || view.legacy_removed.contains(&peer.name);
        if !known && peer.name != own && !peer.url.is_empty() {
            targets.insert(peer.name.clone(), vec![Route::Http(peer.url.clone())]);
        }
    }
    targets
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
            _ = tokio::time::sleep(worker_interval(Duration::from_secs(30))) => {}
        }
        refresh_fleet_view(&backend, &fleet).await;
    }
}

fn peer_router(state: PeerState) -> Router {
    Router::new()
        .route(EXCHANGE_PATH, post(receive_exchange))
        .route(HEAL_PATH, post(receive_heal))
        .route(
            CHECKPOINT_PATH,
            post(receive_checkpoint_request).layer(DefaultBodyLimit::max(16_384)),
        )
        .route(
            JOIN_PATH,
            post(receive_join).layer(DefaultBodyLimit::max(MAX_JOIN_BYTES)),
        )
        .route(
            CLIENT_READ_PATH,
            post(receive_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .layer(DefaultBodyLimit::max(MAX_EXCHANGE_BYTES))
        .with_state(state)
}

/// Hand a read bound for another owner to this node's daemon, which knows the routes and the
/// peers to carry it on. Its answer, or the owner's refusal, comes back unchanged.
async fn forward_client_read(
    state: &PeerState,
    request: &ClientReadRequest,
) -> Result<serde_json::Value> {
    let daemon = Client::unix(state.main_socket.clone());
    daemon
        .post::<_, serde_json::Value>(CLIENT_READ_FORWARD_PATH, request)
        .await
        .map_err(|error| match crate::client::api_error_parts(&error) {
            Some((status, code, message)) => ClientReadRejected {
                code: code.to_owned(),
                status,
                message: message.to_owned(),
            }
            .into(),
            None => error,
        })
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
        if let Some(route) = &request.relay {
            // The path names the member that sent it last, and never this node again.
            anyhow::ensure!(
                route.path.last() == Some(&sender)
                    && route.path.len() <= usize::from(CLIENT_READ_MAX_HOPS) + 1,
                "the relayed client read's path does not end at its sender"
            );
            if route.target != format!("host/{}", state.node) {
                return forward_client_read(&state, &request).await;
            }
        }
        let client = st3_client::Client::unix_as(&state.main_socket, &request.authority_actor);
        match request.request {
            ClientReadOperation::ConversationChanges {
                session_id,
                after,
                wait_ms,
            } => {
                anyhow::ensure!(
                    wait_ms <= CLIENT_READ_MAX_WAIT_MS,
                    "the conversation wait exceeds its bound"
                );
                Ok(serde_json::to_value(
                    client
                        .conversation_changes(&session_id, after.as_deref(), wait_ms)
                        .await?
                        .value,
                )?)
            }
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
            ClientReadOperation::TerminalScreenChange {
                terminal_id,
                after_revision,
                wait_ms,
            } => {
                anyhow::ensure!(
                    wait_ms <= CLIENT_READ_MAX_WAIT_MS,
                    "the terminal screen wait exceeds its bound"
                );
                let value = client
                    .terminal_screen_change(&terminal_id, &after_revision, wait_ms)
                    .await?
                    .value;
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
            ClientReadOperation::AgentWorkspace { identity } => {
                let workspace =
                    crate::config::default_agent_workspace(&identity).map_err(|error| {
                        ClientReadRejected {
                            code: "validation-failed".into(),
                            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                            message: error.to_string(),
                        }
                    })?;
                Ok(serde_json::json!({ "workspace": workspace }))
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

/// One dialer: its peer's current routes and the task that uses them.
type Dialer = (
    Arc<std::sync::RwLock<Vec<Route>>>,
    tokio::task::JoinHandle<()>,
);

fn start_outbound(
    backend: PeerBackend,
    node: String,
    config_peers: Vec<PeerConfig>,
    auth: FleetAuth,
    fleet: FleetContext,
    main_socket: PathBuf,
    notify: watch::Sender<u64>,
) {
    // One dialer per target. Targets follow membership: a new listening member gets a dialer,
    // and a member that ends or turns dial-out loses its dialer.
    tokio::spawn(async move {
        let mut view_changes = fleet.view_changed.subscribe();
        let mut dialers: BTreeMap<String, Dialer> = BTreeMap::new();
        let mut transport_changes = notify.subscribe();
        loop {
            let targets = {
                let view = fleet.view.read().expect("fleet view lock poisoned");
                let local = *fleet.transports.read().expect("transport lock poisoned");
                let mut targets = dial_targets(&view, &node, &config_peers, local);
                // Until membership names the sponsor, dial it the way the join reached it.
                if let Some((sponsor, routes)) = &fleet.bootstrap
                    && view.members.iter().all(|member| member.name != *sponsor)
                {
                    targets
                        .entry(sponsor.clone())
                        .or_insert_with(|| routes.clone());
                }
                targets
            };
            dialers.retain(|name, (_, task)| {
                let keep = targets.contains_key(name);
                if !keep {
                    task.abort();
                }
                keep
            });
            for (name, routes) in targets {
                if let Some((current, _)) = dialers.get(&name) {
                    *current.write().expect("route lock poisoned") = routes;
                    continue;
                }
                let routes = Arc::new(std::sync::RwLock::new(routes));
                let task = tokio::spawn(dial_peer(
                    backend.clone(),
                    node.clone(),
                    name.clone(),
                    routes.clone(),
                    auth.clone(),
                    fleet.clone(),
                    main_socket.clone(),
                    notify.subscribe(),
                ));
                dialers.insert(name, (routes, task));
            }
            tokio::select! {
                changed = view_changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = transport_changes.changed() => {}
                _ = tokio::time::sleep(worker_interval(Duration::from_secs(30))) => {}
            }
        }
    });
}

/// Exchange with one peer for as long as it stays a target, trying its routes in order.
#[allow(clippy::too_many_arguments)]
async fn dial_peer(
    backend: PeerBackend,
    node: String,
    name: String,
    routes: Arc<std::sync::RwLock<Vec<Route>>>,
    auth: FleetAuth,
    fleet: FleetContext,
    main_socket: PathBuf,
    mut notify: watch::Receiver<u64>,
) {
    // Retain the connection pool across both phases and later wakeups for this peer.
    let mut http = replication_http_client();
    let mut backoff = Duration::from_secs(1);
    let mut route = 0_usize;
    loop {
        // A removed node stops dialing; `st3 doctor` says what to do next.
        if fleet.is_removed() {
            tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
            continue;
        }
        let selected = {
            let routes = routes.read().expect("route lock poisoned");
            if routes.is_empty() {
                None
            } else {
                Some(routes[route % routes.len()].clone())
            }
        };
        let url = match selected {
            Some(Route::Http(url)) => Some(url),
            // The worker dials Fabric itself: a local tunnel, reused while it lives.
            Some(Route::Fabric { node, protocol }) => match &fleet.fabric {
                Some(fabric) => fabric
                    .dial(&node, &protocol)
                    .await
                    .ok()
                    .map(|address| format!("http://{address}")),
                None => None,
            },
            None => None,
        };
        let Some(url) = url else {
            route = route.wrapping_add(1);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        };
        let peer = PeerConfig {
            name: name.clone(),
            url,
        };
        match exchange(&http, &backend, &node, &peer, &auth, &fleet, &main_socket).await {
            Ok((moved, heal_now)) => {
                backoff = Duration::from_secs(1);
                if heal_now {
                    heal(&backend, &node, &peer, &auth, &fleet, &main_socket).await;
                }
                if moved {
                    // One exchange carries a bounded batch. Keep going at once while envelopes
                    // still move instead of leaving the rest of a backlog to the timer.
                    notify.borrow_and_update();
                } else {
                    // A busy harness can write several observations while one exchange is in
                    // flight. Keep the first exchange immediate, then coalesce the resulting
                    // wake burst without disabling the 30-second retry path. The window stays
                    // short so a publish is startable on every peer within seconds.
                    let not_before = tokio::time::Instant::now() + REPLICATION_WAKE_COALESCE;
                    tokio::select! {
                        _ = notify.changed() => {}
                        _ = tokio::time::sleep(worker_interval(Duration::from_secs(30))) => {}
                    }
                    tokio::time::sleep_until(not_before).await;
                }
            }
            Err(error) => {
                if let Some(removed) = error.downcast_ref::<RemovedFromFleet>() {
                    fleet.mark_removed(&peer.name, &removed.code);
                    continue;
                }
                // A peer can leave an HTTP stream open without making progress. Once that
                // exchange times out, discard the pooled connection so the next attempt opens
                // a fresh stream, and try the next route.
                http = replication_http_client();
                route = route.wrapping_add(1);
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
}

/// A worker timer. `ST3_WORKER_INTERVAL_MS` caps every one of them, so tests of several nodes on
/// one machine need not wait out the 30- and 60-second defaults.
fn worker_interval(default: Duration) -> Duration {
    static CAP: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    CAP.get_or_init(|| {
        std::env::var("ST3_WORKER_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
    })
    .map_or(default, |cap| cap.min(default))
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
    let body = if deflated(&headers) {
        match inflate(&body) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("replication request body: {error:#}"),
                )
                    .into_response();
            }
        }
    } else {
        body
    };
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
            .receive(&relay, state.auth.fleet_id(), &request, None)
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
        let response = signed_response(
            &state,
            &request_digest,
            response.store_index,
            response.exchange,
        )?;
        deflate_response(response, accepts_deflate(&headers)).await
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

/// A peer's heal question, answered from this node's claims by the main daemon.
async fn receive_heal(State(state): State<PeerState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = if deflated(&headers) {
        match inflate(&body) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("heal request body: {error:#}"),
                )
                    .into_response();
            }
        }
    } else {
        body
    };
    let sender = match state
        .auth
        .verify_sender(&headers, "POST", HEAL_PATH, &body, None, None)
    {
        Ok(sender) => sender,
        Err(error) => {
            return (
                StatusCode::UNAUTHORIZED,
                format!("heal authentication failed: {error:#}"),
            )
                .into_response();
        }
    };
    let request_digest = FleetAuth::body_digest(&body);
    if let Err(refusal) = state.fleet.accept(&sender) {
        return signed_refusal(&state, &request_digest, &refusal)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    let result = async {
        let request: ReplicationHealRequest =
            serde_json::from_slice(&body).context("decode the heal request")?;
        anyhow::ensure!(
            request.fleet_id == state.auth.fleet_id(),
            "the peer belongs to another fleet"
        );
        let answer = state
            .backend
            .heal_answer(&sender.name, state.auth.fleet_id(), &request.query)
            .await?;
        if matches!(
            answer,
            ReplicationHealAnswer::Swapped { .. } | ReplicationHealAnswer::Replayed { .. }
        ) {
            wake_main(&state.main_socket).await;
        }
        let response = signed_response_for(&state, HEAL_PATH, &request_digest, 0, answer)?;
        deflate_response(response, accepts_deflate(&headers)).await
    }
    .await;
    result.unwrap_or_else(|error| {
        signed_error_response_for(
            &state,
            HEAL_PATH,
            &request_digest,
            0,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("heal request failed: {error:#}"),
        )
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    })
}

/// Answer a member or config peer with one page of a checkpoint's manifest. It is authenticated
/// exactly like an exchange, and the answer is signed for this path.
async fn receive_checkpoint_request(
    State(state): State<PeerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sender =
        match state
            .auth
            .verify_sender(&headers, "POST", CHECKPOINT_PATH, &body, None, None)
        {
            Ok(sender) if state.fleet.accept(&sender).is_ok() => sender,
            _ => return (StatusCode::UNAUTHORIZED, "untrusted checkpoint request").into_response(),
        };
    let request_digest = FleetAuth::body_digest(&body);
    let result = async {
        let request: CheckpointManifestRequest =
            serde_json::from_slice(&body).context("decode the checkpoint request")?;
        let page = state.backend.checkpoint_manifest(&request).await?;
        let response = signed_response_for(&state, CHECKPOINT_PATH, &request_digest, 0, page)?;
        deflate_response(response, accepts_deflate(&headers)).await
    }
    .await;
    result.unwrap_or_else(|error| {
        let message = format!("checkpoint request from {} failed: {error:#}", sender.name);
        signed_error_response_for(
            &state,
            CHECKPOINT_PATH,
            &request_digest,
            0,
            StatusCode::UNPROCESSABLE_ENTITY,
            &message,
        )
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, message).into_response())
    })
}

/// Fetch a checkpoint's whole manifest from a peer, page by page. The caller verifies it against
/// the checkpoint's certificate before storing anything from it.
#[allow(dead_code)] // Adoption calls it; see doc/fleet/smalltalk/checkpoint-design, P5.
async fn fetch_checkpoint_manifest(
    http: &reqwest::Client,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    checkpoint: &str,
    cut_unix_ms: u128,
) -> Result<CheckpointManifest> {
    let mut manifest = CheckpointManifest {
        checkpoint: checkpoint.to_owned(),
        cut_unix_ms,
        ..CheckpointManifest::default()
    };
    let mut after = None;
    loop {
        let request = CheckpointManifestRequest {
            checkpoint: checkpoint.to_owned(),
            cut_unix_ms,
            after,
        };
        let page = fetch_checkpoint_manifest_page(http, peer, node, auth, fleet, &request).await?;
        after = manifest.append(page).map_err(anyhow::Error::msg)?;
        if after.is_none() {
            return Ok(manifest);
        }
    }
}

async fn fetch_checkpoint_manifest_page(
    http: &reqwest::Client,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    request: &CheckpointManifestRequest,
) -> Result<CheckpointManifestPage> {
    let body = serde_json::to_vec(request)?;
    let request_digest = FleetAuth::body_digest(&body);
    let headers = auth.request_headers_for(CHECKPOINT_PATH, node, &body)?;
    let endpoint = format!("{}{}", peer.url.trim_end_matches('/'), CHECKPOINT_PATH);
    let response = http
        .post(&endpoint)
        .headers(headers)
        .header("content-type", "application/json")
        .header("accept-encoding", EXCHANGE_ENCODING)
        .body(body)
        .send()
        .await
        .with_context(|| format!("checkpoint request to peer {} failed", peer.name))?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.bytes().await?.to_vec();
    let bytes = if deflated(&headers) {
        inflate(&bytes)?
    } else {
        bytes
    };
    let responder = auth.verify_sender(
        &headers,
        "RESPONSE",
        CHECKPOINT_PATH,
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
    anyhow::ensure!(
        status.is_success(),
        "peer {} returned {status}: {}",
        peer.name,
        String::from_utf8_lossy(&bytes)
    );
    let response: ApiResponse<CheckpointManifestPage> =
        serde_json::from_slice(&bytes).context("decode the signed checkpoint page")?;
    anyhow::ensure!(
        response.api_version == "st3.v1",
        "the peer API version differs"
    );
    anyhow::ensure!(
        response.value.checkpoint == request.checkpoint
            && response.value.cut_unix_ms == request.cut_unix_ms,
        "peer {} answered for another checkpoint",
        peer.name
    );
    Ok(response.value)
}

/// The join route. It exists only while this node sponsors an open invite; otherwise it answers
/// 404 like any unknown path. Every refusal looks the same to the caller.
///
/// There is no request quota: one spent before the proof is checked would let any caller block
/// real joins. A request is cheap to refuse (a bounded body, one lookup, one HMAC), a 128-bit
/// token cannot be guessed, and five bad proofs naming one invite burn that invite, which only
/// someone holding the code can name.
async fn receive_join(State(state): State<PeerState>, body: Bytes) -> Response {
    let refused = || {
        (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"code": "invite-invalid"})),
        )
            .into_response()
    };
    let Ok(request) = serde_json::from_slice::<crate::fleet::handshake::JoinRequest>(&body) else {
        return refused();
    };
    let Some(member) = state.auth.member.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let answer = match state.backend.redeem(&request).await {
        Ok(answer) => answer,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    match answer["status"].as_str() {
        Some("admitted") => {}
        Some("closed") => return StatusCode::NOT_FOUND.into_response(),
        _ => return refused(),
    }
    let text = |field: &str| answer[field].as_str().map(str::to_owned);
    let Some(token) = text("token").and_then(|token| hex::decode(token).ok()) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let payload = crate::fleet::handshake::SealedJoin {
        fleet_id: text("fleet_id").unwrap_or_else(|| state.auth.fleet_id().to_owned()),
        // A migrating node already holds the secret; it is never sent again.
        secret: (!request.migrate).then(|| state.auth.secret_hex()),
        anchor_key: text("anchor_key").unwrap_or_default(),
        sponsor: state.node.clone(),
        writer_floor: answer["writer_floor"].as_u64(),
        fabric_protocol: text("fabric_protocol"),
        admitted_claim: text("admitted_claim"),
    };
    match crate::fleet::handshake::seal_response(&request, &token, &state.node, &member, &payload) {
        Ok(response) => {
            // A test fault point: drop this one answer after the invite is bound, as a lost
            // response would.
            if let Some(marker) = std::env::var_os("ST3_TEST_DROP_JOIN_ANSWER")
                && std::fs::remove_file(&marker).is_ok()
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            // The new member dials soon; let the dialers see it.
            refresh_fleet_view(&state.backend, &state.fleet).await;
            (StatusCode::OK, axum::Json(response)).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
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

/// Say that this node takes compressed requests, and compress a large signed response body
/// for a requester that asked. The signature covers the uncompressed JSON.
async fn deflate_response(response: Response, requested: bool) -> Result<Response> {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        "accept-encoding",
        HeaderValue::from_static(EXCHANGE_ENCODING),
    );
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("read the signed response body")?;
    if !requested || body.len() < DEFLATE_MIN_BYTES {
        return Ok(Response::from_parts(parts, axum::body::Body::from(body)));
    }
    parts.headers.insert(
        "content-encoding",
        HeaderValue::from_static(EXCHANGE_ENCODING),
    );
    parts.headers.remove("content-length");
    Ok(Response::from_parts(
        parts,
        axum::body::Body::from(deflate(&body)?),
    ))
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
) -> Result<(bool, bool)> {
    let first = backend
        .export(auth.fleet_id(), &ReplicationInventory::default(), true, &[])
        .await?
        .exchange;
    let local_digest = first.inventory.digest.clone();
    let query = ReplicationExchange {
        envelopes: Vec::new(),
        ..first
    };
    let started = std::time::Instant::now();
    let (remote, peer_inflates) = post_signed(http, peer, node, auth, fleet, &query, false).await?;
    let round_trip = started.elapsed();
    let different = remote.inventory.digest != local_digest;
    let received = backend
        .receive(&peer.name, auth.fleet_id(), &remote, Some(round_trip))
        .await?;
    // Progress means new envelopes stored on one side or the other. A peer that keeps sending,
    // or keeps being sent, envelopes that are never stored must not keep the worker busy.
    let pulled = received.receipt.received != 0;
    let mut heal_now = received.receipt.heal;
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
        let started = std::time::Instant::now();
        // A peer that says it takes compressed requests gets a large push compressed.
        let (response, _) =
            post_signed(http, peer, node, auth, fleet, &push, peer_inflates).await?;
        let round_trip = started.elapsed();
        // The peer stores a push before it answers, so its inventory moved if the push landed.
        pushed = !push.envelopes.is_empty() && response.inventory.digest != remote.inventory.digest;
        let received = backend
            .receive(&peer.name, auth.fleet_id(), &response, Some(round_trip))
            .await?;
        pulled_follow_up = received.receipt.received != 0;
        heal_now |= received.receipt.heal;
        if received.changed {
            wake_main(main_socket).await;
        }
    }
    Ok((pulled || pulled_follow_up || pushed, heal_now))
}

/// Heal with one peer: carry each question the main daemon asks to the peer, and each answer
/// back, until the main daemon reports the heal. A peer that cannot be asked ends the heal with
/// the reason, which the main daemon reports.
async fn heal(
    backend: &PeerBackend,
    node: &str,
    peer: &PeerConfig,
    auth: &FleetAuth,
    fleet: &FleetContext,
    main_socket: &Path,
) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(HEAL_TIMEOUT)
        .build()
        .expect("the heal HTTP client configuration is valid");
    let mut query = ReplicationHealQuery::Ranges;
    for _ in 0..HEAL_QUESTION_LIMIT {
        let request = ReplicationHealRequest {
            fleet_id: auth.fleet_id().to_owned(),
            query,
        };
        let answer = match post_signed_to::<_, ReplicationHealAnswer>(
            &http, peer, node, auth, fleet, HEAL_PATH, &request, true,
        )
        .await
        {
            Ok((answer, _)) => answer,
            Err(error) => ReplicationHealAnswer::Failed {
                message: format!("{} could not answer: {error:#}", peer.name),
            },
        };
        match backend.heal_next(&peer.name, answer).await {
            Ok(ReplicationHealStep::Ask { query: next }) => query = next,
            Ok(ReplicationHealStep::Done { .. }) | Err(_) => break,
        }
    }
    wake_main(main_socket).await;
}

/// Send one signed exchange, compressed when `compress` is set and the body is large, and return
/// the peer's verified answer and whether the peer takes compressed requests.
async fn post_signed(
    http: &reqwest::Client,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    exchange: &ReplicationExchange,
    compress: bool,
) -> Result<(ReplicationExchange, bool)> {
    post_signed_to(
        http,
        peer,
        node,
        auth,
        fleet,
        EXCHANGE_PATH,
        exchange,
        compress,
    )
    .await
}

/// Send one signed request to a peer path and return the peer's verified answer, as
/// `post_signed` does for an exchange.
#[allow(clippy::too_many_arguments)]
async fn post_signed_to<B: Serialize, R: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    path: &str,
    request: &B,
    compress: bool,
) -> Result<(R, bool)> {
    let body = serde_json::to_vec(request)?;
    let request_digest = FleetAuth::body_digest(&body);
    let headers = auth.request_headers_for(path, node, &body)?;
    let endpoint = format!("{}{}", peer.url.trim_end_matches('/'), path);
    let started = std::time::Instant::now();
    let mut request = http
        .post(&endpoint)
        .headers(headers)
        .header("content-type", "application/json")
        .header("accept-encoding", EXCHANGE_ENCODING);
    request = if compress && body.len() >= DEFLATE_MIN_BYTES {
        request
            .header("content-encoding", EXCHANGE_ENCODING)
            .body(deflate(&body)?)
    } else {
        request.body(body)
    };
    let response = request
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
    // A build older than a path answers an unsigned 404.
    if status == StatusCode::NOT_FOUND && headers.get(HEADER_SIGNATURE).is_none() {
        anyhow::bail!("peer {} runs a build without `{path}`", peer.name);
    }
    let bytes = if deflated(&headers) {
        inflate(&bytes)?
    } else {
        bytes
    };
    let responder = auth.verify_sender(
        &headers,
        "RESPONSE",
        path,
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
    let response: ApiResponse<R> =
        serde_json::from_slice(&bytes).context("decode the signed peer response")?;
    anyhow::ensure!(
        response.api_version == "st3.v1",
        "the peer API version differs"
    );
    Ok((response.value, accepts_deflate(&headers)))
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
    async fn a_gateway_streams_a_remote_terminal_through_owner_long_polls() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let owner_root = tempfile::tempdir().unwrap();
        let gateway_root = tempfile::tempdir().unwrap();
        let app_state = |root: &Path, node: &str| crate::api::AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let owner = app_state(owner_root.path(), "owner-node");
        let mut gateway = app_state(gateway_root.path(), "gateway-node");
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "agent/remote-shell".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/remote-shell".into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("remote-runtime".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("remote-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("remote-shell-running".into()),
            })
            .unwrap();
        gateway
            .store
            .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
            .unwrap();

        // The owner's PTY session: a replay on PEEK, then whatever output the test writes.
        fs::create_dir_all(&owner.pty_root).unwrap();
        let sessions = tokio::net::UnixListener::bind(owner.pty_root.join("remote-runtime.sock"))
            .unwrap();
        let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
        let session_output = output.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = sessions.accept().await {
                let mut output = session_output.subscribe();
                tokio::spawn(async move {
                    let packet = |kind: u8, payload: &[u8]| {
                        let mut packet = vec![kind];
                        packet.extend((payload.len() as u32).to_be_bytes());
                        packet.extend(payload);
                        packet
                    };
                    let mut peek = [0_u8; 6];
                    if stream.read_exact(&mut peek).await.is_err()
                        || stream.write_all(&packet(10, &[0, 24, 0, 80])).await.is_err()
                        || stream.write_all(&packet(5, b"owner shell\r\n$ ")).await.is_err()
                    {
                        return;
                    }
                    while let Ok(bytes) = output.recv().await {
                        if stream.write_all(&packet(0, &bytes)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let owner_socket = owner_root.path().join("st3.sock");
        let main_socket = owner_socket.clone();
        let owner_app = crate::api::router(owner.clone());
        tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&owner_socket)),
            node: "owner-node".into(),
            auth: FleetAuth::test("fleet-test", &[7; 32]),
            fleet: FleetContext::legacy(BTreeSet::from(["gateway-node".into()])),
            main_socket: owner_socket.clone(),
            outbound_notify: watch::channel(0_u64).0,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer)).await });

        let secret = gateway_root.path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        gateway.client_relay = ClientRelay::from_config(&Config {
            node: "gateway-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "owner-node".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let gateway_socket = gateway_root.path().join("st3.sock");
        let served_socket = gateway_socket.clone();
        let gateway_app = crate::api::router(gateway);
        tokio::spawn(async move { crate::api::serve_unix(&served_socket, gateway_app).await });
        for socket in [&owner_socket, &gateway_socket] {
            for _ in 0..200 {
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let client = st3_client::Client::unix_as(&gateway_socket, "person/avery");
        let capabilities = client.capabilities().await.unwrap();
        let attachment = client
            .terminal_attach(
                "action/remote-terminal-attach",
                "remote-terminal-attach-0000001",
                st3_client::Fence {
                    snapshot_id: capabilities.snapshot.id,
                    runtime_incarnation: Some("remote-runtime:i1".into()),
                    terminal_sequence: Some(capabilities.snapshot.store_index),
                    ..st3_client::Fence::default()
                },
                st3_client::TargetParameters {
                    target_id: "terminal/agent/remote-shell".into(),
                    ..st3_client::TargetParameters::default()
                },
            )
            .await
            .unwrap()
            .value
            .terminal_attachment
            .unwrap();
        assert_eq!(attachment.owner_host_id, "host/owner-node");
        let mut stream = client
            .terminal_stream(
                &attachment.terminal_id,
                Some("remote-runtime:i1"),
                attachment.stream_capability.as_deref().unwrap(),
            )
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.value.lines[0].text, "owner shell");
        assert!(
            tokio::time::timeout(Duration::from_millis(1_500), stream.next())
                .await
                .is_err(),
            "an idle remote terminal must send nothing"
        );
        output.send(b"echo remote".to_vec()).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the owner long poll must return the change promptly")
            .unwrap()
            .unwrap();
        assert_eq!(changed.value.lines[1].text, "$ echo remote");
        assert_eq!(changed.value.runtime_incarnation, "remote-runtime:i1");
        assert_ne!(changed.value.revision, first.value.revision);
    }

    /// `st terminals attach` from a host that does not own the terminal: the attach client paints
    /// the owner's screens, and its keystrokes and size reach the owner's PTY as the person.
    #[tokio::test]
    async fn a_cli_attach_from_another_host_paints_screens_and_delivers_input() {
        use pty_core::protocol::{
            MessageType, PacketReader, decode_size, encode_geometry, encode_screen,
            encode_status_response,
        };
        use std::os::unix::io::AsRawFd as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let owner_root = tempfile::tempdir().unwrap();
        let gateway_root = tempfile::tempdir().unwrap();
        let app_state = |root: &Path, node: &str| crate::api::AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let owner = app_state(owner_root.path(), "owner-node");
        let mut gateway = app_state(gateway_root.path(), "gateway-node");
        // A live PTY session as its daemon publishes it: socket, daemon pid, and record.
        let incarnation = format!("{}:now", std::process::id());
        fs::create_dir_all(&owner.pty_root).unwrap();
        fs::write(
            owner.pty_root.join("remote-runtime.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        fs::write(
            owner.pty_root.join("remote-runtime.json"),
            r#"{"createdAt":"now"}"#,
        )
        .unwrap();
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "agent/remote-shell".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/remote-shell".into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("remote-runtime".into())),
                    ("incarnation_id".into(), Value::String(incarnation.clone())),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("remote-shell-running".into()),
            })
            .unwrap();
        gateway
            .store
            .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
            .unwrap();

        // The session is 30x100. A viewer's PEEK gets its replay and then its output; typed input
        // is echoed as output; an ATTACH gets a replay, and the RESIZE after it is recorded.
        let sessions =
            tokio::net::UnixListener::bind(owner.pty_root.join("remote-runtime.sock")).unwrap();
        let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
        let typed = Arc::new(Mutex::new(Vec::<u8>::new()));
        let resized = Arc::new(Mutex::new(Vec::<(u16, u16)>::new()));
        let (session_output, session_typed, session_resized) =
            (output.clone(), typed.clone(), resized.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = sessions.accept().await {
                let mut output = session_output.subscribe();
                let (typed, resized) = (session_typed.clone(), session_resized.clone());
                let echo = session_output.clone();
                tokio::spawn(async move {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut packets = PacketReader::new();
                    let mut buffer = vec![0_u8; 4096];
                    loop {
                        tokio::select! {
                            read = reader.read(&mut buffer) => {
                                let Ok(count @ 1..) = read else { return };
                                for packet in packets.feed(&buffer[..count]).unwrap() {
                                    let reply = match packet.type_ {
                                        MessageType::Peek => [
                                            encode_geometry(30, 100),
                                            encode_screen(b"owner shell\r\n$ "),
                                        ]
                                        .concat(),
                                        MessageType::Attach => encode_screen(b"owner shell\r\n$ "),
                                        MessageType::Status => encode_status_response("{}"),
                                        MessageType::Resize => {
                                            resized.lock().unwrap().push(decode_size(&packet.payload));
                                            continue;
                                        }
                                        MessageType::Data => {
                                            typed.lock().unwrap().extend(&packet.payload);
                                            let _ = echo.send(packet.payload);
                                            continue;
                                        }
                                        _ => continue,
                                    };
                                    if writer.write_all(&reply).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            bytes = output.recv() => {
                                let Ok(bytes) = bytes else { return };
                                let data = pty_core::protocol::encode_data(&bytes);
                                if writer.write_all(&data).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });

        let owner_socket = owner_root.path().join("st3.sock");
        let main_socket = owner_socket.clone();
        let owner_app = crate::api::router(owner.clone());
        tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&owner_socket)),
            node: "owner-node".into(),
            auth: FleetAuth::test("fleet-test", &[7; 32]),
            fleet: FleetContext::legacy(BTreeSet::from(["gateway-node".into()])),
            main_socket: owner_socket.clone(),
            outbound_notify: watch::channel(0_u64).0,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer)).await });
        let secret = gateway_root.path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        gateway.client_relay = ClientRelay::from_config(&Config {
            node: "gateway-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "owner-node".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let gateway_socket = gateway_root.path().join("st3.sock");
        let served_socket = gateway_socket.clone();
        let gateway_app = crate::api::router(gateway);
        tokio::spawn(async move { crate::api::serve_unix(&served_socket, gateway_app).await });
        for socket in [&owner_socket, &gateway_socket] {
            for _ in 0..200 {
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        // The attach client's terminal: stdin and stdout are sockets the test holds the far ends
        // of. Neither is a TTY, so the client asks for its default 24x80.
        let (mut keyboard, stdin) = tokio::net::UnixStream::pair().unwrap();
        let (stdout, mut display) = tokio::net::UnixStream::pair().unwrap();
        let (stderr, _errors) = std::os::unix::net::UnixStream::pair().unwrap();
        let (stdin, stdout) = (stdin.into_std().unwrap(), stdout.into_std().unwrap());
        stdin.set_nonblocking(false).unwrap();
        stdout.set_nonblocking(false).unwrap();
        let io = pty_client::ClientIo {
            stdin: stdin.as_raw_fd(),
            stdout: stdout.as_raw_fd(),
            stderr: stderr.as_raw_fd(),
        };
        let client = st3_client::Client::unix_as(&gateway_socket, "person/avery");
        let attach = tokio::spawn(async move {
            let code = crate::remote_terminal::attach_with_io(
                &client,
                "agent/remote-shell",
                "agent/remote-shell",
                io,
            )
            .await;
            drop((stdin, stdout, stderr));
            code
        });
        let mut shown = String::new();
        let mut wait_for = async |text: &str| {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            let mut buffer = vec![0_u8; 65_536];
            while !shown.contains(text) {
                let count = tokio::time::timeout_at(deadline, display.read(&mut buffer))
                    .await
                    .unwrap_or_else(|_| panic!("`{text}` was never painted; painted {shown:?}"))
                    .unwrap();
                assert!(count > 0, "the display closed before `{text}`");
                shown.push_str(&String::from_utf8_lossy(&buffer[..count]));
            }
        };
        wait_for("owner shell").await;

        keyboard.write_all(b"echo remote\r").await.unwrap();
        wait_for("echo remote").await;
        assert_eq!(typed.lock().unwrap().as_slice(), b"echo remote\r");
        assert_eq!(resized.lock().unwrap().as_slice(), &[(24, 80)]);
        let requested = owner
            .store
            .claims_for("agent/remote-shell", Some("terminal.input.requested"))
            .unwrap();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].actor.as_deref(), Some("person/avery"));

        // Ctrl+\ detaches once its double-tap window passes.
        keyboard.write_all(&[0x1c]).await.unwrap();
        let code = tokio::time::timeout(Duration::from_secs(10), attach)
            .await
            .expect("the attach ends after a detach")
            .unwrap()
            .unwrap();
        assert_eq!(code, 0);
        assert_eq!(typed.lock().unwrap().as_slice(), b"echo remote\r");
    }

    /// `st agents new --host OTHER` without a workspace: the host that runs the agent names the
    /// directory below its own home, asked as a person through the fleet relay.
    #[tokio::test]
    async fn the_owning_host_names_a_new_agents_default_workspace() {
        let owner_root = tempfile::tempdir().unwrap();
        let gateway_root = tempfile::tempdir().unwrap();
        let app_state = |root: &Path, node: &str| crate::api::AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let owner = app_state(owner_root.path(), "owner-node");
        let mut gateway = app_state(gateway_root.path(), "gateway-node");
        let owner_socket = owner_root.path().join("st3.sock");
        let main_socket = owner_socket.clone();
        let owner_app = crate::api::router(owner);
        tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&owner_socket)),
            node: "owner-node".into(),
            auth: FleetAuth::test("fleet-test", &[7; 32]),
            fleet: FleetContext::legacy(BTreeSet::from(["gateway-node".into()])),
            main_socket: owner_socket.clone(),
            outbound_notify: watch::channel(0_u64).0,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer)).await });
        let secret = gateway_root.path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        gateway.client_relay = ClientRelay::from_config(&Config {
            node: "gateway-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "owner-node".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let gateway_app = crate::api::router(gateway);
        let expected = crate::config::default_agent_workspace("site")
            .unwrap()
            .display()
            .to_string();
        assert!(expected.ends_with("/st/agents/site"));
        let ask = |host: &str, person: Option<&str>, identity: &str| {
            let mut request = Request::get(format!(
                "/v1/hosts/{host}/agent-workspace?identity={identity}"
            ));
            if let Some(person) = person {
                request = request.header("x-st3-person", person);
            }
            let app = gateway_app.clone();
            let request = request.body(Body::empty()).unwrap();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&body).unwrap())
            }
        };

        let (status, body) = ask("owner-node", Some("person/avery"), "site").await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["value"]["workspace"], expected);
        assert_eq!(body["value"]["host_id"], "host/owner-node");
        let (status, body) = ask("gateway-node", None, "site").await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["value"]["workspace"], expected);

        let (status, body) = ask("owner-node", None, "site").await;
        assert!(!status.is_success());
        assert_eq!(body["code"], "missing-person", "{body}");
        let (status, body) = ask("owner-node", Some("person/avery"), "..%2Fescape").await;
        assert!(!status.is_success());
        assert_eq!(body["code"], "validation-failed", "{body}");
        assert!(body["message"].as_str().unwrap().contains("../escape"));
        let (status, body) = ask("elsewhere", Some("person/avery"), "site").await;
        assert!(!status.is_success());
        assert_eq!(body["code"], "remote-unavailable", "{body}");
    }

    #[tokio::test]
    async fn a_read_reaches_an_owner_through_a_peer_that_can_dial_it() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // A chain of isolated daemons: the gateway dials only the relay, and only the relay
        // dials the owner. The gateway knows the relay reaches the owner from the relay's
        // replicated transport observation.
        let roots = [(); 3].map(|()| tempfile::tempdir().unwrap());
        let make_state = |root: &Path, node: &str| crate::api::AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let mut gateway = make_state(roots[0].path(), "chain-gateway");
        let mut relay = make_state(roots[1].path(), "chain-relay");
        let owner = make_state(roots[2].path(), "chain-owner");
        let agent = "agent/chain-worker";
        let incarnation = "chain-runtime:i1";
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("chain-runtime".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("chain-worker-running".into()),
            })
            .unwrap();
        relay
            .store
            .record_transport_observation("chain-owner", "up", None, None)
            .unwrap();
        gateway
            .store
            .import_replication("chain-owner", &owner.store.export_replication(0).unwrap())
            .unwrap();
        gateway
            .store
            .import_replication("chain-relay", &relay.store.export_replication(0).unwrap())
            .unwrap();
        // The transcript line is written after replication, so only the owner can answer it.
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "harness.timeline".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("operation".into(), serde_json::json!("append")),
                    ("entry_id".into(), serde_json::json!("timeline-entry/chain-answer")),
                    ("revision".into(), serde_json::json!(1)),
                    ("role".into(), serde_json::json!("assistant")),
                    ("entry_type".into(), serde_json::json!("content")),
                    ("final".into(), serde_json::json!(true)),
                    (
                        "body".into(),
                        serde_json::json!({"media_type":"text/plain","text":"answered two hops away"}),
                    ),
                    ("driver".into(), serde_json::json!("codex")),
                    ("incarnation_id".into(), serde_json::json!(incarnation)),
                    ("sequence".into(), serde_json::json!(1)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("chain-answer".into()),
            })
            .unwrap();

        // The owner's PTY session: a replay on PEEK, then whatever output the test writes.
        fs::create_dir_all(&owner.pty_root).unwrap();
        let sessions =
            tokio::net::UnixListener::bind(owner.pty_root.join("chain-runtime.sock")).unwrap();
        let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
        let session_output = output.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = sessions.accept().await {
                let mut output = session_output.subscribe();
                tokio::spawn(async move {
                    let packet = |kind: u8, payload: &[u8]| {
                        let mut packet = vec![kind];
                        packet.extend((payload.len() as u32).to_be_bytes());
                        packet.extend(payload);
                        packet
                    };
                    let mut peek = [0_u8; 6];
                    if stream.read_exact(&mut peek).await.is_err()
                        || stream
                            .write_all(&packet(10, &[0, 24, 0, 80]))
                            .await
                            .is_err()
                        || stream
                            .write_all(&packet(5, b"far shell\r\n$ "))
                            .await
                            .is_err()
                    {
                        return;
                    }
                    while let Ok(bytes) = output.recv().await {
                        if stream.write_all(&packet(0, &bytes)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let secret = roots[0].path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let relay_config = |node: &str, peer: &str, address: SocketAddr| {
            ClientRelay::from_config(&Config {
                node: node.into(),
                fleet_id: Some("fleet-test".into()),
                shared_secret_file: Some(secret.clone()),
                peers: vec![PeerConfig {
                    name: peer.into(),
                    url: format!("http://{address}"),
                }],
                ..Default::default()
            })
            .unwrap()
            .unwrap()
        };
        // Each node: its daemon on a Unix socket, and a replication worker that accepts the
        // node before it in the chain.
        let serve_worker = |node: &str, accepts: &str, socket: &Path| {
            let peer = PeerState {
                backend: PeerBackend::Main(Client::unix(socket)),
                node: node.into(),
                auth: FleetAuth::test("fleet-test", &[7; 32]),
                fleet: FleetContext::legacy(BTreeSet::from([accepts.into()])),
                main_socket: socket.to_path_buf(),
                outbound_notify: watch::channel(0_u64).0,
            };
            async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, peer_router(peer)).await });
                address
            }
        };
        let sockets = roots.each_ref().map(|root| root.path().join("st3.sock"));
        let owner_worker = serve_worker("chain-owner", "chain-relay", &sockets[2]).await;
        let relay_worker = serve_worker("chain-relay", "chain-gateway", &sockets[1]).await;
        relay.client_relay = Some(relay_config("chain-relay", "chain-owner", owner_worker));
        gateway.client_relay = Some(
            relay_config("chain-gateway", "chain-relay", relay_worker)
                .with_links(gateway.store.clone()),
        );
        for (socket, state) in sockets.iter().zip([gateway, relay, owner.clone()]) {
            let socket = socket.clone();
            tokio::spawn(async move {
                crate::api::serve_unix(&socket, crate::api::router(state)).await
            });
        }
        for socket in &sockets {
            for _ in 0..200 {
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let client = st3_client::Client::unix_as(&sockets[0], "person/avery");
        let session_id = format!(
            "session/{}",
            &hex::encode(sha2::Sha256::digest(
                format!("{agent}:{incarnation}").as_bytes()
            ))[..24]
        );
        let timeline = client
            .timeline(&session_id, None, Some(20))
            .await
            .unwrap()
            .value;
        assert!(
            timeline.items.iter().any(
                |entry| serde_json::to_value(&entry.body).unwrap()["body"]["text"]
                    == "answered two hops away"
            ),
            "the owner's transcript must reach the gateway through the relay"
        );

        let capabilities = client.capabilities().await.unwrap();
        let attachment = client
            .terminal_attach(
                "action/chain-terminal-attach",
                "chain-terminal-attach-00000001",
                st3_client::Fence {
                    snapshot_id: capabilities.snapshot.id,
                    runtime_incarnation: Some(incarnation.into()),
                    terminal_sequence: Some(capabilities.snapshot.store_index),
                    ..st3_client::Fence::default()
                },
                st3_client::TargetParameters {
                    target_id: format!("terminal/{agent}"),
                    ..st3_client::TargetParameters::default()
                },
            )
            .await
            .unwrap()
            .value
            .terminal_attachment
            .unwrap();
        assert_eq!(attachment.owner_host_id, "host/chain-owner");
        let mut stream = client
            .terminal_stream(
                &attachment.terminal_id,
                Some(incarnation),
                attachment.stream_capability.as_deref().unwrap(),
            )
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.value.lines[0].text, "far shell");
        output.send(b"echo far".to_vec()).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the relayed long poll must return the change promptly")
            .unwrap()
            .unwrap();
        assert_eq!(changed.value.lines[1].text, "$ echo far");
    }

    #[tokio::test]
    async fn a_relaying_worker_refuses_a_forged_path_and_a_spent_hop_limit() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let main = crate::api::AppState {
            store: Arc::new(Store::open_memory("middle").unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "middle".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let secret = root.path().join("fleet-secret");
        fs::write(&secret, [5_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let mut main = main;
        main.client_relay = ClientRelay::from_config(&Config {
            node: "middle".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "far".into(),
                url: "http://127.0.0.1:9".into(),
            }],
            ..Default::default()
        })
        .unwrap();
        let served = socket.clone();
        let server =
            tokio::spawn(
                async move { crate::api::serve_unix(&served, crate::api::router(main)).await },
            );
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let auth = FleetAuth::test("fleet-test", &[5; 32]);
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&socket)),
            node: "middle".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["near".into()])),
            main_socket: socket,
            outbound_notify: watch::channel(0_u64).0,
        };
        let send = |route: ClientReadRoute| {
            let body = serde_json::to_vec(&ClientReadRequest {
                authority_actor: "person/test".into(),
                request: ClientReadOperation::TerminalScreen {
                    terminal_id: "terminal/agent/far".into(),
                },
                relay: Some(route),
            })
            .unwrap();
            let mut request = Request::builder()
                .method("POST")
                .uri(CLIENT_READ_PATH)
                .body(Body::from(body.clone()))
                .unwrap();
            *request.headers_mut() = auth
                .request_headers_for(CLIENT_READ_PATH, "near", &body)
                .unwrap();
            let router = peer_router(peer.clone());
            async move {
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES)
                    .await
                    .unwrap();
                let value: ApiResponse<Value> = serde_json::from_slice(&bytes).unwrap();
                (status, value.value)
            }
        };
        // A path that does not end at the member that sent it is someone else's claim.
        let (status, _) = send(ClientReadRoute {
            target: "host/far".into(),
            path: vec!["elsewhere".into()],
            hops_left: 2,
        })
        .await;
        assert!(!status.is_success());
        // A read that has used its hops, or has passed this node before, goes no further.
        for route in [
            ClientReadRoute {
                target: "host/far".into(),
                path: vec!["near".into()],
                hops_left: 0,
            },
            ClientReadRoute {
                target: "host/far".into(),
                path: vec!["middle".into(), "near".into()],
                hops_left: 2,
            },
        ] {
            let (status, value) = send(route).await;
            assert_eq!(status, StatusCode::LOOP_DETECTED);
            assert_eq!(value["code"], "remote-unavailable");
        }
        // A read it can carry on reaches for the next hop; here nothing listens there.
        let (status, value) = send(ClientReadRoute {
            target: "host/far".into(),
            path: vec!["near".into()],
            hops_left: 2,
        })
        .await;
        assert!(!status.is_success());
        assert_eq!(value["code"], "remote-unavailable");
        server.abort();
    }

    #[test]
    fn next_hops_prefer_the_owner_then_the_nearest_peer_that_reaches_it() {
        let links = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect::<Vec<_>>()
        };
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        // A laptop that dials only a desktop reaches a server the desktop observes, in either
        // direction the observation was made.
        assert_eq!(
            client_read_next_hops(
                "laptop",
                "server",
                &names(&["desktop"]),
                &names(&["laptop"]),
                &links(&[("desktop", "server"), ("laptop", "desktop")])
            ),
            names(&["desktop"])
        );
        assert_eq!(
            client_read_next_hops(
                "laptop",
                "server",
                &names(&["desktop"]),
                &names(&["laptop"]),
                &links(&[("server", "desktop")])
            ),
            names(&["desktop"])
        );
        // The owner itself first, then peers nearest to it; a peer with no path is left out.
        assert_eq!(
            client_read_next_hops(
                "a",
                "d",
                &names(&["d", "c", "b", "island"]),
                &names(&["a"]),
                &links(&[("b", "d"), ("c", "b"), ("island", "elsewhere")])
            ),
            names(&["d", "b", "c"])
        );
        // A path through a node the read already passed is no path at all.
        assert_eq!(
            client_read_next_hops(
                "b",
                "d",
                &names(&["a", "c"]),
                &names(&["a", "b"]),
                &links(&[("a", "d"), ("c", "a")])
            ),
            Vec::<String>::new()
        );
        // With no observation of the owner, every peer not yet visited is worth a try.
        assert_eq!(
            client_read_next_hops(
                "a",
                "unseen",
                &names(&["c", "b", "a"]),
                &names(&["a", "b"]),
                &links(&[("a", "c")])
            ),
            names(&["c"])
        );
    }

    #[tokio::test]
    async fn a_gateway_receives_remote_conversation_changes_without_idle_data() {
        let owner_root = tempfile::tempdir().unwrap();
        let gateway_root = tempfile::tempdir().unwrap();
        let make_state = |root: &Path, node: &str| crate::api::AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        let owner = make_state(owner_root.path(), "conversation-owner");
        let mut gateway = make_state(gateway_root.path(), "conversation-gateway");
        let agent = "agent/conversation-peer";
        let incarnation = "conversation-runtime:i1";
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        serde_json::json!("conversation-runtime"),
                    ),
                    ("incarnation_id".into(), serde_json::json!(incarnation)),
                    ("status".into(), serde_json::json!("running")),
                    ("terminal".into(), serde_json::json!(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-peer-runtime".into()),
            })
            .unwrap();
        gateway
            .store
            .import_replication(
                "conversation-owner",
                &owner.store.export_replication(0).unwrap(),
            )
            .unwrap();
        let session_id = format!(
            "session/{}",
            &hex::encode(sha2::Sha256::digest(
                format!("{agent}:{incarnation}").as_bytes()
            ))[..24]
        );
        let owner_socket = owner_root.path().join("st3.sock");
        let served_owner = owner_socket.clone();
        let owner_app = crate::api::router(owner.clone());
        tokio::spawn(async move { crate::api::serve_unix(&served_owner, owner_app).await });
        let peer = PeerState {
            backend: PeerBackend::Main(Client::unix(&owner_socket)),
            node: "conversation-owner".into(),
            auth: FleetAuth::test("fleet-test", &[7; 32]),
            fleet: FleetContext::legacy(BTreeSet::from(["conversation-gateway".into()])),
            main_socket: owner_socket.clone(),
            outbound_notify: watch::channel(0_u64).0,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer)).await });
        let secret = gateway_root.path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        gateway.client_relay = ClientRelay::from_config(&Config {
            node: "conversation-gateway".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "conversation-owner".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let gateway_socket = gateway_root.path().join("st3.sock");
        let served_gateway = gateway_socket.clone();
        tokio::spawn(async move {
            crate::api::serve_unix(&served_gateway, crate::api::router(gateway)).await
        });
        for socket in [&owner_socket, &gateway_socket] {
            for _ in 0..200 {
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let client = st3_client::Client::unix_as(&gateway_socket, "person/example");
        let baseline = client
            .conversation_changes(&session_id, None, 0)
            .await
            .unwrap()
            .value;
        assert!(baseline.items.is_empty());
        let cursor = baseline.next_cursor;
        let mut stream = client.conversation_stream(&session_id, None).await.unwrap();
        let opened = stream.next().await.unwrap().unwrap();
        assert!(opened.value.items.is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err()
        );
        let idle = client
            .conversation_changes(&session_id, Some(&cursor), 100)
            .await
            .unwrap()
            .value;
        assert!(idle.items.is_empty());
        let waiting_client = client.clone();
        let waiting_id = session_id.clone();
        let waiting_cursor = cursor.clone();
        let waiting = tokio::spawn(async move {
            waiting_client
                .conversation_changes(&waiting_id, Some(&waiting_cursor), 1000)
                .await
                .unwrap()
                .value
        });
        tokio::task::yield_now().await;
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "message/conversation-peer-first".into(),
                kind: "message.sent".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::from([
                    ("from".into(), serde_json::json!("person/example")),
                    ("to".into(), serde_json::json!(agent)),
                    ("session_id".into(), serde_json::json!(session_id)),
                    ("content".into(), serde_json::json!("hello")),
                    ("status".into(), serde_json::json!("sent")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-peer-message".into()),
            })
            .unwrap();
        owner.event_notify.send_modify(|value| *value += 1);
        let first = tokio::time::timeout(Duration::from_millis(900), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.items.len(), 2);
        let resume = first.next_cursor;
        let streamed = tokio::time::timeout(Duration::from_millis(900), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(streamed.value.items.len(), 2);
        let stream_cursor = streamed.value.next_cursor;
        stream.close().await;
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "harness.timeline".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("operation".into(), serde_json::json!("append")),
                    (
                        "entry_id".into(),
                        serde_json::json!("timeline-entry/conversation-peer-reply"),
                    ),
                    ("revision".into(), serde_json::json!(1)),
                    ("role".into(), serde_json::json!("assistant")),
                    ("entry_type".into(), serde_json::json!("content")),
                    ("final".into(), serde_json::json!(true)),
                    (
                        "body".into(),
                        serde_json::json!({"media_type":"text/plain","text":"reply"}),
                    ),
                    ("driver".into(), serde_json::json!("codex")),
                    ("incarnation_id".into(), serde_json::json!(incarnation)),
                    ("sequence".into(), serde_json::json!(1)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-peer-reply".into()),
            })
            .unwrap();
        let replay = client
            .conversation_changes(&session_id, Some(&resume), 0)
            .await
            .unwrap()
            .value;
        assert_eq!(replay.items.len(), 1);
        let mut reconnected = client
            .conversation_stream(&session_id, Some(&stream_cursor))
            .await
            .unwrap();
        let resumed = reconnected.next().await.unwrap().unwrap();
        assert_eq!(resumed.value.items.len(), 1);
        reconnected.close().await;
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
            relay: None,
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

    #[test]
    fn exchange_bodies_deflate_and_refuse_a_body_that_inflates_too_far() {
        let body = serde_json::to_vec(&serde_json::json!({"envelopes": vec!["same"; 1_000]})).unwrap();
        let compressed = deflate(&body).unwrap();
        assert!(compressed.len() * 10 < body.len());
        assert_eq!(inflate(&compressed).unwrap(), body);

        let bomb = deflate(&vec![0_u8; MAX_EXCHANGE_BYTES + 1]).unwrap();
        assert!(bomb.len() < 1024 * 1024);
        assert!(inflate(&bomb).is_err());
        assert!(inflate(b"not deflate").is_err());

        let mut headers = HeaderMap::new();
        assert!(!accepts_deflate(&headers));
        headers.insert("accept-encoding", HeaderValue::from_static("gzip, Deflate;q=0.5"));
        assert!(accepts_deflate(&headers));
        headers.insert("accept-encoding", HeaderValue::from_static("gzip, deflated"));
        assert!(!accepts_deflate(&headers));
    }

    #[tokio::test]
    async fn the_peer_route_deflates_only_for_a_requester_that_asks() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[5; 32]);
        // Enough envelopes that the answer to an empty inventory is worth compressing.
        let target = Arc::new(Store::open_memory("target").unwrap());
        target.bind_fleet(fleet).unwrap();
        for index in 0..300 {
            target
                .append_claim(&ClaimInput {
                    subject: format!("host/peer-{index}"),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let state = PeerState {
            backend: PeerBackend::Local(target),
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
        let body = serde_json::to_vec(&exchange).unwrap();
        let request_digest = FleetAuth::body_digest(&body);
        // An older build sends plain JSON and does not ask for compression; a new one sends a
        // compressed request once the peer has answered compressed, and always asks.
        for (compress, ask) in [(false, false), (false, true), (true, true)] {
            let mut request = Request::builder()
                .method("POST")
                .uri(EXCHANGE_PATH)
                .body(Body::from(if compress {
                    deflate(&body).unwrap()
                } else {
                    body.clone()
                }))
                .unwrap();
            request
                .headers_mut()
                .extend(auth.request_headers("source", &body).unwrap());
            if compress {
                request.headers_mut().insert(
                    "content-encoding",
                    HeaderValue::from_static(EXCHANGE_ENCODING),
                );
            }
            if ask {
                request.headers_mut().insert(
                    "accept-encoding",
                    HeaderValue::from_static(EXCHANGE_ENCODING),
                );
            }
            let response = peer_router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers().clone();
            assert!(accepts_deflate(&headers), "a new build takes compressed requests");
            assert_eq!(deflated(&headers), ask);
            let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let response_body = if ask {
                inflate(&response_body).unwrap()
            } else {
                response_body.to_vec()
            };
            assert!(response_body.len() >= DEFLATE_MIN_BYTES);
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
    }

    #[tokio::test]
    async fn a_peer_serves_a_checkpoint_manifest_to_its_fleet_only() {
        use crate::store::{ClaimTombstone, EnvelopeTombstone, checkpoint_cut};
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[6; 32]);
        let target = Arc::new(Store::open_memory("target").unwrap());
        target.bind_fleet(fleet).unwrap();
        let checkpoint = "checkpoint/2026-09-27";
        let cut = checkpoint_cut(checkpoint).unwrap();
        let envelopes = (1..=3)
            .map(|sequence| EnvelopeTombstone {
                writer: "source".into(),
                sequence,
                envelope_hash: format!("{sequence:064x}"),
                accepted_at_unix_ms: cut - 1_000 + u128::from(sequence),
            })
            .collect::<Vec<_>>();
        let claims = envelopes
            .iter()
            .map(|envelope| ClaimTombstone {
                id: format!("claim-{}", envelope.sequence),
                writer: envelope.writer.clone(),
                sequence: envelope.sequence,
                envelope_hash: envelope.envelope_hash.clone(),
                subject: "daemon/source".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                predecessors: Vec::new(),
                operation_id: None,
                request_digest: None,
                accepted_at_unix_ms: envelope.accepted_at_unix_ms,
            })
            .collect::<Vec<_>>();
        target
            .apply_checkpoint_drop(checkpoint, &envelopes, &claims)
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
        let server = tokio::spawn(axum::serve(listener, peer_router(state)).into_future());
        let peer = PeerConfig {
            name: "target".into(),
            url: format!("http://{address}"),
        };
        let http = replication_http_client();
        let dialer = FleetContext::legacy(BTreeSet::from(["target".into()]));
        let manifest =
            fetch_checkpoint_manifest(&http, &peer, "source", &auth, &dialer, checkpoint, cut)
                .await
                .unwrap();
        assert_eq!(
            manifest,
            target.checkpoint_manifest(checkpoint, cut).unwrap()
        );
        assert_eq!(manifest.envelopes, envelopes);
        assert_eq!(manifest.claims, claims);
        crate::store::verify_checkpoint_manifest(
            &manifest,
            &crate::store::drop_digest(&envelopes, &claims),
        )
        .unwrap();

        // A node that is not the target's peer, or holds another fleet secret, gets nothing.
        let stranger =
            fetch_checkpoint_manifest(&http, &peer, "stranger", &auth, &dialer, checkpoint, cut)
                .await;
        assert!(stranger.is_err());
        let other_fleet = FleetAuth::test(fleet, &[7; 32]);
        let forged = fetch_checkpoint_manifest(
            &http,
            &peer,
            "source",
            &other_fleet,
            &dialer,
            checkpoint,
            cut,
        )
        .await;
        assert!(forged.is_err());
        server.abort();
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
        let pushed = exchange(
            &http,
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap()
        .0;
        assert!(pushed, "the peer stored what this node pushed");
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
        let pulled = exchange(
            &http,
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap()
        .0;
        assert!(pulled, "this node stored what the peer sent");
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
        let moved = exchange(
            &replication_http_client(),
            &PeerBackend::Local(source.clone()),
            "source",
            &peer,
            &auth,
            &FleetContext::legacy(BTreeSet::from(["target".into()])),
            Path::new("/no/such/socket"),
        )
        .await
        .unwrap()
        .0;
        assert!(!moved, "converged nodes store nothing, so the worker may rest");
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

    /// A first sync that ends with different graphs heals at once over the signed heal route:
    /// the node that lost claims asks the peer that holds them and admits their envelopes again.
    #[tokio::test]
    async fn a_first_sync_heals_a_node_that_lost_claims_over_the_signed_route() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[6; 32]);
        let root = tempfile::tempdir().unwrap();
        let target_path = root.path().join("target.sqlite3");
        let source = Arc::new(Store::open_memory("source").unwrap());
        let target = Arc::new(Store::open(&target_path, "target").unwrap());
        source.bind_fleet(fleet).unwrap();
        target.bind_fleet(fleet).unwrap();
        target.begin_first_sync("source").unwrap();
        let intent = crate::graph::parse_test_intent(
            "version 2\n exec \"work\" { command \"true\"; restart \"never\" } ",
            "source",
        )
        .unwrap();
        let preview = source
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: "work".into(),
                    source_name: None,
                },
            )
            .unwrap();
        source
            .apply(&intent, &preview.subject_tokens, "work")
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = PeerState {
            backend: PeerBackend::Local(source.clone()),
            node: "source".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["target".into()])),
            main_socket: PathBuf::from("/no/such/socket"),
            outbound_notify: watch::channel(0_u64).0,
        };
        let server = tokio::spawn(async move { axum::serve(listener, peer_router(state)).await });
        let peer = PeerConfig {
            name: "source".into(),
            url: format!("http://{address}"),
        };
        let backend = PeerBackend::Local(target.clone());
        let context = FleetContext::legacy(BTreeSet::from(["source".into()]));
        let socket = Path::new("/no/such/socket");
        let http = replication_http_client();
        // The target stores the source's envelopes, which compares nothing yet.
        target
            .receive_replication_exchange(
                "source",
                fleet,
                &source
                    .export_replication_exchange(fleet, &target.replication_inventory().unwrap())
                    .unwrap(),
            )
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
        assert_eq!(target.first_sync().unwrap().unwrap().state, "syncing");

        // The target loses the desired claim but keeps its envelope, before its first sync ends.
        {
            let connection = rusqlite::Connection::open(&target_path).unwrap();
            connection
                .execute_batch(
                    "PRAGMA foreign_keys=OFF;
                     DELETE FROM claims WHERE kind='intent.desired';",
                )
                .unwrap();
        }
        target.replay_replication_graph().unwrap();
        let graph = |store: &Store| {
            store
                .replication_status(true, Some(fleet), &[])
                .unwrap()
                .graph_digest
        };
        assert_ne!(graph(&target), graph(&source));

        // Transport observations move for an exchange or two before the envelopes match.
        let mut heal_now = false;
        for _ in 0..4 {
            heal_now = exchange(&http, &backend, "target", &peer, &auth, &context, socket)
                .await
                .unwrap()
                .1;
            if heal_now {
                break;
            }
        }
        assert!(
            heal_now,
            "the first comparison of a first sync heals at once"
        );
        heal(&backend, "target", &peer, &auth, &context, socket).await;

        let status = target
            .replication_status(true, Some(fleet), &["source".into()])
            .unwrap();
        let report = status.peers[0].sync.as_ref().unwrap().heal.clone().unwrap();
        assert!(report.healed, "{report:?}");
        assert_eq!(graph(&target), graph(&source));
        assert_eq!((report.refetched, report.pushed), (1, 0));
        let first = status.first_sync.unwrap();
        assert_eq!((first.state.as_str(), first.healed), ("verified", true));
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
        let received = backend
            .receive("source", fleet, &exchange, None)
            .await
            .unwrap();
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
        let still_up = store
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap();
        assert_eq!(still_up.body["fields"]["status"], "up");
        store.age_replication_peer_for_test("source");
        let before = store
            .claims_for("host/source", Some("transport.observed"))
            .unwrap()
            .len();
        for _ in 0..1_440 {
            backend
                .record_failure("source", "down", "test outage")
                .await
                .unwrap();
        }
        let down = store
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap();
        assert_eq!(down.body["fields"]["status"], "down");
        assert_eq!(
            store
                .claims_for("host/source", Some("transport.observed"))
                .unwrap()
                .len(),
            before + 1
        );
        backend
            .receive("source", fleet, &exchange, None)
            .await
            .unwrap();
        let recovered = store
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.body["fields"]["status"], "up");
        server.abort();
    }

    #[tokio::test]
    async fn inbound_only_peer_syncs_both_ways_between_isolated_daemons() {
        let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
        let auth = FleetAuth::test(fleet, &[8; 32]);
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let mut servers = Vec::new();
        let mut stores = Vec::new();
        let mut sockets = Vec::new();
        for (name, root) in [("source", source_dir.path()), ("target", target_dir.path())] {
            let store = Arc::new(Store::open(&root.join("graph.db"), name).unwrap());
            store.bind_fleet(fleet).unwrap();
            let socket = root.join("main.sock");
            let app = crate::api::router(crate::api::AppState {
                store: store.clone(),
                notify: Arc::new(tokio::sync::Notify::new()),
                event_notify: watch::channel(0_u64).0,
                node: name.into(),
                state_dir: root.into(),
                pty_root: root.join("pty"),
                pty_binary: PathBuf::from("pty"),
                fleet_id: Some(fleet.into()),
                configured_peers: vec![if name == "source" { "target" } else { "source" }.into()],
                client_relay: None,
                native_session_home: None,
                planner_default: crate::model::PlannerSpec::default(),
            });
            let served = socket.clone();
            servers.push(tokio::spawn(async move {
                crate::api::serve_unix(&served, app).await
            }));
            stores.push(store);
            sockets.push(socket);
        }
        for socket in &sockets {
            for _ in 0..100 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(socket.exists());
        }
        let source = &stores[0];
        let target = &stores[1];
        source
            .record_transport_observation("target", "up", None, None)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let inbound = PeerConfig {
            name: "source".into(),
            url: String::new(),
        };
        assert!(
            dial_targets(
                &FleetView::default(),
                "target",
                &[inbound],
                LocalTransports::default()
            )
            .is_empty()
        );
        let state = PeerState {
            backend: PeerBackend::Main(Client::unix(&sockets[1])),
            node: "target".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            main_socket: sockets[1].clone(),
            outbound_notify: watch::channel(0_u64).0,
        };
        let peer_server =
            tokio::spawn(async move { axum::serve(listener, peer_router(state)).await });
        let peer = PeerConfig {
            name: "target".into(),
            url: format!("http://{address}"),
        };
        let backend = PeerBackend::Main(Client::unix(&sockets[0]));
        let context = FleetContext::legacy(BTreeSet::from(["target".into()]));
        let http = replication_http_client();
        exchange(
            &http,
            &backend,
            "source",
            &peer,
            &auth,
            &context,
            &sockets[0],
        )
        .await
        .unwrap();
        assert!(
            target
                .latest_claim("host/target", Some("transport.observed"))
                .unwrap()
                .is_some()
        );
        target
            .record_transport_observation("source", "up", None, None)
            .unwrap();
        exchange(
            &http,
            &backend,
            "source",
            &peer,
            &auth,
            &context,
            &sockets[0],
        )
        .await
        .unwrap();
        assert!(
            source
                .latest_claim("host/source", Some("transport.observed"))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            source
                .replication_status(true, Some(fleet), &[])
                .unwrap()
                .authority_digest,
            target
                .replication_status(true, Some(fleet), &[])
                .unwrap()
                .authority_digest
        );
        peer_server.abort();
        for server in servers {
            server.abort();
        }
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

    #[test]
    fn the_dial_set_comes_from_membership_and_skips_dial_out_members() {
        let member =
            |name: &str, state: &str, mode: &str, endpoints: Vec<Value>| crate::fleet::MemberView {
                name: name.into(),
                member_key: format!("{name}-key"),
                state: state.into(),
                mode: mode.into(),
                endpoints,
                start: 1,
                end: None,
                ended: None,
            };
        let loopback = |port: u16| {
            vec![
                serde_json::json!({"transport": "loopback", "address": format!("127.0.0.1:{port}")}),
            ]
        };
        let view = FleetView {
            anchor: Some("a-key".into()),
            members: vec![
                member("a", "current", "listening", loopback(1)),
                member("server", "current", "listening", loopback(2)),
                member("laptop", "current", "dial-out", Vec::new()),
                member("gone", "ended", "listening", loopback(3)),
                member("quiet", "current", "listening", Vec::new()),
            ],
            legacy_removed: vec!["old".into()],
        };
        let config = |name: &str, port: u16| PeerConfig {
            name: name.into(),
            url: format!("http://127.0.0.1:{port}"),
        };
        let targets = dial_targets(
            &view,
            "a",
            &[
                config("server", 9002),
                config("laptop", 9003),
                config("gone", 9004),
                config("old", 9005),
                config("legacy", 9006),
                PeerConfig {
                    name: "inbound".into(),
                    url: String::new(),
                },
            ],
            LocalTransports::default(),
        );
        assert_eq!(
            targets,
            BTreeMap::from([
                (
                    "server".to_owned(),
                    vec![
                        Route::Http("http://127.0.0.1:9002".into()),
                        Route::Http("http://127.0.0.1:2".into())
                    ]
                ),
                (
                    "legacy".to_owned(),
                    vec![Route::Http("http://127.0.0.1:9006".into())]
                ),
            ])
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
            view_changed: watch::channel(0).0,
            config_peers: config_peers.iter().map(|peer| (*peer).into()).collect(),
            legacy,
            own_key: Some(own.public().into()),
            bootstrap_keys: bootstrap.iter().map(|key| (*key).into()).collect(),
            transports: Arc::default(),
            fabric: None,
            bootstrap: None,
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
