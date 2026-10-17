//! stui's mission semantics, independent of its collection model and terminal UI.

use st3_client::MissionStep;
use std::collections::BTreeMap;

/// One word naming who has to move, shared by every mission surface.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Word {
    Decision,
    Stalled,
    /// Ready, and the agent that would take it is stopped or broken: a person can fix that.
    Unstaffed,
    /// Ready, and st does not say who will take it.
    Unclaimed,
    /// Ready, waiting its turn behind other work on a busy agent. Nothing to do.
    Queued,
    Working,
    /// Kept open by st so its observers can start other missions. Nothing is running.
    Watching,
    Held,
    Idle,
    Done,
    Failed,
    /// Someone or something stopped it before it finished.
    Cancelled,
    /// Published, and nobody has started a run of it.
    NotStarted,
    /// st serves no presentation this client reads: an older st, one that has not read its agents
    /// yet, or a word this client does not know. A client keeps no derivation of its own.
    Unknown,
}

impl Word {
    /// Really running, or about to: an agent is doing a step now, or one waits its turn on a busy
    /// agent. A count of "active missions" counts these and nothing else (Nathan, 2026-10-11:
    /// "54 active", when most of them were stuck, paused or waiting on a person).
    pub fn is_active(self) -> bool {
        matches!(self, Word::Working | Word::Queued)
    }

    /// Open, and not moving: a step nobody takes or whose owner is not moving.
    pub fn is_stuck(self) -> bool {
        matches!(self, Word::Stalled | Word::Unstaffed | Word::Unclaimed)
    }

    pub fn name(self) -> &'static str {
        match self {
            Word::Decision => "needs you",
            Word::Stalled => "stalled",
            Word::Unstaffed => "unstaffed",
            Word::Unclaimed => "unclaimed",
            Word::Queued => "queued",
            Word::Working => "working",
            Word::Watching => "watching",
            Word::Held => "held",
            Word::Idle => "idle",
            Word::Done => "done",
            Word::Failed => "failed",
            Word::Cancelled => "cancelled",
            Word::NotStarted => "not started",
            Word::Unknown => "unknown",
        }
    }
    pub fn explain(self) -> &'static str {
        match self {
            Word::Decision => "a step is waiting for your answer",
            Word::Stalled => "a step has an owner who is not moving",
            Word::Unstaffed => "a step is ready but its agent is stopped or broken",
            Word::Unclaimed => "a step is ready; st has not said which agent takes it",
            Word::Queued => "a step is waiting its turn on a busy agent; nothing to do",
            Word::Working => "an agent is doing a step now",
            Word::Watching => "st keeps this open and starts other missions when something happens",
            Word::Held => "waiting on something outside the fleet",
            Word::Idle => "running, with nothing ready",
            Word::Done => "every step finished",
            Word::Failed => "a step failed and nothing retried it",
            Word::Cancelled => "it was stopped before it finished",
            Word::NotStarted => "published, and never started",
            Word::Unknown => "st does not say: an older st, or one that has not read its agents yet",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Done,
    Cancelled,
    Working,
    Ready,
    Waiting,
    NeedsYou,
    Failed,
    Pending,
    /// st serves no presentation this client reads.
    Unknown,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Step {
    /// Stable step-run identity, including when several open runs share a path.
    pub id: String,
    /// The claimant, else assignee; absent for agentless or unassigned steps.
    pub seat: Option<String>,
    pub name: String,
    pub state: StepState,
    pub owner: Option<String>,
    pub note: Option<String>,
    pub after: Vec<String>,
    pub age: String,
    pub goals: Vec<String>,
    pub constraints: Vec<String>,
    pub gates: Vec<String>,
    pub attempt: u32,
    pub blockers: Vec<String>,
}

/// Raw step facts, independent of state-specific blocker presentation and queue notes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct StepMetadata {
    /// st retains the last reason even after a step moves on or ends.
    pub blocked_reason: Option<String>,
    pub last_progress: Option<String>,
    /// How long ago st nudged the step's holder for idling while holding it with nothing set to
    /// wake it, as an age label such as `40m`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nudged: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Mission {
    pub id: String,
    pub title: String,
    pub word: Word,
    pub age: String,
    pub host: String,
    pub goals: Vec<String>,
    pub steps: Vec<Step>,
    /// Metadata for the same steps, keyed by stable step-run ID rather than display path.
    /// Values are retained verbatim, regardless of state or display text policies.
    pub step_metadata: BTreeMap<String, StepMetadata>,
    pub agents: Vec<String>,
    pub decision: Option<String>,
    pub worktree: Option<String>,
    pub parent: Option<String>,
    pub system: bool,
    /// The mission's declaration as written, when st provides it.
    pub kdl: Option<String>,
    /// The outcome a person or an authorized agent set on its finished run, and why.
    pub outcome: Option<String>,
    /// When st last changed the mission (RFC 3339), so a list can age out what ended long ago.
    /// Empty when unknown.
    pub updated_at: String,
    /// The mission's own state as st stores it, shown only when st serves no presentation.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state: String,
}

