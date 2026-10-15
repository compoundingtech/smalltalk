//! An attention row's presentation: the Now group and card it shows as, who **Chat about this**
//! reaches, who raised it, and the step it holds. st serves it on every attention row
//! (`Attention.presentation`), so clients read one answer instead of deriving their own
//! (docs/clients/ui-contract.md, Attention row).

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// The Now groups a card shows in, by their `fixtures/clients/words.json` `tier` ids, in display
/// order. No card is in the words' `alert` group ("something broke") yet: a fault is on no Now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Somebody is stopped on the person.
    Stopped,
    Today,
    /// When there is time.
    Later,
}

/// The card an attention row shows as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Review,
    /// A human gate in feedback mode.
    Feedback,
    Request,
    /// Information the person asked for: nothing waits on it.
    Update,
    Launch,
    Revision,
    /// A prompt the seat's harness shows and waits on.
    Prompt,
    Login,
    Fault,
}

/// An agent or other actor, with the name a person reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Who {
    pub id: String,
    pub label: String,
}

/// The step an attention row holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Waiting {
    pub step: String,
    /// The step's name within its mission.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Presentation {
    /// The Now group, or `None` for a row that is not on Now (never "unknown").
    pub tier: Option<Tier>,
    /// The card, or `None` for a row that is no card at all (an unread message stays in its
    /// conversation).
    pub kind: Option<Kind>,
    /// Who **Chat about this** reaches; `None` means the chief of staff.
    pub agent: Option<Who>,
    pub raised_by: Option<Who>,
    pub waiting: Option<Waiting>,
}

/// The facts of an attention row the presentation reads. st deserializes them straight from the
/// row it serves.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Facts {
    pub attention_kind: String,
    pub review_mode: Option<String>,
    /// Present on a person step that carries an update; its content is not read here.
    pub update: Option<IgnoredAny>,
    pub source_id: String,
    pub conversation_id: Option<String>,
    pub requester_id: Option<String>,
    pub mission_id: Option<String>,
    pub step_run_id: Option<String>,
}

/// What st knows beside the row: its agents and its steps.
pub trait Fleet {
    /// The name a person reads for this agent, when st knows the agent.
    fn agent_label(&self, agent: &str) -> Option<String>;
    /// The agent holding this step.
    fn claimant(&self, step: &str) -> Option<String>;
    /// An agent holding a step of this mission.
    fn working_in(&self, mission: &str) -> Option<String>;
    /// This step's name within its mission.
    fn step_path(&self, step: &str) -> Option<String>;
}

/// The Now group and card for an attention kind.
pub fn card(
    attention_kind: &str,
    review_mode: Option<&str>,
    update: bool,
) -> (Option<Tier>, Option<Kind>) {
    match attention_kind {
        "human-gate" if review_mode == Some("feedback") => {
            (Some(Tier::Stopped), Some(Kind::Feedback))
        }
        "human-gate" => (Some(Tier::Stopped), Some(Kind::Review)),
        "person-step" if update => (Some(Tier::Later), Some(Kind::Update)),
        "person-step" | "agent-request" => (Some(Tier::Stopped), Some(Kind::Request)),
        "launch-approval" => (Some(Tier::Today), Some(Kind::Launch)),
        "revision-approval" => (Some(Tier::Today), Some(Kind::Revision)),
        custom if custom.starts_with("custom.") => (Some(Tier::Today), Some(Kind::Request)),
        "harness-prompt" => (Some(Tier::Stopped), Some(Kind::Prompt)),
        "harness-login" => (Some(Tier::Stopped), Some(Kind::Login)),
        // st sends each fault to the agent that owns it: a fault card, never on Now.
        "fault" => (None, Some(Kind::Fault)),
        // A message stays in its conversation.
        _ => (None, None),
    }
}

/// Whether this actor is st's own machinery, which a person reads as "st".
fn is_st(id: &str) -> bool {
    id.starts_with("daemon/") || id == "agent/st3/reconciler"
}

fn agent(id: &str, fleet: &impl Fleet) -> Who {
    Who {
        id: id.to_owned(),
        label: fleet
            .agent_label(id)
            .unwrap_or_else(|| id.trim_start_matches("agent/").to_owned()),
    }
}

pub fn present(row: &Facts, fleet: &impl Fleet) -> Presentation {
    let (tier, kind) = card(
        &row.attention_kind,
        row.review_mode.as_deref(),
        row.update.is_some(),
    );
    // The conversation st names, else the step's claimant, else an agent working in the
    // mission, else the agent that asked.
    let agent_id = row
        .conversation_id
        .clone()
        .or_else(|| {
            row.step_run_id
                .as_deref()
                .and_then(|step| fleet.claimant(step))
        })
        .or_else(|| {
            row.mission_id
                .as_deref()
                .and_then(|mission| fleet.working_in(mission))
        })
        .or_else(|| {
            row.requester_id
                .clone()
                .filter(|id| id.starts_with("agent/"))
        });
    // The requester st names, else the agent behind it; a gate or fault nobody filed by hand
    // comes from its mission.
    let raised_by = match row.requester_id.as_deref().or(agent_id.as_deref()) {
        Some(id) if is_st(id) => Some(Who {
            id: id.to_owned(),
            label: "st".into(),
        }),
        Some(id) if id.starts_with("agent/") => Some(agent(id, fleet)),
        Some(id) => Some(Who {
            id: id.to_owned(),
            label: id.to_owned(),
        }),
        None => row.mission_id.as_deref().map(|mission| Who {
            id: mission.to_owned(),
            label: format!("the {} mission", mission.trim_start_matches("mission/")),
        }),
    };
    let waiting = row.step_run_id.as_deref().and_then(|step| {
        Some(Waiting {
            step: step.to_owned(),
            path: fleet.step_path(step)?,
        })
    });
    Presentation {
        tier,
        kind,
        agent: agent_id.as_deref().map(|id| agent(id, fleet)),
        raised_by,
        waiting,
    }
}

#[cfg(test)]
mod tests;
