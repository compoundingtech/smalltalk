//! smallclaims: a replicated claims graph.
//!
//! The generic layers of a claims database that knows nothing about agents or missions: claims
//! and their envelopes, the canonical order every shared projection folds claims in, digests,
//! and replication between members. smalltalk builds its runtime on top of it.

pub mod claim;
pub mod error;
pub mod fleet;
pub mod hash;
pub mod replication;

pub use claim::{
    ClaimInput, ClaimRecord, ReplicaBatch, ReplicaEnvelope, ReplicaEnvelopeId,
    ReplicaEnvelopePayload, ReplicaEnvelopeSignature,
};
pub use error::Error;
