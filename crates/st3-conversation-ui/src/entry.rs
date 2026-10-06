/// How much of a conversation's tool work shows: every call with its output, or (simplified)
/// each call on one line and a run of calls on one line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Density {
    #[default]
    Full,
    Simple,
}

/// The id that opens a run of tool calls starting at `first`, in the expanded set.
pub fn bundle_id(first: &str) -> String {
    format!("bundle:{first}")
}

/// How many rows the person sees in the simplified conversation: each entry is one, and a run of
/// tool calls is one for the whole run.
pub fn display_rows(entries: &[Entry]) -> usize {
    let mut rows = 0;
    let mut in_run = false;
    for entry in entries {
        let tool = matches!(entry.body, Body::Tool { .. });
        if !(tool && in_run) {
            rows += 1;
        }
        in_run = tool;
    }
    rows
}

/// Whether an entry folds until opened: tool calls, and mail the person is not part of.
pub fn folds(body: &Body) -> bool {
    match body {
        Body::Tool { .. } => true,
        Body::Mail { from, to, .. } => from != "you" && to != "you",
        _ => false,
    }
}

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

/// An image a message carries: st reads it by `sha256`, for a reader of `message`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct MailImage {
    pub sha256: String,
    pub message: String,
    pub media_type: String,
    pub name: Option<String>,
    pub size: u64,
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
        /// The images it carries; their bytes stay with st until a reader opens one.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        images: Vec<MailImage>,
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
