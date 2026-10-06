//! The sync worker: who it dials and how it backs off, the exchange, heal and checkpoint
//! adoption it runs with each peer, and the signed routes it answers on.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
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
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::{
    Backend, CHECKPOINT_PATH, DEFLATE_MIN_BYTES, EXCHANGE_ENCODING, EXCHANGE_PATH, FleetAuth,
    HEADER_SIGNATURE, HEAL_PATH, JOIN_PATH, MAX_EXCHANGE_BYTES, MAX_JOIN_BYTES, PEER_API_VERSION,
    PeerResponse, accepts_deflate, deflate, deflated, inflate,
};
use crate::fleet::transport::{
    Fabric, FabricGrantRefusal, LocalTransports, Route, bindable_tailnet_addresses,
    default_fabric_protocol, is_tailnet_address, local_addresses, parse_route, resolve_tool,
    routes_from_endpoints, tailscale_addresses,
};
use crate::fleet::{Acceptance, FleetFile, FleetView, MemberKey, PeerConfig, Refusal, Sender};
use crate::replication::{
    InventoryCheckpoint, ReplicationExchange, ReplicationHealAnswer, ReplicationHealQuery,
    ReplicationHealRequest, ReplicationHealStep, ReplicationInventory,
};
use crate::store::{CheckpointManifest, CheckpointManifestPage, CheckpointManifestRequest};

const REPLICATION_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
// Leave five seconds for signing, serialization and transport before the caller's deadline.
const EXCHANGE_RESPONSE_BUDGET: Duration = Duration::from_secs(15);
const EXCHANGE_POLL_BUDGET: Duration = Duration::from_secs(300);
const OVERLOAD_RETRY: Duration = Duration::from_secs(5);
const MAX_EXCHANGE_JOBS: usize = 4;
const MAX_OVERLOAD_POLLS: usize = 60;

struct ExchangeJobs {
    peers: std::sync::Mutex<BTreeMap<String, ExchangeJob>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for ExchangeJobs {
    fn default() -> Self {
        Self {
            peers: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_EXCHANGE_JOBS)),
        }
    }
}

type ExchangeAnswer = Result<crate::replication::ReplicationExportResponse, String>;

struct ExchangeJob {
    digest: String,
    answer: watch::Receiver<Option<ExchangeAnswer>>,
}

const REPLICATION_WAKE_COALESCE: Duration = Duration::from_secs(1);
const PEER_PROBE_INTERVAL: Duration = Duration::from_secs(3);
const PEER_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Recently working HTTP routes can return without any transport presence event. Probe
/// briefly without exporting inventories or writing failures; long absences stay quiet.
const PEER_PROBE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// A heal question can make the peer replay its graph from nothing, which takes 41 seconds on a
/// 2 GB store and longer under load.
const HEAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Questions one heal asks before it gives up until the next.
const HEAL_QUESTION_LIMIT: usize = 48;

/// What every route on the peer listener shares: the store, this node, its fleet auth, and
/// what it knows of the fleet. A runtime adds its own routes with this as their state.
#[derive(Clone)]
pub struct PeerState<B> {
    backend: B,
    node: String,
    auth: FleetAuth,
    fleet: FleetContext,
    outbound_notify: watch::Sender<u64>,
}

