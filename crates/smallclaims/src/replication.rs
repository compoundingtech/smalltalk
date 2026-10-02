//! What members exchange to replicate their claims, heal their graphs, and report sync.

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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceipt {
    pub received: usize,
    pub duplicate: usize,
    /// Envelope signatures stored for the first time.
    #[serde(default)]
    pub signatures: usize,
    pub inventory: ReplicationInventory,
    /// The two nodes hold the same envelopes but project different graphs, so the worker should
    /// heal with this peer now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub heal: bool,
}

/// One writer's range of batch sequences, aligned to the replication bucket width.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ClaimRange {
    pub writer: String,
    pub start: u64,
}

/// The digest of the claims one node projects from one writer range.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimRangeDigest {
    pub writer: String,
    pub start: u64,
    pub count: u64,
    pub digest: String,
}

/// The digest of the claims one node projects about one subject from one writer range.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimSubjectDigest {
    pub writer: String,
    pub start: u64,
    pub subject: String,
    pub count: u64,
    pub digest: String,
}

/// One claim a node projects, and the envelope that carries it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HealClaim {
    pub writer: String,
    pub sequence: u64,
    pub claim_id: String,
    pub subject: String,
    /// The envelope that carries the claim, when the node still holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_hash: Option<String>,
}

/// One question of a heal. When two nodes hold the same envelopes but project different
/// graphs, one of them narrows the difference: the digests of the claims each projects by
/// writer range, then by subject within the ranges that differ, then the claims of the subjects
/// that differ. The envelopes that carry claims one side lacks are then admitted again on that
/// side. When both project the same claims, a replay from nothing decides the graph.
#[derive(Clone, Debug, Deserialize, Serialize)]
// Externally tagged: an internally tagged enum buffers its fields, and the buffer cannot hold
// the u128 times that envelopes and reports carry.
#[serde(rename_all = "kebab-case")]
pub enum ReplicationHealQuery {
    Ranges,
    Subjects {
        ranges: Vec<ClaimRange>,
    },
    Claims {
        ranges: Vec<ClaimRange>,
        subjects: Vec<String>,
    },
    /// Admit `push` again, which carries claims the asker projects and this node lacks, and
    /// send the envelopes in `want`, which carry claims this node projects and the asker lacks.
    Swap {
        push: Vec<ReplicaEnvelope>,
        want: Vec<ReplicaEnvelopeId>,
    },
    /// Replay this node's graph from nothing.
    Replay,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
// Externally tagged: an internally tagged enum buffers its fields, and the buffer cannot hold
// the u128 times that envelopes and reports carry.
#[serde(rename_all = "kebab-case")]
pub enum ReplicationHealAnswer {
    Ranges {
        graph_digest: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        projection_digests: BTreeMap<String, String>,
        ranges: Vec<ClaimRangeDigest>,
    },
    Subjects {
        ranges: Vec<ClaimRange>,
        subjects: Vec<ClaimSubjectDigest>,
    },
    Claims {
        ranges: Vec<ClaimRange>,
        subjects: Vec<String>,
        claims: Vec<HealClaim>,
    },
    Swapped {
        /// Pushed claims this node now projects.
        admitted: u64,
        /// Pushed claims this node still cannot admit, with the reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<String>,
        envelopes: Vec<ReplicaEnvelope>,
        graph_digest: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        projection_digests: BTreeMap<String, String>,
    },
    Replayed {
        /// False while this node's replay backoff has not passed.
        replayed: bool,
        graph_digest: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        projection_digests: BTreeMap<String, String>,
    },
    /// The asking node could not reach the peer or the peer could not answer. The worker
    /// hands it to the main daemon, which ends the heal with it.
    Failed { message: String },
}

/// A heal question as it travels between peers.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationHealRequest {
    pub fleet_id: String,
    pub query: ReplicationHealQuery,
}

/// A peer's heal question, handed by the worker to the main daemon to answer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationHealAnswerRequest {
    pub peer: String,
    pub fleet_id: String,
    pub query: ReplicationHealQuery,
}

/// A peer's heal answer, handed by the worker to the main daemon to compare with its own claims.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationHealNextRequest {
    pub peer: String,
    pub answer: ReplicationHealAnswer,
}

/// What the worker does next in a heal: ask the peer another question, or stop.
#[derive(Clone, Debug, Deserialize, Serialize)]
// Externally tagged: an internally tagged enum buffers its fields, and the buffer cannot hold
// the u128 times that envelopes and reports carry.
#[serde(rename_all = "kebab-case")]
pub enum ReplicationHealStep {
    Ask { query: ReplicationHealQuery },
    Done { report: ReplicationHealReport },
}

/// What the last heal with one peer found and did.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplicationHealReport {
    pub at_unix_ms: u128,
    /// Writer ranges whose projected claims differed.
    pub ranges: u64,
    /// Subjects whose projected claims differed within those ranges.
    pub subjects: u64,
    /// Claims the peer projected that this node lacked and admitted again.
    pub refetched: u64,
    /// Claims this node projected that the peer lacked and admitted again.
    pub pushed: u64,
    /// This node replayed its graph from nothing.
    #[serde(default)]
    pub replayed: bool,
    /// The peer replayed its graph from nothing.
    #[serde(default)]
    pub peer_replayed: bool,
    /// The two graph digests were equal when the heal ended.
    pub healed: bool,
    /// Why the graphs still differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
}

/// A node's first sync after it joins: it ends once the node holds the same envelopes as a
/// peer. Matching registries also check the graph; mixed builds verify the wire log.
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
    /// The graphs matched only after a heal.
    #[serde(default)]
    pub healed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExportRequest {
    pub fleet_id: String,
    pub inventory: ReplicationInventory,
    #[serde(default)]
    pub summary_only: bool,
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
pub struct ReplicationPeerStatus {
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
    pub authority_digest: Option<String>,
    pub graph_digest: Option<String>,
    #[serde(default)]
    pub projection_digests: BTreeMap<String, String>,
    /// Comparable shared tables that differ at the last inventory-aligned comparison.
    #[serde(default)]
    pub differing_tables: Vec<String>,
    /// Different registries or locally waiting claims make projection comparisons premature.
    #[serde(default)]
    pub projection_comparison_waiting: bool,
    /// How far apart the two envelope sets were at the last exchange that measured them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<ReplicationPeerSync>,
}

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
    /// Envelopes received from the peer per second over recent exchanges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_rate_per_second: Option<f64>,
    /// How fast `peer_only_envelopes` shrinks, net of the envelopes the peer keeps writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_rate_per_second: Option<f64>,
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
    /// The last heal with this peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heal: Option<ReplicationHealReport>,
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