impl Mission {
    /// What a list calls the mission: its word, or, when st serves none (an older st, or one
    /// that has not read its agents yet), its raw state and that st does not say.
    pub fn label(&self) -> String {
        match self.word {
            Word::Unknown if !self.state.is_empty() => format!("{} · st does not say", self.state),
            word => word.name().into(),
        }
    }

    pub fn progress(&self) -> (usize, usize) {
        (
            self.steps
                .iter()
                .filter(|step| step.state == StepState::Done)
                .count(),
            self.steps.len(),
        )
    }
}

/// Caller-owned display policies; none reads a clock or requires a renderer.
pub struct Display<'a> {
    pub mission_label: &'a dyn Fn(&st3_client::Mission) -> String,
    /// Format an RFC3339 timestamp relative to the explicitly supplied RFC3339 `now`.
    pub age_label: &'a dyn Fn(&str, &str) -> String,
    /// Normalize goals, constraints and outcome reasons for display.
    pub clean_text: &'a dyn Fn(&str) -> String,
}

impl Display<'_> {
    fn age(&self, then: &str, now: &str) -> String {
        let mut label = (self.age_label)(then, now);
        label.truncate(label.trim_end_matches(" ago").len());
        label
    }
}

/// The steps st sent with a mission: its open runs' steps, or its latest run's when none is open.
fn mission_steps(mission: &st3_client::Mission) -> Vec<&MissionStep> {
    let finished = |status: &str| matches!(status, "completed" | "failed" | "cancelled");
    let has_open = mission.run_details.iter().any(|run| !finished(&run.status));
    mission
        .run_details
        .iter()
        .enumerate()
        .filter(|(index, run)| {
            if has_open {
                !finished(&run.status)
            } else {
                *index + 1 == mission.run_details.len()
            }
        })
        .map(|(_, run)| run)
        .flat_map(|run| run.steps.iter().flatten())
        .collect()
}

/// Who set the outcome of the mission's finished latest run, from what, and why.
fn run_outcome(mission: &st3_client::Mission, now: &str, display: &Display<'_>) -> Option<String> {
    let outcome = mission.run_details.last()?.outcome.as_ref()?;
    let was = outcome
        .previous_status
        .as_deref()
        .map(|previous| format!(" (was {previous})"))
        .unwrap_or_default();
    Some(format!(
        "{}{was} · set by {} {} ago: {}",
        outcome.status,
        outcome.actor,
        display.age(&outcome.at, now),
        (display.clean_text)(&outcome.reason)
    ))
}

/// What st says of the agent whose queue holds a ready step.
pub(crate) struct QueueOwner {
    /// Stopped, failed or faulted: the step waits for an agent that cannot take it.
    pub down: bool,
}

/// The mission word, first match wins: an open human gate on the mission (decision); no runs
/// (not started); a blocked mission or step (stalled); completed (done); a failed mission or step
/// (failed); cancelled; only keep-open steps still running (watching); a step in hand (working); a
/// ready unclaimed step, unstaffed when its queue owner is down, queued when one queues it, and
/// unclaimed otherwise; a waiting or pending step (held); standing, running, ready or draft (idle);
/// otherwise done.
pub(crate) fn word(
    mission: &st3_client::Mission,
    work: &[&MissionStep],
    decision: bool,
    queue_owner: impl Fn(&str) -> Option<QueueOwner>,
) -> Word {
    let states = work.iter().map(|step| step.state.as_str());
    if decision {
        Word::Decision
    } else if mission.runs.is_empty() && mission.run_details.is_empty() && work.is_empty() {
        Word::NotStarted
    } else if mission.state == "blocked" || states.clone().any(|state| state == "blocked") {
        Word::Stalled
    } else if mission.state == "completed" {
        // A run someone set to completed keeps the steps that failed.
        Word::Done
    } else if mission.state == "failed" || states.clone().any(|state| state == "failed") {
        Word::Failed
    } else if mission.state == "cancelled" {
        Word::Cancelled
    } else if !work.is_empty()
        && work.iter().all(|step| {
            step.state == "completed" || (in_hand(&step.state) && step.agentless && step.keep_open)
        })
        && work.iter().any(|step| step.state != "completed")
    {
        // Only st's own keep-open steps are running: an intake that watches.
        Word::Watching
    } else if states.clone().any(in_hand) {
        Word::Working
    } else if let Some(ready) = work
        .iter()
        .find(|step| step.state == "ready" && step.claimant.is_none())
    {
        // Who has this step queued decides whether a person is needed.
        match queue_owner(&ready.id) {
            Some(owner) if owner.down => Word::Unstaffed,
            Some(_) => Word::Queued,
            None => Word::Unclaimed,
        }
    } else if states
        .clone()
        .any(|state| matches!(state, "waiting" | "pending"))
    {
        Word::Held
    } else if matches!(
        mission.state.as_str(),
        "standing" | "running" | "ready" | "draft"
    ) {
        Word::Idle
    } else {
        Word::Done
    }
}

/// The mission word from its row and open gates alone, reading no agent queue: a ready step
/// nobody holds reads unclaimed. A count of active missions reads this, so it never depends on
/// which agent owns a ready queue.
pub fn row_word(mission: &st3_client::Mission, gates: &[Gate]) -> Word {
    let decision = gates.iter().any(|gate| gate.mission == mission.header.id);
    word(mission, &mission_steps(mission), decision, |_| None)
}

