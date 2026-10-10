//! The application owns layout and input; published pieces own transport and conversation.
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};
use st3_client::{
    ActionStatus, Agent, Client, ClientError, Fence, MessageSendParameters, Resource,
};
use st3_conversation_ui::{Cache, Theme, Timeline, adapt};
use st3_feed::{Command, Update, Window};
use std::collections::{BTreeMap, HashSet};

/// Kept unchanged until st acknowledges it. Retrying a lost response cannot duplicate the send.
#[derive(Clone)]
pub struct Send {
    pub id: String,
    pub key: String,
    pub to: String,
    pub body: String,
}

pub async fn send(client: &Client, request: &Send) -> Result<String, ClientError> {
    let snapshot = client.capabilities().await?.snapshot.id;
    let result = client
        .message_send(
            request.id.clone(),
            request.key.clone(),
            Fence {
                snapshot_id: snapshot,
                ..Fence::default()
            },
            MessageSendParameters {
                to: request.to.clone(),
                content: request.body.clone(),
                title: None,
                in_reply_to: None,
                session_id: None,
                tags: vec![],
                fyi: false,
                question: false,
                attachments: vec![],
                signature: None,
            },
        )
        .await?;
    if !matches!(
        result.value.status,
        ActionStatus::Accepted | ActionStatus::Completed
    ) {
        return Err(ClientError::Protocol(format!(
            "Message action {:?}: {}",
            result.value.status, result.value.operation_id
        )));
    }
    // Acceptance is not delivery; the conversation will show subsequent delivery evidence.
    Ok(format!("Accepted: {}", result.value.operation_id))
}

pub struct App {
    pub agents: Vec<Agent>,
    pub selected: Option<String>,
    pub timeline: Timeline,
    pub client: Option<Client>,
    pub live: bool,
    pub draft: String,
    pub pending: Option<Send>,
    pub sending: bool,
    pub status: String,
    pub conversation_status: String,
    cache: Cache,
}

impl Default for App {
    fn default() -> Self {
        Self {
            agents: vec![],
            selected: None,
            timeline: Timeline::default(),
            client: None,
            live: false,
            draft: String::new(),
            pending: None,
            sending: false,
            status: "Connecting…".into(),
            conversation_status: "Choose an agent".into(),
            cache: Cache::default(),
        }
    }
}

impl App {
    /// Feed updates replace windows, while the shared timeline merges conversation revisions.
    pub fn update(&mut self, update: Update) -> Option<Command> {
        match update {
            Update::Connected(client) => {
                self.client = Some(client);
                self.live = false;
                self.status = "Connected · waiting for agents…".into();
            }
            Update::Offline(reason) => {
                self.live = false;
                self.client = None;
                self.status = format!("Offline · {reason} · showing last received rows");
            }
            Update::Window {
                window: Window::Agents,
                items,
                has_more,
                ..
            } => {
                self.agents = items
                    .into_iter()
                    .filter_map(|row| match row {
                        Resource::Agent(agent) => Some(agent),
                        _ => None,
                    })
                    .collect();
                self.live = true;
                self.status = if has_more {
                    "Live · first 200 agents"
                } else {
                    "Live"
                }
                .into();
                if self.pending.is_none()
                    && self
                        .selected
                        .as_ref()
                        .is_none_or(|id| !self.agents.iter().any(|agent| &agent.header.id == id))
                {
                    self.selected = self.agents.first().map(|agent| agent.header.id.clone());
                    self.timeline = Timeline::default();
                    self.conversation_status = if self.selected.is_some() {
                        "Loading conversation…"
                    } else {
                        "No agents"
                    }
                    .into();
                    return Some(self.subscription());
                }
            }
            Update::WindowFailed(Window::Agents, reason) => {
                self.live = false;
                self.status = format!("Agents unavailable · {reason}");
            }
            Update::Conversation {
                target,
                session_id,
                replace,
                has_more,
                items,
            } if self.selected.as_ref() == Some(&target) => {
                self.timeline.apply(st3_conversation_ui::Frame {
                    replace,
                    has_more,
                    items,
                    session_id: Some(session_id),
                });
                self.conversation_status = if self.timeline.more_before() {
                    "Recent conversation · earlier history omitted"
                } else {
                    "Conversation"
                }
                .into();
            }
            Update::ConversationFailed {
                target, message, ..
            } if self.selected.as_ref() == Some(&target) => {
                self.conversation_status = format!("Conversation unavailable · {message}")
            }
            _ => {}
        }
        None
    }

    fn subscription(&self) -> Command {
        Command::Converse {
            targets: self.selected.iter().cloned().collect(),
        }
    }

    pub fn select(&mut self, delta: isize) -> Option<Command> {
        if self.agents.is_empty() || self.pending.is_some() || self.sending {
            return None;
        }
        let current = self
            .agents
            .iter()
            .position(|agent| Some(&agent.header.id) == self.selected.as_ref())
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.agents.len() as isize) as usize;
        self.selected = Some(self.agents[next].header.id.clone());
        self.timeline = Timeline::default();
        self.conversation_status = "Loading conversation…".into();
        Some(self.subscription())
    }

    pub fn submit(&mut self) -> Option<Send> {
        if !self.live || self.sending || self.draft.trim().is_empty() {
            return None;
        }
        if self.pending.is_none() {
            let to = self.selected.clone()?;
            let (id, key) = st3_feed::action_pair();
            self.pending = Some(Send {
                id,
                key,
                to,
                body: self.draft.clone(),
            });
        }
        self.sending = true;
        self.status = "Sending…".into();
        self.pending.clone()
    }

    pub fn sent(&mut self, result: Result<String, ClientError>) {
        self.sending = false;
        match result {
            Ok(notice) => {
                self.draft.clear();
                self.pending = None;
                self.status = notice;
            }
            Err(error) => {
                self.status = format!(
                    "{} · Enter retries the same send; Esc discards retry",
                    error.plain()
                )
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(frame.area());
        frame.render_widget(
            Paragraph::new(format!(
                "↑/↓ agent · type message · Enter send/retry · Esc clear · Ctrl-C quit\n{}",
                self.status
            )),
            rows[0],
        );
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(28), Constraint::Min(1)])
            .split(rows[1]);
        let items: Vec<_> = self
            .agents
            .iter()
            .map(|agent| ListItem::new(format!("{}\n{}", agent.name, agent.state)))
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(if self.live {
                "Agents"
            } else {
                "Agents · stale"
            }))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut selected = ListState::default().with_selected(
            self.agents
                .iter()
                .position(|agent| Some(&agent.header.id) == self.selected.as_ref()),
        );
        frame.render_stateful_widget(list, columns[0], &mut selected);
        let block = Block::bordered().title(self.conversation_status.clone());
        let inner = block.inner(columns[1]);
        frame.render_widget(block, columns[1]);
        let names = self
            .agents
            .iter()
            .map(|agent| (agent.header.id.clone(), agent.name.clone()))
            .collect::<BTreeMap<_, _>>();
        let entries = adapt::conversation(&self.timeline.items, &names);
        let doc = self.cache.render(
            &entries,
            inner.width.max(1) as usize,
            &HashSet::new(),
            "…",
            &Theme::default(),
        );
        let tail = doc.lines.len().saturating_sub(inner.height as usize);
        frame.render_widget(
            Paragraph::new(doc.lines.into_iter().skip(tail).collect::<Vec<_>>()),
            inner,
        );
        frame.render_widget(
            Paragraph::new(self.draft.as_str()).block(
                Block::bordered().title(format!("To {}", self.selected.as_deref().unwrap_or("—"))),
            ),
            rows[2],
        );
    }
}
