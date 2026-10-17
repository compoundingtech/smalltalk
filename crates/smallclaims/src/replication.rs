//! What members exchange to replicate their claims and report sync.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "test-support"))]
use crate::claim::ReplicaBatch;
use crate::claim::{ReplicaEnvelope, ReplicaEnvelopeId, ReplicaEnvelopeSignature};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplicationInventory {
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub envelopes: Vec<ReplicaEnvelopeId>,
    /// A compact inventory: one digest per writer sequence range. When present, `envelopes`
    /// lists only the identities in ranges that differ from the peer's ranges. Older peers
    /// ignore this field and exchange the full inventory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<ReplicationInventoryBucket>,
    /// The most envelopes the sending node takes in one exchange. Older peers leave it out and
    /// are sent at most 512.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts: Option<u32>,
    /// The newest stable checkpoint the sending node has trimmed or adopted. A node that has
    /// not applied it fetches its manifest from this peer. Older peers leave it out and ignore
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<InventoryCheckpoint>,
}

/// A checkpoint as an inventory advertises it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InventoryCheckpoint {
    pub id: String,
    pub cut_unix_ms: u128,
    pub drop_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplicationInventoryBucket {
    pub writer: String,
    /// First sequence in this range; ranges are aligned to the bucket width.
    pub start: u64,
    pub count: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExchange {
    pub peer: String,
    pub fleet_id: String,
    pub schema_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_registry_digest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projection_versions: BTreeMap<String, u64>,
    pub authority_digest: String,
    /// Six-table compatibility digest for older peers. Modern peers compare projection_digests.
    pub graph_digest: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projection_digests: BTreeMap<String, String>,
    pub inventory: ReplicationInventory,
    #[serde(default)]
    pub envelopes: Vec<ReplicaEnvelope>,
    /// Envelopes the sender holds but cannot admit until it has their writer's signature.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signature_requests: Vec<ReplicaEnvelopeId>,
    /// Signatures answering the other side's `signature_requests`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<ReplicaEnvelopeSignature>,
    /// The sender's build, so a member that finds the same claims projecting differently can
    /// name both builds in the bug it records. Older peers leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceipt {
    pub received: usize,
    pub duplicate: usize,
    /// Envelope signatures stored for the first time.
    #[serde(default)]
    pub signatures: usize,
    pub inventory: ReplicationInventory,
}

/// A node's first sync after it joins: it ends once the node holds the same envelopes as a
/// peer and has admitted them. Mixed builds verify the wire log.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplicationFirstSync {
    /// `syncing`, `verified`, or `failed`.
    pub state: String,
    pub started_at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_unix_ms: Option<u128>,
    /// The peer whose envelopes and graph this node matched, or failed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelopes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_graph_digest: Option<String>,
    /// Mixed builds verify the complete wire log while their projections wait for an upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExportRequest {
    pub fleet_id: String,
    pub inventory: ReplicationInventory,
    #[serde(default)]
    pub summary_only: bool,
    /// Classify a full summary requested only by the local recent-inbound guard.
    /// Older local daemons ignore this field; it never changes the exported exchange.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub authority_guard: bool,
    /// The other side's signature requests, to answer in the exported exchange.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signature_requests: Vec<ReplicaEnvelopeId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExportResponse {
    pub exchange: ReplicationExchange,
    pub store_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceiveRequest {
    pub peer: String,
    pub fleet_id: String,
    pub exchange: ReplicationExchange,
    /// How long the worker's request that returned this exchange took, when it made one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<u64>,
    /// Local dial evidence, never part of the signed or replicated exchange.
    #[serde(default)]
    pub carrier: ReplicationCarrier,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceiveResponse {
    pub receipt: ReplicationReceipt,
    pub changed: bool,
    pub store_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationPeerFailureRequest {
    pub peer: String,
    pub status: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<ReplicationCarrier>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaRecordView {
    pub record_ref: String,
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub position: u64,
    pub state: String,
    pub claim_id: Option<String>,
    pub subject: Option<String>,
    pub kind: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub replacement_claim_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplicationStatus {
    pub configured: bool,
    pub fleet_id: Option<String>,
    pub authority_digest: String,
    /// Aggregate of every shared projection table and admitted immutable claim sources.
    pub graph_digest: String,
    #[serde(default)]
    pub projection_digests: BTreeMap<String, String>,
    pub received_envelopes: u64,
    pub pending_records: u64,
    pub valid_records: u64,
    /// Compatibility name for claims waiting for a newer build.
    pub unknown_records: u64,
    /// Authenticated claims retained without a projection until this build knows their schema.
    #[serde(default)]
    pub waiting_claims: u64,
    pub invalid_records: u64,
    pub repaired_records: u64,
    /// Envelopes of keyed writers held until their writer's signature arrives.
    #[serde(default)]
    pub unsigned_envelopes: u64,
    /// Envelopes held because no incarnation of their writer holds their sequence.
    #[serde(default)]
    pub fenced_envelopes: u64,
    /// Envelopes a checkpoint dropped here. Their identities stay in the inventory.
    #[serde(default)]
    pub checkpointed_envelopes: u64,
    pub unhealthy_projections: u64,
    /// Each unhealthy projection, such as one replicated claim this build could not project.
    #[serde(default)]
    pub unhealthy: Vec<UnhealthyProjection>,
    pub peers: Vec<ReplicationPeerStatus>,
    /// Where this process spent replication time since it started.
    #[serde(default)]
    pub timings: ReplicationTimings,
    /// This node's first sync, when it joined a fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_sync: Option<ReplicationFirstSync>,
    /// A member refused this node as removed or left, so it no longer syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<crate::fleet::RemovalNotice>,
}

/// Cumulative replication time in one daemon process since it started, split by stage, for
/// profiling a sync. Each store stage starts once it holds the store's write connection, so the
/// stages do not overlap one another, except that admission includes verification and snapshot
/// includes signing. SQLite time is every statement the process ran, inside any stage or outside
/// all of them.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplicationTimings {
    /// Exchanges this node received from peers, in either direction.
    pub exchanges: u64,
    pub envelopes_received: u64,
    /// This node's own requests to peers, from send to response, including the peer's work.
    pub round_trip_ms: u64,
    /// Comparing inventories and reading envelopes to send.
    pub export_ms: u64,
    /// Refreshing this node's inventory and digests after a write, including sealing and
    /// signing its own new envelopes.
    pub snapshot_ms: u64,
    /// Storing received envelopes.
    pub receipt_ms: u64,
    /// Validating and admitting received envelopes as claims.
    pub admission_ms: u64,
    /// The part of admission spent decoding envelopes, checking their hashes and claim schemas,
    /// and looking up their stored signatures. Receipt verifies the signatures themselves.
    pub verify_ms: u64,
    /// Reducing admitted claims into the current graph.
    pub projection_ms: u64,
    pub repair_ms: u64,
    /// Signing this node's own envelopes with its member key.
    pub signing_ms: u64,
    pub sqlite_ms: u64,
    /// SQLite commits, and their part of `sqlite_ms`. Each one waits for a disk flush.
    pub commits: u64,
    pub commit_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UnhealthyProjection {
    pub aggregate: String,
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationWorkerStatusRequest {
    pub peer: String,
    pub worker: ReplicationWorkerStatus,
}

/// Host-local worker state; absent after restart until the worker next attempts a peer.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplicationWorkerStatus {
    pub phase: String,
    pub last_attempt_at_unix_ms: u128,
    pub next_retry_at_unix_ms: Option<u128>,
    /// Selected outbound route preference. Absent when an update only changes worker phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_is_preferred: Option<bool>,
    /// Preferred-route cooldown deadline; cleared when a route update has no pending retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_retry_at_unix_ms: Option<u128>,
}

/// The carrier attempted locally, or unknown when this host has no dial evidence.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationCarrier {
    Tailscale,
    Fabric,
    Loopback,
    #[default]
    Unknown,
}

impl ReplicationCarrier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tailscale => "tailscale",
            Self::Fabric => "fabric",
            Self::Loopback => "loopback",
            Self::Unknown => "unknown",
        }
    }
}

/// One locally initiated exchange observation. Absent after restart; never a replicated claim.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplicationExchangeTransport {
    pub at_unix_ms: u128,
    pub carrier: ReplicationCarrier,
    /// Fabric exposes no evidence for the direct/relay path of this particular dial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fabric_route: Option<String>,
}