/// Whether a step state means an agent (or st) has it in hand.
pub(crate) fn in_hand(state: &str) -> bool {
    matches!(state, "claimed" | "working" | "running" | "verifying")
}

/// How a client names who holds a step: "you" for its reader, else the name st serves.
fn owner_text(owner: &Actor, reader: &str) -> Option<String> {
    match owner {
        Actor::Person { person, .. } if person == reader => Some("you".into()),
        Actor::Person { label, .. } => Some(label.clone()),
        Actor::Agent { agent, label } => Some(format!("{label} · {agent}")),
        Actor::St => Some("st".into()),
        Actor::Nobody => None,
    }
}

/// Mission rows for a client, from typed borrowed projections.
///
/// What each mission and step reads as comes from the presentation st serves on the mission row
/// (`Mission.presentation`); a row without one reads `unknown`, and this keeps no derivation of
/// its own. `attention` must already be unresolved and filtered to `reader`. Collection loading,
/// clocks, naming policy and text normalization remain owned by the application.
pub fn adapt<'a>(
    missions: impl Iterator<Item = &'a st3_client::Mission> + Clone,
    agents: impl Iterator<Item = &'a st3_client::Agent> + Clone,
    attention: impl Iterator<Item = &'a st3_client::Attention> + Clone,
    reader: &str,
    now: &str,
    display: &Display<'_>,
) -> Vec<Mission> {
    missions
        .map(|mission| {
            let work = mission_steps(mission);
            let step_metadata = work
                .iter()
                .map(|step| {
                    (
                        step.id.clone(),
                        StepMetadata {
                            blocked_reason: step.blocked_reason.clone(),
                            last_progress: step.last_progress.clone(),
                            nudged: step.nudged_at.as_deref().map(|at| display.age(at, now)),
                        },
                    )
                })
                .collect();
            let decision = attention
                .clone()
                .find(|item| {
                    item.attention_kind == "human-gate"
                        && item.mission_id.as_deref() == Some(mission.header.id.as_str())
                })
                .map(|item| item.header.id.clone());
            let presented = mission
                .presentation
                .as_ref()
                .and_then(|value| <Presentation as serde::Deserialize>::deserialize(value).ok());
            let steps = work
                .iter()
                .map(|step| {
                    let shown = presented.as_ref().and_then(|presented| {
                        presented.steps.iter().find(|shown| shown.id == step.id)
                    });
                    Step {
                        id: step.id.clone(),
                        seat: (!step.agentless)
                            .then(|| step.claimant.clone().or_else(|| step.assignee.clone()))
                            .flatten(),
                        name: step.path.clone(),
                        state: shown.map_or(StepState::Unknown, |shown| shown.word),
                        owner: shown.and_then(|shown| owner_text(&shown.owner, reader)),
                        note: shown.and_then(|shown| shown.note.clone()),
                        after: vec![],
                        age: display.age(&step.since, now),
                        goals: step
                            .goals
                            .iter()
                            .map(|goal| (display.clean_text)(goal))
                            .collect(),
                        constraints: step
                            .constraints
                            .iter()
                            .map(|constraint| (display.clean_text)(constraint))
                            .collect(),
                        gates: vec![],
                        attempt: step.attempt,
                        blockers: step.blockers.clone(),
                    }
                })
                .collect::<Vec<_>>();
            // Read a mission like a pipeline: what finished, what is happening, what is next.
            // st says nothing about declaration order, so a later step must not sit above the
            // one working now.
            let mut steps = steps;
            steps.sort_by_key(|step| match step.state {
                StepState::Done | StepState::Cancelled => 0,
                StepState::NeedsYou | StepState::Failed => 1,
                StepState::Working => 2,
                StepState::Ready => 3,
                StepState::Waiting => 4,
                StepState::Pending => 5,
                StepState::Unknown => 6,
            });
            let active_agents = agents
                .clone()
                .filter(|agent| {
                    agent
                        .current_work_ids
                        .iter()
                        .any(|id| work.iter().any(|step| &step.id == id))
                })
                .map(|agent| agent.header.id.clone())
                .collect();
            Mission {
                id: mission.header.id.clone(),
                title: (display.mission_label)(mission),
                word: presented
                    .as_ref()
                    .map_or(Word::Unknown, |presented| presented.word),
                age: display.age(&mission.header.updated_at, now),
                host: String::new(),
                goals: vec![],
                steps,
                step_metadata,
                agents: active_agents,
                kdl: None,
                outcome: run_outcome(mission, now, display),
                updated_at: mission.header.updated_at.clone(),
                state: mission.state.clone(),
                decision,
                worktree: None,
                parent: None,
                system: mission.system,
            }
        })
        .collect()
}

mod presentation;
pub use presentation::{
    Action, Actor, Card, Fleet, Gate, Offer, Presentation, Progress, QueuedBehind, Seat, SeatRef,
    StepPresentation, Stuck, Work, present,
};

#[cfg(test)]
mod tests;
