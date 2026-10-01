//! smallclaims: a replicated claims graph.
//!
//! The generic layers of a claims database that knows nothing about agents or missions: claims
//! and their envelopes, the canonical order every shared projection folds claims in, digests,
//! and replication between members. smalltalk builds its runtime on top of it.

pub mod claim;
pub mod error;
pub mod fleet;
pub mod hash;
pub mod performance;
/// Opt-in accounting of where the daemon's time goes, turned on by `ST3_PROFILE_DIR`.
pub mod profile;
pub mod replication;
pub mod sqlite;

pub use claim::{
    ClaimInput, ClaimRecord, ReplicaBatch, ReplicaEnvelope, ReplicaEnvelopeId,
    ReplicaEnvelopePayload, ReplicaEnvelopeSignature,
};
pub use error::Error;
