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
    ReplicationExportResponse, ReplicationInventory, ReplicationPeerFailureRequest, ReplicationReceiveRequest, ReplicationReceiveResponse,
};
use crate::store::Store;
use crate::relay_trace::{self, Outcome, Phase};
use crate::store::{
    CheckpointAction, CheckpointManifest, CheckpointManifestNeed, CheckpointManifestPage,
    CheckpointManifestRequest,
};
mod raw_lease;

/// The peer listener's shared state, with this node's daemon as the store.
type PeerState = smallclaims::sync::PeerState<MainBackend>;

pub const CLIENT_READ_PATH: &str = "/v1/peer/client-read";
pub const RAW_TERMINAL_PATH: &str = "/v1/peer/raw-terminal";

/// The header that names the gateway a raw PEEK route came from.
pub const RAW_ORIGIN_HEADER: &str = "x-st3-raw-origin";

/// What binds a raw PEEK route to the lease its gateway holds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawLeaseBinding {
    pub lease_id: String,
    pub gateway: String,
    pub gateway_epoch: String,
    pub actor: String,
    pub person: String,
    pub authorization_epoch: String,
    pub grant_subject: Option<String>,
    pub grant_digest: Option<String>,
    pub gateway_member_key: Option<String>,
    pub owner_member_key: Option<String>,
    pub terminal: String,
    pub owner: String,
    pub incarnation: String,
    pub mode: String,
    pub absolute_deadline_unix_ms: u128,
}

