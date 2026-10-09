//! Native observation admission. The Unix peer identifies the seat; the publishing runtime
//! is checked in the same writer transaction as its observation, including replay/dedupe.
use crate::model::ClaimInput;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Publication {
    pub runtime_incarnation: String,
    pub sequence: u64,
    pub claim: ClaimInput,
}

/// A durable accounting stop, independent of whether its current status sample was accepted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageFlush {
    pub subject: String,
    pub runtime_incarnation: String,
}
