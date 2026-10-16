//! A mission's presentation: its word, who must act, each step, the stuck step and what a person
//! can do. st serves it on every mission row (`Mission.presentation`), so clients read one answer
//! instead of deriving their own (docs/clients/ui-contract.md, Mission).

use super::{MissionStep, QueueOwner, StepState, Word, in_hand, mission_steps, word};
use serde::Serialize;

/// What st says of an agent a mission reads: the seat that holds or queues one of its steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seat {
    pub id: String,
    pub label: String,
    /// The agent row's own state (`running`, `stopped`, `failed`, …).
    pub state: String,
    pub fault: bool,
    /// The step it holds now, as `(mission, path)`.
    pub busy_with: Option<(String, String)>,
}

impl Seat {
    fn down(&self) -> bool {
        matches!(self.state.as_str(), "failed" | "stopped") || self.fault
    }
}

/// An open human gate: the person it waits on, and the step it holds when it holds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub mission: String,
    pub step: Option<String>,
    pub person: String,
}

/// What a mission presentation reads beyond the mission row itself.
pub trait Fleet {
    /// The agent with this id.
    fn seat(&self, agent: &str) -> Option<Seat>;
    /// The agent whose queue holds this step.
    fn queued_for(&self, step: &str) -> Option<Seat>;
    /// A mission named for display.
    fn mission_label(&self, mission: &str) -> String;
    /// A person named for display.
    fn person_label(&self, person: &str) -> String;
}

