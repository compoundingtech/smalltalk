//! stui's mission semantics, independent of its collection model and terminal UI.

use st3_client::MissionStep;
use std::collections::BTreeSet;

/// One word naming who has to move, shared by every mission surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
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
}

impl Word {
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
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
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Step {
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

#[derive(Clone, Debug, serde::Serialize)]
pub struct Mission {
    pub id: String,
    pub title: String,
    pub word: Word,
    pub age: String,
    pub host: String,
    pub goals: Vec<String>,
    pub steps: Vec<Step>,
    pub agents: Vec<String>,
    pub decision: Option<String>,
    pub worktree: Option<String>,
    pub parent: Option<String>,
    pub system: bool,
    /// The mission's declaration as written, when st provides it.
    pub kdl: Option<String>,
    /// The outcome a person or an authorized agent set on its finished run, and why.
    pub outcome: Option<String>,
}

impl Mission {
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
    pub agent_label: &'a dyn Fn(&st3_client::Agent) -> String,
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

fn label_text<'a>(
    mut missions: impl Iterator<Item = &'a st3_client::Mission>,
    label: &st3_client::WorkLabel,
    display: &Display<'_>,
) -> String {
    let mission = missions
        .find(|mission| mission.header.id == label.mission_id)
        .map(display.mission_label)
        .unwrap_or_else(|| {
            label
                .mission_id
                .trim_start_matches("mission/")
                .trim_start_matches("agent/")
                .to_owned()
        });
    format!("{mission} › {}", label.path)
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

/// Whether an agentless step only holds its run open (so the run's observers keep watching).
/// st does not say whether an agentless step waits on a flag or runs a command, so this goes
/// by the names the fleet uses for keep-open steps; anything else counts as work.
fn keeps_open(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "keep-watch" | "retire" | "steward-intake" | "standing" | "keep-open"
    ) || name.ends_with("-retirement")
}

/// The agent whose queue holds this work, if st says.
fn queued_for<'a>(
    mut agents: impl Iterator<Item = &'a st3_client::Agent>,
    work: &str,
) -> Option<&'a st3_client::Agent> {
    agents.find(|agent| {
        agent.next_work_id.as_deref() == Some(work)
            || agent.upcoming_work_ids.iter().any(|id| id == work)
    })
}

/// Who holds or will take a step: its claimant, else its assignee, named.
fn step_owner<'a>(
    mut agents: impl Iterator<Item = &'a st3_client::Agent>,
    step: &MissionStep,
    display: &Display<'_>,
) -> Option<String> {
    if step.agentless {
        return Some("st".into());
    }
    let id = step.claimant.as_deref().or(step.assignee.as_deref())?;
    Some(
        agents
            .find(|agent| agent.header.id == id)
            .map(|agent| format!("{} · {id}", (display.agent_label)(agent)))
            .unwrap_or_else(|| id.to_owned()),
    )
}

/// Derive mission semantics from typed borrowed projections.
///
/// `attention` must already be unresolved and filtered to the caller's actor. Collection loading,
/// clocks, naming policy and text normalization remain owned by the application.
pub fn adapt<'a>(
    missions: impl Iterator<Item = &'a st3_client::Mission> + Clone,
    agents: impl Iterator<Item = &'a st3_client::Agent> + Clone,
    attention: impl Iterator<Item = &'a st3_client::Attention> + Clone,
    now: &str,
    display: &Display<'_>,
) -> Vec<Mission> {
    // A step a person's gate holds: its attention item is about the step itself.
    let gated = attention
        .clone()
        .filter(|item| item.attention_kind == "human-gate")
        .map(|item| item.source_id.as_str())
        .collect::<BTreeSet<_>>();
    missions
        .clone()
        .map(|mission| {
            let work = mission_steps(mission);
            let decision = attention
                .clone()
                .find(|item| {
                    item.attention_kind == "human-gate"
                        && item.mission_id.as_deref() == Some(mission.header.id.as_str())
                })
                .map(|item| item.header.id.clone());
            let states = work.iter().map(|step| step.state.as_str());
            let word = if decision.is_some() {
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
                    step.state == "completed"
                        || (matches!(
                            step.state.as_str(),
                            "claimed" | "working" | "running" | "verifying"
                        ) && step.agentless
                            && keeps_open(&step.path))
                })
                && work.iter().any(|step| step.state != "completed")
            {
                // Only st's own keep-open steps are running: an intake that watches.
                Word::Watching
            } else if states
                .clone()
                .any(|state| matches!(state, "claimed" | "working" | "running" | "verifying"))
            {
                Word::Working
            } else if let Some(ready) = work
                .iter()
                .find(|step| step.state == "ready" && step.claimant.is_none())
            {
                // Who has this step queued decides whether a person is needed.
                match queued_for(agents.clone(), &ready.id) {
                    Some(agent)
                        if matches!(agent.state.as_str(), "failed" | "stopped")
                            || agent.fault.is_some() =>
                    {
                        Word::Unstaffed
                    }
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
            };
            let steps = work
                .iter()
                .map(|step| {
                    let state = match step.state.as_str() {
                        _ if gated.contains(step.id.as_str()) => StepState::NeedsYou,
                        "completed" => StepState::Done,
                        "claimed" | "working" | "running" | "verifying" => StepState::Working,
                        "ready" => StepState::Ready,
                        "waiting" | "pending" | "blocked" => StepState::Waiting,
                        "cancelled" => StepState::Cancelled,
                        "failed" => StepState::Failed,
                        _ => StepState::Pending,
                    };
                    let note = if step.state == "ready" && step.claimant.is_none() {
                        queued_for(agents.clone(), &step.id).map(|agent| {
                            let label = (display.agent_label)(agent);
                            match agent.current_work.first() {
                                Some(current)
                                    if !matches!(agent.state.as_str(), "failed" | "stopped") =>
                                {
                                    format!(
                                        "queued for {label}, which is busy with {}",
                                        label_text(missions.clone(), current, display)
                                    )
                                }
                                _ => format!("queued for {label}, which is {}", agent.state),
                            }
                        })
                    } else {
                        None
                    };
                    Step {
                        name: step.path.clone(),
                        state,
                        owner: if state == StepState::NeedsYou {
                            Some("you".into())
                        } else {
                            step_owner(agents.clone(), step, display)
                        },
                        // st keeps a step's last reason after it moves on; only a step still
                        // waiting is held up by it.
                        note: note.or_else(|| {
                            matches!(step.state.as_str(), "waiting" | "blocked")
                                .then(|| step.blocked_reason.clone())
                                .flatten()
                        }),
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
                word,
                age: display.age(&mission.header.updated_at, now),
                host: String::new(),
                goals: vec![],
                steps,
                agents: active_agents,
                kdl: None,
                outcome: run_outcome(mission, now, display),
                decision,
                worktree: None,
                parent: None,
                system: is_system_mission(&mission.header.id),
            }
        })
        .collect()
}

/// Plumbing the Missions tab folds away until `x` shows it: st's own loop rounds, and CI
/// (a `ci` segment, as in `mission/fleet/smalltalk/ci/run`). The graph has no mark for this
/// yet, so the name decides. What needs a person still reaches Home as attention.
fn is_system_mission(id: &str) -> bool {
    id.starts_with("mission/__st3/") || id.split('/').any(|segment| segment == "ci")
}

#[cfg(test)]
mod tests;