impl<B> PeerState<B> {
    pub fn new(backend: B, node: String, auth: FleetAuth, fleet: FleetContext) -> Self {
        Self {
            backend,
            node,
            auth,
            fleet,
            outbound_notify: watch::channel(0).0,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn auth(&self) -> &FleetAuth {
        &self.auth
    }

    /// Whether this node takes requests from `sender`.
    pub fn accept(&self, sender: &Sender) -> Result<Acceptance, Refusal> {
        self.fleet.accept(sender)
    }

    /// Record authenticated traffic from `name`, which interrupts a failure wait for it.
    pub fn note_activity(&self, name: &str) {
        self.fleet.note_activity(name);
    }
}

/// What this worker knows about the fleet: the membership view, its config peers, and whether
/// it still accepts HMAC-only exchanges from config peers.
#[derive(Clone)]
pub struct FleetContext {
    view: Arc<std::sync::RwLock<FleetView>>,
    /// Bumped whenever the view changes, so the dial set follows membership.
    view_changed: watch::Sender<u64>,
    config_peers: BTreeSet<String>,
    configured_fabric_peers: BTreeMap<String, String>,
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
    /// Successful inbound exchanges suppress redundant dials and interrupt failure backoff.
    inbound: Arc<std::sync::RwLock<BTreeMap<String, tokio::time::Instant>>>,
    inbound_changed: watch::Sender<u64>,
    /// Inventory advertised by the last inbound response; graph wakes it covers need no dial.
    inbound_authority: Arc<std::sync::RwLock<BTreeMap<String, String>>>,
    connectivity_changed: watch::Sender<u64>,
    /// Sign of life from Fabric, distinct from a completed replication exchange.
    online: Arc<std::sync::RwLock<BTreeMap<String, tokio::time::Instant>>>,
    /// Authenticated traffic and successful probes interrupt failure waits without waking
    /// healthy anti-entropy exchanges or cancelling their coalescing window.
    activity: Arc<std::sync::RwLock<BTreeMap<String, tokio::time::Instant>>>,
    activity_changed: watch::Sender<u64>,
    exchange_jobs: Arc<ExchangeJobs>,
}

impl FleetContext {
    /// A node without membership: config peers only, as before.
    #[cfg(any(test, feature = "test-support"))]
    pub fn legacy(config_peers: BTreeSet<String>) -> Self {
        Self {
            view: Arc::default(),
            view_changed: watch::channel(0).0,
            config_peers,
            configured_fabric_peers: BTreeMap::new(),
            legacy: true,
            own_key: None,
            bootstrap_keys: BTreeSet::new(),
            transports: Arc::default(),
            fabric: None,
            bootstrap: None,
            removed: Arc::default(),
            state_dir: None,
            inbound: Arc::default(),
            inbound_changed: watch::channel(0).0,
            inbound_authority: Arc::default(),
            connectivity_changed: watch::channel(0).0,
            online: Arc::default(),
            activity: Arc::default(),
            activity_changed: watch::channel(0).0,
            exchange_jobs: Arc::default(),
        }
    }

    /// A member that holds `own`, with the membership `store` projects.
    #[cfg(any(test, feature = "test-support"))]
    pub fn member(
        store: &crate::store::Store,
        own: &MemberKey,
        bootstrap: &[&str],
        config_peers: &[&str],
        legacy: bool,
    ) -> Self {
        Self {
            view: Arc::new(std::sync::RwLock::new(store.fleet_view().unwrap())),
            view_changed: watch::channel(0).0,
            config_peers: config_peers.iter().map(|peer| (*peer).into()).collect(),
            configured_fabric_peers: BTreeMap::new(),
            legacy,
            own_key: Some(own.public().into()),
            bootstrap_keys: bootstrap.iter().map(|key| (*key).into()).collect(),
            transports: Arc::default(),
            fabric: None,
            bootstrap: None,
            removed: Arc::default(),
            state_dir: None,
            inbound: Arc::default(),
            inbound_changed: watch::channel(0).0,
            inbound_authority: Arc::default(),
            connectivity_changed: watch::channel(0).0,
            online: Arc::default(),
            activity: Arc::default(),
            activity_changed: watch::channel(0).0,
            exchange_jobs: Arc::default(),
        }
    }

    pub fn accept(&self, sender: &Sender) -> Result<Acceptance, Refusal> {
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

    pub fn note_activity(&self, name: &str) {
        self.activity
            .write()
            .expect("activity lock poisoned")
            .insert(name.to_owned(), tokio::time::Instant::now());
        self.activity_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Record that a member refused this node for good, so it stops dialing and `st3 doctor`
    /// can say what happened.
    fn mark_removed(&self, reported_by: &str, removed: &RemovedFromFleet) {
        if self.removed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        eprintln!(
            "st3: {reported_by} refused this node ({}): {}; it stops syncing",
            removed.code, removed.message
        );
        if let Some(state_dir) = &self.state_dir
            && let Ok(Some(mut file)) = crate::fleet::FleetFile::load(state_dir)
            && file.removed.is_none()
        {
            // The details go beside fleet.toml first, so its removal always has them.
            let _ = crate::fleet::RemovalNotice {
                reported_by: reported_by.into(),
                code: removed.code.clone(),
                message: removed.message.clone(),
                learned_at_unix_ms: crate::store::now_ms(),
            }
            .save(state_dir);
            file.removed = Some(crate::fleet::FleetRemoval {
                reported_by: reported_by.into(),
                code: removed.code.clone(),
            });
            let _ = file.save(state_dir);
        }
    }
}

/// A signed refusal from a member that names this node's own key as ended.
#[derive(Debug)]
struct RemovedFromFleet {
    code: String,
    /// The refusal's message, which names who removed this node and why when the member knows.
    message: String,
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

/// How one node takes part in sync: its name, its fleet and secret, the peers configured by hand,
/// where it listens, and its `fleet.toml` once it is a member.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub node: String,
    pub fleet_id: String,
    pub secret_file: PathBuf,
    /// Holds `fleet/`; a member that learns it was removed records it there.
    pub state_dir: PathBuf,
    pub peers: Vec<PeerConfig>,
    /// The replication listener, such as `127.0.0.1:31313`. A dial-out member ignores it.
    pub peer_listen: Option<String>,
    pub fleet: Option<FleetFile>,
}

impl WorkerConfig {
    /// A member's worker as its `STATE/fleet/fleet.toml` describes it: the name it joined
    /// under (else `node`), its secret, and a loopback listener on its port.
    pub fn from_fleet_file(state_dir: &Path, node: &str) -> Result<Self> {
        let file = FleetFile::load(state_dir)?
            .with_context(|| format!("{} is missing", FleetFile::path(state_dir).display()))?;
        let peer_listen = match (file.mode, file.port) {
            (crate::fleet::FleetMode::Listening, Some(port)) => Some(format!("127.0.0.1:{port}")),
            _ => None,
        };
        Ok(Self {
            node: file.node.clone().unwrap_or_else(|| node.to_owned()),
            fleet_id: file.fleet_id.clone(),
            secret_file: file.secret_path(state_dir),
            state_dir: state_dir.to_path_buf(),
            peers: Vec::new(),
            peer_listen,
            fleet: Some(file),
        })
    }
}

/// Keep `backend`'s store in step with the fleet until the process ends: dial every listening
/// member, answer members that dial in, heal, adopt checkpoints, and redeem joins. Bump `wake`
/// whenever the store has something new to send. `routes` serve beside the sync routes on the
/// same listener, with the same state.
pub async fn run<B: Backend>(
    config: WorkerConfig,
    backend: B,
    wake: watch::Sender<u64>,
    routes: Router<PeerState<B>>,
) -> Result<()> {
    let fleet_id = config.fleet_id.as_str();
    let secret_file = config.secret_file.as_path();
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
    let fabric = if wants("fabric")
        || config
            .peers
            .iter()
            .any(|peer| peer.url.starts_with("fabric://"))
    {
        resolve_tool(
            config
                .fleet
                .as_ref()
                .and_then(|file| file.fabric.as_deref()),
            "fabric",
        )
        .map(Fabric::new)
    } else {
        None
    };
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
        configured_fabric_peers: config
            .peers
            .iter()
            .filter_map(|peer| match parse_route(&peer.url)? {
                Route::Fabric { node, .. } => Some((node, peer.name.clone())),
                Route::Http(_) => None,
            })
            .collect(),
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
        inbound: Arc::default(),
        inbound_changed: watch::channel(0).0,
        inbound_authority: Arc::default(),
        connectivity_changed: watch::channel(0).0,
        online: Arc::default(),
        activity: Arc::default(),
        activity_changed: watch::channel(0).0,
        exchange_jobs: Arc::default(),
    };
    tokio::spawn(keep_connectivity_current(fleet.clone()));
    if let Some(fabric) = fabric.clone() {
        tokio::spawn(keep_fabric_presence_current(fabric, fleet.clone()));
    }
    backend.ready().await;
    let notify = wake;
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
        fleet.clone(),
        notify,
    );
    let endpoints = Endpoints::default();
    let app = peer_router(state, routes);
    let listening = config
        .fleet
        .as_ref()
        .is_none_or(|file| file.mode == crate::fleet::FleetMode::Listening);
    let listener = match config.peer_listen.as_deref().filter(|_| listening) {
        Some(address) => Some(
            TcpListener::bind(address)
                .await
                .with_context(|| format!("bind the replication listener at {address}"))?,
        ),
        None => None,
    };
    let bound_address = listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok());
    let loopback = bound_address.filter(|address| address.ip().is_loopback());
    let tailnet = bound_address.filter(|address| is_tailnet_address(&address.ip()));
    if let Some(file) = &config.fleet {
        if let (Some(address), true) = (loopback, file.advertise_loopback) {
            endpoints.set_loopback(Some(address));
        }
        if let Some(address) = tailnet {
            endpoints.update(|set| set.tailscale = vec![address]);
        }
        if let Some(tailscale) = tailscale {
            tokio::spawn(keep_tailnet_current(
                tailscale,
                bound_address.map(|address| (address.port(), app.clone())),
                tailnet.map(|address| address.ip()),
                endpoints.clone(),
                fleet_transports,
                notify_for_transports,
                fleet.connectivity_changed.subscribe(),
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
                fleet.connectivity_changed.subscribe(),
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
    use axum::serve::ListenerExt as _;
    let listener = listener.tap_io(|tcp| {
        if let Err(error) = tcp.set_nodelay(true) {
            tracing::warn!(%error, "could not disable Nagle on peer socket");
        }
    });
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
    already_bound: Option<std::net::IpAddr>,
    endpoints: Endpoints,
    transports: Arc<std::sync::RwLock<LocalTransports>>,
    notify: watch::Sender<u64>,
    mut connectivity: watch::Receiver<u64>,
) {
    let mut bound = already_bound.into_iter().collect::<BTreeSet<_>>();
    loop {
        let bindable = match tailscale_addresses(&tailscale).await {
            Ok(reported) => bindable_tailnet_addresses(&reported, &local_addresses()),
            Err(_) => Vec::new(),
        };
        let bindable = bindable.into_iter().collect::<BTreeSet<_>>();
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
            for &address in &bindable {
                if bound.contains(&address) {
                    continue;
                }
                if let Ok(listener) = TcpListener::bind(SocketAddr::new(address, *port)).await {
                    bound.insert(address);
                    let app = app.clone();
                    tokio::spawn(async move {
                        use axum::serve::ListenerExt as _;
                        let listener = listener.tap_io(|tcp| {
                            if let Err(error) = tcp.set_nodelay(true) {
                                tracing::warn!(%error, "could not disable Nagle on peer socket");
                            }
                        });
                        let _ = axum::serve(listener, app).await;
                    });
                }
            }
            let addresses = bound
                .iter()
                .filter(|address| bindable.contains(address))
                .map(|address| SocketAddr::new(*address, *port))
                .collect::<Vec<_>>();
            endpoints.update(|set| set.tailscale = addresses);
        }
        tokio::select! {
            _ = connectivity.changed() => {},
            _ = tokio::time::sleep(worker_interval(Duration::from_secs(60))) => {},
        }
    }
}

/// Keep a persisted loopback listener exposed through Fabric, refreshing it on a local
/// reconnect and periodically for older Fabric. Leave/uninstall removes the declaration.
async fn keep_fabric_exposed(
    fabric: Fabric,
    protocol: String,
    address: SocketAddr,
    endpoints: Endpoints,
    mut connectivity: watch::Receiver<u64>,
) {
    loop {
        if fabric.expose(&protocol, &address.to_string()).await.is_ok()
            && let Ok(node) = fabric.id().await
        {
            endpoints.update(|set| set.fabric = Some((node, protocol.clone())));
        }
        tokio::select! {
            _ = connectivity.changed() => {},
            _ = tokio::time::sleep(worker_interval(Duration::from_secs(60))) => {},
        }
    }
}

/// A return from suspend or a local network/Fabric change announces this node to every
/// reachable member, regardless of how far their individual retries have backed off.
async fn keep_connectivity_current(fleet: FleetContext) {
    let interval = Duration::from_secs(5);
    let mut previous = None;
    let mut last_wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    loop {
        let addresses = local_addresses();
        let fabric = match &fleet.fabric {
            Some(fabric) => fabric.addresses().await.ok(),
            None => None,
        };
        let current = (addresses, fabric);
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if previous.as_ref().is_some_and(|before| *before != current)
            || wall.saturating_sub(last_wall) > 15_000
        {
            fleet
                .connectivity_changed
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
        previous = Some(current);
        last_wall = wall;
        tokio::time::sleep(worker_interval(interval)).await;
    }
}

/// Consume authenticated transport admissions without probing a remote peer. Offline
/// transitions are normal; online transitions wake only the member named by its endpoints.
fn apply_fabric_presence(fleet: &FleetContext, value: &Value) {
    if value["reset"] == true {
        fleet
            .connectivity_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    let Some(events) = value["events"].as_array() else {
        return;
    };
    let view = fleet.view.read().expect("fleet view lock poisoned");
    let mut online = fleet.online.write().expect("online lock poisoned");
    let mut changed = false;
    for event in events.iter().filter(|event| event["online"] == true) {
        let Some(id) = event["peer_id"].as_str() else {
            continue;
        };
        if let Some(name) = fleet.configured_fabric_peers.get(id) {
            online.insert(name.clone(), tokio::time::Instant::now());
            changed = true;
        }
        for member in view
            .members
            .iter()
            .filter(|member| member.state == "current")
        {
            if member
                .endpoints
                .iter()
                .any(|endpoint| endpoint["transport"] == "fabric" && endpoint["node"] == id)
            {
                online.insert(member.name.clone(), tokio::time::Instant::now());
                changed = true;
            }
        }
    }
    drop(online);
    drop(view);
    if changed {
        fleet
            .inbound_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

async fn keep_fabric_presence_current(fabric: Fabric, fleet: FleetContext) {
    loop {
        if let Ok(mut child) = fabric.peer_events() {
            if let Some(output) = child.stdout.take() {
                let mut lines = tokio::io::BufReader::new(output).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(value) = serde_json::from_str(&line) {
                        apply_fabric_presence(&fleet, &value);
                    }
                }
            }
            let _ = child.wait().await;
        }
        tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
    }
}

async fn refresh_fleet_view<B: Backend>(backend: &B, fleet: &FleetContext) {
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
async fn keep_endpoints_published<B: Backend>(
    backend: B,
    mode: &'static str,
    endpoints: Endpoints,
) {
    loop {
        let _ = backend.publish_endpoints(mode, &endpoints.list()).await;
        tokio::select! {
            _ = endpoints.changed.notified() => {}
            _ = tokio::time::sleep(worker_interval(Duration::from_secs(60))) => {}
        }
    }
}

/// Who this node dials, and the loopback URLs that reach each, most preferred first: every
/// current listening member other than itself, and every config peer that has never been a
/// member. A dial-out member is never dialed. A config peer entry for a member is that
/// member's first route from this machine.
pub fn dial_targets(
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
            .filter_map(|peer| parse_route(&peer.url))
            .collect::<Vec<_>>();
        routes.extend(routes_from_endpoints(&member.endpoints, local));
        if !routes.is_empty() {
            targets.insert(member.name.clone(), routes);
        }
    }
    for peer in config_peers {
        let known = view.members.iter().any(|member| member.name == peer.name)
            || view.legacy_removed.contains(&peer.name);
        if !known
            && peer.name != own
            && !peer.url.is_empty()
            && let Some(route) = parse_route(&peer.url)
        {
            targets.entry(peer.name.clone()).or_default().push(route);
        }
    }
    targets
}

/// Reread membership on every graph change and at least every 30 seconds.
async fn keep_fleet_view_current<B: Backend>(
    backend: B,
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

/// The sync routes beside `routes`, served with `state`.
pub fn peer_router<B: Backend>(state: PeerState<B>, routes: Router<PeerState<B>>) -> Router {
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
        .merge(routes)
        .layer(DefaultBodyLimit::max(MAX_EXCHANGE_BYTES))
        .with_state(state)
}

/// One dialer: its peer's current routes and the task that uses them.
type Dialer = (watch::Sender<Vec<Route>>, tokio::task::JoinHandle<()>);

fn start_outbound<B: Backend>(
    backend: B,
    node: String,
    config_peers: Vec<PeerConfig>,
    auth: FleetAuth,
    fleet: FleetContext,
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
                    if *current.borrow() != routes {
                        current.send_replace(routes);
                    }
                    continue;
                }
                let (routes, route_changes) = watch::channel(routes);
                let task = tokio::spawn(dial_peer(
                    backend.clone(),
                    node.clone(),
                    name.clone(),
                    route_changes,
                    auth.clone(),
                    fleet.clone(),
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
async fn dial_peer<B: Backend>(
    backend: B,
    node: String,
    name: String,
    mut routes: watch::Receiver<Vec<Route>>,
    auth: FleetAuth,
    fleet: FleetContext,
    mut notify: watch::Receiver<u64>,
) {
    // Retain the connection pool across both phases and later wakeups for this peer.
    let mut http = replication_http_client();
    let mut backoff = PeerBackoff::default();
    let mut inbound_changes = fleet.inbound_changed.subscribe();
    let mut activity_changes = fleet.activity_changed.subscribe();
    let mut connectivity = fleet.connectivity_changed.subscribe();
    let mut must_send = false;
    // The last exchange moved envelopes, so more may wait on either side: exchange again at
    // once, even though the peer dialed in recently.
    let mut moving = false;
    let mut route = 0_usize;
    let mut last_http_success = None;
    let mut refused = Vec::<(Route, tokio::time::Instant)>::new();
    let mut last_attempt_unix_ms = crate::store::now_ms();
    loop {
        // A removed node stops dialing; `st3 doctor` says what to do next.
        if fleet.is_removed() {
            tokio::time::sleep(worker_interval(Duration::from_secs(60))).await;
            continue;
        }
        // A connection in either direction exchanges both inventories. Local changes can
        // still request a push; quiet anti-entropy does not open a second connection.
        if connectivity.has_changed().unwrap_or(false) {
            connectivity.borrow_and_update();
            backoff = PeerBackoff::default();
            must_send = true;
        }
        if fleet
            .online
            .read()
            .expect("online lock poisoned")
            .get(&name)
            .is_some_and(|at| at.elapsed() < REPLICATION_WAKE_COALESCE)
        {
            backoff = PeerBackoff::default();
            must_send = true;
        }
        let inbound_at = fleet
            .inbound
            .read()
            .expect("inbound lock poisoned")
            .get(&name)
            .copied();
        let continuing = std::mem::take(&mut moving);
        if let Some(at) = inbound_at.filter(|_| !continuing) {
            if must_send && at.elapsed() < worker_interval(Duration::from_secs(30)) {
                let offered = fleet
                    .inbound_authority
                    .read()
                    .expect("inbound inventory lock poisoned")
                    .get(&name)
                    .cloned();
                if let Some(offered) = offered
                    && let Ok(current) = backend
                        .export(auth.fleet_id(), &ReplicationInventory::default(), true, &[])
                        .await
                {
                    // The other side already knows this inventory and is draining both
                    // queues. Notifications from that exchange must not trigger a reverse
                    // connection. A genuinely new local envelope still requests a push.
                    if current.exchange.authority_digest == offered {
                        must_send = false;
                    }
                }
            }
            let window = if must_send {
                REPLICATION_WAKE_COALESCE
            } else {
                worker_interval(Duration::from_secs(30))
            };
            if at.elapsed() < window {
                tokio::select! {
                    _ = tokio::time::sleep_until(at + window) => {}
                    _ = notify.changed() => { must_send = true; }
                    _ = routes.changed() => { must_send = true; }
                    _ = inbound_changes.changed() => {}
                    _ = connectivity.changed() => { backoff = PeerBackoff::default(); must_send = true; }
                }
                continue;
            }
        }
        // Grant refusals belong to the route. Graph writes, inbound exchanges, presence
        // and local network changes cannot make that member grant this service.
        if routes.has_changed().unwrap_or(false) {
            refused.clear();
        }
        // Retry waits must retain signs of life received during this attempt, including
        // an inbound request that arrives just before the outbound request fails.
        let attempt_started = tokio::time::Instant::now();
        let attempt_unix_ms = crate::store::now_ms();
        let selected = {
            let routes = routes.borrow_and_update();
            let now = tokio::time::Instant::now();
            refused.retain(|(route, until)| routes.contains(route) && *until > now);
            (0..routes.len())
                .map(|offset| routes[(route.wrapping_add(offset)) % routes.len()].clone())
                .find(|candidate| !refused.iter().any(|(route, _)| route == candidate))
        };
        if selected.is_none() && !refused.is_empty() {
            let deadline = refused.iter().map(|(_, until)| *until).min().unwrap();
            record_worker(
                &backend,
                &name,
                "backoff",
                last_attempt_unix_ms,
                Some(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = routes.changed() => { refused.clear(); }
            }
            continue;
        }
        last_attempt_unix_ms = attempt_unix_ms;
        record_worker(&backend, &name, "exchange", attempt_unix_ms, None).await;
        let is_http = matches!(selected, Some(Route::Http(_)));
        let url = match selected {
            Some(Route::Http(url)) => Some(url),
            Some(selected @ Route::Fabric { .. }) => {
                let Route::Fabric { node, protocol } = &selected else {
                    unreachable!()
                };
                match &fleet.fabric {
                    Some(fabric) => match fabric.dial(node, protocol).await {
                        Ok(address) => Some(format!("http://{address}")),
                        Err(error) => {
                            if error.is::<FabricGrantRefusal>() {
                                refused.push((
                                    selected,
                                    tokio::time::Instant::now() + fabric_refusal_delay(),
                                ));
                                let _ = backend
                                    .record_failure(&name, "refused", &error.to_string())
                                    .await;
                                route = route.wrapping_add(1);
                                continue;
                            }
                            let _ = backend
                                .record_failure(&name, "down", &error.to_string())
                                .await;
                            None
                        }
                    },
                    None => None,
                }
            }
            None => None,
        };
        let Some(url) = url else {
            route = route.wrapping_add(1);
            let delay = retry_delay(backoff.next(), &fleet, &name);
            record_worker(&backend, &name, "backoff", attempt_unix_ms, Some(delay)).await;
            if wait_peer_retry(
                &backend,
                attempt_unix_ms,
                delay,
                &mut routes,
                &mut inbound_changes,
                &mut activity_changes,
                &mut connectivity,
                &fleet,
                &name,
                attempt_started,
                last_http_success.as_ref(),
                &http,
            )
            .await
            {
                backoff = PeerBackoff::default();
            }
            continue;
        };
        let peer = PeerConfig {
            name: name.clone(),
            url,
        };
        match exchange(&http, &backend, &node, &peer, &auth, &fleet).await {
            Ok((moved, heal_now)) => {
                if is_http {
                    last_http_success = Some((peer.url.clone(), tokio::time::Instant::now()));
                }
                backoff = PeerBackoff::default();
                must_send = false;
                if heal_now {
                    record_worker(&backend, &name, "heal", attempt_unix_ms, None).await;
                    heal(&backend, &node, &peer, &auth, &fleet).await;
                }
                record_worker(&backend, &name, "idle", attempt_unix_ms, None).await;
                if moved {
                    // One exchange carries a bounded batch. Keep going at once while envelopes
                    // still move instead of leaving the rest of a backlog to the timer, or to
                    // the quiet window after the peer's last inbound exchange.
                    notify.borrow_and_update();
                    moving = true;
                } else {
                    // A busy harness can write several observations while one exchange is in
                    // flight. Keep the first exchange immediate, then coalesce the resulting
                    // wake burst without disabling the 30-second retry path. The window stays
                    // short so a publish is startable on every peer within seconds.
                    let not_before = tokio::time::Instant::now() + REPLICATION_WAKE_COALESCE;
                    tokio::select! {
                        _ = notify.changed() => { must_send = true; }
                        _ = routes.changed() => { must_send = true; }
                        _ = inbound_changes.changed() => {}
                        _ = connectivity.changed() => { backoff = PeerBackoff::default(); must_send = true; }
                        _ = tokio::time::sleep(worker_interval(Duration::from_secs(30))) => {}
                    }
                    tokio::time::sleep_until(not_before).await;
                }
            }
            Err(error) => {
                if let Some(removed) = error.downcast_ref::<RemovedFromFleet>() {
                    fleet.mark_removed(&peer.name, removed);
                    continue;
                }
                // A peer can leave an HTTP stream open without making progress. Once that
                // exchange times out, discard the pooled connection so the next attempt opens
                // a fresh stream, and try the next route.
                http = replication_http_client();
                route = route.wrapping_add(1);
                let overloaded = error.is::<PeerOverloaded>();
                let status = if overloaded {
                    "overloaded"
                } else if error.to_string().contains("signature")
                    || error.to_string().contains("fleet")
                {
                    "auth-failed"
                } else {
                    "down"
                };
                let _ = backend
                    .record_failure(&peer.name, status, &error.to_string())
                    .await;
                let delay = retry_delay(backoff.next(), &fleet, &name);
                record_worker(&backend, &name, "backoff", attempt_unix_ms, Some(delay)).await;
                if wait_peer_retry(
                    &backend,
                    attempt_unix_ms,
                    delay,
                    &mut routes,
                    &mut inbound_changes,
                    &mut activity_changes,
                    &mut connectivity,
                    &fleet,
                    &name,
                    if overloaded {
                        tokio::time::Instant::now()
                    } else {
                        attempt_started
                    },
                    last_http_success.as_ref(),
                    &http,
                )
                .await
                {
                    backoff = PeerBackoff::default();
                }
            }
        }
    }
}

async fn record_worker<B: Backend>(
    backend: &B,
    peer: &str,
    phase: &str,
    last_attempt: u128,
    delay: Option<Duration>,
) {
    // Visibility must not turn a blocked daemon read into a longer replication deadline.
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        backend.record_worker(
            peer,
            crate::replication::ReplicationWorkerStatus {
                phase: phase.into(),
                last_attempt_at_unix_ms: last_attempt,
                next_retry_at_unix_ms: delay
                    .map(|delay| crate::store::now_ms() + delay.as_millis()),
            },
        ),
    )
    .await;
}

fn retry_delay(delay: Duration, fleet: &FleetContext, name: &str) -> Duration {
    if fleet
        .activity
        .read()
        .expect("activity lock poisoned")
        .get(name)
        .is_some_and(|at| at.elapsed() < PEER_PROBE_WINDOW)
    {
        delay.min(Duration::from_secs(30))
    } else {
        delay
    }
}

/// Probe grants again without depending on Fabric's event version. Even the shortest jitter
/// allows at most three dials per hour, including the first refusal. Never apply worker caps.
fn fabric_refusal_delay() -> Duration {
    let mut random = [0; 2];
    let _ = getrandom::fill(&mut random);
    fabric_refusal_delay_with_jitter(u16::from_le_bytes(random))
}

fn fabric_refusal_delay_with_jitter(jitter: u16) -> Duration {
    Duration::from_millis(1800 * (800 + u64::from(jitter) % 401))
}

/// Failed connections have no graph wake dependency: unrelated local writes must not
/// turn an absent member into a hot loop. Jitter spreads fleet retries across an hour.
#[derive(Default)]
struct PeerBackoff {
    failures: u32,
}

impl PeerBackoff {
    fn delay(failures: u32, jitter: u16) -> Duration {
        // The first retries cover brief interruptions; long absences grow to an hour.
        let seconds = (1_u64 << failures.min(12)).min(3600);
        Duration::from_millis(seconds * (800 + u64::from(jitter) % 401))
    }

    fn next(&mut self) -> Duration {
        let mut random = [0; 2];
        let _ = getrandom::fill(&mut random);
        let delay = Self::delay(self.failures, u16::from_le_bytes(random));
        self.failures = self.failures.saturating_add(1);
        delay
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_peer_retry<B: Backend>(
    backend: &B,
    last_attempt_unix_ms: u128,
    delay: Duration,
    routes: &mut watch::Receiver<Vec<Route>>,
    inbound_changes: &mut watch::Receiver<u64>,
    activity_changes: &mut watch::Receiver<u64>,
    connectivity: &mut watch::Receiver<u64>,
    fleet: &FleetContext,
    name: &str,
    attempt_started: tokio::time::Instant,
    last_http_success: Option<&(String, tokio::time::Instant)>,
    http: &reqwest::Client,
) -> bool {
    let started = tokio::time::Instant::now();
    let mut deadline = started + delay;
    let mut next_probe = started + PEER_PROBE_INTERVAL;
    loop {
        // Check before sleeping as well as after notification: watch channels coalesce
        // events, and an accepted request can predate entry to this retry wait.
        if fleet
            .inbound
            .read()
            .expect("inbound lock poisoned")
            .get(name)
            .is_some_and(|at| *at >= attempt_started)
            || fleet
                .online
                .read()
                .expect("online lock poisoned")
                .get(name)
                .is_some_and(|at| *at >= attempt_started)
            || fleet
                .activity
                .read()
                .expect("activity lock poisoned")
                .get(name)
                .is_some_and(|at| *at >= attempt_started)
        {
            return true;
        }
        let probe_url = last_http_success
            .filter(|(_, at)| {
                at.elapsed() < PEER_PROBE_WINDOW
                    || fleet
                        .activity
                        .read()
                        .expect("activity lock poisoned")
                        .get(name)
                        .is_some_and(|at| at.elapsed() < PEER_PROBE_WINDOW)
            })
            .filter(|(url, _)| {
                routes
                    .borrow()
                    .iter()
                    .any(|route| matches!(route, Route::Http(current) if current == url))
            })
            .map(|(url, _)| url);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            changed = routes.changed() => return changed.is_ok(),
            changed = connectivity.changed() => return changed.is_ok(),
            changed = inbound_changes.changed() => {
                if changed.is_err() { return false; }
            }
            changed = activity_changes.changed() => {
                if changed.is_err() { return false; }
            }
            alive = async {
                tokio::time::sleep_until(next_probe).await;
                // HEAD is answered by the existing router without accessing the graph.
                // Any HTTP response proves transport life, including an older peer's 405.
                // Only the ensuing signed exchange can authenticate or import peer data.
                http.head(format!("{}{}", probe_url.unwrap().trim_end_matches('/'), EXCHANGE_PATH))
                    .timeout(PEER_PROBE_TIMEOUT).send().await.is_ok()
            }, if probe_url.is_some() => {
                next_probe = tokio::time::Instant::now() + PEER_PROBE_INTERVAL;
                if alive {
                    // Transport is alive, but another expensive exchange has just failed.
                    // Retain backoff and allow at most one attempt every 30 seconds at the cap.
                    // A cheap probe is not authenticated activity. It must not refresh
                    // its own eligibility window and keep a failing peer hot indefinitely.
                    let capped = started + Duration::from_secs(30);
                    if capped < deadline {
                        deadline = capped;
                        record_worker(backend, name, "backoff", last_attempt_unix_ms,
                            Some(deadline.saturating_duration_since(tokio::time::Instant::now()))).await;
                    }
                }
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

/// The HTTP client one dialer exchanges with: short connects, and a bound on each exchange.
pub fn replication_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(REPLICATION_EXCHANGE_TIMEOUT)
        .build()
        .expect("the replication HTTP client configuration is valid")
}

async fn receive_exchange<B: Backend>(
    State(state): State<PeerState<B>>,
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
    // Authentication proves the connection returned before inventory processing finishes.
    state.fleet.note_activity(&sender.name);
    let relay = sender.name;
    let mut answer = {
        let mut jobs = state
            .fleet
            .exchange_jobs
            .peers
            .lock()
            .expect("exchange jobs lock poisoned");
        if jobs
            .get(&relay)
            .is_some_and(|job| job.answer.has_changed().is_err() && job.answer.borrow().is_none())
        {
            jobs.remove(&relay);
        }
        if let Some(job) = jobs.get(&relay)
            && job.digest != request_digest
            && job.answer.borrow().is_none()
        {
            return signed_overload(&state, &request_digest);
        }
        let reuse = jobs
            .get(&relay)
            .is_some_and(|job| job.digest == request_digest);
        if !reuse {
            let Ok(permit) = state
                .fleet
                .exchange_jobs
                .permits
                .clone()
                .try_acquire_owned()
            else {
                return signed_overload(&state, &request_digest);
            };
            let request: ReplicationExchange = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(_) => {
                    return signed_error_response(
                        &state,
                        &request_digest,
                        0,
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid replication exchange",
                    )
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            };
            // Bound retained answers as well as active backend work. Completed answers may be
            // recomputed safely: envelope receipt is idempotent.
            if jobs.len() >= MAX_EXCHANGE_JOBS {
                jobs.retain(|_, job| job.answer.borrow().is_none());
            }
            let (done, answer) = watch::channel(None);
            jobs.insert(
                relay.clone(),
                ExchangeJob {
                    digest: request_digest.clone(),
                    answer,
                },
            );
            let state = state.clone();
            let relay = relay.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    let received = state
                        .backend
                        .receive(&relay, state.auth.fleet_id(), &request, None)
                        .await?;
                    if received.changed {
                        state.backend.changed().await;
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

                    Ok::<_, anyhow::Error>(response)
                }
                .await
                .map_err(|error| format!("replication request failed: {error:#}"));
                done.send_replace(Some(result));
            });
        }
        jobs.get(&relay).unwrap().answer.clone()
    };
    let ready = tokio::time::timeout(EXCHANGE_RESPONSE_BUDGET, async {
        loop {
            if let Some(result) = answer.borrow_and_update().clone() {
                break Some(result);
            }
            if answer.changed().await.is_err() {
                break None;
            }
        }
    })
    .await;
    match ready {
        Ok(Some(Ok(export))) => {
            finish_exchange_job(&state.fleet, &relay, &answer);
            let authority_digest = export.exchange.authority_digest.clone();
            let response =
                match signed_response(&state, &request_digest, export.store_index, export.exchange)
                {
                    Ok(response) => response,
                    Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                };
            state
                .fleet
                .inbound_authority
                .write()
                .expect("inbound inventory lock poisoned")
                .insert(relay.clone(), authority_digest);
            state
                .fleet
                .inbound
                .write()
                .expect("inbound lock poisoned")
                .insert(relay, tokio::time::Instant::now());
            state
                .fleet
                .inbound_changed
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            deflate_response(response, accepts_deflate(&headers))
                .await
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(Some(Err(message))) => {
            // Let the next request retry a failed job. Reporting failure must never delay headers.
            finish_exchange_job(&state.fleet, &relay, &answer);
            signed_error_response(
                &state,
                &request_digest,
                0,
                StatusCode::INTERNAL_SERVER_ERROR,
                &message,
            )
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(None) => {
            finish_exchange_job(&state.fleet, &relay, &answer);
            signed_error_response(
                &state,
                &request_digest,
                0,
                StatusCode::INTERNAL_SERVER_ERROR,
                "replication backend job ended without an answer",
            )
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => signed_overload(&state, &request_digest),
    }
}

fn finish_exchange_job(
    fleet: &FleetContext,
    relay: &str,
    answer: &watch::Receiver<Option<ExchangeAnswer>>,
) {
    let mut jobs = fleet
        .exchange_jobs
        .peers
        .lock()
        .expect("exchange jobs lock poisoned");
    // Concurrent retries can finish after a newer request has already started.
    if jobs
        .get(relay)
        .is_some_and(|job| job.answer.same_channel(answer))
    {
        jobs.remove(relay);
    }
}

fn signed_overload<B>(state: &PeerState<B>, request_digest: &str) -> Response {
    let result = signed_response_for(
        state,
        EXCHANGE_PATH,
        request_digest,
        0,
        serde_json::json!({"code": "replication-overloaded", "retry_after_ms": OVERLOAD_RETRY.as_millis()}),
    );
    match result {
        Ok(mut response) => {
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("5"));
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn receive_heal<B: Backend>(
    State(state): State<PeerState<B>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
    state.fleet.note_activity(&sender.name);
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
            state.backend.changed().await;
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
async fn receive_checkpoint_request<B: Backend>(
    State(state): State<PeerState<B>>,
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
    state.fleet.note_activity(&sender.name);
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
pub async fn fetch_checkpoint_manifest(
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
    let response: PeerResponse<CheckpointManifestPage> =
        serde_json::from_slice(&bytes).context("decode the signed checkpoint page")?;
    anyhow::ensure!(
        response.api_version == PEER_API_VERSION,
        "the peer API version differs"
    );
    fleet.note_activity(&peer.name);
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
async fn receive_join<B: Backend>(State(state): State<PeerState<B>>, body: Bytes) -> Response {
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

fn signed_refusal<B: Backend>(
    state: &PeerState<B>,
    request_digest: &str,
    refusal: &Refusal,
) -> Result<Response> {
    let envelope = PeerResponse::new(
        &state.node,
        0,
        serde_json::json!({
            "code": refusal.code,
            "message": refusal.message,
            "member_key": refusal.member_key,
        }),
    );
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

fn signed_response<B, T: Serialize>(
    state: &PeerState<B>,
    request_digest: &str,
    store_index: u64,
    value: T,
) -> Result<Response> {
    signed_response_for(state, EXCHANGE_PATH, request_digest, store_index, value)
}

pub fn signed_response_for<B, T: Serialize>(
    state: &PeerState<B>,
    path: &str,
    request_digest: &str,
    store_index: u64,
    value: T,
) -> Result<Response> {
    let envelope = PeerResponse::new(&state.node, store_index, value);
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

fn signed_error_response<B: Backend>(
    state: &PeerState<B>,
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

pub fn signed_error_response_for<B: Backend>(
    state: &PeerState<B>,
    path: &str,
    request_digest: &str,
    store_index: u64,
    status: StatusCode,
    message: &str,
) -> Result<Response> {
    let envelope = PeerResponse::new(
        &state.node,
        store_index,
        serde_json::json!({
            "code": "replication-request-failed",
            "message": message,
        }),
    );
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

/// Run one exchange with `peer`: send this node's summary, take what the peer sends, push what
/// it lacks, and adopt a checkpoint it advertises. Returns whether envelopes moved either way and
/// whether a heal is due.
pub async fn exchange<B: Backend>(
    http: &reqwest::Client,
    backend: &B,
    node: &str,
    peer: &PeerConfig,
    auth: &FleetAuth,
    fleet: &FleetContext,
) -> Result<(bool, bool)> {
    let mut heal_now = false;
    // One compact round, then at most one full-inventory round if the compact prefix
    // cannot make progress. Payloadless checkpoint identities can differ indefinitely.
    for full_inventory in [false, true] {
        let first = backend
            .export(auth.fleet_id(), &ReplicationInventory::default(), true, &[])
            .await?
            .exchange;
        let own_checkpoint = first.inventory.checkpoint.clone();
        let mut query = ReplicationExchange {
            envelopes: Vec::new(),
            ..first
        };
        if full_inventory {
            // Keep the digest: an empty listing is not proof of an empty inventory.
            // Existing peers answer a request without buckets with their complete inventory.
            query.inventory.buckets.clear();
        }
        let started = std::time::Instant::now();
        let (remote, peer_inflates) =
            post_signed(http, backend, peer, node, auth, fleet, &query, false).await?;
        let round_trip = started.elapsed();
        let different = remote.inventory.digest != query.inventory.digest;
        let received = backend
            .receive(&peer.name, auth.fleet_id(), &remote, Some(round_trip))
            .await?;
        // Progress means new envelopes stored on one side or the other. A peer that keeps sending,
        // or keeps being sent, envelopes that are never stored must not keep the worker busy.
        let pulled = received.receipt.received != 0;
        heal_now |= received.receipt.heal;
        if received.changed {
            backend.changed().await;
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
                post_signed(http, backend, peer, node, auth, fleet, &push, peer_inflates).await?;
            let round_trip = started.elapsed();
            // The peer stores a push before it answers, so its inventory moved if the push landed.
            pushed =
                !push.envelopes.is_empty() && response.inventory.digest != remote.inventory.digest;
            let received = backend
                .receive(&peer.name, auth.fleet_id(), &response, Some(round_trip))
                .await?;
            pulled_follow_up = received.receipt.received != 0;
            heal_now |= received.receipt.heal;
            if received.changed {
                backend.changed().await;
            }
        }
        let adopted = match &remote.inventory.checkpoint {
            Some(advertised) => {
                adopt_advertised_checkpoint(
                    http,
                    backend,
                    node,
                    peer,
                    auth,
                    fleet,
                    own_checkpoint.as_ref(),
                    advertised,
                )
                .await
            }
            None => false,
        };
        if adopted {
            backend.changed().await;
        }
        let progressed = pulled || pulled_follow_up || pushed || adopted;
        if progressed || !different || remote.inventory.buckets.is_empty() {
            return Ok((progressed, heal_now));
        }
    }
    // A full comparison can still differ only by tombstones. Let the worker rest;
    // checkpoint-lineage reconciliation, not more envelope requests, must settle those.
    Ok((false, heal_now))
}

/// When an adoption last failed, by this node and the checkpoint's drop digest, so a manifest
/// that cannot be adopted is not fetched again after every exchange.
static ADOPTION_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<(String, String), tokio::time::Instant>>,
> = std::sync::LazyLock::new(Default::default);

/// A peer advertised a checkpoint this node has not applied. If the daemon needs exactly that
/// checkpoint's manifest, fetch it from this peer and hand it over to adopt. The daemon checks
/// the whole manifest against the certificate before it stores anything. Returns whether the
/// node adopted it.
#[allow(clippy::too_many_arguments)]
async fn adopt_advertised_checkpoint<B: Backend>(
    http: &reqwest::Client,
    backend: &B,
    node: &str,
    peer: &PeerConfig,
    auth: &FleetAuth,
    fleet: &FleetContext,
    own: Option<&InventoryCheckpoint>,
    advertised: &InventoryCheckpoint,
) -> bool {
    if own.is_some_and(|own| own == advertised || own.cut_unix_ms > advertised.cut_unix_ms) {
        return false;
    }
    let need = match backend.checkpoint_need().await {
        Ok(Some(need)) => need,
        Ok(None) => return false,
        Err(error) => {
            eprintln!("st3: checkpoint need unavailable: {error:#}");
            return false;
        }
    };
    if need.checkpoint != advertised.id || need.drop_digest != advertised.drop_digest {
        return false;
    }
    let key = (node.to_owned(), need.drop_digest.clone());
    let retry_after = worker_interval(Duration::from_secs(10 * 60));
    if ADOPTION_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_some_and(|failed| failed.elapsed() < retry_after)
    {
        return false;
    }
    let adopted = async {
        let manifest = fetch_checkpoint_manifest(
            http,
            peer,
            node,
            auth,
            fleet,
            &need.checkpoint,
            need.cut_unix_ms,
        )
        .await?;
        backend.adopt_checkpoint(&manifest).await
    }
    .await;
    match adopted {
        Ok(actions) => {
            for action in &actions {
                eprintln!(
                    "st3: checkpoint {} (manifest from {})",
                    serde_json::to_string(action).unwrap_or_default(),
                    peer.name
                );
            }
            !actions.is_empty()
        }
        Err(error) => {
            eprintln!(
                "st3: adopting {} from {} failed: {error:#}",
                need.checkpoint, peer.name
            );
            ADOPTION_FAILURES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, tokio::time::Instant::now());
            false
        }
    }
}

/// Heal with one peer: carry each question the main daemon asks to the peer, and each answer
/// back, until the main daemon reports the heal. A peer that cannot be asked ends the heal with
/// the reason, which the main daemon reports.
pub async fn heal<B: Backend>(
    backend: &B,
    node: &str,
    peer: &PeerConfig,
    auth: &FleetAuth,
    fleet: &FleetContext,
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
    backend.changed().await;
}

#[derive(Debug)]
struct PeerOverloaded {
    retry_after: Duration,
}

impl std::fmt::Display for PeerOverloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authenticated peer is overloaded")
    }
}
impl std::error::Error for PeerOverloaded {}

/// Send one signed exchange, compressed when `compress` is set and the body is large, and return
/// the peer's verified answer and whether the peer takes compressed requests.
#[allow(clippy::too_many_arguments)]
async fn post_signed<B: Backend>(
    http: &reqwest::Client,
    backend: &B,
    peer: &PeerConfig,
    node: &str,
    auth: &FleetAuth,
    fleet: &FleetContext,
    exchange: &ReplicationExchange,
    compress: bool,
) -> Result<(ReplicationExchange, bool)> {
    let deadline = tokio::time::Instant::now() + EXCHANGE_POLL_BUDGET;
    let attempted = crate::store::now_ms();
    for _ in 0..MAX_OVERLOAD_POLLS {
        record_worker(backend, &peer.name, "exchange", attempted, None).await;
        let result = match tokio::time::timeout_at(
            deadline,
            post_signed_to(
                http,
                peer,
                node,
                auth,
                fleet,
                EXCHANGE_PATH,
                exchange,
                compress,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                return Err(PeerOverloaded {
                    retry_after: OVERLOAD_RETRY,
                }
                .into());
            }
        };
        match result {
            Err(error) if error.is::<PeerOverloaded>() => {
                let delay = error.downcast_ref::<PeerOverloaded>().unwrap().retry_after;
                record_worker(backend, &peer.name, "overload", attempted, Some(delay)).await;
                if tokio::time::timeout_at(deadline, tokio::time::sleep(delay))
                    .await
                    .is_err()
                {
                    return Err(PeerOverloaded {
                        retry_after: OVERLOAD_RETRY,
                    }
                    .into());
                }
            }
            result => return result,
        }
    }
    Err(PeerOverloaded {
        retry_after: OVERLOAD_RETRY,
    }
    .into())
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
        let refusal = serde_json::from_slice::<PeerResponse<serde_json::Value>>(&bytes)
            .ok()
            .map(|response| response.value);
        let code = refusal
            .as_ref()
            .and_then(|value| value["code"].as_str())
            .unwrap_or_default();
        if path == EXCHANGE_PATH
            && status == StatusCode::SERVICE_UNAVAILABLE
            && code == "replication-overloaded"
        {
            fleet
                .activity
                .write()
                .expect("activity lock poisoned")
                .insert(peer.name.clone(), tokio::time::Instant::now());
            let millis = refusal
                .as_ref()
                .and_then(|value| value["retry_after_ms"].as_u64())
                .unwrap_or(5000)
                .clamp(5000, 30_000);
            return Err(PeerOverloaded {
                retry_after: Duration::from_millis(millis),
            }
            .into());
        }
        let about_us = refusal
            .as_ref()
            .and_then(|value| value["member_key"].as_str())
            .is_some_and(|key| Some(key) == fleet.own_key.as_deref());
        if matches!(code, "member-removed" | "member-left") && about_us {
            let message = refusal
                .as_ref()
                .and_then(|value| value["message"].as_str())
                .unwrap_or_default();
            return Err(RemovedFromFleet {
                code: code.into(),
                message: message.into(),
            }
            .into());
        }
        anyhow::bail!(
            "peer {} returned {status}: {}",
            peer.name,
            String::from_utf8_lossy(&bytes)
        );
    }
    let response: PeerResponse<R> =
        serde_json::from_slice(&bytes).context("decode the signed peer response")?;
    anyhow::ensure!(
        response.api_version == PEER_API_VERSION,
        "the peer API version differs"
    );
    fleet.note_activity(&peer.name);
    Ok((response.value, accepts_deflate(&headers)))
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
