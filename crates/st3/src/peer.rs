//! The peer listener as smalltalk runs it: the sync worker from `smallclaims::sync`, reaching the
//! store through this node's daemon, with the client-read relay and raw terminal routes beside
//! the sync routes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State, OriginalUri, Query};
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use notify::Watcher as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

pub use smallclaims::sync::{
    Backend, FleetAuth, Local, MAX_EXCHANGE_BYTES, MAX_MANIFEST_BYTES, WorkerConfig,
};
use smallclaims::sync::{dial_targets, signed_error_response_for, signed_response_for};

use crate::client::Client;
use crate::config::{Config, PeerConfig};
use crate::fleet::transport::{Fabric, LocalTransports, Route, local_addresses, parse_route, resolve_tool};
use crate::fleet::{FleetView, MemberKey};
use crate::model::{
    ApiResponse, ReplicaEnvelopeId, ReplicationExchange, ReplicationExportRequest,
    ReplicationExportResponse, ReplicationHealAnswer, ReplicationHealAnswerRequest,
    ReplicationHealNextRequest, ReplicationHealQuery, ReplicationHealStep, ReplicationInventory,
    ReplicationPeerFailureRequest, ReplicationReceiveRequest, ReplicationReceiveResponse,
};
use crate::store::Store;
use crate::store::{
    CheckpointAction, CheckpointManifest, CheckpointManifestNeed, CheckpointManifestPage,
    CheckpointManifestRequest,
};

/// The peer listener's shared state, with this node's daemon as the store.
type PeerState = smallclaims::sync::PeerState<MainBackend>;

const CLIENT_READ_PATH: &str = "/v1/peer/client-read";
const CURRENT_VALUE_PATH: &str = "/v1/peer/current-value";
const RAW_TERMINAL_PATH: &str = "/v1/peer/raw-terminal";
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
/// How long an owner's stale-fence refusal counts as a recent fence conflict.
const CLIENT_READ_FENCE_WINDOW: Duration = Duration::from_secs(60);
/// The daemon route a replication worker hands a read to when it must forward it.
pub const CLIENT_READ_FORWARD_PATH: &str = "/v1/internal/client-read/forward";

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
        /// Ask the owner for its best-effort session facts too.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        facts: bool,
    },
    /// Wait on the owner until the screen's revision differs from `after_revision`.
    TerminalScreenChange {
        terminal_id: String,
        after_revision: String,
        wait_ms: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        facts: bool,
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
    /// A portable suspended seat payload, read only for the exact fenced resume request.
    SeatSnapshot { subject: String, suspend_operation: String, resume_operation: String, offset: u64 },
    /// The directory this host gives a new agent that names no workspace.
    AgentWorkspace {
        identity: String,
    },
    /// Up to 512 KiB of an attachment from `offset`, from the member that took the upload. The
    /// answer is base64 in JSON with the file's size; the reader asks again until it has it all.
    Blob {
        sha256: String,
        message: String,
        offset: u64,
    },
    /// The messages to and from one agent, as the host that owns the agent lists them.
    Messages {
        actor: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        history: bool,
        limit: Option<usize>,
        cursor: Option<String>,
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
    /// Why and where the read failed, such as `reason`, `owner_host_id`, `attempts`.
    pub details: serde_json::Map<String, Value>,
}

impl ClientReadRejected {
    pub fn new(code: impl Into<String>, status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            status: status.as_u16(),
            message: message.into(),
            details: serde_json::Map::new(),
        }
    }

    /// A read that could not reach its owner for `reason`: `no-route`, `dial-failed`,
    /// `timed-out`, `refused`, `hop-limit`, `transport-error` or `owner-error`.
    pub fn unreachable(reason: &str, message: impl Into<String>) -> Self {
        let mut rejected = Self::new(
            "remote-unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            message,
        );
        rejected.details.insert("reason".into(), reason.into());
        rejected
    }

    /// The reason a read could not reach its owner, if this says.
    pub fn reason(&self) -> Option<&str> {
        self.details.get("reason").and_then(Value::as_str)
    }
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
    fabric: Option<Fabric>,
    // Reuse Fabric listeners and reqwest connections; retain no publication bytes or retry jobs.
    current_connections: Arc<std::sync::Mutex<BTreeMap<(String, String), String>>>,
    legacy: bool,
    /// When each owner last refused this node's reads as stale, for provenance.
    fence_conflicts:
        Arc<std::sync::Mutex<BTreeMap<String, std::collections::VecDeque<std::time::Instant>>>>,
}

/// How a relayed read got its answer, so a client can tell a slow gateway from a down owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientReadProvenance {
    pub owner_host_id: String,
    /// The first node the read went to; the owner itself when `direct`.
    pub via: String,
    pub direct: bool,
    /// How that first hop was dialed: `fabric` or `http`.
    pub transport: &'static str,
    /// Milliseconds from sending the read to receiving the owner's answer.
    pub rtt_ms: u64,
    /// How many of this node's reads the owner refused as stale in the last minute.
    pub fence_conflicts: u32,
}

