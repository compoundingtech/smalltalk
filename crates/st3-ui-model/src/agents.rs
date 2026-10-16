//! An agent's presentation: its state word, list group, why it needs someone, what it holds now
//! and what is queued next. st serves it on every agent row (`Agent.presentation`), so clients
//! read one answer instead of deriving their own (docs/clients/ui-contract.md, Agent).

use serde::{Deserialize, Serialize};

/// The agent state words, by their `fixtures/clients/words.json` ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting on a person.
    NeedsYou,
    /// Its harness is not logged in to its provider; a login on its host fixes it.
    NeedsLogin,
    /// Broken: a fault, a failed runtime, a stale message path, or blocked on its own channel.
    Fault,
    Working,
    /// Its harness is compacting its conversation: a status, never an alert.
    Compacting,
    Idle,
    Starting,
    /// Declared, with no running session after its launch window.
    Down,
    Stopped,
    /// Retired by a person: it takes no more work and is never started again.
    Retired,
    Unknown,
}

impl State {
    /// Every state, in `words.json` order.
    pub const ALL: [State; 11] = [
        State::NeedsYou,
        State::NeedsLogin,
        State::Fault,
        State::Working,
        State::Compacting,
        State::Idle,
        State::Starting,
        State::Down,
        State::Stopped,
        State::Retired,
        State::Unknown,
    ];

    /// The list group an agent in this state belongs to.
    pub fn group(self) -> Group {
        match self {
            State::NeedsYou => Group::NeedsYou,
            State::NeedsLogin => Group::NeedsLogin,
            State::Fault | State::Down => Group::Fault,
            State::Working | State::Compacting => Group::Working,
            State::Idle | State::Starting => Group::Idle,
            State::Stopped | State::Unknown => Group::Stopped,
            State::Retired => Group::Retired,
        }
    }
}

/// The agents list groups, in display order. Sessions st found running but did not start are
/// not agent rows; clients list them in their own `found_running` group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    NeedsYou,
    NeedsLogin,
    Fault,
    Working,
    Idle,
    Stopped,
    Retired,
}

/// A step an agent holds or has queued, named for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Work {
    pub step: String,
    pub mission: String,
    pub path: String,
    pub title: Option<String>,
    /// `mission › path`, with the mission named without its `mission/` prefix.
    pub label: String,
}

/// What an agent holds now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Now {
    #[serde(flatten)]
    pub work: Work,
    pub goal: Option<String>,
    /// When it took the step (RFC 3339).
    pub since: String,
    /// When its holder lost its session, if it has (RFC 3339).
    pub holder_down_since: Option<String>,
}

/// How many queued steps `next` names at most.
pub const NEXT_LIMIT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Presentation {
    pub state: State,
    pub group: Group,
    /// Why it needs someone, in plain words, when its state says it does.
    pub because: Option<String>,
    pub now: Option<Now>,
    /// Up to [`NEXT_LIMIT`] queued steps, next first.
    pub next: Vec<Work>,
    /// How many steps are queued in all.
    pub queued: u64,
}

/// The facts of an agent row the presentation reads. st deserializes them straight from the row
/// it serves, so a read never builds a whole agent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Facts {
    pub state: String,
    pub reachability: String,
    pub harness_state: Option<String>,
    pub harness_error_state: Option<String>,
    pub observation: Option<String>,
    pub blocked_on: Option<String>,
    pub ask: Option<String>,
    pub reason: Option<String>,
    pub activity: Option<String>,
    pub down_since: Option<String>,
    pub fault: Option<String>,
    pub delivery: Option<Delivery>,
    pub current_work: Vec<st3_client::WorkLabel>,
    pub next_work: Option<st3_client::WorkLabel>,
    pub upcoming_work: Vec<st3_client::WorkLabel>,
    pub queued_work_count: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Delivery {
    pub state: String,
    pub reason: Option<String>,
}

