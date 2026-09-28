//! The view model. Screens draw only this; they never see st3 client types.
//!
//! The live client and the demo fixtures both produce a `World`. Anything the graph cannot
//! answer yet is an `Option` or a `Load`, so a screen can say "unknown" instead of guessing.

use std::collections::BTreeMap;

/// Something that arrives later. `Loading` is never drawn as empty.
#[derive(Clone, Debug)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Live,
    Connecting,
    Offline(String),
}

#[derive(Clone, Debug)]
pub struct World {
    pub person: String,
    pub host: String,
    pub link: Link,
    pub attention: Load<Vec<Attention>>,
    pub agents: Load<Vec<Agent>>,
    pub missions: Load<Vec<Mission>>,
    pub machines: Load<Vec<Machine>>,
    pub worktrees: Load<Vec<Worktree>>,
    pub conversations: BTreeMap<String, Load<Vec<Entry>>>,
    /// Missions nobody needs the person for, counted so Home can say what it is not showing.
    pub quiet_missions: usize,
}

// ------------------------------------------------------------------ attention

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Clone, Debug)]
pub struct Attention {
    pub id: String,
    pub tier: Tier,
    pub title: String,
    /// Who is waiting, e.g. "Atlas Builder on harbor".
    pub waiting: Option<String>,
    pub age: String,
    pub mission: Option<String>,
    pub kind: AttentionKind,
    /// The actions st offers for this item. Empty in the demo, where every button works.
    pub actions: Vec<String>,
}

#[derive(Clone, Debug)]
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
        preview: MissionPreview,
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
        }
    }
}

#[derive(Clone, Debug)]
pub struct MissionPreview {
    pub name: String,
    pub goals: Vec<String>,
    pub steps: Vec<PreviewStep>,
    pub agents: Vec<PreviewAgent>,
    pub workspace: String,
}

#[derive(Clone, Debug)]
pub struct PreviewStep {
    pub name: String,
    pub assignee: String,
    pub after: Vec<String>,
    pub asks_you: bool,
}

#[derive(Clone, Debug)]
pub struct PreviewAgent {
    pub name: String,
    pub harness: Harness,
    pub host: String,
}

// --------------------------------------------------------------------- agents

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Clone, Debug)]
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
}

// ------------------------------------------------------------------- missions

/// One word naming who has to move, shared by every mission surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Word {
    Decision,
    Stalled,
    Unclaimed,
    Working,
    Held,
    Idle,
    Done,
    Failed,
}

impl Word {
    pub fn name(self) -> &'static str {
        match self {
            Word::Decision => "needs you",
            Word::Stalled => "stalled",
            Word::Unclaimed => "unclaimed",
            Word::Working => "working",
            Word::Held => "held",
            Word::Idle => "idle",
            Word::Done => "done",
            Word::Failed => "failed",
        }
    }
    pub fn explain(self) -> &'static str {
        match self {
            Word::Decision => "a step is waiting for your answer",
            Word::Stalled => "a step has an owner who is not moving",
            Word::Unclaimed => "a step is ready and nobody has taken it",
            Word::Working => "an agent is doing a step now",
            Word::Held => "waiting on something outside the fleet",
            Word::Idle => "running, with nothing ready",
            Word::Done => "every step finished",
            Word::Failed => "a step failed and nothing retried it",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Done,
    Working,
    Ready,
    Waiting,
    NeedsYou,
    Failed,
    Pending,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub name: String,
    pub state: StepState,
    pub owner: Option<String>,
    pub note: Option<String>,
    pub after: Vec<String>,
    pub age: String,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct Machine {
    pub name: String,
    pub online: bool,
    pub platform: String,
    pub seen: String,
    pub load: Option<String>,
    pub links: Vec<(String, bool, String)>,
    pub you_are_here: bool,
}

#[derive(Clone, Debug)]
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

// -------------------------------------------------------------- conversations

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub at: String,
    pub body: Body,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
pub enum Body {
    /// Text typed into the harness: the person, or a delivery the harness shows as a prompt.
    User(String),
    Assistant(String),
    Thinking(String),
    Tool {
        title: String,
        state: ToolState,
        output: Vec<String>,
    },
    /// A Small Talk message between agents or people, shown in the same stream.
    Mail {
        from: String,
        to: String,
        subject: String,
        body: String,
    },
    /// A graph event worth a line: a step became ready, a run started.
    Event(String),
}