impl ClientRelay {
    /// One bounded attempt per reachable peer, with no queue, replay or fallback to claims.
    pub(crate) async fn publish_current_value(&self, record: crate::model::ClaimRecord) {
        let Ok(body) = serde_json::to_vec(&record) else {
            return;
        };
        if body.len() > MAX_CLIENT_READ_BYTES {
            return;
        }
        let Ok(headers) = self
            .auth
            .request_headers_for(CURRENT_VALUE_PATH, &self.node, &body)
        else {
            return;
        };
        let view = self
            .links
            .as_ref()
            .and_then(|s| s.fleet_view().ok())
            .unwrap_or_default();
        let targets = dial_targets(
            &view,
            &self.node,
            &self.peers,
            LocalTransports {
                fabric: self.fabric.is_some(),
                tailscale: local_addresses()
                    .iter()
                    .any(crate::fleet::transport::is_tailnet_address),
            },
        );
        let attempts = targets
            .into_iter()
            .filter(|(peer, _)| {
                // A current sample never opens connections around the durable worker's
                // offline/overload backoff. Drop it; recovery sends no saved current bytes.
                let Some(store) = &self.links else { return true; };
                if store.replication_worker(peer).is_some_and(|worker| {
                    matches!(worker.phase.as_str(), "backoff" | "overload")
                }) {
                    return false;
                }
                // The worker briefly leaves backoff while checking a still-offline route.
                // Its existing, indexed connectivity evidence remains down until a successful
                // exchange. Current samples must not create their own probes during that check.
                !matches!(store.replication_peer_up(peer), Ok((false, Some(_))) | Err(_))
            })
            .filter_map(|(_, routes)| routes.into_iter().next())
            .map(|route| {
                let body = body.clone();
                let headers = headers.clone();
                async move {
                    let connection_key = match &route {
                        Route::Fabric { node, protocol } => Some((node.clone(), protocol.clone())),
                        Route::Http(_) => None,
                    };
                    let attempt = async {
                        let url = match route {
                            Route::Http(url) => url,
                            Route::Fabric { node, protocol } => {
                                let key = (node.clone(), protocol.clone());
                                let cached = self
                                    .current_connections
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .get(&key)
                                    .cloned();
                                if let Some(url) = cached {
                                    url
                                } else {
                                    let Some(fabric) = &self.fabric else {
                                        return false;
                                    };
                                    let Ok(address) = fabric.dial(&node, &protocol).await else {
                                        return false;
                                    };
                                    let url = format!("http://{address}");
                                    self.current_connections
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(key, url.clone());
                                    url
                                }
                            }
                        };
                        let response = self
                            .http
                            .post(format!("{}{CURRENT_VALUE_PATH}", url.trim_end_matches('/')))
                            .headers(headers)
                            .header("content-type", "application/json")
                            .body(body)
                            .send()
                            .await;
                        match response {
                            Ok(response) => {
                                let accepted = response.status().is_success();
                                accepted && response.bytes().await.is_ok()
                            }
                            Err(_) => false,
                        }
                    };
                    // Fleet budget covers both network legs plus the receiver's 100 ms local hop.
                    let accepted = tokio::time::timeout(Duration::from_millis(250), attempt)
                        .await
                        .unwrap_or(false);
                    if !accepted && let Some(key) = connection_key {
                        self.current_connections
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&key);
                    }
                }
            });
        futures_util::future::join_all(attempts).await;
    }
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
    fn next_hops(&self, target: &str, visited: &[String]) -> Vec<PeerConfig> {
        let view = self
            .links
            .as_ref()
            .and_then(|store| store.fleet_view_sealed().ok())
            .unwrap_or_default();
        let local = LocalTransports {
            fabric: self.fabric.is_some(),
            tailscale: local_addresses()
                .iter()
                .any(crate::fleet::transport::is_tailnet_address),
        };
        let dialable = dial_targets(&view, &self.node, &self.peers, local);
        let names = dialable
            .keys()
            .filter(|name| !visited.contains(name))
            .cloned()
            .collect::<Vec<_>>();
        let links = self.observed_links();
        let owner_unreachable = self
            .links
            .as_ref()
            .is_some_and(|store| store.replication_peer_up(target).is_ok_and(|(up, _)| !up));
        client_read_next_hops(
            &self.node,
            target,
            &names,
            visited,
            &links,
            owner_unreachable,
        )
        .into_iter()
        .flat_map(|name| {
            dialable
                .get(&name)
                .into_iter()
                .flatten()
                .map(move |route| PeerConfig {
                    name: name.clone(),
                    url: match route {
                        Route::Http(url) => url.clone(),
                        Route::Fabric { node, protocol } => {
                            format!("fabric://{node}/{protocol}")
                        }
                    },
                })
        })
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
            auth: FleetAuth::load(fleet, secret)?.with_member_key(
                config
                    .fleet
                    .as_ref()
                    .map(|file| {
                        MemberKey::load(&file.node_key_path(&config.state_dir)).map(Arc::new)
                    })
                    .transpose()?,
            ),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            links: None,
            legacy: config.fleet.as_ref().is_none_or(|file| file.legacy_peers),
            observed: Arc::default(),
            current_connections: Arc::default(),
            fence_conflicts: Arc::default(),
            fabric: resolve_tool(
                config
                    .fleet
                    .as_ref()
                    .and_then(|file| file.fabric.as_deref()),
                "fabric",
            )
            .map(Fabric::new),
        }))
    }

    /// Read from the owner of `host_id`, directly or through the peers that can reach it.
    pub async fn read(
        &self,
        host_id: &str,
        request: &ClientReadRequest,
    ) -> Result<serde_json::Value> {
        self.read_traced(host_id, request)
            .await
            .map(|(value, _)| value)
    }

    /// [`Self::read`], with how the answer arrived. A failure carries the same facts in the
    /// details of its [`ClientReadRejected`], so a slow gateway reads differently from a down
    /// owner.
    pub async fn read_traced(
        &self,
        host_id: &str,
        request: &ClientReadRequest,
    ) -> Result<(serde_json::Value, ClientReadProvenance)> {
        let target = host_id
            .strip_prefix("host/")
            .context("the owner host ID is invalid")?;
        let started = std::time::Instant::now();
        match self
            .send_toward(
                target,
                request,
                vec![self.node.clone()],
                CLIENT_READ_MAX_HOPS,
            )
            .await
        {
            Ok((value, peer)) => Ok((
                value,
                ClientReadProvenance {
                    owner_host_id: host_id.to_owned(),
                    via: format!("host/{}", peer.name),
                    direct: peer.name == target,
                    transport: if peer.url.starts_with("fabric://") {
                        "fabric"
                    } else {
                        "http"
                    },
                    rtt_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    fence_conflicts: self.recent_fence_conflicts(target),
                },
            )),
            Err(error) => {
                let mut rejected = match error.downcast::<ClientReadRejected>() {
                    Ok(rejected) => rejected,
                    Err(error) => ClientReadRejected::unreachable(
                        "transport-error",
                        format!("the read to {host_id} failed: {error:#}"),
                    ),
                };
                if rejected.code == "stale-fence" {
                    self.note_fence_conflict(target);
                }
                rejected
                    .details
                    .insert("owner_host_id".into(), host_id.into());
                rejected.details.insert(
                    "elapsed_ms".into(),
                    u64::try_from(started.elapsed().as_millis())
                        .unwrap_or(u64::MAX)
                        .into(),
                );
                rejected.details.insert(
                    "fence_conflicts".into(),
                    self.recent_fence_conflicts(target).into(),
                );
                Err(rejected.into())
            }
        }
    }

    fn note_fence_conflict(&self, target: &str) {
        let mut conflicts = self
            .fence_conflicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let recent = conflicts.entry(target.to_owned()).or_default();
        recent.retain(|at| now.duration_since(*at) < CLIENT_READ_FENCE_WINDOW);
        recent.push_back(now);
    }

    /// How many of this node's reads the owner refused as stale lately.
    fn recent_fence_conflicts(&self, target: &str) -> u32 {
        let mut conflicts = self
            .fence_conflicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let Some(recent) = conflicts.get_mut(target) else {
            return 0;
        };
        recent.retain(|at| now.duration_since(*at) < CLIENT_READ_FENCE_WINDOW);
        u32::try_from(recent.len()).unwrap_or(u32::MAX)
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
            let mut rejected = ClientReadRejected::unreachable(
                "hop-limit",
                format!("{} cannot carry this read any further", self.node),
            );
            rejected.status = StatusCode::LOOP_DETECTED.as_u16();
            return Err(rejected.into());
        }
        let mut path = route.path.clone();
        path.push(self.node.clone());
        self.send_toward(target, request, path, route.hops_left - 1)
            .await
            .map(|(value, _)| value)
    }

    /// Try each next hop in turn. An owner's own answer, including a refusal such as a stale
    /// fence, ends the attempt; a hop that cannot be reached, or cannot reach further, does not.
    /// When none can, the failure says which hops were tried and why each failed.
    async fn send_toward(
        &self,
        target: &str,
        request: &ClientReadRequest,
        path: Vec<String>,
        hops_left: u8,
    ) -> Result<(serde_json::Value, PeerConfig)> {
        let mut attempts = Vec::new();
        let mut last_reason = None;
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
            match self.send(&peer, &outgoing).await {
                Ok(value) => return Ok((value, peer)),
                Err(error) => {
                    let rejected = match error.downcast::<ClientReadRejected>() {
                        Ok(rejected) => rejected,
                        Err(error) => {
                            ClientReadRejected::unreachable("transport-error", format!("{error:#}"))
                        }
                    };
                    if rejected.code != "remote-unavailable" {
                        return Err(rejected.into());
                    }
                    let reason = rejected.reason().unwrap_or("transport-error").to_owned();
                    let mut attempt = serde_json::json!({
                        "via": format!("host/{}", peer.name),
                        "reason": reason,
                    });
                    if let Some(next) = rejected.details.get("attempts") {
                        attempt["next"] = next.clone();
                    }
                    attempts.push(attempt);
                    last_reason = Some(reason);
                }
            }
        }
        let (reason, message) = match &last_reason {
            None => (
                "no-route".to_owned(),
                format!(
                    "no route from {} to owner host/{target}: it is not a peer and no peer reaches it",
                    self.node
                ),
            ),
            Some(reason) => (
                reason.clone(),
                format!(
                    "owner host/{target} did not answer from {} ({reason} after {} attempt(s))",
                    self.node,
                    attempts.len()
                ),
            ),
        };
        let mut rejected = ClientReadRejected::unreachable(&reason, message);
        rejected.details.insert("path".into(), path.into());
        rejected.details.insert("attempts".into(), attempts.into());
        Err(rejected.into())
    }

    async fn send(
        &self,
        peer: &PeerConfig,
        request: &ClientReadRequest,
    ) -> Result<serde_json::Value> {
        let name = peer.name.as_str();
        let url = match parse_route(&peer.url).context("invalid peer client route")? {
            Route::Http(url) => url,
            Route::Fabric { node, protocol } => {
                let address = match self.fabric.as_ref() {
                    Some(fabric) => fabric.dial(&node, &protocol).await,
                    None => Err(anyhow::anyhow!("Fabric is unavailable")),
                }
                .map_err(|error| {
                    ClientReadRejected::unreachable(
                        "dial-failed",
                        format!("dial {node} over Fabric: {error:#}"),
                    )
                })?;
                format!("http://{address}")
            }
        };
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
            .post(format!("{}{}", url.trim_end_matches('/'), CLIENT_READ_PATH))
            .headers(headers)
            .header("content-type", "application/json")
            .timeout(timeout)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                let reason = if error.is_timeout() {
                    "timed-out"
                } else if error.is_connect() {
                    "dial-failed"
                } else {
                    "transport-error"
                };
                ClientReadRejected::unreachable(reason, format!("peer {name}: {error}"))
            })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ClientReadRejected::unreachable(
                "refused",
                format!("peer {name} does not accept this node's client reads"),
            )
            .into());
        }
        let response_headers = response.headers().clone();
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_CLIENT_READ_BYTES as u64),
            "the peer client read response exceeds its bound"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            ClientReadRejected::unreachable(
                if error.is_timeout() {
                    "timed-out"
                } else {
                    "transport-error"
                },
                format!("peer {name}: {error}"),
            )
        })? {
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
                details: envelope.value["details"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            }
            .into());
        }
        Ok(envelope.value)
    }

    /// Open a persistent authenticated byte route to the terminal's owning member.
    /// Unlike screen reads this never polls or creates a temporary geometry writer.
    pub async fn raw_terminal(
        &self,
        host: &str,
        person: &str,
        terminal_id: &str,
        incarnation: &str,
        mode: st3_client::RawTerminalMode,
    ) -> Result<tokio::net::UnixStream> {
        let target = host.strip_prefix("host/").context("invalid owner host")?;
        let mode = match mode {
            st3_client::RawTerminalMode::Attach => "attach",
            st3_client::RawTerminalMode::Peek => "peek",
        };
        let path = format!("{RAW_TERMINAL_PATH}?person={}&terminal={}&incarnation={}&mode={mode}",
            urlencoding::encode(person), urlencoding::encode(terminal_id), urlencoding::encode(incarnation));
        let mut last = None;
        let profile = crate::profile::current();
        let route_span = profile.as_ref().map(|op| op.wall_span("raw/route"));
        let peers = self.next_hops(target, std::slice::from_ref(&self.node));
        drop(route_span);
        for peer in peers.into_iter().filter(|peer| peer.name == target) {
            let result = async {
                let dial_span = profile.as_ref().map(|op| op.wall_span("raw/dial"));
                let url = match parse_route(&peer.url).context("invalid raw terminal route")? {
                    Route::Http(url) => url,
                    Route::Fabric { node, protocol } => {
                        let address = self.fabric.as_ref().context("Fabric unavailable")?.dial(&node, &protocol).await?;
                        format!("http://{address}")
                    }
                };
                drop(dial_span);
                let base = url.strip_prefix("http://").context("peer byte transport requires HTTP")?;
                let mut request = format!("ws://{}{path}", base.trim_end_matches('/')).into_client_request()?;
                request.headers_mut().extend(self.auth.request_headers_method("GET", &path, &self.node, &[])?);
                let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(64 * 1024)).max_frame_size(Some(64 * 1024));
                let (socket, response) = tokio::time::timeout(CLIENT_READ_TIMEOUT, async {
                    let host = request.uri().host().context("raw terminal route has no host")?
                        .trim_matches(['[', ']']);
                    let port = request.uri().port_u16().unwrap_or(80);
                    let tcp_span = profile.as_ref().map(|op| op.wall_span("raw/tcp-dial"));
                    let tcp = tokio::net::TcpStream::connect((host, port)).await?;
                    drop(tcp_span);
                    let upgrade_span = profile.as_ref().map(|op| op.wall_span("raw/peer-upgrade"));
                    let result = tokio_tungstenite::client_async_with_config(request, tcp, Some(config)).await?;
                    drop(upgrade_span);
                    Ok::<_, anyhow::Error>(result)
                }).await??;
                let verify_span = crate::profile::span("raw/peer-verify");
                let sender = self.auth.verify_sender(response.headers(), "RESPONSE", &path, &[], Some(&peer.name), Some(&FleetAuth::body_digest(&[])))?;
                let view = self.links.as_ref().and_then(|store| store.fleet_view_sealed().ok()).unwrap_or_default();
                crate::fleet::accept(&view, &sender, self.peers.iter().any(|peer| peer.name == sender.name), self.legacy)
                    .map_err(|refusal| anyhow::anyhow!("raw terminal member refused: {refusal:?}"))?;
                drop(verify_span);
                let (client, bridge) = tokio::net::UnixStream::pair()?;
                let bridge = bridge.into_std()?;
                let monitor = tokio::io::unix::AsyncFd::new(bridge.try_clone()?)?;
                let bridge = tokio::net::UnixStream::from_std(bridge)?;
                tokio::spawn(async move {
                    let (mut sink, mut source) = socket.split();
                    let (mut reader, mut writer) = bridge.into_split();
                    let flush = tokio::sync::Notify::new();
                    let upload = async {
                        let mut bytes = [0_u8; 16 * 1024];
                        loop {
                            tokio::select! {
                                read = reader.read(&mut bytes) => {
                                    let Ok(count) = read else { break; };
                                    if count == 0 || sink.send(tokio_tungstenite::tungstenite::Message::Binary(bytes[..count].to_vec().into())).await.is_err() { break; }
                                }
                                () = flush.notified() => {
                                    if sink.flush().await.is_err() { break; }
                                }
                            }
                        }
                    };
                    let download = async {
                        while let Some(Ok(message)) = source.next().await {
                            match message {
                                tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                                    if writer.write_all(&bytes).await.is_err() { break; }
                                }
                                tokio_tungstenite::tungstenite::Message::Ping(_) => flush.notify_one(),
                                tokio_tungstenite::tungstenite::Message::Pong(_) => {}
                                _ => break,
                            }
                        }
                    };
                    let closed = async {
                        loop {
                            let Ok(mut ready) = monitor.readable().await else { break; };
                            if ready.ready().is_read_closed() || ready.ready().is_error() { break; }
                            ready.clear_ready();
                        }
                    };
                    tokio::select! { () = upload => {}, () = download => {}, () = closed => {} }
                });
                Ok::<_, anyhow::Error>(client)
            }.await;
            match result {
                Ok(stream) => return Ok(stream),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no direct member route to owner {host}")))
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
    owner_unreachable: bool,
) -> Vec<String> {
    let mut ordered = client_read_hops(node, target, dialable, visited, links);
    // A read waits out its whole timeout on an owner this node cannot reach before it tries a
    // peer that can, so an owner this node has not exchanged with lately goes last.
    if owner_unreachable && ordered.first().is_some_and(|first| first == target) {
        ordered.rotate_left(1);
    }
    ordered
}

