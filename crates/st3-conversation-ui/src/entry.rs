#[derive(Clone, Debug, Hash, serde::Serialize)]
pub struct Entry {
    pub id: String,
    pub at: String,
    pub body: Body,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
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
        /// The recipient's harness has it: st delivered it into the agent's session.
        delivered: bool,
        /// Spoken and transcribed (st's `dictated` tag): it may hold transcription mistakes.
        dictated: bool,
    },
    /// A graph event worth a line: a step became ready, a run started.
    Event(String),
    /// A message sent from here that st has not reported back yet, or that failed.
    /// `unconfirmed` means st did not answer: the message may have arrived, and sending it
    /// again is safe because st answers a repeat with the first result.
    Pending {
        text: String,
        failed: Option<String>,
        unconfirmed: bool,
    },
}
