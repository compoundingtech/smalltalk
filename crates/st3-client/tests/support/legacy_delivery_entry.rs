// TimelineEntry copied from ab38717e7123913c1f25b62300f77e437f86c09f.
// Nested TimelineBody/TimelineRole are unchanged by the delivery-link slice.
use serde::{Deserialize, Serialize};
use st3_client::{TimelineBody, TimelineRole};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TimelineEntry {
    pub id: String,
    pub sequence: u64,
    pub revision: u32,
    pub timestamp: String,
    pub role: TimelineRole,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(flatten)]
    pub body: TimelineBody,
}
