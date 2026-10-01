//! The view model. Screens draw only this; they never see st3 client types.
//!
//! The live client and the demo fixtures both produce a `World`. Anything the graph cannot
//! answer yet is an `Option` or a `Load`, so a screen can say "unknown" instead of guessing.

use std::collections::BTreeMap;

/// Something that arrives later. `Loading` is never drawn as empty.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(value) => Some(value),
            _ => None,
        }
    }
}

impl<T> Load<Vec<T>> {
    pub fn items(&self) -> &[T] {
        self.ready().map(Vec::as_slice).unwrap_or(&[])
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Live,
    Connecting,
    Offline(String),
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct World {
    pub person: String,
    pub host: String,
    pub link: Link,
    /// Peers this host's graph has diverged from: the same envelopes project a different graph
    /// here, so what stui shows can be wrong until the host is repaired.
    pub diverged: Vec<String>,
    pub attention: Load<Vec<Attention>>,
    pub agents: Load<Vec<Agent>>,
    pub missions: Load<Vec<Mission>>,
    pub machines: Load<Vec<Machine>>,
    pub worktrees: Load<Vec<Worktree>>,
    /// The person's paired devices.
    pub devices: Load<Vec<Device>>,
    pub conversations: BTreeMap<String, Load<Vec<Entry>>>,
    /// Missions nobody needs the person for, counted so Home can say what it is not showing.
    pub quiet_missions: usize,
}

// ------------------------------------------------------------------ attention

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// An agent or mission cannot move until the person answers.
    Stopped,
    /// Something broke or crossed a line.
    Alert,
    /// Worth doing today; nothing is blocked on it.
    Today,
    /// Messages and notes.
    Later,
}

impl Tier {
    pub fn title(self) -> &'static str {
        match self {
            Tier::Stopped => "somebody is stopped on you",
            Tier::Alert => "something broke",
            Tier::Today => "today",
            Tier::Later => "when there is time",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Attention {
    pub id: String,
    pub tier: Tier,
    pub title: String,
    /// Who is waiting, e.g. "Atlas Builder on harbor".
    pub waiting: Option<String>,
    pub age: String,
    pub mission: Option<String>,
    /// The agent that did the work or raised the item: who to talk to about it.
    pub agent: Option<String>,
    pub kind: AttentionKind,
    /// The actions st offers for this item. Empty in the demo, where every button works.
    pub actions: Vec<String>,
    /// Graph subjects the item points at, with their state when st says.
    pub related: Vec<(String, Option<String>)>,
    /// Who raised it, when st says.
    pub raised_by: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionKind {
    /// A human review gate: approve, or send it back with a reason the agent will read.
    Review {
        question: String,
        because: String,
        look_at: Vec<(String, String)>,
        step: String,
    },
    /// A human feedback gate: the agent wants words, not a verdict.
    Feedback {
        question: String,
        subject: String,
        excerpt: Vec<String>,
        link: Option<String>,
    },
    /// A planner proposes a mission; show the mission itself.
    Launch {
        planner: String,
        name: String,
        /// The proposed mission. It can be missing: a planner may not have produced one
        /// that validates, and then there is nothing honest to show but why.
        preview: Load<MissionPreview>,
    },
    /// A running mission wants to change its own plan.
    Revision {
        reason: String,
        changes: Vec<(char, String)>,
    },
    Fault {
        what: String,
        because: String,
        fix: Option<String>,
        source: String,
    },
    Message {
        from: String,
        body: String,
    },
    /// An agent is stopped until the person decides or answers something.
    Request {
        /// Who asks, named, and its graph id to reply to.
        from: String,
        from_id: String,
        question: String,
    },
}

impl AttentionKind {
    pub fn word(&self) -> &'static str {
        match self {
            AttentionKind::Review { .. } => "review",
            AttentionKind::Feedback { .. } => "feedback",
            AttentionKind::Launch { .. } => "launch",
            AttentionKind::Revision { .. } => "revision",
            AttentionKind::Fault { .. } => "fault",
            AttentionKind::Message { .. } => "message",
            AttentionKind::Request { .. } => "request",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MissionPreview {
    pub name: String,
    pub goals: Vec<String>,
    pub steps: Vec<PreviewStep>,
    pub agents: Vec<PreviewAgent>,
    pub workspace: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PreviewStep {
    pub name: String,
    pub assignee: String,
    pub after: Vec<String>,
    pub asks_you: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PreviewAgent {
    pub name: String,
    pub harness: Harness,
    pub host: String,
}

// --------------------------------------------------------------------- agents

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
    Omp,
    Pi,
    Unknown,
}

impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Omp => "omp",
            Harness::Pi => "pi",
            Harness::Unknown => "?",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Waiting on the person.
    NeedsYou,
    Fault,
    Working,
    Idle,
    Starting,
    Stopped,
    Unknown,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Agent {
    /// The graph path, a stable id.
    pub id: String,
    pub name: String,
    pub harness: Harness,
    pub state: AgentState,
    pub host: String,
    pub worktree: Option<String>,
    pub mission: Option<String>,
    pub step: Option<String>,
    pub activity: String,
    /// Found running on a host but not started by st.
    pub unmanaged: bool,
    pub parent: Option<String>,
    pub details: AgentDetails,
    /// A live terminal the person can open.
    pub terminal: bool,
}

/// What the details pane shows about an agent. Every field is optional: st may not say.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct AgentDetails {
    /// The goal of the step it holds now.
    pub goal: Option<String>,
    pub claimed: Option<String>,
    /// The next step queued for it, as "mission › step".
    pub next: Option<String>,
    pub queue: Vec<String>,
    pub queued: u64,
    pub harness_state: Option<String>,
    pub runtime: Option<String>,
    pub fault: Option<String>,
    pub under: Option<String>,
}

// ------------------------------------------------------------------- missions

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

// ---------------------------------------------------------------- fleet, trees

/// How this machine knows another fleet member is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    /// The machine stui is talking to.
    Here,
    /// It replicates with this machine directly.
    Direct,
    /// No direct link, but its agents were heard from recently through other members.
    Indirect,
    /// Not heard from recently.
    Offline,
    /// st cannot tell: its reports about the machine disagree.
    Unknown,
}

impl Reach {
    pub fn online(self) -> bool {
        matches!(self, Reach::Here | Reach::Direct | Reach::Indirect)
    }
    pub fn word(self) -> &'static str {
        match self {
            Reach::Here => "this machine",
            Reach::Direct => "connected",
            Reach::Indirect => "online via others",
            Reach::Offline => "offline",
            Reach::Unknown => "st cannot tell",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Machine {
    pub name: String,
    pub reach: Reach,
    /// Operating system and architecture, when st reports them; empty otherwise.
    pub platform: String,
    /// When anything was last heard from it: "4s ago", or "never".
    pub seen: String,
    pub load: Option<String>,
    pub links: Vec<(String, bool, String)>,
    pub you_are_here: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Worktree {
    pub path: String,
    pub host: String,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: u32,
    pub agents: Vec<String>,
    pub missions: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub state: String,
    pub scopes: Vec<String>,
    pub expires: String,
}

// -------------------------------------------------------------- conversations

pub use st3_conversation_ui::{Body, Entry, ToolState};