fn client_read_hops(
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

#[derive(Deserialize)]
struct RawTerminalQuery {
    person: String,
    terminal: String,
    incarnation: String,
    mode: st3_client::RawTerminalMode,
}

async fn receive_raw_terminal(
    websocket: WebSocketUpgrade,
    State(state): State<PeerState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<RawTerminalQuery>,
    headers: HeaderMap,
) -> Response {
    let profile = crate::profile::Op::start("GET /v1/peer/raw-terminal", None);
    let path = uri.path_and_query().map_or(uri.path(), |value| value.as_str());
    if path.len() > 16_384 || !query.person.starts_with("person/") || query.person.matches('/').count() != 1 {
        return (StatusCode::BAD_REQUEST, "invalid raw terminal route").into_response();
    }
    let auth_span = profile.as_ref().map(|op| op.wall_span("raw/peer-authenticate"));
    let _sender = match state.auth().verify_sender(&headers, "GET", path, &[], None, None) {
        Ok(sender) if state.accept(&sender).is_ok() => sender,
        _ => return (StatusCode::UNAUTHORIZED, "untrusted raw terminal member").into_response(),
    };
    drop(auth_span);
    // The owner daemon validates its current graph incarnation and person authority, then
    // connects once. The peer worker carries that one connection, not synthetic screens.
    let client = st3_client::Client::unix_as(state.backend().socket(), &query.person);
    let transport = async {
        let attachment_span = profile.as_ref().map(|op| op.wall_span("raw/owner-attachment"));
        let attachment = client.raw_terminal_attachment(&query.terminal, &query.incarnation, query.mode).await?;
        drop(attachment_span);
        if attachment.owner_host_id != format!("host/{}", state.node()) {
            return Err(st3_client::ClientError::Protocol("raw terminal route is not owner-local".into()));
        }
        let open_span = profile.as_ref().map(|op| op.wall_span("raw/owner-open"));
        let transport = client.raw_terminal_stream(&attachment).await;
        drop(open_span);
        transport
    }.await;
    if let Some(profile) = profile {
        profile.finish();
    }
    let transport = match transport {
        Ok(transport) => transport,
        Err(st3_client::ClientError::Api(_, _, _)) => return (StatusCode::CONFLICT, "owner rejected raw terminal incarnation or authority").into_response(),
        Err(_) => return (StatusCode::BAD_GATEWAY, "owner raw terminal unavailable").into_response(),
    };
    let signed = match state.auth().response_headers_for(path, state.node(), &[], &FleetAuth::body_digest(&[])) {
        Ok(headers) => headers,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "raw terminal signature failed").into_response(),
    };
    let mut response = websocket.max_message_size(64 * 1024).max_frame_size(64 * 1024)
        .on_upgrade(move |socket| crate::api::raw_terminal_splice(socket, transport, None));
    response.headers_mut().extend(signed);
    response
}