/// Who must act on a mission, or holds a step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// A person must decide. A client says "you" when the person is its reader.
    Person {
        person: String,
        label: String,
    },
    Agent {
        agent: String,
        label: String,
    },
    /// st's own agentless work.
    St,
    Nobody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepPresentation {
    pub id: String,
    pub path: String,
    pub title: Option<String>,
    pub word: StepState,
    pub owner: Actor,
    /// Why it waits: what queue it sits in, or st's reason while it waits.
    pub note: Option<String>,
    pub attempts: u32,
    pub blockers: Vec<String>,
    pub keep_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SeatRef {
    pub id: String,
    pub label: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stuck {
    pub step: String,
    pub word: StepState,
    pub attempts: u32,
    pub blockers: Vec<String>,
    /// The step's seat: its claimant, else its assignee, else the agent queuing it.
    pub agent: Option<SeatRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Work {
    pub mission: String,
    pub path: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueuedBehind {
    pub agent: String,
    pub label: String,
    /// The step it holds, when it is busy.
    pub busy_with: Option<Work>,
    /// Its state, when it is not busy.
    pub state: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Card {
    WhatYouCanDo,
    NothingToDo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Offer {
    RetryStep,
    RestartAgent,
    CancelRun,
    Chat,
}

/// Something a person can do, as the client-v0 action that does it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Action {
    pub id: Offer,
    pub action: &'static str,
    pub target: String,
    /// Binding: the client asks for a second tap first.
    pub confirm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Presentation {
    pub word: Word,
    pub system: bool,
    pub who: Actor,
    pub steps: Vec<StepPresentation>,
    pub stuck: Option<Stuck>,
    pub queued_behind: Option<QueuedBehind>,
    pub card: Option<Card>,
    pub actions: Vec<Action>,
    pub progress: Progress,
    pub last_progress: Option<String>,
    /// When it completed, failed or was cancelled (RFC 3339).
    pub settled_at: Option<String>,
}

fn step_word(step: &MissionStep, gated: bool) -> StepState {
    match step.state.as_str() {
        _ if gated => StepState::NeedsYou,
        "completed" => StepState::Done,
        state if in_hand(state) => StepState::Working,
        "ready" => StepState::Ready,
        "waiting" | "pending" | "blocked" => StepState::Waiting,
        "cancelled" => StepState::Cancelled,
        "failed" => StepState::Failed,
        _ => StepState::Pending,
    }
}

/// Display order: what finished, what needs someone, what runs, what is next.
fn rank(state: StepState) -> u8 {
    match state {
        StepState::Done | StepState::Cancelled => 0,
        StepState::NeedsYou | StepState::Failed => 1,
        StepState::Working => 2,
        StepState::Ready => 3,
        StepState::Waiting => 4,
        StepState::Pending => 5,
    }
}

fn agent(seat: &Seat) -> Actor {
    Actor::Agent {
        agent: seat.id.clone(),
        label: seat.label.clone(),
    }
}

/// The step's seat: its claimant, else its assignee; for a ready unclaimed step, the agent
/// queuing it.
fn seat_of(step: &MissionStep, fleet: &dyn Fleet) -> Option<Seat> {
    if step.agentless {
        return None;
    }
    let held = step.claimant.as_deref().or(step.assignee.as_deref());
    let seat = held.map(|id| {
        fleet.seat(id).unwrap_or_else(|| Seat {
            id: id.to_owned(),
            label: id.trim_start_matches("agent/").to_owned(),
            state: "unknown".into(),
            fault: false,
            busy_with: None,
        })
    });
    match seat {
        Some(seat) if step.claimant.is_some() || step.state != "ready" => Some(seat),
        // A ready step waits in whichever queue holds it, whoever it was assigned to.
        held => fleet.queued_for(&step.id).or(held),
    }
}

/// The mission's presentation, from its row, its open gates and the agents its steps name.
pub fn present(mission: &st3_client::Mission, gates: &[Gate], fleet: &dyn Fleet) -> Presentation {
    let work = mission_steps(mission);
    let gates = gates
        .iter()
        .filter(|gate| gate.mission == mission.header.id)
        .collect::<Vec<_>>();
    let gated = |step: &MissionStep| {
        gates
            .iter()
            .any(|gate| gate.step.as_deref() == Some(step.id.as_str()))
    };
    let word = word(mission, &work, !gates.is_empty(), |step| {
        fleet
            .queued_for(step)
            .map(|seat| QueueOwner { down: seat.down() })
    });
    let mut steps = work
        .iter()
        .map(|step| {
            let state = step_word(step, gated(step));
            let seat = seat_of(step, fleet);
            let queued = (step.state == "ready" && step.claimant.is_none())
                .then(|| fleet.queued_for(&step.id))
                .flatten();
            let note = queued
                .map(|seat| match &seat.busy_with {
                    Some((mission, path)) if !seat.down() => format!(
                        "queued for {}, which is busy with {} › {path}",
                        seat.label,
                        fleet.mission_label(mission)
                    ),
                    _ => format!("queued for {}, which is {}", seat.label, seat.state),
                })
                .or_else(|| {
                    matches!(step.state.as_str(), "waiting" | "blocked")
                        .then(|| step.blocked_reason.clone())
                        .flatten()
                });
            let owner = if state == StepState::NeedsYou {
                gates
                    .iter()
                    .find(|gate| gate.step.as_deref() == Some(step.id.as_str()))
                    .map(|gate| Actor::Person {
                        person: gate.person.clone(),
                        label: fleet.person_label(&gate.person),
                    })
                    .unwrap_or(Actor::Nobody)
            } else if step.agentless {
                Actor::St
            } else {
                seat.as_ref().map(agent).unwrap_or(Actor::Nobody)
            };
            (
                StepPresentation {
                    id: step.id.clone(),
                    path: step.path.clone(),
                    title: step.title.clone(),
                    word: state,
                    owner,
                    note,
                    attempts: step.attempt,
                    blockers: step.blockers.clone(),
                    keep_open: step.keep_open,
                },
                seat,
            )
        })
        .collect::<Vec<_>>();
    steps.sort_by_key(|(step, _)| rank(step.word));
    let stuck = matches!(
        word,
        Word::Stalled | Word::Failed | Word::Unstaffed | Word::Queued
    )
    .then(|| {
        steps.iter().find(|(step, _)| {
            matches!(
                step.word,
                StepState::Failed | StepState::Ready | StepState::Waiting
            )
        })
    })
    .flatten()
    .map(|(step, seat)| Stuck {
        step: step.id.clone(),
        word: step.word,
        attempts: step.attempts,
        blockers: step.blockers.clone(),
        agent: seat.as_ref().map(|seat| SeatRef {
            id: seat.id.clone(),
            label: seat.label.clone(),
            state: seat.state.clone(),
        }),
    });
    let queued_behind =
        (word == Word::Queued)
            .then(|| {
                work.iter()
                    .find(|step| step.state == "ready" && step.claimant.is_none())
                    .and_then(|step| fleet.queued_for(&step.id))
            })
            .flatten()
            .map(|seat| QueuedBehind {
                agent: seat.id.clone(),
                label: seat.label.clone(),
                busy_with: seat.busy_with.as_ref().filter(|_| !seat.down()).map(
                    |(mission, path)| Work {
                        mission: mission.clone(),
                        path: path.clone(),
                        label: format!("{} › {path}", fleet.mission_label(mission)),
                    },
                ),
                state: (seat.busy_with.is_none() || seat.down()).then(|| seat.state.clone()),
            });
    let who = match word {
        Word::Decision => gates
            .first()
            .map(|gate| Actor::Person {
                person: gate.person.clone(),
                label: fleet.person_label(&gate.person),
            })
            .unwrap_or(Actor::Nobody),
        Word::Working | Word::Watching => steps
            .iter()
            .find(|(step, _)| step.word == StepState::Working)
            .map(|(step, _)| step.owner.clone())
            .unwrap_or(Actor::Nobody),
        Word::Queued | Word::Unstaffed | Word::Stalled | Word::Failed => stuck
            .as_ref()
            .and_then(|stuck| stuck.agent.as_ref())
            .map(|seat| Actor::Agent {
                agent: seat.id.clone(),
                label: seat.label.clone(),
            })
            .unwrap_or(Actor::Nobody),
        _ => Actor::Nobody,
    };
    let card = match word {
        Word::Stalled | Word::Failed | Word::Unstaffed => Some(Card::WhatYouCanDo),
        Word::Queued => Some(Card::NothingToDo),
        _ => None,
    };
    let mut actions = Vec::new();
    if card == Some(Card::WhatYouCanDo)
        && let Some(stuck) = &stuck
    {
        if stuck.word == StepState::Failed {
            actions.push(Action {
                id: Offer::RetryStep,
                action: "work.retry",
                target: stuck.step.clone(),
                confirm: false,
            });
        }
        if let Some(seat) = steps
            .iter()
            .find(|(step, _)| step.id == stuck.step)
            .and_then(|(_, seat)| seat.as_ref())
        {
            if seat.down() {
                actions.push(Action {
                    id: Offer::RestartAgent,
                    // A stopped seat starts; a running one that faulted restarts its runtime.
                    action: if seat.state == "stopped" {
                        "agent.start"
                    } else {
                        "runtime.restart"
                    },
                    target: seat.id.clone(),
                    confirm: true,
                });
            }
            actions.push(Action {
                id: Offer::Chat,
                action: "message.send",
                target: seat.id.clone(),
                confirm: false,
            });
        }
        if let Some(run) = mission
            .run_details
            .iter()
            .find(|run| run.steps.iter().flatten().any(|step| step.id == stuck.step))
        {
            actions.push(Action {
                id: Offer::CancelRun,
                action: "mission.cancel",
                target: run.id.clone(),
                confirm: true,
            });
        }
    }
    let done = steps
        .iter()
        .filter(|(step, _)| step.word == StepState::Done)
        .count();
    Presentation {
        word,
        system: mission.system,
        who,
        progress: Progress {
            done,
            total: steps.len(),
        },
        last_progress: work
            .iter()
            .filter(|step| in_hand(&step.state))
            .find_map(|step| step.last_progress.clone()),
        settled_at: matches!(mission.state.as_str(), "completed" | "failed" | "cancelled")
            .then(|| mission.header.updated_at.clone()),
        steps: steps.into_iter().map(|(step, _)| step).collect(),
        stuck,
        queued_behind,
        card,
        actions,
    }
}

#[cfg(test)]
mod tests;
