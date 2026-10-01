//! Claims, the batches that carry them, and the envelopes that replicate a batch.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimInput {
    pub subject: String,
    pub kind: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub expected_subject: Option<Option<String>>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimRecord {
    pub id: String,
    pub store_index: u64,
    pub batch_id: String,
    pub subject: String,
    pub kind: String,
    pub origin: String,
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    pub body: Value,
    pub predecessors: Vec<String>,
    pub accepted_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ReplicaEnvelopeId {
    pub writer: String,
    pub sequence: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaEnvelope {
    pub writer: String,
    pub sequence: u64,
    pub previous_hash: Option<String>,
    pub hash: String,
    pub accepted_at_unix_ms: u128,
    /// Base64-encoded CBOR. Receipt does not decode this field.
    pub payload: String,
    /// The writer's member key and its signature over this envelope, when the writer is a
    /// keyed fleet member. Older peers ignore and drop both fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// A writer's signature for an envelope that the other side already holds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplicaEnvelopeSignature {
    pub writer: String,
    pub sequence: u64,
    pub hash: String,
    pub member_key: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaBatch {
    pub id: String,
    pub origin: String,
    pub replica_sequence: u64,
    pub previous_hash: Option<String>,
    pub hash: String,
    pub accepted_at_unix_ms: u128,
    pub claims: Vec<ClaimRecord>,
}

/// What a replica envelope's payload decodes to: the batch and the blobs its claims name.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaEnvelopePayload {
    pub batch: ReplicaBatch,
    pub blobs: BTreeMap<String, Vec<u8>>,
}
