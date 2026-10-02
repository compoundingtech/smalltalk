//! smallclaims: a replicated claims graph.
//!
//! The generic layers of a claims database that knows nothing about agents or missions: claims
//! and their envelopes, the canonical order every shared projection folds claims in, digests,
//! and replication between members. smalltalk builds its runtime on top of it.
//!
//! [`store::Store`] is the database. A runtime opens it with its own [`store::Runtime`], which
//! knows the runtime's claim kinds, keeps its projections in the same SQLite file, and decides
//! what a checkpoint may drop. Members sync by exchanging envelopes ([`replication`]); fleet
//! membership decides whose envelopes a member admits ([`fleet`]).

pub mod claim;
pub mod error;
pub mod fleet;
pub mod hash;
pub mod performance;
pub mod principal;
/// Opt-in accounting of where the daemon's time goes, turned on by `ST3_PROFILE_DIR`.
pub mod profile;
pub mod replication;
pub mod rules;
pub mod sqlite;
pub mod store;
pub mod sync;

pub use claim::{
    ClaimInput, ClaimRecord, ReplicaBatch, ReplicaEnvelope, ReplicaEnvelopeId,
    ReplicaEnvelopePayload, ReplicaEnvelopeSignature,
};
pub use error::Error;
pub use store::{Runtime, Store};