/// The agent's state word, first match wins.
pub fn state(agent: &Facts) -> State {
    match (agent.state.as_str(), agent.harness_state.as_deref()) {
        // Retired is final: nothing else about the seat matters.
        ("retired", _) => State::Retired,
        _ if agent.fault.is_some() => State::Fault,
        // A seat whose message path stopped polling or cannot hand off takes no messages,
        // however ready its harness looks. An `outdated` path still delivers.
        _ if stale_delivery(agent) => State::Fault,
        ("failed", _) => State::Fault,
        // st decides a seat with no session is down, and since when.
        ("down", _) => State::Down,
        // Signed out of its provider: a login on its host fixes it, not a restart. st names the
        // known error in `harness_error_state`; older daemons say `unauthenticated`.
        ("waiting", _) if agent.harness_error_state.as_deref() == Some("needs-login") => {
            State::NeedsLogin
        }
        ("waiting", Some("unauthenticated" | "needs-login")) => State::NeedsLogin,
        ("waiting", _) if agent.reason.as_deref() == Some("providerAuth") => State::NeedsLogin,
        ("running", _) if agent.activity.as_deref() == Some("compacting") => State::Compacting,
        ("running", Some("working")) => State::Working,
        ("running", _) => State::Idle,
        ("waiting", Some("ready" | "working" | "idle"))
            if agent.blocked_on.as_deref() == Some("human") =>
        {
            State::NeedsYou
        }
        // Blocked on its st channel is the seat's to fix (its mail is held), not the person's:
        // answering it cannot unblock it.
        ("waiting", Some("blocked")) if agent.blocked_on.as_deref() == Some("channel") => {
            State::Fault
        }
        ("waiting", Some("blocked")) => State::NeedsYou,
        // st withdraws an idle claim it has not heard renewed lately: the harness reads
        // `indeterminate` and the seat `waiting`, though it is up and reachable. That is an idle
        // seat nobody has spoken to, not one starting.
        ("waiting", Some("indeterminate"))
            if agent.observation.as_deref() == Some("stale")
                && matches!(agent.reachability.as_str(), "reachable" | "local") =>
        {
            State::Idle
        }
        ("waiting" | "starting" | "desired", _) => State::Starting,
        ("stopped", _) => State::Stopped,
        _ => State::Unknown,
    }
}

fn stale_delivery(agent: &Facts) -> bool {
    agent
        .delivery
        .as_ref()
        .is_some_and(|delivery| delivery.state == "stale")
}

/// Why an agent in `state` needs someone, from what st says about it.
fn because(agent: &Facts, state: State) -> Option<String> {
    match state {
        State::Down => agent.fault.clone().or_else(|| {
            agent
                .down_since
                .as_ref()
                .map(|since| format!("No running session since {since}"))
        }),
        State::Fault => agent
            .fault
            .clone()
            .or_else(|| {
                agent
                    .delivery
                    .as_ref()
                    .filter(|delivery| delivery.state == "stale")
                    .and_then(|delivery| delivery.reason.clone())
                    .filter(|reason| reason.starts_with("delivery-control-unavailable:"))
            })
            .or_else(|| {
                (agent.blocked_on.as_deref() == Some("channel")).then(|| {
                    let reason = agent.reason.as_deref();
                    let reason = reason
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default();
                    format!("Its st channel is not attached, so its mail is held{reason}")
                })
            })
            .or_else(|| {
                Some(if stale_delivery(agent) {
                    "Its message path stopped taking messages".into()
                } else {
                    "Its runtime failed".into()
                })
            }),
        State::NeedsYou => agent.ask.clone(),
        State::NeedsLogin => Some("Its harness is not logged in to its provider".into()),
        _ => None,
    }
}

fn work(label: &st3_client::WorkLabel) -> Work {
    Work {
        step: label.id.clone(),
        mission: label.mission_id.clone(),
        path: label.path.clone(),
        title: label.title.clone(),
        label: format!(
            "{} › {}",
            label.mission_id.trim_start_matches("mission/"),
            label.path
        ),
    }
}

/// The agent's presentation, from its row alone.
pub fn present(agent: &Facts) -> Presentation {
    let state = state(agent);
    Presentation {
        state,
        group: state.group(),
        because: because(agent, state),
        now: agent.current_work.first().map(|label| Now {
            work: work(label),
            goal: label.goal.clone(),
            since: label.since.clone(),
            holder_down_since: label.holder_down_since.clone(),
        }),
        next: agent
            .next_work
            .iter()
            .chain(agent.upcoming_work.iter().filter(|label| {
                agent
                    .next_work
                    .as_ref()
                    .is_none_or(|next| next.id != label.id)
            }))
            .take(NEXT_LIMIT)
            .map(work)
            .collect(),
        queued: agent.queued_work_count,
    }
}

#[cfg(test)]
mod tests;