impl ReplicationExchangeTransport {
    pub fn new(at_unix_ms: u128, carrier: ReplicationCarrier) -> Self {
        Self {
            at_unix_ms,
            carrier,
            fabric_route: (carrier == ReplicationCarrier::Fabric).then(|| "unknown".into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationPeerStatus {
    /// Last successful locally initiated exchange, with its original dial evidence and time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_exchange: Option<ReplicationExchangeTransport>,
    /// Last failed locally initiated exchange/attempt, independent of success and last_error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failed_exchange: Option<ReplicationExchangeTransport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<ReplicationWorkerStatus>,
    pub peer: String,
    pub status: String,
    pub last_success_at_unix_ms: Option<u128>,
    /// The last attempt to reach the peer that failed after its last exchange; cleared by the
    /// next exchange in either direction.
    pub last_error: Option<String>,
    /// When `last_error` happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at_unix_ms: Option<u128>,
    /// A direct Fabric route was refused by the member's service grants, not an outage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,
    pub schema_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_registry_digest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projection_versions: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible_projection_tables: Vec<String>,
    pub authority_digest: Option<String>,
    pub graph_digest: Option<String>,
    #[serde(default)]
    pub projection_digests: BTreeMap<String, String>,
    /// Comparable shared tables that differ at the last inventory-aligned comparison.
    #[serde(default)]
    pub differing_tables: Vec<String>,
    /// Pending projection, unsealed claims, different registries, or an older advertised
    /// inventory make the projection comparison premature.
    #[serde(default)]
    pub projection_comparison_waiting: bool,
    /// Why the comparison is premature, in words; absent when it is not waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_comparison_waiting_reason: Option<String>,
    /// What the reported comparison found: whether every comparable projection was equal.
    /// Absent while waiting and for a peer that sends no projection digests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_graphs_equal: Option<bool>,
    /// When the exchange that made that comparison happened. A comparison held from an
    /// earlier aligned exchange is older than the latest exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_compared_at_unix_ms: Option<u128>,
    /// How far apart the two envelope sets were at the last exchange that measured them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<ReplicationPeerSync>,
}

/// Largest whole-second duration representable exactly by safe-integer clients (2^53 - 1).
/// Larger forecasts are unavailable, rather than clamped to a fabricated duration.
pub const MAX_SAFE_DURATION_SECONDS: u64 = (1_u64 << 53) - 1;

/// The difference between this node's envelopes and one peer's, measured from the inventory the
/// peer sent in its last exchange.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ReplicationPeerSync {
    /// Envelopes the peer holds that this node lacks.
    pub peer_only_envelopes: u64,
    /// Envelopes this node holds that the peer lacks.
    pub local_only_envelopes: u64,
    pub measured_at_unix_ms: u128,
    /// The peer has not exchanged since a quiet interval passed, so the measurement may no
    /// longer hold: it says what the two nodes held then, not now.
    #[serde(default)]
    pub stale: bool,
    /// Envelopes this node gained since the measurement, which the counts above leave out.
    /// Until the next exchange, the peer has not seen them from this node.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub added_since_measured_envelopes: u64,
    /// Envelopes received from the peer per second over recent exchanges; absent after a gap
    /// between measurements longer than `REPLICATION_SYNC_STALE_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_rate_per_second: Option<f64>,
    /// How fast `peer_only_envelopes` shrinks, net of the envelopes the peer keeps writing; zero
    /// once a window passes in which it did not shrink, and absent after a gap between
    /// measurements longer than `REPLICATION_SYNC_STALE_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_rate_per_second: Option<f64>,
    /// Forecast in exact JSON-safe integer seconds up to [`MAX_SAFE_DURATION_SECONDS`], from
    /// [`ReplicationPeerSync::closing_rate_per_second`]; absent when no finite positive rate or
    /// representable forecast exists, including while the gap is not closing and once the
    /// measurement is `stale`, even one with nothing missing. Missing a forecast does not mean
    /// the peer is caught up. Zero
    /// means nothing the peer listed was missing here, at arrival: admission, projection and
    /// visibility can lag behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_catch_up_seconds: Option<u64>,
    /// The peer recently held more envelopes than one exchange carries, so this node's views
    /// can show early history as current.
    #[serde(default)]
    pub catching_up: bool,
    /// When this node last compared its graph digest with the peer's. Each graph projects the
    /// envelopes its node holds, so only an exchange at which both nodes hold the same
    /// envelopes compares them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_compared_at_unix_ms: Option<u128>,
    /// When the comparisons began finding the two graphs different; `None` while the latest
    /// comparison found them equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_differs_since_unix_ms: Option<u128>,
    /// Consecutive comparisons found the same envelopes projecting different graphs. More
    /// exchanges cannot fix that, so this node's views can be wrong until it is repaired.
    #[serde(default)]
    pub diverged: bool,
}

impl ReplicationPeerSync {
    /// The rate the forecast divides by: how fast the gap closes, capped at the rate envelopes
    /// arrive. `None` unless both rates are known and finite, so an unknown arrival rate never
    /// counts as progress; zero while the gap is not closing or nothing arrives.
    ///
    /// The cap is a guard, not a proof. The gap can also shrink without a receipt, when a
    /// checkpoint drops envelopes or the comparison narrows. A window that spans a change of the
    /// peer's advertised checkpoint, of this node's inventory below its tail (which its own
    /// checkpoints move), or between range and full comparison measures no rate, but a peer
    /// that advertises no checkpoint and a narrowing within range comparison
    /// are not seen: when receipts coincide with such a drop, the forecast can still be too
    /// short. It measures arrival only, not admission, projection or visibility.
    pub fn closing_rate_per_second(&self) -> Option<f64> {
        let usable = |rate: Option<f64>| rate.filter(|rate| rate.is_finite() && *rate >= 0.0);
        let closing = usable(self.catch_up_rate_per_second)?;
        let received = usable(self.receive_rate_per_second)?;
        Some(closing.min(received))
    }
}

/// Two members that hold the same envelopes and project one table differently. A projection is
/// a pure function of the claims, so this is a bug to reproduce and fix, never something to
/// repair from a peer. It carries what a reproduction needs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectionDivergence {
    pub peer: String,
    /// The shared projection (table) whose digests differ, or `graph` for a peer that sends only
    /// the older whole-graph digest.
    pub projection: String,
    /// The claims both members projected: the inventory digest they share and each writer's
    /// highest sequence in it.
    pub inventory_digest: String,
    pub writer_high_water: BTreeMap<String, u64>,
    pub local_digest: String,
    pub peer_digest: String,
    pub local_build: String,
    /// Absent for a peer whose build does not say.
    pub peer_build: Option<String>,
    pub since_unix_ms: u128,
    pub compared_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationRepairRequest {
    pub record_ref: String,
    pub replacement_claim_id: String,
    pub reason: String,
    pub actor: String,
    pub idempotency_key: String,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationBatch {
    pub peer: String,
    #[serde(default)]
    pub replica_heads: BTreeMap<String, u64>,
    pub batches: Vec<ReplicaBatch>,
    pub blobs: BTreeMap<String, Vec<u8>>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationResponse {
    pub accepted_through: u64,
    pub missing_sequences: Vec<u64>,
    #[serde(default)]
    pub accepted_heads: BTreeMap<String, u64>,
    #[serde(default)]
    pub missing_ranges: Vec<ReplicaRange>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaRange {
    pub origin: String,
    pub from: u64,
    pub through: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