/// A gateway's open PEEK lease as the relay checks it. The API's raw terminal lease implements
/// it, so the relay carries a lease without naming the API.
pub trait RawTerminalLease: Send + Sync {
    fn binding(&self) -> &RawLeaseBinding;
    fn revalidate(&self) -> Result<()>;
    fn check(&self) -> Result<()>;
    fn proof(&self, epoch: &str, sequence: u64) -> Result<()>;
    fn expired(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// Pumps an accepted raw terminal websocket into the owner's terminal socket. The daemon binary
/// passes the API's splice to `run_worker`, so the peer listener serves the route without naming
/// the API.
pub type RawSplice = fn(
    axum::extract::ws::WebSocket,
    tokio::net::UnixStream,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
tokio::task_local! {
    /// Set while a request is handled; a read relayed from another machine's owner marks it.
    static SERVED_REMOTE: Arc<std::sync::atomic::AtomicBool>;
}

/// Run a request's handler with its remote-read flag in scope. The flag follows the handler's
/// own task. A relay awaited inside a `tokio::spawn` would lose it, so such a spawn must pass
/// its own flag, as the collections socket does for the conversation follower.
pub async fn track_remote_reads<F: Future>(
    flag: Arc<std::sync::atomic::AtomicBool>,
    handler: F,
) -> F::Output {
    SERVED_REMOTE.scope(flag, handler).await
}

/// The request being handled read from another machine, so its target is the remote one.
pub fn note_remote_read() {
    let _ = SERVED_REMOTE.try_with(|remote| remote.store(true, std::sync::atomic::Ordering::Relaxed));
}
pub const MAX_CLIENT_READ_BYTES: usize = 1_048_576;
/// A relayed long poll must answer well inside the relay's 15-second request timeout.
const CLIENT_READ_MAX_WAIT_MS: u64 = 10_000;
/// How long one hop waits for an owner's answer beyond the read's own long poll.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// The most nodes a client read may be forwarded through on its way to its owner.
pub const CLIENT_READ_MAX_HOPS: u8 = 4;
/// The peer HTTP client allows this extra wait per hop. The daemon's forwarded-read
/// envelope independently caps local work at 15 seconds plus the operation wait; an
/// earlier relay can therefore return its generic deadline before the owner's reply.
const CLIENT_READ_HOP_MARGIN: Duration = Duration::from_secs(10);
/// How long a relay reuses the fleet's observed links before it reads them again.
pub const CLIENT_READ_LINKS_TTL: Duration = Duration::from_secs(5);
/// How long an owner's stale-fence refusal counts as a recent fence conflict.
const CLIENT_READ_FENCE_WINDOW: Duration = Duration::from_secs(60);
pub use crate::client::CLIENT_READ_FORWARD_PATH;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ClientReadOperation {
    ConversationContent {
        session_id: String,
        reference: String,
        offset: u64,
    },
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
    pub fn wait(&self) -> Duration {
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
    /// `timed-out`, `refused`, `hop-limit`, `transport-error`, `owner-error` or `dial-out-owner`.
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

    pub fn dial_out_owner(host: &str) -> Self {
        let mut rejected = Self::unreachable(
            "dial-out-owner",
            format!(
                "unreachable: dial-out owner {host} has no inbound route; cached data remains usable"
            ),
        );
        rejected.details.insert("owner_host_id".into(), host.into());
        rejected
            .details
            .insert("attempts".into(), serde_json::json!([]));
        rejected
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
type ObservedMembership = (std::time::Instant, Arc<FleetView>);

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
    pub peers: Vec<PeerConfig>,
    auth: FleetAuth,
    http: reqwest::Client,
    /// The store whose replicated transport observations say which nodes reach which.
    links: Option<Arc<Store>>,
    /// The links last read from that store, and when, so a busy gateway reads them rarely.
    observed: Arc<std::sync::Mutex<Option<ObservedLinks>>>,
    /// Reuse sealed membership across per-item reachability checks for the same short TTL.
    pub membership: Arc<std::sync::Mutex<Option<ObservedMembership>>>,
    pub fabric: Option<Fabric>,
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
    pub fn fleet_view(&self) -> Arc<FleetView> {
        let Some(store) = &self.links else {
            return Arc::default();
        };
        let mut cached = self
            .membership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((read_at, view)) = cached.as_ref()
            && read_at.elapsed() < CLIENT_READ_LINKS_TTL
        {
            return view.clone();
        }
        let view = Arc::new(store.fleet_view_sealed().unwrap_or_default());
        *cached = Some((std::time::Instant::now(), view.clone()));
        view
    }

    pub fn is_dial_out_owner(&self, host: &str) -> bool {
        host.strip_prefix("host/").is_some_and(|name| {
            self.fleet_view().members.iter().any(|member| {
                member.name == name && member.state == "current" && member.mode == "dial-out"
            })
        })
    }

    /// Choose routes by the transport observations in this store.
    pub fn with_links(mut self, store: Arc<Store>) -> Self {
        self.links = Some(store);
        self
    }

    pub fn legacy_authority(&self) -> bool {
        self.legacy
    }

    fn accept_raw_watcher(&self, sender: &crate::fleet::Sender) -> Result<()> {
        let view = self
            .links
            .as_ref()
            .context("raw watch membership authority unavailable")?
            .fleet_view_for_client()?;
        crate::fleet::accept(
            &view,
            sender,
            self.peers.iter().any(|peer| peer.name == sender.name),
            self.legacy,
        )
        .map_err(|refusal| anyhow::anyhow!("raw watcher refused: {refusal:?}"))?;
        Ok(())
    }

    /// Whether a read for this host has somewhere to go: the host itself, or a peer that can
    /// carry it on.
    pub fn reaches(&self, host_id: &str) -> bool {
        host_id.strip_prefix("host/").is_some_and(|name| {
            name != self.node
                && !self.is_dial_out_owner(host_id)
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
    pub fn next_hops(&self, target: &str, visited: &[String]) -> Vec<PeerConfig> {
        let view = self.fleet_view();
        // Sync from a dial-out member supplies no reverse owner-RPC or PTY transport.
        // In particular, dropping its old links must not enable the unseen-owner fallback.
        if view.members.iter().any(|member| {
            member.name == target && member.state == "current" && member.mode == "dial-out"
        }) {
            return Vec::new();
        }
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
            membership: Arc::default(),
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
        note_remote_read();
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
        let host = format!("host/{target}");
        let next_hops = relay_trace::result(Phase::Route, || -> Result<_> {
            if self.is_dial_out_owner(&host) {
                return Err(ClientReadRejected::dial_out_owner(&host).into());
            }
            Ok(self.next_hops(target, &path))
        })?;
        let mut attempts = Vec::new();
        let mut last_reason = None;
        for (ordinal, peer) in next_hops.into_iter().enumerate() {
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
            let diagnostic = relay_trace::current().map(|trace| {
                trace.attempt(
                    u8::try_from(ordinal).unwrap_or(u8::MAX),
                    if peer.url.starts_with("fabric://") {
                        "fabric"
                    } else {
                        "http"
                    },
                )
            });
            if let Some(trace) = &diagnostic { trace.selected_peer(&peer.name); }
            let sent = relay_trace::scope(
                diagnostic,
                relay_trace::future(Phase::Attempt, self.send(&peer, &outgoing)),
            )
            .await;
            match sent {
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
                if let Some(trace) = relay_trace::current() { trace.fabric_target(&node, &protocol); }
                let address = relay_trace::future(Phase::FabricDial, async {
                    match self.fabric.as_ref() {
                        Some(fabric) => fabric.dial(&node, &protocol).await,
                        None => Err(anyhow::anyhow!("Fabric is unavailable")),
                    }
                })
                .await
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
        if let Some(trace) = relay_trace::current() {
            trace.bind(&digest);
        }
        let headers = self
            .auth
            .request_headers_for(CLIENT_READ_PATH, &self.node, &body)?;
        let mut headers_span = relay_trace::span(Phase::Headers);
        let response = self
            .http
            .post(format!("{}{}", url.trim_end_matches('/'), CLIENT_READ_PATH))
            .headers(headers)
            .header("content-type", "application/json")
            .timeout(timeout)
            .body(body)
            .send()
            .await;
        headers_span.finish(match &response {
            Ok(_) => Outcome::Completed,
            Err(error) if error.is_timeout() => Outcome::TimedOut,
            Err(_) => Outcome::Failed,
        });
        let mut response = response.map_err(|error| {
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
        if let Some(trace) = relay_trace::current() {
            trace.http_status(status.as_u16());
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ClientReadRejected::unreachable(
                "refused",
                format!("peer {name} does not accept this node's client reads"),
            )
            .into());
        }
        let response_headers = response.headers().clone();
        let mut body_span = relay_trace::span(Phase::Body);
        let bytes: Result<Vec<u8>> = async {
            anyhow::ensure!(
                response
                    .content_length()
                    .is_none_or(|length| length <= MAX_CLIENT_READ_BYTES as u64),
                "the peer client read response exceeds its bound"
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                body_span.finish(if error.is_timeout() {
                    Outcome::TimedOut
                } else {
                    Outcome::Failed
                });
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
            Ok(bytes)
        }
        .await;
        body_span.finish(if bytes.is_ok() {
            Outcome::Completed
        } else {
            Outcome::Failed
        });
        let bytes = bytes?;
        relay_trace::result(Phase::Verify, || {
            self.auth.verify(
                &response_headers,
                "RESPONSE",
                CLIENT_READ_PATH,
                &bytes,
                Some(name),
                Some(&digest),
            )
        })?;
        let envelope: ApiResponse<serde_json::Value> =
            relay_trace::result(Phase::Decode, || -> Result<_> {
                let envelope: ApiResponse<serde_json::Value> = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(
                    envelope.api_version == "st3.v1",
                    "the peer client read protocol differs"
                );
                Ok(envelope)
            })?;
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
        lease: Option<Arc<dyn RawTerminalLease>>,
    ) -> Result<(tokio::net::UnixStream, tokio::sync::mpsc::Sender<Value>)> {
        let target = host.strip_prefix("host/").context("invalid owner host")?;
        if self.is_dial_out_owner(host) {
            return Err(ClientReadRejected::dial_out_owner(host).into());
        }
        let mode = match mode {
            st3_client::RawTerminalMode::Attach => "attach",
            st3_client::RawTerminalMode::Peek => "peek",
        };
        let mut path = format!(
            "{RAW_TERMINAL_PATH}?person={}&terminal={}&incarnation={}&mode={mode}",
            urlencoding::encode(person),
            urlencoding::encode(terminal_id),
            urlencoding::encode(incarnation)
        );
        if let Some(lease) = &lease {
            lease.revalidate()?;
            path.push_str("&binding=");
            path.push_str(&urlencoding::encode(&serde_json::to_string(
                &lease.binding(),
            )?));
        }
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
                    tcp.set_nodelay(true)?;
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
                raw_lease::gateway_bridge(socket, lease.clone(), self.clone(), peer.name.clone(), sender.member_key)
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
pub fn client_read_next_hops(
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
    binding: Option<String>,
}

async fn receive_raw_terminal(
    websocket: WebSocketUpgrade,
    State(state): State<PeerState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<RawTerminalQuery>,
    axum::Extension(splice): axum::Extension<RawSplice>,
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
    if query.mode == st3_client::RawTerminalMode::Peek {
        let binding: RawLeaseBinding = match query
            .binding
            .as_deref()
            .and_then(|binding| serde_json::from_str(binding).ok())
        {
            Some(binding) => binding,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "PEEK route requires original lease binding",
                )
                    .into_response();
            }
        };
        if binding.gateway_member_key != _sender.member_key
            || binding.owner_member_key.as_deref() != state.auth().member_key()
            || binding.gateway != format!("host/{}", _sender.name)
            || binding.owner != format!("host/{}", state.node())
            || binding.person != query.person
            || binding.terminal != query.terminal
            || binding.incarnation != query.incarnation
            || binding.mode != "peek"
        {
            return (StatusCode::FORBIDDEN, "raw lease route binding differs").into_response();
        }
        return raw_lease::receive_owner(
            websocket,
            state,
            path.to_owned(),
            binding,
            _sender.member_key,
        )
        .await;
    }
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
        .on_upgrade(move |socket| splice(socket, transport));
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
    let authentication_clock = relay_trace::peer_authentication_clock();
    let sender =
        match state
            .auth()
            .verify_sender(&headers, "POST", CLIENT_READ_PATH, &body, None, None)
        {
            Ok(sender) if state.accept(&sender).is_ok() => sender.name,
            _ => return (StatusCode::UNAUTHORIZED, "untrusted fleet client read").into_response(),
        };
    let authentication_elapsed = authentication_clock.map(|clock| clock.elapsed());
    state.note_activity(&sender);
    let request_digest = FleetAuth::body_digest(&body);
    let mut diagnostic = None;
    let mut diagnostic_root = None;
    let result: Result<serde_json::Value> = async {
        anyhow::ensure!(
            body.len() <= 16_384,
            "the client read request exceeds its bound"
        );
        let request: ClientReadRequest = serde_json::from_slice(&body)?;
        if matches!(&request.request, ClientReadOperation::Timeline { .. }) {
            diagnostic = relay_trace::authenticated_peer(&request_digest, &sender);
            diagnostic_root = diagnostic.as_ref().map(relay_trace::Trace::guard);
            if let (Some(trace), Some(elapsed)) = (&diagnostic, authentication_elapsed) {
                trace.authenticated_after(elapsed);
            }
        }
        relay_trace::scope(diagnostic.clone(), relay_trace::future(Phase::Request, async {
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
                return relay_trace::future(Phase::ForwardCall, forward_client_read(&state, &request)).await;
            }
        }
        let client = st3_client::Client::unix_as(state.backend().socket(), &request.authority_actor);
        match request.request {
            ClientReadOperation::ConversationContent {
                session_id,
                reference,
                offset,
            } => Ok(serde_json::to_value(
                client.conversation_content_chunk(&session_id, &reference, offset).await?.value,
            )?),
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
                let answer = relay_trace::future(Phase::OwnerCall,
                    client.timeline(&session_id, cursor.as_deref(), Some(limit))).await;
                if let Some(trace) = relay_trace::current() {
                    match &answer {
                        Ok(envelope) => trace.response(&envelope.request_id),
                        Err(st3_client::ClientError::Api(_, _, error)) => trace.response(&error.request_id),
                        _ => {},
                    }
                }
                let value = answer?.value;
                relay_trace::result(Phase::Envelope, || Ok(serde_json::to_value(value)?))
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
        })).await
    }
    .await;
    let response = match result {
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
                                | st3_client::ErrorCode::ConversationContentInvalidated
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
    };
    if let Some(trace) = &diagnostic {
        trace.finish(if response.status().is_success() {
            Outcome::Completed
        } else {
            Outcome::Failed
        });
    }
    drop(diagnostic_root);
    response
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
                    authority_guard: false,
                    signature_requests: signature_requests.to_vec(),
                },
            )
            .await
    }

    async fn guard_summary(&self, fleet_id: &str) -> Result<ReplicationExportResponse> {
        // This marker goes only to our local daemon, never into a signed peer exchange.
        // Old daemons ignore it and return the same complete summary.
        self.client
            .post(
                "/v1/internal/replication/export",
                &ReplicationExportRequest {
                    fleet_id: fleet_id.to_owned(),
                    inventory: ReplicationInventory::default(),
                    summary_only: true,
                    authority_guard: true,
                    signature_requests: Vec::new(),
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
        carrier: smallclaims::replication::ReplicationCarrier,
    ) -> Result<ReplicationReceiveResponse> {
        self.client
            .post(
                "/v1/internal/replication/receive",
                &ReplicationReceiveRequest {
                    peer: peer.to_owned(),
                    fleet_id: fleet_id.to_owned(),
                    exchange: exchange.clone(),
                    round_trip_ms: round_trip.map(|duration| duration.as_millis() as u64),
                    carrier,
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
                &crate::model::FleetEndpointsRequest {
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

    async fn record_failure(
        &self,
        peer: &str,
        status: &str,
        error: &str,
        carrier: Option<smallclaims::replication::ReplicationCarrier>,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .client
            .post(
                "/v1/internal/replication/peer-failure",
                &ReplicationPeerFailureRequest {
                    peer: peer.to_owned(),
                    status: status.to_owned(),
                    error: error.to_owned(),
                    carrier,
                },
            )
            .await?;
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
pub fn smalltalk_routes(splice: RawSplice) -> Router<PeerState> {
    relay_trace::init();
    Router::new()
        .route(
            CLIENT_READ_PATH,
            post(receive_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .route(RAW_TERMINAL_PATH, get(receive_raw_terminal).layer(axum::Extension(splice)))
}

/// Run the replication worker: sync this node's store, through its daemon, with the fleet.
pub async fn run_worker(config: Config, splice: RawSplice) -> Result<()> {
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
        smalltalk_routes(splice),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_read_through_the_relay_marks_its_request_as_a_remote_read() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        // The only peer refuses connections: the read fails, but it was a read from another
        // machine, so the request that made it counts toward the remote target.
        let relay = ClientRelay::from_config(&Config {
            node: "gateway-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "owner-node".into(),
                url: "http://127.0.0.1:1".into(),
            }],
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        let read = ClientReadRequest {
            authority_actor: "person/example".into(),
            relay: None,
            request: ClientReadOperation::Timeline {
                session_id: "session/example".into(),
                limit: 1,
                cursor: None,
            },
        };
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = track_remote_reads(flag.clone(), relay.read("host/owner-node", &read)).await;
        assert!(result.is_err());
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        // Outside a request there is nothing to mark, and the read still runs.
        assert!(relay.read("host/owner-node", &read).await.is_err());
    }

    use crate::model::ClaimInput;

    

    

    use serde_json::Value;

    use std::collections::BTreeMap;

    

    

    


    

    use smallclaims::sync::{
        FleetContext, PeerState, exchange, fetch_checkpoint_manifest, peer_router,
        replication_http_client,
    };

    use std::collections::BTreeSet;

    use std::os::unix::fs::PermissionsExt as _;

    use tokio::net::TcpListener;



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
            .unwrap();
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

    #[tokio::test]
    async fn a_first_sync_settles_on_claims_and_never_repairs_from_the_peer() {
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
        for _ in 0..4 {
            exchange(&http, &backend, "target", &peer, &auth, &context)
                .await
                .unwrap();
        }
        let status = target
            .replication_status(true, Some(fleet), &["source".into()])
            .unwrap();
        let first = status.first_sync.unwrap();
        assert_eq!(first.state, "verified", "{first:?}");
        assert_eq!(first.message, None, "{first:?}");
        assert_ne!(
            graph(&target),
            graph(&source),
            "the lost claim is not repaired from the peer"
        );
        let desired: u64 = target
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE kind='intent.desired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desired, 0, "no envelope is admitted again to restore the lost claim");
        let heal = http
            .post(format!("http://{address}/v1/peer/heal"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(heal.status(), reqwest::StatusCode::NOT_FOUND);
        server.abort();
    }
}

#[cfg(test)]
mod timeline_relay_tests;