/// Hand a read bound for another owner to this node's daemon, which knows the routes and the
/// peers to carry it on. Its answer, or the owner's refusal, comes back unchanged.
async fn forward_client_read(
    state: &PeerState,
    request: &ClientReadRequest,
) -> Result<serde_json::Value> {
    let daemon = Client::unix(state.backend().socket().to_path_buf());
    daemon
        .post::<_, serde_json::Value>(CLIENT_READ_FORWARD_PATH, request)
        .await
        .map_err(|error| match crate::client::api_error_parts(&error) {
            Some((status, code, message, details)) => ClientReadRejected {
                code: code.to_owned(),
                status,
                message: message.to_owned(),
                details: details.clone(),
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
            .auth()
            .verify_sender(&headers, "POST", CLIENT_READ_PATH, &body, None, None)
        {
            Ok(sender) if state.accept(&sender).is_ok() => sender.name,
            _ => return (StatusCode::UNAUTHORIZED, "untrusted fleet client read").into_response(),
        };
    state.note_activity(&sender);
    let request_digest = FleetAuth::body_digest(&body);
    let result: Result<serde_json::Value> = async {
        anyhow::ensure!(
            body.len() <= 16_384,
            "the client read request exceeds its bound"
        );
        let request: ClientReadRequest = serde_json::from_slice(&body)?;
        // Free mode: a member relays a read for a person or for one of the fleet's agents.
        anyhow::ensure!(
            request.authority_actor.starts_with("person/")
                && request.authority_actor.matches('/').count() == 1
                || request.authority_actor.starts_with("agent/"),
            "a fleet client read needs one concrete person or agent"
        );
        if let Some(route) = &request.relay {
            // The path names the member that sent it last, and never this node again.
            anyhow::ensure!(
                route.path.last() == Some(&sender)
                    && route.path.len() <= usize::from(CLIENT_READ_MAX_HOPS) + 1,
                "the relayed client read's path does not end at its sender"
            );
            if route.target != format!("host/{}", state.node()) {
                return forward_client_read(&state, &request).await;
            }
        }
        let client = st3_client::Client::unix_as(state.backend().socket(), &request.authority_actor);
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
            ClientReadOperation::TerminalScreen { terminal_id, facts } => {
                let value = if facts {
                    client.terminal_screen_with_facts(&terminal_id).await?.value
                } else {
                    client.terminal_screen(&terminal_id).await?.value
                };
                Ok(serde_json::to_value(value)?)
            }
            ClientReadOperation::TerminalScreenChange {
                terminal_id,
                after_revision,
                wait_ms,
                facts,
            } => {
                anyhow::ensure!(
                    wait_ms <= CLIENT_READ_MAX_WAIT_MS,
                    "the terminal screen wait exceeds its bound"
                );
                let value = if facts {
                    client
                        .terminal_screen_change_with_facts(&terminal_id, &after_revision, wait_ms)
                        .await?
                        .value
                } else {
                    client
                        .terminal_screen_change(&terminal_id, &after_revision, wait_ms)
                        .await?
                        .value
                };
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
                // The owner checks its receipt before validating the terminal fence. A
                // retry after an accepted input must not be refused because that input
                // already changed the screen. Send the original terminal fence unchanged.
                let snapshot = client.capabilities().await?.snapshot;
                let fence = st3_client::Fence {
                    snapshot_id: snapshot.id,
                    subject_revisions: Default::default(),
                    mission_generation: None,
                    step_definition: None,
                    attempt: None,
                    readiness_epoch: None,
                    runtime_incarnation: Some(runtime_incarnation),
                    runtime_desired_revision: None,
                    terminal_sequence: Some(expected_sequence),
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
            ClientReadOperation::Messages {
                actor,
                history,
                limit,
                cursor,
            } => {
                anyhow::ensure!(
                    limit.is_none_or(|limit| (1..=200).contains(&limit)),
                    "the messages limit is invalid"
                );
                let page = client
                    .messages_list_for_peer(&actor, cursor.as_deref(), limit, history)
                    .await?
                    .value;
                Ok(serde_json::to_value(page)?)
            }
            ClientReadOperation::SeatSnapshot { subject, suspend_operation, resume_operation, offset } => {
                let local = crate::client::Client::unix(state.backend().socket());
                local.post::<_, serde_json::Value>("/v1/internal/seat-snapshot", &serde_json::json!({
                    "subject": subject, "suspend_operation": suspend_operation,
                    "resume_operation": resume_operation, "offset": offset, "actor": request.authority_actor
                })).await.map_err(|error| ClientReadRejected::new(
                    crate::client::api_error_code(&error).unwrap_or("remote-unavailable"),
                    StatusCode::CONFLICT, format!("{error:#}")
                ).into())
            }
            ClientReadOperation::AgentWorkspace { identity } => {
                let workspace =
                    crate::config::default_agent_workspace(&identity).map_err(|error| {
                        ClientReadRejected::new(
                            "validation-failed",
                            StatusCode::UNPROCESSABLE_ENTITY,
                            error.to_string(),
                        )
                    })?;
                Ok(serde_json::json!({ "workspace": workspace }))
            }
            ClientReadOperation::Blob {
                sha256,
                message,
                offset,
            } => {
                // Only this member's own files: it is the one the claim names.
                let chunk = client
                    .blob_chunk(&sha256, Some(&message), offset, true)
                    .await?
                    .value;
                Ok(serde_json::to_value(chunk)?)
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
            let (status, code, message, details) =
                if let Some(rejected) = error.downcast_ref::<ClientReadRejected>() {
                    (
                        StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::CONFLICT),
                        rejected.code.clone(),
                        rejected.message.clone(),
                        rejected.details.clone(),
                    )
                } else {
                    match error.downcast_ref::<st3_client::ClientError>() {
                        Some(st3_client::ClientError::Api(code, message, details)) => {
                            let status = match code {
                                st3_client::ErrorCode::PageCursorExpired
                                | st3_client::ErrorCode::CursorGap
                                | st3_client::ErrorCode::BlobExpired => StatusCode::GONE,
                                st3_client::ErrorCode::NotFound
                                | st3_client::ErrorCode::BlobNotFound => StatusCode::NOT_FOUND,
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
                                details
                                    .details
                                    .iter()
                                    .map(|(key, value)| (key.clone(), value.clone()))
                                    .collect(),
                            )
                        }
                        _ => {
                            let failed = ClientReadRejected::unreachable(
                                "owner-error",
                                format!("fleet client read from {sender} failed: {error:#}"),
                            );
                            (
                                StatusCode::UNPROCESSABLE_ENTITY,
                                failed.code,
                                failed.message,
                                failed.details,
                            )
                        }
                    }
                };
            signed_client_read_failure(&state, &request_digest, status, &code, &message, details)
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
    details: serde_json::Map<String, Value>,
) -> Result<Response> {
    let envelope = ApiResponse {
        api_version: "st3.v1".into(),
        request_id: uuid::Uuid::now_v7().to_string(),
        snapshot_host: state.node().to_owned(),
        store_index: 0,
        value: serde_json::json!({"code":code,"message":message,"details":details}),
    };
    let body = serde_json::to_vec(&envelope)?;
    let headers =
        state
            .auth()
            .response_headers_for(CLIENT_READ_PATH, state.node(), &body, request_digest)?;
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response.headers_mut().extend(headers);
    Ok(response)
}

/// The store as this node's daemon serves it over its local socket. The daemon is the store's
/// only writer, so the worker hands it every exchange.
#[derive(Clone)]
pub struct MainBackend {
    socket: PathBuf,
    client: Client,
}

impl MainBackend {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            client: Client::unix(socket.clone()),
            socket,
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Backend for MainBackend {
    async fn ready(&self) {
        loop {
            if self.client.get::<serde_json::Value>("/v1/health").await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn export(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        self.client
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

    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
    ) -> Result<ReplicationReceiveResponse> {
        self.client
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

    async fn heal_answer(
        &self,
        peer: &str,
        fleet_id: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        self.client
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

    async fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        self.client
            .post(
                "/v1/internal/replication/heal/next",
                &ReplicationHealNextRequest {
                    peer: peer.to_owned(),
                    answer,
                },
            )
            .await
    }

    async fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.client
            .post("/v1/internal/replication/checkpoint", request)
            .await
    }

    async fn checkpoint_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        self.client
            .post(
                "/v1/internal/replication/checkpoint-need",
                &serde_json::json!({}),
            )
            .await
    }

    async fn adopt_checkpoint(&self, manifest: &CheckpointManifest) -> Result<Vec<CheckpointAction>> {
        self.client
            .post("/v1/internal/replication/checkpoint-adopt", manifest)
            .await
    }

    async fn publish_endpoints(&self, mode: &str, endpoints: &[Value]) -> Result<()> {
        let _: Value = self
            .client
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

    async fn redeem(&self, request: &crate::fleet::handshake::JoinRequest) -> Result<Value> {
        self.client.post("/v1/internal/fleet/redeem", request).await
    }

    async fn fleet_view(&self) -> Result<FleetView> {
        self.client.get("/v1/internal/fleet/membership").await
    }

    async fn record_worker(
        &self,
        peer: &str,
        worker: smallclaims::replication::ReplicationWorkerStatus,
    ) -> Result<()> {
        let _: Value = self
            .client
            .post(
                "/v1/internal/replication/worker-status",
                &smallclaims::replication::ReplicationWorkerStatusRequest {
                    peer: peer.to_owned(),
                    worker,
                },
            )
            .await?;
        Ok(())
    }

    async fn record_failure(&self, peer: &str, status: &str, error: &str) -> Result<()> {
        let _ = tokio::time::timeout(
            crate::client::LATEST_VALUE_TIMEOUT,
            self.client.post::<_, serde_json::Value>(
                "/v1/internal/replication/peer-failure",
                &ReplicationPeerFailureRequest {
                    peer: peer.to_owned(),
                    status: status.to_owned(),
                    error: error.to_owned(),
                },
            ),
        )
        .await;
        Ok(())
    }

    async fn changed(&self) {
        if !self.socket.exists() {
            return;
        }
        let _ = self
            .client
            .post::<_, serde_json::Value>("/v1/internal/replication-wake", &serde_json::json!({}))
            .await;
    }
}

/// The routes smalltalk serves beside the sync routes on the peer listener.
fn smalltalk_routes() -> Router<PeerState> {
    Router::new()
        .route(
            CLIENT_READ_PATH,
            post(receive_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .route(RAW_TERMINAL_PATH, get(receive_raw_terminal))
        .route(
            CURRENT_VALUE_PATH,
            post(receive_current_value).layer(DefaultBodyLimit::max(MAX_CLIENT_READ_BYTES)),
        )
}

async fn receive_current_value(
    State(state): State<PeerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sender =
        match state
            .auth()
            .verify_sender(&headers, "POST", CURRENT_VALUE_PATH, &body, None, None)
        {
            Ok(sender) if state.accept(&sender).is_ok() => sender.name,
            _ => return (StatusCode::UNAUTHORIZED, "untrusted current value").into_response(),
        };
    let Ok(record) = serde_json::from_slice::<crate::model::ClaimRecord>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if record.origin != sender || !crate::store::is_current_value(&record.kind) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let client = Client::unix(state.backend().socket());
    let result = tokio::time::timeout(
        crate::client::LATEST_VALUE_TIMEOUT,
        client.post::<_, Value>("/v1/internal/current-value", &record),
    )
    .await;
    match result {
        Ok(Ok(value)) => signed_response_for(
            &state,
            CURRENT_VALUE_PATH,
            &FleetAuth::body_digest(&body),
            0,
            value,
        )
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Run the replication worker: sync this node's store, through its daemon, with the fleet.
pub async fn run_worker(config: Config) -> Result<()> {
    config.validate()?;
    let worker = WorkerConfig {
        node: config.node.clone(),
        fleet_id: config
            .fleet_id
            .clone()
            .context("the replication worker needs fleet_id")?,
        secret_file: config
            .shared_secret_file
            .clone()
            .context("the replication worker needs shared_secret_file")?,
        state_dir: config.state_dir.clone(),
        peers: config.peers.clone(),
        peer_listen: config.peer_listen.clone(),
        fleet: config.fleet.clone(),
    };
    // The daemon touches this file whenever the graph changes; each touch wakes the dialers.
    let (wake, _) = watch::channel(0_u64);
    let wake_file = config.state_dir.join("replication.wake");
    if !wake_file.exists() {
        fs::write(&wake_file, b"worker-start\n")?;
    }
    let watcher_wake = wake.clone();
    let mut database_watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                watcher_wake.send_modify(|generation| *generation = generation.saturating_add(1));
            }
        })?;
    database_watcher.watch(&wake_file, notify::RecursiveMode::NonRecursive)?;
    smallclaims::sync::run(
        worker,
        MainBackend::new(config.socket.clone()),
        wake,
        smalltalk_routes(),
    )
    .await
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

    use sha2::{Digest as _, Sha256};
    use smallclaims::fleet::MemberKey;
    use smallclaims::sync::{
        FleetContext, PeerState, exchange, fetch_checkpoint_manifest, heal,
        peer_router, replication_http_client,
    };
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn current_fleet_attempt_reuses_fabric_connection_and_bounds_the_whole_hop() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let delay = Arc::new(AtomicU64::new(120));
        let app = axum::Router::new().route(
            CURRENT_VALUE_PATH,
            axum::routing::post({
                let (calls, delay) = (calls.clone(), delay.clone());
                move || {
                    let (calls, delay) = (calls.clone(), delay.clone());
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(delay.load(Ordering::SeqCst)))
                            .await;
                        axum::Json(serde_json::json!({"changed":true}))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dial_log = root.path().join("dials");
        let fabric = root.path().join("fabric");
        fs::write(
            &fabric,
            format!(
                "#!/bin/sh\nprintf 'dial\\n' >> '{}'\nprintf '{}\\n'\n",
                dial_log.display(),
                address
            ),
        )
        .unwrap();
        fs::set_permissions(&fabric, fs::Permissions::from_mode(0o700)).unwrap();
        let secret = root.path().join("secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let mut relay = ClientRelay::from_config(&Config {
            node: "owner".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "peer".into(),
                url: "fabric://peer-node/current-tests".into(),
            }],
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        relay.fabric = Some(Fabric::new(fabric));
        let source = Arc::new(Store::open_memory("owner").unwrap());
        relay = relay.with_links(source.clone());
        let record = source
            .append_claim(&ClaimInput {
                subject: "agent/example/seat".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/example/seat".into()),
                fields: serde_json::from_value(
                    serde_json::json!({"state":"working","driver":"codex","incarnation_id":"one"}),
                )
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        for _ in 0..2 {
            relay.publish_current_value(record.clone()).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read_to_string(&dial_log).unwrap().lines().count(), 1);
        assert_eq!(relay.current_connections.lock().unwrap().len(), 1);
        delay.store(500, Ordering::SeqCst);
        let started = std::time::Instant::now();
        relay.publish_current_value(record.clone()).await;
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_millis(400));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "no retry inside one fleet attempt"
        );
        assert!(relay.current_connections.lock().unwrap().is_empty());
        delay.store(0, Ordering::SeqCst);
        relay.publish_current_value(record.clone()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(fs::read_to_string(&dial_log).unwrap().lines().count(), 2);
        source.record_replication_worker("peer", smallclaims::replication::ReplicationWorkerStatus {
            phase: "backoff".into(), last_attempt_at_unix_ms: u128::from(st_drivers::message::now_ms()),
            next_retry_at_unix_ms: Some(u128::from(st_drivers::message::now_ms()) + 30_000),
        });
        relay.publish_current_value(record.clone()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4, "current hints respect offline backoff");
        source.record_replication_worker("peer", smallclaims::replication::ReplicationWorkerStatus {
            phase: "idle".into(), ..Default::default()
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 4, "recovery does not replay a dropped hint");
        relay.publish_current_value(record).await;
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        server.abort();
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
        FleetContext::member(store, own, bootstrap, config_peers, legacy)
    }

    include!("peer/raw_terminal_tests.rs");

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
        let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });

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
        let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
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
        let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
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
        assert_eq!(body["details"]["reason"], "no-route", "{body}");
        assert_eq!(body["details"]["owner_host_id"], "host/elsewhere", "{body}");
        assert!(
            !body["message"].as_str().unwrap().contains("temporarily"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn an_attachment_travels_from_the_member_that_took_it_and_never_through_sync() {
        use crate::blobs::BlobDir;
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
        let gateway_store = gateway.store.clone();
        let owner_store = owner.store.clone();
        let owner_socket = owner_root.path().join("st3.sock");
        let main_socket = owner_socket.clone();
        let owner_app = crate::api::router(owner);
        tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
        let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
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
        let gateway_socket = gateway_root.path().join("st3.sock");
        let serving = gateway_socket.clone();
        let served = gateway_app.clone();
        tokio::spawn(async move { crate::api::serve_unix(&serving, served).await });

        for _ in 0..100 {
            if owner_socket.exists() && gateway_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Pasted on the owner, 1.3 MiB: three relayed chunks.
        let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
        image.extend((0..1_300_000_u32).map(|index| (index % 251) as u8));
        let hash = hex::encode(Sha256::digest(&image));
        let uploaded = st3_client::Client::unix_as(&owner_socket, "person/avery")
            .upload_blob(image.clone(), "image/png")
            .await
            .unwrap()
            .value;
        assert_eq!(uploaded.blob, format!("blob/{hash}"));
        let message = crate::client::Client::unix(&owner_socket)
            .send_message(&crate::model::MessageSendRequest {
                idempotency_key: "pasted-image".into(),
                from: "person/avery".into(),
                to: "agent/gateway-node.seat".into(),
                content: "look at this".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: vec![crate::model::AttachmentInput {
                    blob: uploaded.blob.clone(),
                    media_type: "image/png".into(),
                    name: Some("paste.png".into()),
                }],
            })
            .await
            .unwrap()
            .message;
        assert_eq!(message.attachments[0].origin, "host/owner-node");

        // Sync carries the claim to the gateway; the bytes are not part of it.
        for claim in owner_store.claims_for(&message.subject, Some("message.sent")).unwrap() {
            gateway_store
                .append_claim(&crate::model::ClaimInput {
                    subject: message.subject.clone(),
                    kind: "message.sent".into(),
                    actor: Some("person/avery".into()),
                    fields: claim.body["fields"]
                        .as_object()
                        .unwrap()
                        .clone()
                        .into_iter()
                        .collect(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some("pasted-image".into()),
                })
                .unwrap();
        }
        assert!(!BlobDir::under(gateway_root.path()).path(&hash).exists());

        let read = |person: &'static str, message: String| {
            let app = gateway_app.clone();
            let hash = hash.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::get(format!("/v1/client/blobs/{hash}?message={message}"))
                            .header("x-st3-person", person)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                (status, to_bytes(response.into_body(), usize::MAX).await.unwrap())
            }
        };
        // A seat on the gateway gets the file written where its harness can open it.
        let seat_files = gateway_root.path().join("seat/attachments");
        let delivered = gateway_store.message(&message.subject).unwrap().unwrap();
        let notices = crate::blobs::materialize(
            &gateway_socket,
            "agent/gateway-node.seat",
            &seat_files,
            &delivered,
        )
        .await
        .unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].name.as_deref(), Some("paste.png"));
        assert_eq!(fs::read(notices[0].path.as_deref().unwrap()).unwrap(), image);
        assert!(notices[0].path.as_deref().unwrap().ends_with(&format!("{hash}.png")));
        fs::remove_file(BlobDir::under(gateway_root.path()).path(&hash)).unwrap();
        let (status, bytes) = read("person/avery", message.subject.clone()).await;
        assert!(status.is_success(), "{}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
        assert_eq!(bytes.as_ref(), image.as_slice());
        assert_eq!(
            BlobDir::under(gateway_root.path()).read(&hash).unwrap().unwrap(),
            image,
            "the gateway keeps a copy for its own readers"
        );
        // The gateway's graph holds a reference, never the bytes.
        assert!(gateway_store.get_blob(&hash).unwrap().is_none());

        // When the owner's copy is gone and the gateway's too, the read says so.
        fs::remove_file(BlobDir::under(owner_root.path()).path(&hash)).unwrap();
        fs::remove_file(BlobDir::under(gateway_root.path()).path(&hash)).unwrap();
        let (status, bytes) = read("person/avery", message.subject.clone()).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["code"], "blob-expired");
        // An owner that cannot be reached is a different answer.
        let message_elsewhere = {
            let mut fields = owner_store
                .claims_for(&message.subject, Some("message.sent"))
                .unwrap()[0]
                .body["fields"]
                .clone();
            fields["attachments"][0]["origin"] = "host/offline-node".into();
            gateway_store
                .append_claim(&crate::model::ClaimInput {
                    subject: "message/offline".into(),
                    kind: "message.sent".into(),
                    actor: Some("person/avery".into()),
                    fields: fields.as_object().unwrap().clone().into_iter().collect(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some("pasted-image-elsewhere".into()),
                })
                .unwrap()
                .subject
        };
        let (status, bytes) = read("person/avery", message_elsewhere).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["code"], "remote-unavailable");
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
            let peer = PeerState::new(MainBackend::new(PathBuf::from(socket)), node.into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from([accepts.into()])));
            async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
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

        // The gateway says how it got a screen: through the relay, not straight from the owner.
        // This stand-in session never answers a stats query, so the owner has no facts to give
        // and the screen still arrives.
        let screen = client
            .terminal_screen_with_facts(&attachment.terminal_id)
            .await
            .unwrap()
            .value;
        let provenance = screen
            .relay
            .expect("a relayed screen carries its provenance");
        assert_eq!(provenance.owner_host_id, "host/chain-owner");
        assert_eq!(provenance.via, "host/chain-relay");
        assert!(!provenance.direct);
        assert_eq!(provenance.transport, "http");
        assert_eq!(provenance.capability_ttl_s, 300);
        assert_eq!(provenance.fence_conflicts, 0);
        assert!(screen.facts.is_none());
    }

    #[tokio::test]
    async fn a_gateway_lists_a_remote_agents_messages_from_its_owner_and_says_when_it_cannot() {
        let roots = [(); 2].map(|()| tempfile::tempdir().unwrap());
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
        let mut gateway = make_state(roots[0].path(), "lag-gateway");
        let owner = make_state(roots[1].path(), "lag-owner");
        let agent = "agent/lag-worker";
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("lag-runtime".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("lag-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("lag-worker-running".into()),
            })
            .unwrap();
        gateway
            .store
            .import_replication("lag-owner", &owner.store.export_replication(0).unwrap())
            .unwrap();
        // The message reaches the owner, and has not replicated to the gateway yet.
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "message/lag-first".into(),
                kind: "message.sent".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String(agent.into())),
                    ("to".into(), Value::String("person/avery".into())),
                    (
                        "content".into(),
                        Value::String("done, ready for review".into()),
                    ),
                    ("status".into(), Value::String("sent".into())),
                    ("title".into(), Value::String("done".into())),
                    ("in_reply_to".into(), Value::Null),
                    ("tags".into(), Value::Array(Vec::new())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("lag-first".into()),
            })
            .unwrap();

        let secret = roots[0].path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let sockets = roots.each_ref().map(|root| root.path().join("st3.sock"));
        let peer = PeerState::new(
            MainBackend::new(sockets[1].clone()),
            "lag-owner".into(),
            FleetAuth::test("fleet-test", &[7; 32]),
            FleetContext::legacy(BTreeSet::from(["lag-gateway".into()])),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let worker = tokio::spawn(async move {
            axum::serve(listener, peer_router(peer, smalltalk_routes())).await
        });
        gateway.client_relay = ClientRelay::from_config(&Config {
            node: "lag-gateway".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "lag-owner".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap();
        for (socket, state) in sockets.iter().zip([gateway, owner]) {
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
        let page = client
            .messages_list_for_peer(agent, None, Some(20), false)
            .await
            .unwrap()
            .value;
        assert_eq!(page.items.len(), 1, "the owner's list reaches the gateway");
        let replicated = page
            .replicated
            .expect("a remote agent's list says where it came from");
        assert_eq!(replicated.owner_host_id, "host/lag-owner");
        assert_eq!(replicated.source, "owner");
        assert!(replicated.complete);
        assert_eq!(replicated.state, "current");

        // With the owner out of reach the gateway shows what it has and says the list may be
        // missing items; it never passes an empty replica off as the whole list.
        worker.abort();
        let _ = worker.await;
        let page = client
            .messages_list_for_peer(agent, None, Some(20), false)
            .await
            .unwrap()
            .value;
        assert!(page.items.is_empty());
        let replicated = page
            .replicated
            .expect("a replica says it is not the owner's list");
        assert_eq!(replicated.source, "replica");
        assert!(!replicated.complete);
        assert_eq!(replicated.state, "unverified");
        assert!(replicated.reason.is_some(), "{replicated:?}");

        // An agent on this host, or any list that names no remote agent, carries no notice.
        let page = client
            .messages_list_for_recipient("person/avery", None, Some(20), false)
            .await
            .unwrap()
            .value;
        assert!(page.replicated.is_none());
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
        let peer = PeerState::new(MainBackend::new(socket.to_path_buf()), "middle".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["near".into()])));
        let send = |route: ClientReadRoute| {
            let body = serde_json::to_vec(&ClientReadRequest {
                authority_actor: "person/test".into(),
                request: ClientReadOperation::TerminalScreen {
                    terminal_id: "terminal/agent/far".into(),
                    facts: false,
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
            let router = peer_router(peer.clone(), smalltalk_routes());
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
                &links(&[("desktop", "server"), ("laptop", "desktop")]),
                false
            ),
            names(&["desktop"])
        );
        assert_eq!(
            client_read_next_hops(
                "laptop",
                "server",
                &names(&["desktop"]),
                &names(&["laptop"]),
                &links(&[("server", "desktop")]),
                false
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
                &links(&[("b", "d"), ("c", "b"), ("island", "elsewhere")]),
                false
            ),
            names(&["d", "b", "c"])
        );
        // An owner this node has not exchanged with lately is tried after the peers that
        // reach it, rather than waiting out a read's whole timeout first (#898).
        assert_eq!(
            client_read_next_hops(
                "a",
                "d",
                &names(&["d", "c", "b", "island"]),
                &names(&["a"]),
                &links(&[("b", "d"), ("c", "b"), ("island", "elsewhere")]),
                true
            ),
            names(&["b", "c", "d"])
        );
        // A path through a node the read already passed is no path at all.
        assert_eq!(
            client_read_next_hops(
                "b",
                "d",
                &names(&["a", "c"]),
                &names(&["a", "b"]),
                &links(&[("a", "d"), ("c", "a")]),
                false
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
                &links(&[("a", "c")]),
                false
            ),
            names(&["c"])
        );
    }

    #[tokio::test]
    async fn a_followed_conversation_says_why_while_its_owner_cannot_be_reached() {
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
        // The owner is a configured peer, but nothing answers at its address.
        let address = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
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
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(&gateway_socket)
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = st3_client::Client::unix_as(&gateway_socket, "person/example");
        let mut stream = client.collection_stream().await.unwrap();
        stream
            .subscribe_conversation("conversation", agent)
            .await
            .unwrap();
        // The subscription stays (st retries it) and says why, so a client showing its last
        // copy can say that copy is stale.
        let event = tokio::time::timeout(Duration::from_secs(20), stream.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let st3_client::CollectionEvent::Resync { id, code, message } = event else {
            panic!("expected a resync, got {event:?}");
        };
        assert_eq!(id, "conversation");
        assert_eq!(code, Some(st3_client::ErrorCode::RemoteUnavailable));
        assert!(
            message.is_some_and(|message| message.contains("conversation-owner")),
            "the message names the owner"
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
        let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "conversation-owner".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["conversation-gateway".into()])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
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
        let peer = PeerState::new(MainBackend::new(socket.to_path_buf()), "owner".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["source".into()])));
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
        let response = peer_router(peer.clone(), smalltalk_routes()).oneshot(request).await.unwrap();
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
        let response = peer_router(peer, smalltalk_routes()).oneshot(rejected).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        server.abort();
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
        let state = PeerState::new(Local(target.clone()), "target".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["source".into()])));
        let server = tokio::spawn(axum::serve(listener, peer_router(state, Router::new())).into_future());
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

    /// A node that joins after a trim sees the checkpoint in its peer's inventory, fetches the
    /// manifest from that peer, adopts it, and ends with the same inventory as the peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_node_adopts_the_checkpoint_its_peer_advertises() {
        use crate::store::{CheckpointContext, newest_due_cut};
        const DAY_MS: u128 = 24 * 60 * 60 * 1_000;
        let fleet = "3c2b6f0e-4d1a-4f7e-9a53-2e8b1c0d7f64";
        let auth = FleetAuth::test(fleet, &[8; 32]);
        let scratch = tempfile::tempdir().unwrap();
        let context = CheckpointContext {
            now_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                + 3 * DAY_MS,
            configured_peers: Vec::new(),
            scratch: scratch.path().to_path_buf(),
            reviewer: "person/operator".into(),
        };
        let [alder, birch] = ["alder", "birch"].map(|name| {
            let store = Arc::new(Store::open_memory(name).unwrap());
            store.bind_fleet(fleet).unwrap();
            store
        });
        let sync = || {
            for (source, target) in [(&alder, &birch), (&birch, &alder)] {
                let exchange = source
                    .export_replication_exchange(fleet, &target.replication_inventory().unwrap())
                    .unwrap();
                target
                    .receive_replication_exchange(source.origin(), fleet, &exchange)
                    .unwrap();
                target.validate_replication_backlog().unwrap();
                target.project_replication_backlog().unwrap();
            }
        };
        for (store, count) in [(&alder, 6), (&birch, 4)] {
            for index in 0..count {
                store
                    .append_claim(&ClaimInput {
                        subject: format!("daemon/{}", store.origin()),
                        kind: "daemon.diagnostic".into(),
                        actor: None,
                        fields: BTreeMap::from([
                            ("severity".into(), Value::String("warning".into())),
                            ("code".into(), Value::String("slow-request".into())),
                            ("reason".into(), Value::String(format!("slow {index}"))),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("{}-slow-{index}", store.origin())),
                    })
                    .unwrap();
            }
        }
        sync();
        // Seal, verify, then trim on both.
        for _ in 0..3 {
            for store in [&alder, &birch] {
                store.checkpoint_step(&context).unwrap();
            }
            sync();
        }
        let advertised = alder.trimmed_checkpoint().unwrap().expect("alder trimmed");
        assert_eq!(
            birch.trimmed_checkpoint().unwrap().as_ref(),
            Some(&advertised)
        );
        assert_eq!(advertised.cut_unix_ms, newest_due_cut(context.now_unix_ms));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = PeerState::new(Local(alder.clone()), "alder".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["cedar".into()])));
        let server = tokio::spawn(axum::serve(listener, peer_router(state, Router::new())).into_future());
        let peer = PeerConfig {
            name: "alder".into(),
            url: format!("http://{address}"),
        };
        let cedar = Arc::new(Store::open_memory("cedar").unwrap());
        cedar.bind_fleet(fleet).unwrap();
        let http = replication_http_client();
        let dialer = FleetContext::legacy(BTreeSet::from(["alder".into()]));
        for _ in 0..10 {
            let moved = exchange(
                &http,
                &Local(cedar.clone()),
                "cedar",
                &peer,
                &auth,
                &dialer,
            )
            .await
            .unwrap()
            .0;
            if !moved {
                break;
            }
        }
        assert_eq!(
            cedar.trimmed_checkpoint().unwrap().as_ref(),
            Some(&advertised)
        );
        assert_eq!(cedar.checkpoint_manifest_need().unwrap(), None);
        let status = |store: &Store| {
            let status = store.replication_status(true, Some(fleet), &[]).unwrap();
            (status.authority_digest, status.graph_digest)
        };
        assert_eq!(status(&cedar), status(&alder));
        assert_eq!(
            cedar
                .checkpoint_manifest(&advertised.id, advertised.cut_unix_ms)
                .unwrap(),
            alder
                .checkpoint_manifest(&advertised.id, advertised.cut_unix_ms)
                .unwrap()
        );
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
        let state = PeerState::new(Local(source.clone()), "source".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["target".into()])));
        let server = tokio::spawn(async move { axum::serve(listener, peer_router(state, Router::new())).await });
        let peer = PeerConfig {
            name: "source".into(),
            url: format!("http://{address}"),
        };
        let backend = Local(target.clone());
        let context = FleetContext::legacy(BTreeSet::from(["source".into()]));
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
            crate::store::configure_projection_writer(&connection).unwrap();
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
            heal_now = exchange(&http, &backend, "target", &peer, &auth, &context)
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
        heal(&backend, "target", &peer, &auth, &context).await;

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
            .append_legacy_claim(&ClaimInput {
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
        let backend = MainBackend::new(socket);
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
        assert_eq!(down.body["fields"]["status"], "unknown");
        assert_eq!(
            store
                .claims_for("host/source", Some("transport.observed"))
                .unwrap()
                .len(),
            before
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
        let state = PeerState::new(MainBackend::new(sockets[1].to_path_buf()), "target".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["source".into()])));
        let peer_server =
            tokio::spawn(async move { axum::serve(listener, peer_router(state, smalltalk_routes())).await });
        let peer = PeerConfig {
            name: "target".into(),
            url: format!("http://{address}"),
        };
        let backend = MainBackend::new(sockets[0].to_path_buf());
        let context = FleetContext::legacy(BTreeSet::from(["target".into()]));
        let http = replication_http_client();
        exchange(&http, &backend, "source", &peer, &auth, &context)
            .await
            .unwrap();
        target
            .receive_current_value(&source.own_transport_value("target").unwrap().unwrap())
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
        exchange(&http, &backend, "source", &peer, &auth, &context)
            .await
            .unwrap();
        source
            .receive_current_value(&target.own_transport_value("source").unwrap().unwrap())
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

}
