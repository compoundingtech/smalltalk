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

/// A replaceable categorical snapshot, independent of the ordered event acknowledgement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentPublication {
    pub runtime_incarnation: String,
    pub claim: ClaimInput,
}
