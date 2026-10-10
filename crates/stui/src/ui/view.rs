//! The view model. Screens draw only this; they never see st3 client types.
//!
//! The live client and the demo fixtures both produce a `World`. Anything the graph cannot
//! answer yet is an `Option` or a `Load`, so a screen can say "unknown" instead of guessing.

pub use st3_ui_model::missions::{Mission, Step, StepState, Word};
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
    /// Lists st stopped serving after they loaded, each with st's reason: the rows shown are the
    /// last it sent, and may be out of date until it serves them again. Left out of the shared
    /// contract while empty: the phone shows this on its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<String>,
    /// Whether the missions window is followed now. Not part of the shared contract.
    #[serde(skip)]
    pub missions_followed: bool,
    /// The number of active missions st reports, when the mission rows are not followed.
    #[serde(skip)]
    pub active_missions: Option<usize>,
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
    /// Token spend over the Usage tab's period: one row per agent, mission run, step, model,
    /// account and host, as st reports it. Not in the shared demo world: the phone has no
    /// usage screen yet.
    #[serde(skip)]
    pub usage: Load<Vec<st3_client::UsageRow>>,
    /// Each account's freshest limits reading, from the same read.
    #[serde(skip)]
    pub usage_limits: Vec<st3_client::UsageLimit>,
    #[serde(skip)]
    pub agent_messages: Option<st3_client::AgentMessageEstimate>,
    /// The clients connected to this member now and those seen in the last few minutes.
    pub clients: Load<Vec<Connected>>,
}

/// A client connected to this member, as it describes itself.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Connected {
    /// Its reported name and build, "stui 0.1.0+ab12cd3"; empty when it sent none.
    pub client: String,
    /// The person or agent it acts for, and the paired device when one is acting.
    pub who: String,
    pub device: Option<String>,
    pub member: String,
    /// `local`, `gateway` or `tailscale`.
    pub via: String,
    pub connected: bool,
    /// "since 4m", or "seen 2m ago" once it has gone.
    pub when: String,
    /// What it follows: windows, conversations and terminals.
    pub follows: Vec<String>,
    /// Its reported build is older than the member's own.
    pub older: bool,
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
    /// The mission step waiting on this ask, on an ask a mission step made.
    pub blocked: Option<Blocked>,
    /// Every agent conversation this item shows in, as st names them: the item is an alert in
    /// each. Empty when st does not name one (an older st), and then `agent` is the guess.
    pub conversations: Vec<String>,
}

impl Attention {
    /// Whether it blocks or waits on the person. An update asks nothing and a message stays in
    /// its conversation; every other card waits on an answer.
    pub fn is_alert(&self) -> bool {
        self.kind.is_alert()
    }

    /// Whether it shows in the conversation of `agent`.
    pub fn is_in(&self, agent: &str) -> bool {
        if self.conversations.is_empty() {
            self.agent.as_deref() == Some(agent)
        } else {
            self.conversations.iter().any(|conversation| conversation == agent)
        }
    }
}

/// The mission step that asked and waits for the answer.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Blocked {
    pub step: String,
    pub goal: String,
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
        /// A feedback gate (`mode="feedback"`): sending it back asks for changes, a new
        /// attempt, where an approval gate's sends back a rejection that fails the step.
        feedback: bool,
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
    /// A native prompt a seat's harness shows in its terminal and waits on: a permission, a
    /// question or a review. It clears when the harness reports it gone, however it was answered.
    Prompt {
        /// The seat, and its graph id.
        seat: String,
        seat_id: String,
        /// What st says it is about; the call it would make is in the seat's conversation.
        text: String,
        /// Answers a client may send (`allow`, `deny`); empty when the prompt is answered in
        /// the seat's terminal.
        answers: Vec<String>,
        /// The harness observation that opened it, which an answer names.
        episode: String,
    },
    /// A harness not signed in to its provider; one sign-in answers every seat that shares it.
    Login {
        text: String,
        seats: Vec<String>,
    },
    /// An agent is stopped until the person decides or answers something.
    Request {
        /// Who asks, named, and its graph id to reply to.
        from: String,
        from_id: String,
        question: String,
        /// A structured ask's typed fields (#1010): summary, reasons, links, named answers and
        /// a recommendation. Absent on a free-text ask.
        structured: Option<Box<st3_client::StructuredRequest>>,
    },
    /// Information the person asked an agent for (`st work update`): it asks nothing, and it
    /// clears once they read it.
    Update {
        from: String,
        body: String,
        /// The person's run or step, or their message, it answers.
        about: String,
        /// Links it names: label and where.
        subjects: Vec<(String, String)>,
    },
}

impl AttentionKind {
    /// Whether a card of this kind blocks or waits on the person.
    pub fn is_alert(&self) -> bool {
        !matches!(self, AttentionKind::Update { .. } | AttentionKind::Message { .. })
    }

    pub fn word(&self) -> &'static str {
        match self {
            AttentionKind::Review { .. } => "review",
            AttentionKind::Feedback { .. } => "feedback",
            AttentionKind::Launch { .. } => "launch",
            AttentionKind::Revision { .. } => "revision",
            AttentionKind::Fault { .. } => "fault",
            AttentionKind::Message { .. } => "message",
            AttentionKind::Prompt { .. } => "prompt",
            AttentionKind::Login { .. } => "login",
            AttentionKind::Request { .. } => "request",
            AttentionKind::Update { .. } => "update",
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
    /// Its harness is not logged in to its provider: someone has to log it in on its host.
    /// It clears by itself once st sees the harness signed in; no restart is needed.
    NeedsLogin,
    Fault,
    Working,
    /// Its harness is compacting its conversation. A status of the seat, not work and never an
    /// alert: nothing waits on the person.
    Compacting,
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
    /// The subagents its harness runs now, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<Subagent>,
}

/// A subagent an agent's harness runs inside its own session. It belongs to the agent and is not
/// a seat: it has no conversation, terminal or actions of its own.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Subagent {
    /// The harness's own ID for it.
    pub id: String,
    pub kind: Option<String>,
    pub description: Option<String>,
    /// How long it has run, as an agent's activity reads ("3m"); empty when st does not say.
    pub age: String,
}

impl Subagent {
    /// What it does, else its ID, then its type.
    pub fn label(&self) -> String {
        let mut label = self.description.clone().unwrap_or_else(|| self.id.clone());
        if let Some(kind) = &self.kind {
            label.push_str(" · ");
            label.push_str(kind);
        }
        label
    }
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
    /// The model its harness last reported using, as reported ("claude-sonnet-5-5").
    pub model: Option<String>,
    /// What the step it holds last reported (`st work progress`): the status a person reads first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
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
