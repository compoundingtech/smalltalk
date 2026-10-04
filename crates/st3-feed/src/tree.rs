use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MissionsTree {
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub standing_queues: Vec<SeatQueue>,
    #[serde(default)]
    pub unstarted_missions: Vec<UnstartedMission>,
    #[serde(default)]
    pub agents: Vec<Seat>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub mission: String,
    pub state: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SeatQueue {
    pub agent_id: String,
    #[serde(default)]
    pub current_work_ids: Vec<String>,
    pub next_work_id: Option<String>,
    #[serde(default)]
    pub runs: Vec<QueuedRun>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct QueuedRun {
    pub mission_run_id: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct UnstartedMission {
    pub id: String,
    pub title: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Seat {
    pub id: String,
    pub name: String,
    pub host_id: Option<String>,
    pub seat_kind: Option<String>,
    pub driver: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub state: String,
    pub harness_state: Option<String>,
}

impl MissionsTree {
    pub fn from_response(response: serde_json::Value) -> Result<Self> {
        let value = response
            .get("value")
            .cloned()
            .context("missions tree response has no value")?;
        serde_json::from_value(value).context("decode missions tree")
    }
}
