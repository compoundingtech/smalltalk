mod cache;
mod model;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use model::{Model, clean_message_text, timeline_line};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use st3_client::{
    AttentionResolveParameters, Client, ClientError, ErrorCode, Fence, LaunchCreateParameters,
    LaunchTarget, LaunchVariantParameters, MessageSendParameters, Resource, TargetParameters,
    TerminalInputMode, TerminalInputParameters, TerminalScreen,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    io::{self, IsTerminal, Stdout},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const TABS: [&str; 4] = ["Now", "Chat", "Control", "Fleet"];
const TIMELINE_REFRESH: Duration = Duration::from_secs(30);
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Confirm,
    ActionReason,
    ImportConfirm,
    Chat,
    Title,
    Request,
    Mission,
    Workspace,
}
struct Attached {
    terminal_id: String,
    attachment_id: String,
    screen: TerminalScreen,
}
enum Update {
    Partial(Box<Model>),
    Model(Box<Model>),
    Timeline(String, Vec<st3_client::TimelineEntry>, bool, usize),
    Messages(String, model::Collection),
    TimelineInvalidated(String),
    TimelineCursorGap,
    Error(String),
}
struct App {
    model: Model,
    tab: usize,
    selected: [usize; 4],
    scroll: [u16; 4],
    sidebar: bool,
    show_system_missions: bool,
    mode: Mode,
    input: String,
    launch: [String; 4],
    attached: Option<Attached>,
    selection_mode: bool,
    selection_frame_drawn: bool,
    history_open: bool,
    history_scroll: u16,
    history_max_scroll: Cell<u16>,
    history_page_limit: usize,
    timeline_requested_pages: usize,
    select_control: Cell<Rect>,
    history_control: Cell<Rect>,
    history_load_control: Cell<Rect>,
    status_details: bool,
    pending_action: Option<(String, String)>,
    pending_reason: Option<String>,
    pending_import: Option<String>,
    action_result: Option<String>,
    notice: Option<String>,
    chat_max_scroll: Cell<u16>,
    sidebar_offsets: [Cell<usize>; 4],
    live_ready: bool,
    dirty: bool,
    timeline_requested: Option<String>,
    timeline_cache: BTreeMap<String, (Vec<st3_client::TimelineEntry>, bool)>,
    chat_scroll_cache: BTreeMap<String, u16>,
    chat_draft_cache: BTreeMap<String, String>,
    last_timeline: Instant,
    messages_requested: Option<String>,
    last_messages: Instant,
    last_terminal: Instant,
}
impl App {
    fn new(model: Model) -> Self {
        Self {
            model,
            tab: 0,
            selected: [0; 4],
            scroll: [0, u16::MAX, 0, 0],
            sidebar: true,
            show_system_missions: false,
            mode: Mode::Normal,
            input: String::new(),
            launch: Default::default(),
            attached: None,
            selection_mode: false,
            selection_frame_drawn: false,
            history_open: false,
            history_scroll: 0,
            history_max_scroll: Cell::new(0),
            history_page_limit: model::MAX_PAGES,
            timeline_requested_pages: model::MAX_PAGES,
            select_control: Cell::new(Rect::default()),
            history_control: Cell::new(Rect::default()),
            history_load_control: Cell::new(Rect::default()),
            status_details: false,
            pending_action: None,
            pending_reason: None,
            pending_import: None,
            action_result: None,
            notice: None,
            chat_max_scroll: Cell::new(0),
            sidebar_offsets: [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)],
            live_ready: false,
            dirty: true,
            timeline_requested: None,
            timeline_cache: BTreeMap::new(),
            chat_scroll_cache: BTreeMap::new(),
            chat_draft_cache: BTreeMap::new(),
            last_timeline: Instant::now(),
            messages_requested: None,
            last_messages: Instant::now(),
            last_terminal: Instant::now(),
        }
    }
    fn remember_chat_scroll(&mut self) {
        if let Some(id) = self.selected_session_id() {
            self.chat_scroll_cache.insert(id, self.scroll[1]);
            while self.chat_scroll_cache.len() > 32 {
                if let Some(oldest) = self.chat_scroll_cache.keys().next().cloned() {
                    self.chat_scroll_cache.remove(&oldest);
                }
            }
        }
    }
    fn invalidate_timelines(&mut self) {
        self.timeline_cache.clear();
        // Keep the selected conversation visible while the fresh page loads.
        // A later selection still starts from an empty cache, never from this view.
        self.last_timeline = Instant::now() - TIMELINE_REFRESH;
    }
    fn restore_chat_scroll(&mut self) {
        self.scroll[1] = self
            .selected_session_id()
            .and_then(|id| self.chat_scroll_cache.get(&id).copied())
            .unwrap_or(u16::MAX);
    }
    fn chat_draft_key(&self) -> Option<String> {
        self.selected_session_id()
            .or_else(|| self.peer().map(|peer| peer.header.id.clone()))
    }
    fn start_chat_composer(&mut self) {
        self.input = self
            .chat_draft_key()
            .and_then(|key| self.chat_draft_cache.get(&key).cloned())
            .unwrap_or_default();
        self.mode = Mode::Chat;
    }
    fn remember_chat_draft(&mut self) {
        if let Some(key) = self.chat_draft_key() {
            if self.input.is_empty() {
                self.chat_draft_cache.remove(&key);
            } else {
                self.chat_draft_cache.insert(key, self.input.clone());
                while self.chat_draft_cache.len() > 32 {
                    if let Some(oldest) = self.chat_draft_cache.keys().next().cloned() {
                        self.chat_draft_cache.remove(&oldest);
                    }
                }
            }
        }
    }
    fn peer(&self) -> Option<&st3_client::Agent> {
        self.agent_tree()
            .get(self.selected[1])
            .map(|(agent, _)| *agent)
    }
    fn agent_tree(&self) -> Vec<(&st3_client::Agent, usize)> {
        let mut agents = self
            .model
            .agents()
            .filter(|agent| !matches!(agent.state.as_str(), "stopped" | "failed"))
            .collect::<Vec<_>>();
        // The API can include stopped historical seats. Keep them browseable,
        // but open Chat on an active conversation instead of an old fixture.
        agents.sort_by(|left, right| {
            let priority = |agent: &st3_client::Agent| {
                (
                    if agent.state == "running" { 0 } else { 1 },
                    if agent.active_work_count > 0 { 0 } else { 1 },
                )
            };
            priority(left)
                .cmp(&priority(right))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.header.id.cmp(&right.header.id))
        });
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        fn add<'a>(
            agent: &'a st3_client::Agent,
            depth: usize,
            agents: &[&'a st3_client::Agent],
            seen: &mut HashSet<String>,
            result: &mut Vec<(&'a st3_client::Agent, usize)>,
        ) {
            if !seen.insert(agent.header.id.clone()) {
                return;
            }
            result.push((agent, depth));
            for child in agents
                .iter()
                .filter(|child| agent_is_child_of(child, agent))
            {
                add(child, depth + 1, agents, seen, result);
            }
        }
        for agent in &agents {
            if !agents.iter().any(|parent| agent_is_child_of(agent, parent)) {
                add(agent, 0, &agents, &mut seen, &mut result);
            }
        }
        for agent in &agents {
            add(agent, 0, &agents, &mut seen, &mut result);
        }
        result
    }
    fn undeclared_session(&self) -> Option<&st3_client::Session> {
        let index = self.selected[1].checked_sub(self.agent_tree().len())?;
        self.model.undeclared_sessions().nth(index)
    }
    fn selected_session_id(&self) -> Option<String> {
        self.peer()
            .and_then(|peer| {
                peer.current_session_id.clone().or_else(|| {
                    self.model
                        .sessions
                        .items
                        .iter()
                        .find_map(|item| match item {
                            st3_client::Resource::Session(session)
                                if session.owner_id == peer.header.id
                                    && session.state == "running" =>
                            {
                                Some(session.header.id.clone())
                            }
                            _ => None,
                        })
                })
            })
            .or_else(|| {
                self.undeclared_session()
                    .map(|session| session.header.id.clone())
            })
    }
    fn runtime(&self) -> Option<&st3_client::Runtime> {
        let peer = self.peer()?;
        self.model
            .runtimes()
            .find(|runtime| runtime.owner_id == peer.header.id && runtime.terminal_id.is_some())
    }
    fn agent_on_host(&self, agent_id: &str, host_id: &str) -> bool {
        self.model
            .runtimes()
            .any(|runtime| runtime.owner_id == agent_id && runtime.owner_host_id == host_id)
    }
    fn work_age(&self, work_id: &str) -> Option<String> {
        self.model
            .work()
            .find(|work| work.header.id == work_id)
            .map(|work| age_label(&work.header.updated_at, &chrono::Utc::now().to_rfc3339()))
    }
    fn count(&self) -> usize {
        self.count_for(self.tab)
    }
    fn count_for(&self, tab: usize) -> usize {
        match tab {
            0 => self.model.attention().count(),
            1 => self.agent_tree().len() + self.model.undeclared_sessions().count(),
            2 => self.control_missions().len(),
            _ => self.model.machines().count(),
        }
    }
    fn mission_group(&self, mission: &st3_client::Mission) -> &'static str {
        let steps = self
            .model
            .work()
            .filter(|work| mission.runs.last() == Some(&work.mission_run_id));
        let states = steps.map(|work| work.state.as_str()).collect::<Vec<_>>();
        if mission.state == "blocked" || states.contains(&"blocked") {
            "Blocked"
        } else if states
            .iter()
            .any(|state| matches!(*state, "claimed" | "running"))
            || mission.state == "running"
        {
            "Running"
        } else if mission.state == "standing" {
            "Standing"
        } else if states
            .iter()
            .any(|state| matches!(*state, "waiting" | "blocked"))
        {
            "Waiting"
        } else if matches!(mission.state.as_str(), "ready" | "draft") {
            "Drafts"
        } else {
            "Archive"
        }
    }
    fn control_missions(&self) -> Vec<&st3_client::Mission> {
        let mut missions = self
            .model
            .missions()
            .filter(|mission| {
                self.show_system_missions || !mission.header.id.starts_with("mission/__st3/")
            })
            .collect::<Vec<_>>();
        missions.sort_by_key(|mission| match self.mission_group(mission) {
            "Blocked" => 0,
            "Waiting" => 1,
            "Running" => 2,
            "Standing" => 3,
            "Drafts" => 4,
            _ => 5,
        });
        missions
    }
    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        if self.attached.is_some() {
            frame.render_widget(
                Paragraph::new(" ← Return to Smalltalk (Ctrl+\\)")
                    .style(Style::default().fg(Color::Cyan)),
                chunks[0],
            );
        } else {
            let connection = if self.live_ready {
                "● Online"
            } else if self.model.status.is_empty()
                || self.model.status.starts_with("Loading")
                || self.model.status.starts_with("Cached")
                || self.model.status.starts_with("Connected")
            {
                "◌ Connecting"
            } else {
                "○ Offline"
            };
            let state = if self.selection_mode {
                "SELECT".to_owned()
            } else {
                connection.to_owned()
            };
            let status_width = (state.chars().count() as u16 + 2).min(area.width);
            let select_label = if self.selection_mode {
                "v Return"
            } else {
                "Select text [v]"
            };
            let select_width =
                (select_label.len() as u16 + 1).min(area.width.saturating_sub(status_width));
            let history_label = if self.tab == 1 { "History [h]" } else { "" };
            let history_width = (history_label.len() as u16 + 1)
                .min(area.width.saturating_sub(status_width + select_width));
            let tabs_width = area
                .width
                .saturating_sub(status_width + select_width + history_width);
            frame.render_widget(
                Tabs::new(TABS.map(Line::from))
                    .select(self.tab)
                    .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                    .divider("  ·  "),
                Rect {
                    width: tabs_width,
                    ..chunks[0]
                },
            );
            self.history_control.set(Rect {
                x: area.x + tabs_width,
                width: history_width,
                ..chunks[0]
            });
            self.select_control.set(Rect {
                x: area.x + tabs_width + history_width,
                width: select_width,
                ..chunks[0]
            });
            if history_width > 0 {
                frame.render_widget(
                    Paragraph::new(history_label).style(Style::default().fg(
                        if self.history_open {
                            Color::Yellow
                        } else {
                            Color::Cyan
                        },
                    )),
                    self.history_control.get(),
                );
            }
            frame.render_widget(
                Paragraph::new(select_label).style(Style::default().fg(Color::Cyan)),
                self.select_control.get(),
            );
            frame.render_widget(
                Paragraph::new(state).style(Style::default().fg(if self.live_ready {
                    Color::Green
                } else {
                    Color::Yellow
                })),
                Rect {
                    x: area.x + area.width.saturating_sub(status_width),
                    width: status_width,
                    ..chunks[0]
                },
            );
        }
        if let Some(attached) = &self.attached {
            let columns = Layout::horizontal([
                Constraint::Length(if self.sidebar && area.width >= 66 {
                    17
                } else {
                    0
                }),
                Constraint::Min(1),
            ])
            .split(chunks[1]);
            if columns[0].width > 0 {
                frame.render_widget(
                    Paragraph::new("Now\nChat\nControl\nFleet")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(Block::default().borders(Borders::ALL)),
                    columns[0],
                );
            }
            let lines: Vec<Line> = attached
                .screen
                .lines
                .iter()
                .map(|line| {
                    Line::from(if line.redacted {
                        "[redacted]"
                    } else {
                        &line.text
                    })
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .title(attached.screen.title.as_str())
                        .borders(Borders::ALL),
                ),
                columns[1],
            );
            frame.render_widget(
                Paragraph::new("Interactive terminal · Ctrl+\\ or click Return to detach"),
                chunks[2],
            );
            return;
        }
        let history_width = history_width(self, area.width);
        let columns = Layout::horizontal([
            Constraint::Length(sidebar_width(self, area.width)),
            Constraint::Min(1),
            Constraint::Length(history_width),
        ])
        .split(chunks[1]);
        if columns[0].width > 0 {
            let list: Vec<String> = match self.tab {
                0 => self
                    .model
                    .attention()
                    .map(|v| {
                        format!(
                            "{}  {}\n   {} · {}",
                            priority_glyph(&v.priority),
                            v.title,
                            v.attention_kind.replace('-', " "),
                            age_label(&v.requested_at, &chrono::Utc::now().to_rfc3339())
                        )
                    })
                    .collect(),
                1 => self
                    .agent_tree()
                    .iter()
                    .map(|(v, depth)| {
                        format!(
                            "{}{} {}{}  ·  {}",
                            "  ".repeat(*depth),
                            state_glyph(&v.state),
                            agent_label(v),
                            if v.active_work_count > 0 || !v.current_work_ids.is_empty() {
                                format!(
                                    " · {} work",
                                    v.active_work_count.max(v.current_work_ids.len() as u64)
                                )
                            } else if v.queued_work_count > 0 {
                                format!(" · {} queued", v.queued_work_count)
                            } else {
                                String::new()
                            },
                            v.reachability
                        )
                    })
                    .chain(self.model.undeclared_sessions().map(|v| {
                        let driver = v
                            .extra
                            .get("driver")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("harness");
                        format!("[undeclared] {driver} {}", v.header.id)
                    }))
                    .collect(),
                2 => self
                    .control_missions()
                    .iter()
                    .map(|v| {
                        let (done, total) = mission_progress(&self.model, v);
                        let current = mission_current_work(&self.model, v);
                        format!(
                            "{}\n   {}  ·  {}  ·  {}",
                            mission_display_label(v),
                            self.mission_group(v),
                            mission_progress_label(&self.model, v, done, total),
                            current
                                .map(|work| work.path.as_str())
                                .unwrap_or("no active step")
                        )
                    })
                    .collect(),
                _ => self
                    .model
                    .machines()
                    .map(|v| format!("{} {}", v.name, v.state))
                    .collect(),
            };
            let entries = list
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    ListItem::new(format!("{v}\n")).style(Style::default().fg(
                        if i == self.selected[self.tab] {
                            Color::White
                        } else if self.tab == 1 {
                            self.agent_tree()
                                .get(i)
                                .map(|(agent, _)| state_color(&agent.state))
                                .unwrap_or(Color::DarkGray)
                        } else if self.tab == 0 {
                            self.model
                                .attention()
                                .nth(i)
                                .map(|attention| priority_color(&attention.priority))
                                .unwrap_or(Color::Gray)
                        } else {
                            Color::Gray
                        },
                    ))
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default()
                .with_offset(self.sidebar_offsets[self.tab].get())
                .with_selected(Some(self.selected[self.tab]));
            frame.render_stateful_widget(
                List::new(if entries.is_empty() {
                    vec![ListItem::new("No items")]
                } else {
                    entries
                })
                .block(Block::default().title(" Browse ").borders(Borders::ALL))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol("› "),
                columns[0],
                &mut state,
            );
            self.sidebar_offsets[self.tab].set(state.offset());
        }
        let mut lines = Vec::new();
        if self.status_details {
            lines.push(format!("Connection: {}", self.model.status));
            lines.push("i hide connection details".into());
            lines.push(String::new());
        }
        if let Some(result) = &self.action_result {
            lines.push(format!("RESULT  {result}"));
            lines.push(String::new());
        }
        if let Some((id, action)) = &self.pending_action {
            let title = self
                .model
                .attention()
                .find(|attention| &attention.header.id == id)
                .map(|attention| attention.title.as_str())
                .unwrap_or(id);
            lines.push(format!("CONFIRM  {} · {title}", action_label(action)));
            if let Some(reason) = &self.pending_reason {
                lines.push(format!("Reason: {reason}"));
            }
            lines.push(String::new());
        }
        match self.tab {
            0 => {
                lines.push("NOW  /  YOUR ATTENTION".into());
                lines.push(String::new());
                if let Some(v) = self.model.attention().nth(self.selected[0]) {
                    lines.extend([
                        format!("┌─ {}  {}", priority_glyph(&v.priority), v.title),
                        format!(
                            "│  {} · requested {}",
                            v.attention_kind.replace('-', " "),
                            age_label(&v.requested_at, &chrono::Utc::now().to_rfc3339())
                        ),
                        "│".into(),
                        v.detail.clone(),
                        format!("Source  {}", v.source_id),
                        "│".into(),
                        "AVAILABLE ACTIONS".into(),
                    ]);
                    for action in &v.actions {
                        lines.push(format!("  {}", action_label(action)));
                    }
                    lines.push("Choose a key, then confirm with y. CLI actions need st3.".into());
                    lines.push("└────────────────────────".into());
                } else {
                    lines.push("Nothing needs your attention.".into());
                }
                if self.model.now.truncated {
                    lines.push("[More attention beyond bounded view]".into());
                }
            }
            1 => {
                if let Some(peer) = self.peer() {
                    lines.push(format!("AGENT  /  {}", peer.name));
                    lines.push(String::new());
                    lines.push(format!(
                        "{}  ·  {}  ·  observed {}",
                        format!("{} {}", state_glyph(&peer.state), peer.state),
                        peer.reachability,
                        age_label(&peer.header.updated_at, &chrono::Utc::now().to_rfc3339())
                    ));
                    if let Some(driver) = &peer.driver {
                        lines.push(format!(
                            "Harness: {driver} · {}",
                            peer.harness_state.as_deref().unwrap_or("unknown")
                        ));
                    }
                    lines.push(format!(
                        "Current session: {}",
                        self.selected_session_id().as_deref().unwrap_or("none")
                    ));
                    if !peer.current_work_ids.is_empty() || peer.next_work_id.is_some() {
                        lines.push(String::new());
                        lines.push("MISSION WORK".into());
                        for current in &peer.current_work_ids {
                            lines.push(format!("  Current: {current}"));
                        }
                        if let Some(next) = &peer.next_work_id {
                            lines.push(format!("  Next: {next}"));
                            lines.push(format!("  {} ready across runs", peer.queued_work_count));
                            if let Some(age) = self.work_age(next) {
                                lines.push(format!("  Ready {age}"));
                            }
                        }
                    }
                    lines.push(String::new());
                    let mut recent = self
                        .model
                        .messages(self.selected_session_id().as_deref(), &peer.header.id)
                        .collect::<Vec<_>>();
                    let conversation = self
                        .model
                        .timeline
                        .iter()
                        .rev()
                        .filter(|entry| match &entry.body {
                            st3_client::TimelineBody::Content(content) => content
                                .text
                                .as_deref()
                                .is_none_or(|text| !clean_message_text(text).is_empty()),
                            _ => false,
                        })
                        .take(12)
                        .collect::<Vec<_>>();
                    if !conversation.is_empty() {
                        recent.truncate(4);
                    }
                    recent.sort_by(|a, b| {
                        a.sent_at
                            .cmp(&b.sent_at)
                            .then_with(|| a.header.id.cmp(&b.header.id))
                    });
                    lines.push(if conversation.is_empty() && !recent.is_empty() {
                        "ST3 MESSAGES · native transcript unavailable".into()
                    } else {
                        "ST3 MESSAGES".into()
                    });
                    if conversation.is_empty() && recent.is_empty() {
                        lines.push(
                            if self.messages_requested.as_deref() == Some(peer.header.id.as_str())
                                && self.model.messages.snapshot.is_none()
                            {
                                "Loading ST3 messages…".into()
                            } else if self
                                .selected_session_id()
                                .is_some_and(|id| self.timeline_cache.contains_key(&id))
                            {
                                "No conversation in recent timeline.".into()
                            } else {
                                "Loading conversation…".into()
                            },
                        );
                    }
                    for message in &recent {
                        let cleaned = clean_message_text(&message.content);
                        lines.push(format!("  {}:", message.from));
                        let body = if cleaned.is_empty() {
                            message.title.as_deref().unwrap_or("(notification)")
                        } else {
                            cleaned.as_str()
                        };
                        lines.extend(body.split('\n').map(|line| format!("    {line}")));
                        lines.push(String::new());
                    }
                    if !conversation.is_empty() {
                        lines.push("NATIVE CONVERSATION".into());
                    }
                    for entry in conversation.into_iter().rev() {
                        for line in timeline_line(entry).lines() {
                            lines.push(format!("  {line}"));
                        }
                        lines.push(String::new());
                    }
                } else if let Some(session) = self.undeclared_session() {
                    let driver = session
                        .extra
                        .get("driver")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("harness");
                    let exact = session
                        .extra
                        .get("native_session_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some();
                    lines.push(format!("Undeclared {driver} · {}", session.state));
                    lines.push(format!("Session: {}", session.header.id));
                    lines.push(
                        if exact {
                            "Exact native session · read-only"
                        } else {
                            "Unresolved running process · read-only"
                        }
                        .into(),
                    );
                    if let Some(workspace) = session
                        .extra
                        .get("workspace")
                        .and_then(serde_json::Value::as_str)
                    {
                        lines.push(format!("Workspace: {workspace}"));
                    }
                    lines.push("── Normalized history ──".into());
                    if self.model.timeline.is_empty() {
                        lines.push(if self.timeline_cache.contains_key(&session.header.id) {
                            "No conversation in recent timeline.".into()
                        } else {
                            "Loading conversation…".into()
                        });
                    }
                    for entry in &self.model.timeline {
                        for line in timeline_line(entry).lines() {
                            lines.push(format!("  {line}"));
                        }
                    }
                    lines.push(if session_is_importable(session) {
                        "m import session into st3 · confirmation stops the exact process and resumes it under st3".into()
                    } else if exact {
                        format!(
                            "Import unavailable: {}",
                            session
                                .extra
                                .get("import_reason")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("native discovery did not mark this session importable")
                        )
                    } else {
                        "Import requires an exact native session ID; this process is unresolved.".into()
                    });
                } else {
                    lines.push("No agents or undeclared sessions available.".into());
                }
            }
            2 => {
                lines.push("CONTROL  /  MISSIONS".into());
                lines.push(String::new());
                if let Some(mission) = self.control_missions().get(self.selected[2]).copied() {
                    lines.push(mission_display_label(mission));
                    lines.push(format!(
                        "{}  ·  {} runs",
                        self.mission_group(mission),
                        mission.runs.len()
                    ));
                    let (done, total) = mission_progress(&self.model, mission);
                    lines.push(format!(
                        "Progress  {}",
                        mission_progress_label(&self.model, mission, done, total)
                    ));
                    lines.push(mission.header.id.clone());
                    lines.push(String::new());
                    if let Some(current) = mission_current_work(&self.model, mission) {
                        lines.push(format!(
                            "NEXT  {}  ·  {}",
                            current.path,
                            next_action_label(current)
                        ));
                        lines.push(format!("Owner  {}", work_owner(&self.model, current)));
                        if matches!(current.state.as_str(), "blocked" | "waiting")
                            && let Some(reason) = &current.blocked_reason
                        {
                            lines.push(format!("Blocker  {reason}"));
                        }
                        lines.push(String::new());
                    }
                    lines.push("CURRENT WORK".into());
                    let steps = self
                        .model
                        .work()
                        .filter(|work| mission_work_matches(mission, work));
                    let mut count = 0;
                    for step in steps {
                        if matches!(step.state.as_str(), "completed" | "cancelled") {
                            continue;
                        }
                        lines.push(format!(
                            "  {} {}  ·  {}",
                            state_glyph(&step.state),
                            step.path,
                            step.state
                        ));
                        if matches!(step.state.as_str(), "blocked" | "waiting")
                            && let Some(reason) = &step.blocked_reason
                        {
                            lines.push(format!("    Blocked: {reason}"));
                        }
                        if let Some(goal) = step.goals.first() {
                            lines.push(format!("    Goal: {goal}"));
                        }
                        lines.push(format!("    Owner: {}", work_owner(&self.model, step)));
                        count += 1;
                    }
                    if count == 0 {
                        lines.push("  No active steps.".into());
                    }
                } else {
                    lines.push("No current missions.".into());
                }
                lines.push(String::new());
                if self.model.launches().next().is_some() {
                    lines.push("PLANNER LAUNCHES".into());
                    for launch in self.model.launches().take(4) {
                        lines.push(format!("  {}  ·  {}", launch.title, launch.phase));
                    }
                }
                lines.push(String::new());
                lines.push(format!(
                    "c create mission  ·  x {} system missions",
                    if self.show_system_missions {
                        "hide"
                    } else {
                        "show"
                    }
                ));
                if self.model.work.truncated {
                    lines.push("Older completed steps omitted".into());
                }
            }
            _ => {
                lines.push("FLEET  /  MACHINES".into());
                lines.push(String::new());
                let selected_machine = self.model.machines().nth(self.selected[3]);
                if let Some(machine) = selected_machine {
                    lines.push(format!("{}  ·  {}", machine.name, machine.state));
                    lines.push(if machine.capacity.state == "unknown" {
                        format!("Capacity: not reported ({})", machine.capacity.reason)
                    } else {
                        format!("Capacity: {}", machine.capacity.state)
                    });
                    lines.push(format!(
                        "Running runtimes: {}",
                        machine.occupancy.running_runtimes
                    ));
                    lines.push(String::new());
                    lines.push("CONNECTIVITY".into());
                    for transport in &machine.transports {
                        lines.push(format!("  {:<18} {}", transport.protocol, transport.status));
                    }
                } else {
                    lines.push("No machines in the current snapshot.".into());
                }
                lines.push(String::new());
                lines.push("AGENT WORK".into());
                for agent in self.model.agents().filter(|agent| {
                    selected_machine.is_some_and(|machine| {
                        self.agent_on_host(&agent.header.id, &machine.host_id)
                    }) && (agent.active_work_count > 0 || agent.queued_work_count > 0)
                }) {
                    lines.push(format!(
                        "  {}  ·  {} active · {} queued",
                        agent_label(agent),
                        agent.active_work_count,
                        agent.queued_work_count
                    ));
                    if let Some(next) = &agent.next_work_id {
                        lines.push(format!("    Next: {next}"));
                        if let Some(age) = self.work_age(next) {
                            lines.push(format!("    Ready {age}"));
                        }
                    }
                }
                let gateway = self
                    .model
                    .sessions
                    .snapshot
                    .as_ref()
                    .map(|s| s.host_id.as_str())
                    .unwrap_or("connected machine");
                if selected_machine.is_some_and(|machine| machine.host_id == gateway) {
                    lines.push(String::new());
                    lines.push("YOUR DEVICES (connected gateway)".into());
                    for v in self.model.devices() {
                        lines.push(format!("  {}  ·  {}", device_label(v), v.state));
                    }
                    lines.push(String::new());
                    lines.push(format!("UNDECLARED ON {gateway}"));
                    for session in self.model.undeclared_sessions() {
                        let driver = session
                            .extra
                            .get("driver")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("harness");
                        let exact = session
                            .extra
                            .get("native_session_id")
                            .and_then(serde_json::Value::as_str)
                            .is_some();
                        lines.push(format!(
                            "  {driver}  ·  {}  ·  {}",
                            if exact {
                                "native session"
                            } else {
                                "unresolved process"
                            },
                            session.header.id
                        ));
                    }
                    lines.push(String::new());
                    lines.push("Discovery is local to the connected gateway.".into());
                }
                if self.model.machines.truncated || self.model.devices.truncated {
                    lines.push("[More fleet items beyond bounded view]".into());
                }
            }
        }
        let detail = Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .block(Block::default().title(TABS[self.tab]).borders(Borders::ALL));
        let visible = columns[1].height.saturating_sub(2) as usize;
        let max_scroll = detail
            .line_count(columns[1].width.saturating_sub(2))
            .saturating_sub(2)
            .saturating_sub(visible)
            .min(u16::MAX as usize) as u16;
        if self.tab == 1 {
            self.chat_max_scroll.set(max_scroll);
        }
        let scroll = if self.scroll[self.tab] == u16::MAX {
            max_scroll
        } else {
            self.scroll[self.tab].min(max_scroll)
        };
        frame.render_widget(detail.scroll((scroll, 0)), columns[1]);
        self.history_load_control.set(Rect::default());
        if columns[2].width > 0 {
            let all_content = self
                .model
                .timeline
                .iter()
                .filter(|entry| matches!(entry.body, st3_client::TimelineBody::Content(_)))
                .collect::<Vec<_>>();
            let older = all_content.len().saturating_sub(12);
            let mut history = vec![if older == 0 && self.model.timeline_truncated {
                "More history available".into()
            } else {
                format!("{} older messages", older)
            }];
            if self.model.timeline_truncated {
                history.push(if self.history_page_limit < 32 {
                    "o Load older pages".into()
                } else {
                    "History limit reached".into()
                });
            } else {
                history.push("All available pages loaded".into());
            }
            history.push(String::new());
            history.push(format!(
                "Session: {}",
                self.selected_session_id().as_deref().unwrap_or("none")
            ));
            history.push(String::new());
            for entry in all_content.into_iter().take(older) {
                history.extend(timeline_line(entry).lines().map(str::to_owned));
                history.push(String::new());
            }
            let panel = Paragraph::new(history.join("\n"))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title("History & details")
                        .borders(Borders::ALL),
                );
            let visible = columns[2].height.saturating_sub(2) as usize;
            let max_scroll = panel
                .line_count(columns[2].width.saturating_sub(2))
                .saturating_sub(2)
                .saturating_sub(visible)
                .min(u16::MAX as usize) as u16;
            self.history_max_scroll.set(max_scroll);
            if self.model.timeline_truncated
                && self.history_page_limit < 32
                && self.history_scroll == 0
                && columns[2].height > 3
            {
                self.history_load_control.set(Rect {
                    x: columns[2].x + 1,
                    y: columns[2].y + 2,
                    width: columns[2].width.saturating_sub(2),
                    height: 1,
                });
            }
            frame.render_widget(
                panel.scroll((self.history_scroll.min(max_scroll), 0)),
                columns[2],
            );
        }
        let footer = if self.selection_mode {
            "Drag to select text with your terminal · press v to return".to_owned()
        } else {
            match self.mode {
            Mode::Normal => match self.tab {
                0 => "↑↓/click cards · wheel/Pg scroll · choose action key · v select · q quit".into(),
                1 if self.undeclared_session().is_some_and(session_is_importable) => {
                    "↑↓/click agent · wheel/Pg scroll · h history · m import · v select · q quit".into()
                }
                1 => {
                    let selected = self.peer().map(|peer| format!("{} {} · observed {} · ", state_glyph(&peer.state), agent_label(peer), age_label(&peer.header.updated_at, &chrono::Utc::now().to_rfc3339()))).unwrap_or_default();
                    let graph_only = self.peer().is_some_and(|peer| {
                        self.model.messages(None, &peer.header.id).next().is_some()
                    }) && self.model.timeline.iter().all(|entry| !matches!(entry.body, st3_client::TimelineBody::Content(_)));
                    if graph_only {
                        format!("{selected}ST3 messages · transcript unavailable · Pg/wheel scroll · Enter terminal")
                    } else if self.runtime().is_some() {
                        format!("{selected}Enter terminal · Pg/wheel scroll · h history · c message · v select")
                    } else {
                        format!("{selected}Pg/wheel scroll · h history · c message · v select")
                    }
                }
                2 => "↑↓/click mission · wheel/Pg scroll · c new mission · q quit".into(),
                _ => "↑↓/click machine · wheel/Pg scroll · q quit".into(),
            },
            Mode::Confirm => self
                .pending_action
                .as_ref()
                .map(|(_, action)| {
                    format!("Confirm {}?  y proceed · Esc cancel", action_label(action))
                })
                .unwrap_or_else(|| "Esc cancel".into()),
            Mode::ActionReason => format!("Reason: {}█ · Enter continue · Esc cancel", self.input),
            Mode::ImportConfirm => format!(
                "y confirm import {} · Esc cancel · stops exact process, resumes under st3",
                self.pending_import.as_deref().unwrap_or("session")
            ),
            Mode::Chat => format!("Message: {}█ · Enter send · Esc cancel", self.input),
            Mode::Title => format!("Launch title: {}█", self.input),
            Mode::Request => format!("Request: {}█", self.input),
            Mode::Mission => format!("Mission ID: {}█", self.input),
            Mode::Workspace => format!("Workspace: {}█ · Enter create", self.input),
        }
        };
        let footer = self.notice.as_deref().unwrap_or(&footer);
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }
}

fn action_pair() -> (String, String) {
    let id = format!("action/{}", uuid::Uuid::now_v7());
    (id.clone(), id)
}
fn mission_label(mission: &st3_client::Mission) -> String {
    let slug = mission.title.rsplit('/').next().unwrap_or(&mission.title);
    slug.split('-')
        .map(|word| match word.to_ascii_lowercase().as_str() {
            "tui" => "TUI".into(),
            "ios" => "iOS".into(),
            "st3" => "ST3".into(),
            "omp" => "OMP".into(),
            "api" => "API".into(),
            "pty" => "PTY".into(),
            _ => {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn mission_display_label(mission: &st3_client::Mission) -> String {
    let path = mission
        .header
        .id
        .strip_prefix("mission/")
        .unwrap_or(&mission.header.id);
    let scope = path.rsplit_once('/').map_or(path, |(scope, _)| scope);
    format!("{} · {}", scope, mission_label(mission))
}
fn action_label(action: &str) -> String {
    match action {
        "attention.resolve" => "Resolve [r]".into(),
        "review.approve" | "launch.approve" => "Approve [a]".into(),
        "review.reject" => "Reject [j]".into(),
        "launch.cancel" => "Cancel [d]".into(),
        "mission.approve-revision" => "Approve revision [CLI]".into(),
        "mission.cancel-revision" => "Cancel revision [CLI]".into(),
        "message.read" => "Mark read [m]".into(),
        other => format!("{} [CLI]", other.replace('.', " ")),
    }
}
fn action_key(action: &str) -> Option<char> {
    match action {
        "attention.resolve" => Some('r'),
        "review.approve" | "launch.approve" => Some('a'),
        "review.reject" => Some('j'),
        "launch.cancel" => Some('d'),
        "message.read" => Some('m'),
        _ => None,
    }
}
fn priority_glyph(priority: &str) -> &'static str {
    match priority {
        "critical" => "◆",
        "high" => "▲",
        "normal" => "●",
        _ => "·",
    }
}
fn state_glyph(state: &str) -> &'static str {
    match state {
        "running" | "claimed" | "active" => "●",
        "ready" | "waiting" | "standing" => "◌",
        "completed" | "approved" => "✓",
        "blocked" | "failed" => "!",
        _ => "·",
    }
}
fn state_color(state: &str) -> Color {
    match state {
        "running" | "claimed" | "active" => Color::Green,
        "ready" | "waiting" => Color::Yellow,
        "blocked" | "failed" => Color::Red,
        _ => Color::Gray,
    }
}
fn priority_color(priority: &str) -> Color {
    match priority {
        "critical" => Color::Red,
        "high" => Color::Yellow,
        _ => Color::Gray,
    }
}
fn mission_work_matches(mission: &st3_client::Mission, work: &st3_client::Work) -> bool {
    if mission.runs.last() != Some(&work.mission_run_id) {
        return false;
    }
    mission
        .run_generations
        .get(&work.mission_run_id)
        .is_none_or(|current| current == &work.generation_id)
}

fn mission_progress(model: &Model, mission: &st3_client::Mission) -> (usize, usize) {
    let steps = model
        .work()
        .filter(|work| mission_work_matches(mission, work))
        .collect::<Vec<_>>();
    (
        steps
            .iter()
            .filter(|work| work.state == "completed")
            .count(),
        steps.len(),
    )
}
fn mission_progress_label(
    model: &Model,
    mission: &st3_client::Mission,
    done: usize,
    total: usize,
) -> String {
    if mission.runs.is_empty() {
        "Not started".into()
    } else if model.work.snapshot.is_none() {
        "Loading work".into()
    } else if model.work.truncated && total == 0 {
        "No steps in recent history".into()
    } else if model.work.truncated {
        format!("{done}/{total}+ recent steps")
    } else {
        format!("{done}/{total} steps")
    }
}
fn mission_current_work<'a>(
    model: &'a Model,
    mission: &st3_client::Mission,
) -> Option<&'a st3_client::Work> {
    model
        .work()
        .filter(|work| mission_work_matches(mission, work))
        .filter(|work| !matches!(work.state.as_str(), "completed" | "cancelled" | "failed"))
        .min_by_key(|work| match work.state.as_str() {
            "blocked" => 0,
            "claimed" | "running" => 1,
            "verifying" => 2,
            "ready" => 3,
            _ => 4,
        })
}
fn next_action_label(work: &st3_client::Work) -> &'static str {
    if work
        .extra
        .get("agentless")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return "Stewardship active";
    }
    match work.state.as_str() {
        "claimed" | "running" => "Agent working",
        "blocked" => "Resolve blocker",
        "verifying" => "Await review",
        "ready" => "Agent can claim",
        "waiting" => "Await dependency",
        _ => "Inspect step",
    }
}
fn work_owner(model: &Model, work: &st3_client::Work) -> String {
    if work
        .extra
        .get("agentless")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return "Agentless step".into();
    }
    let agent_id = work.claimant.as_deref().or_else(|| {
        model
            .agents()
            .find(|agent| {
                agent.next_work_id.as_deref() == Some(&work.header.id)
                    || agent.current_work_ids.contains(&work.header.id)
            })
            .map(|agent| agent.header.id.as_str())
    });
    match agent_id {
        Some(id) => model
            .agents()
            .find(|agent| agent.header.id == id)
            .map(|agent| format!("{} · {id}", agent_label(agent)))
            .unwrap_or_else(|| id.to_owned()),
        None => "Unassigned".into(),
    }
}
fn sidebar_item_at(app: &App, column: u16, row: u16, width: u16) -> Option<usize> {
    if column >= sidebar_width(app, width) || row < 2 {
        return None;
    }
    // Text::raw uses str::lines, which drops the trailing newline appended in
    // render. Now and Control have two real lines; Chat and Fleet have one.
    let item_height = if matches!(app.tab, 0 | 2) { 2 } else { 1 };
    let index = app.sidebar_offsets[app.tab].get() + usize::from((row - 2) / item_height);
    (index < app.count()).then_some(index)
}

fn sidebar_width(app: &App, width: u16) -> u16 {
    if app.sidebar && width >= 66 && !(app.tab == 1 && app.history_open && width < 110) {
        width / 3
    } else {
        0
    }
}

fn history_width(app: &App, width: u16) -> u16 {
    if app.tab == 1 && app.history_open && width >= 60 {
        (width / 4).max(22)
    } else {
        0
    }
}

fn point_in(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn toggle_text_selection(app: &mut App) -> Result<()> {
    app.selection_mode = !app.selection_mode;
    app.selection_frame_drawn = false;
    if app.selection_mode {
        execute!(io::stdout(), DisableMouseCapture)?;
    } else {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    app.dirty = true;
    Ok(())
}

fn load_older_history(app: &mut App) {
    app.history_page_limit = (app.history_page_limit + 4).min(32);
    app.last_timeline = Instant::now() - TIMELINE_REFRESH;
    app.dirty = true;
}

fn scroll_detail(app: &mut App, up: bool) {
    let tab = app.tab;
    let current = if tab == 1 && app.scroll[tab] == u16::MAX {
        app.chat_max_scroll.get()
    } else {
        app.scroll[tab]
    };
    let next = if up {
        current.saturating_sub(3)
    } else {
        current.saturating_add(3)
    };
    app.scroll[tab] = if tab == 1 && next >= app.chat_max_scroll.get() {
        u16::MAX
    } else {
        next
    };
    if tab == 1 {
        app.remember_chat_scroll();
    }
    app.dirty = true;
}
fn age_label(then: &str, now: &str) -> String {
    let parsed = chrono::DateTime::parse_from_rfc3339(then).ok();
    let current = chrono::DateTime::parse_from_rfc3339(now).ok();
    let Some(seconds) = parsed
        .zip(current)
        .map(|(then, now)| (now - then).num_seconds().max(0))
    else {
        return "unknown age".into();
    };
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}
fn device_label(device: &st3_client::Device) -> String {
    let name = device
        .name
        .as_deref()
        .unwrap_or_else(|| device.header.id.trim_start_matches("device/"));
    format!("{name} ({})", device.person_id)
}
fn person_from_config(path: &std::path::Path) -> Result<Option<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = toml::from_str(&raw)?;
    Ok(value
        .get("person")
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}
fn configured_person() -> Result<Option<String>> {
    if let Ok(person) = std::env::var("ST3_PERSON") {
        return Ok(Some(person));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| person_from_config(&base.join("st3/config.toml")))
        .transpose()
        .map(Option::flatten)
}
fn stdin_hung_up() -> bool {
    if !io::stdin().is_terminal() {
        return true;
    }
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // A closed PTY master leaves a POLLHUP/POLLERR on the slave; crossterm's
    // event reader can otherwise spin or block after the terminal disappears.
    let result = unsafe { libc::poll(&mut fd, 1, 0) };
    result > 0 && terminal_closed(fd.revents)
}
fn terminal_closed(revents: libc::c_short) -> bool {
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    #[cfg(target_os = "macos")]
    if revents & libc::POLLIN != 0 {
        // Darwin reports a tmux pane's closed slave as readable EOF without POLLHUP.
        // crossterm then reads zero bytes in a tight loop unless we check the queue.
        let mut queued: libc::c_int = 0;
        if unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) } == 0 && queued == 0 {
            return true;
        }
    }
    false
}
fn poll_terminal() -> Result<bool> {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut fd, 1, 100) };
    if result < 0 {
        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            return Ok(true);
        }
        return Err(io::Error::last_os_error().into());
    }
    if terminal_closed(fd.revents) {
        return Ok(false);
    }
    Ok(true)
}
#[cfg(target_os = "macos")]
fn watch_terminal_hangup() {
    // On Darwin, crossterm can loop inside event::poll on PTY EOF and never
    // return to the main loop's terminal check. A separate poller observes the
    // hangup without consuming input. Once the PTY is gone there is no terminal
    // left to restore, so end the process even if crossterm is stuck.
    std::thread::spawn(|| {
        loop {
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } > 0 && terminal_closed(fd.revents) {
                std::thread::sleep(Duration::from_millis(25));
                if stdin_hung_up() {
                    std::process::exit(0);
                }
            }
        }
    });
}
fn agent_label(agent: &st3_client::Agent) -> String {
    let slug = agent.name.rsplit('/').next().unwrap_or(&agent.name);
    let label = |slug: &str| {
        slug.split('-')
            .map(|word| match word.to_ascii_lowercase().as_str() {
                "st3" => "ST3".to_string(),
                "cos" => "COS".to_string(),
                "ios" => "iOS".to_string(),
                "tui" => "TUI".to_string(),
                "pty" => "PTY".to_string(),
                "omp" => "OMP".to_string(),
                _ => {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if slug.eq_ignore_ascii_case("omp") {
        if let Some(parent) = agent.name.rsplit('/').nth(1) {
            return format!("{} · OMP", label(parent));
        }
    }
    label(slug)
}
fn agent_is_child_of(child: &st3_client::Agent, parent: &st3_client::Agent) -> bool {
    if child.header.id == parent.header.id {
        return false;
    }
    if child
        .under
        .iter()
        .any(|relation| relation.agent_id == parent.header.id)
    {
        return true;
    }
    child.under.is_empty()
        && parent.header.id == "agent/fleet/st3/standing/st3"
        && (child.header.id.starts_with("agent/fleet/st3/")
            || child.header.id.starts_with("agent/st3/"))
}
fn session_is_importable(session: &st3_client::Session) -> bool {
    session
        .extra
        .get("native_session_id")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && session
            .extra
            .get("importable")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}
fn key_input(key: KeyEvent) -> Option<String> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => Some(format!("C-{c}")),
        KeyCode::Char(c) => Some(c.to_string()),
        KeyCode::Enter => Some("return".into()),
        KeyCode::Tab => Some("tab".into()),
        KeyCode::Backspace => Some("backspace".into()),
        KeyCode::Esc => Some("escape".into()),
        KeyCode::Up => Some("Up".into()),
        KeyCode::Down => Some("Down".into()),
        KeyCode::Left => Some("Left".into()),
        KeyCode::Right => Some("Right".into()),
        KeyCode::Delete => Some("Delete".into()),
        KeyCode::Home => Some("Home".into()),
        KeyCode::End => Some("End".into()),
        _ => None,
    }
}

fn is_detach_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('\\' | '4'))
}

async fn attach(app: &mut App, client: &Client) -> Result<()> {
    let Some(runtime) = app.runtime() else {
        app.model.status = "No controllable terminal for selected agent".into();
        app.dirty = true;
        return Ok(());
    };
    let Some(terminal_id) = runtime.terminal_id.clone() else {
        app.model.status = "No terminal on selected runtime".into();
        return Ok(());
    };
    let runtime_id = runtime.header.id.clone();
    let mut attached = None;
    for attempt in 0..3 {
        let current = client.runtimes_get(&runtime_id).await?;
        let Resource::Runtime(runtime) = current.value else {
            anyhow::bail!("Selected runtime is no longer available");
        };
        anyhow::ensure!(
            runtime.terminal_id.as_deref() == Some(&terminal_id),
            "Selected terminal changed; refresh Chat"
        );
        let fence = Fence {
            snapshot_id: current.snapshot.id,
            runtime_incarnation: runtime.incarnation_id,
            terminal_sequence: runtime.terminal_sequence,
            ..Fence::default()
        };
        let (id, key) = action_pair();
        match client
            .terminal_attach(
                id,
                key,
                fence,
                TargetParameters {
                    target_id: terminal_id.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(response) => {
                attached = response.value.terminal_attachment;
                break;
            }
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 2 => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let attachment = attached.context("attach returned no viewer")?;
    let screen = if let Some(capability) = attachment.stream_capability.as_deref() {
        client
            .terminal_frames(
                &terminal_id,
                None,
                Some(&attachment.runtime_incarnation),
                capability,
                Some(0),
            )
            .await?
            .screen
            .value
    } else {
        client.terminal_screen(&terminal_id).await?.value
    };
    app.attached = Some(Attached {
        terminal_id,
        attachment_id: attachment.attachment_id,
        screen,
    });
    app.notice = None;
    app.dirty = true;
    Ok(())
}
async fn detach(app: &mut App, client: &Client) -> Result<()> {
    let Some(attached) = app.attached.as_ref() else {
        return Ok(());
    };
    let terminal_id = attached.terminal_id.clone();
    let attachment_id = attached.attachment_id.clone();
    let incarnation = attached.screen.runtime_incarnation.clone();
    for attempt in 0..3 {
        let fence = terminal_fence(client, &terminal_id, &incarnation).await?;
        let (id, key) = action_pair();
        match client
            .terminal_detach(
                id,
                key,
                fence,
                TargetParameters {
                    target_id: attachment_id.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => break,
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 2 => continue,
            Err(error) => return Err(error.into()),
        }
    }
    app.attached = None;
    app.dirty = true;
    Ok(())
}
async fn terminal_fence(client: &Client, terminal_id: &str, incarnation: &str) -> Result<Fence> {
    let screen = client.terminal_screen(terminal_id).await?;
    terminal_screen_fence(&screen, incarnation)
}
fn terminal_screen_fence(
    screen: &st3_client::Envelope<st3_client::TerminalScreen>,
    incarnation: &str,
) -> Result<Fence> {
    anyhow::ensure!(
        screen.value.runtime_incarnation == incarnation,
        "terminal incarnation changed; reattach before sending input"
    );
    Ok(Fence {
        snapshot_id: screen.snapshot.id.clone(),
        runtime_incarnation: Some(incarnation.to_owned()),
        terminal_sequence: Some(screen.value.next_sequence),
        ..Fence::default()
    })
}
async fn run_attention_action(
    app: &mut App,
    client: &Client,
    attention_id: &str,
    action: &str,
    reason: Option<String>,
) -> Result<String> {
    anyhow::ensure!(app.live_ready, "Reconnect before acting");
    let current = client.attention_get(attention_id).await?;
    let Resource::Attention(attention) = &current.value else {
        anyhow::bail!("Attention changed; refresh and choose again");
    };
    anyhow::ensure!(
        attention.person_id == app.model.actor
            && attention
                .actions
                .iter()
                .any(|available| available == action),
        "Action is no longer available"
    );
    let mut fence = Fence {
        snapshot_id: current.snapshot.id,
        ..Fence::default()
    };
    fence.subject_revisions.insert(
        attention.header.id.clone(),
        attention.header.revision.clone(),
    );
    let source = attention.source_id.clone();
    if action.starts_with("launch.") {
        let launch = client.launches_get(&source).await?;
        let Resource::Launch(launch_resource) = launch.value else {
            anyhow::bail!("Launch is no longer available");
        };
        fence.snapshot_id = launch.snapshot.id;
        fence
            .subject_revisions
            .insert(launch_resource.header.id, launch_resource.header.revision);
    }
    let (id, idem) = action_pair();
    let result = match action {
        "attention.resolve" => {
            client
                .attention_resolve(
                    id,
                    idem,
                    fence,
                    AttentionResolveParameters {
                        attention_id: attention.header.id.clone(),
                        outcome: "resolved".into(),
                        reason: None,
                    },
                )
                .await?
        }
        "review.approve" => {
            client
                .review_approve(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        reason,
                        ..Default::default()
                    },
                )
                .await?
        }
        "review.reject" => {
            client
                .review_reject(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        reason,
                        ..Default::default()
                    },
                )
                .await?
        }
        "message.read" => {
            client
                .message_read(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        ..Default::default()
                    },
                )
                .await?
        }
        "launch.cancel" => {
            client
                .launch_cancel(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        ..Default::default()
                    },
                )
                .await?
        }
        "launch.approve" => {
            let variants = client.launch_variants_list(&source, None, Some(50)).await?;
            let variant = variants
                .value
                .items
                .iter()
                .filter_map(|item| match item {
                    Resource::LaunchVariant(variant) if variant.preview_token.is_some() => {
                        Some(variant)
                    }
                    _ => None,
                })
                .max_by_key(|variant| variant.ordinal)
                .context("No current launch preview; review it in the CLI")?;
            fence.preview_token = variant.preview_token.clone();
            client
                .launch_approve(
                    id,
                    idem,
                    fence,
                    LaunchVariantParameters {
                        launch_id: source,
                        variant_id: variant.header.id.clone(),
                    },
                )
                .await?
        }
        _ => anyhow::bail!("This action needs the CLI: {action}"),
    };
    let outcome = format!("{}: {}", action_label(action), result.value.kind);
    match app.model.reload(client).await {
        Ok(()) => Ok(outcome),
        Err(error) => Ok(format!("{outcome}; refresh failed: {error}")),
    }
}
async fn fresh_import_fence(client: &Client, target: &str) -> Result<Fence> {
    let mut cursor = None;
    for _ in 0..4 {
        let page = client
            .sessions_list_native(cursor.as_deref(), Some(50), false)
            .await?;
        for item in &page.value.items {
            if let Resource::Session(session) = item {
                if session.header.id == target {
                    anyhow::ensure!(
                        session.state == "running"
                            && session
                                .extra
                                .get("managed")
                                .and_then(serde_json::Value::as_bool)
                                == Some(false)
                            && session_is_importable(session),
                        "session is no longer an importable, exact, running undeclared session"
                    );
                    return Ok(Fence {
                        snapshot_id: page.snapshot.id,
                        subject_revisions: BTreeMap::from([(
                            target.to_owned(),
                            session.header.revision.clone(),
                        )]),
                        ..Default::default()
                    });
                }
            }
        }
        if !page.value.page.has_more {
            anyhow::bail!("session is no longer in native discovery");
        }
        cursor = page.value.page.next_cursor;
        anyhow::ensure!(
            cursor.is_some(),
            "native discovery has no continuation cursor"
        );
    }
    anyhow::bail!("session is beyond the bounded native discovery view")
}

async fn import_session(app: &mut App, client: &Client, target: &str) -> Result<String> {
    let fence = fresh_import_fence(client, target).await?;
    let (id, idem) = action_pair();
    let result = client
        .session_import(
            id,
            idem,
            fence,
            TargetParameters {
                target_id: target.to_owned(),
                ..Default::default()
            },
        )
        .await?;
    let outcome = format!("Import {}: {}", target, result.value.kind);
    match app.model.reload(client).await {
        Ok(()) => Ok(outcome),
        Err(error) => Ok(format!("{outcome}; refresh failed: {error}")),
    }
}

async fn handle_key(app: &mut App, client: &Client, key: KeyEvent) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }
    if let Some(attached) = &app.attached {
        if is_detach_key(key) {
            detach(app, client).await?;
            return Ok(false);
        }
        if let Some(value) = key_input(key) {
            for attempt in 0..3 {
                let fence = terminal_fence(
                    client,
                    &attached.terminal_id,
                    &attached.screen.runtime_incarnation,
                )
                .await?;
                let (id, idem) = action_pair();
                match client
                    .terminal_input(
                        id,
                        idem,
                        fence,
                        TerminalInputParameters {
                            terminal_id: attached.terminal_id.clone(),
                            mode: TerminalInputMode::Key,
                            value: value.clone(),
                        },
                    )
                    .await
                {
                    Ok(_) => break,
                    Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 2 => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        return Ok(false);
    }
    if app.selection_mode {
        match key.code {
            KeyCode::Char('v') => toggle_text_selection(app)?,
            KeyCode::Esc => toggle_text_selection(app)?,
            KeyCode::Char('q') => return Ok(true),
            _ => {}
        }
        return Ok(false);
    }
    if app.mode == Mode::Confirm {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                app.mode = Mode::Normal;
                app.pending_action = None;
                app.pending_reason = None;
            }
            KeyCode::Char('y') => {
                if let Some((attention_id, action)) = app.pending_action.take() {
                    app.mode = Mode::Normal;
                    let reason = app.pending_reason.take();
                    app.action_result = Some(
                        match run_attention_action(app, client, &attention_id, &action, reason)
                            .await
                        {
                            Ok(result) => result,
                            Err(error) => format!("{} failed: {error}", action_label(&action)),
                        },
                    );
                    app.scroll[0] = 0;
                }
            }
            _ => {}
        }
        app.dirty = true;
        return Ok(false);
    }
    if app.mode == Mode::ImportConfirm {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                app.mode = Mode::Normal;
                app.pending_import = None;
            }
            KeyCode::Char('y') => {
                if let Some(target) = app.pending_import.take() {
                    app.mode = Mode::Normal;
                    app.action_result = Some(match import_session(app, client, &target).await {
                        Ok(result) => result,
                        Err(error) => format!("Import failed: {error}"),
                    });
                }
            }
            _ => {}
        }
        app.dirty = true;
        return Ok(false);
    }
    if app.mode != Mode::Normal {
        match key.code {
            KeyCode::Esc => {
                if app.mode == Mode::Chat {
                    app.remember_chat_draft();
                }
                if app.mode == Mode::ActionReason {
                    app.pending_action = None;
                    app.pending_reason = None;
                }
                app.mode = Mode::Normal;
                app.input.clear();
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Enter => {
                let value = std::mem::take(&mut app.input);
                match app.mode {
                    Mode::ActionReason => {
                        if value.trim().is_empty() {
                            app.action_result =
                                Some("Enter a reason for this review decision".into());
                        } else {
                            app.action_result = None;
                            app.pending_reason = Some(value);
                            app.mode = Mode::Confirm;
                        }
                    }
                    Mode::Chat => {
                        if !value.trim().is_empty() {
                            if !app.live_ready {
                                app.input = value;
                                app.model.status = "Reconnect before sending".into();
                                app.dirty = true;
                                return Ok(false);
                            }
                            let peer = app.peer().context("no selected agent")?;
                            let fence = Fence {
                                snapshot_id: app
                                    .model
                                    .messages
                                    .snapshot
                                    .as_ref()
                                    .or(app.model.agents.snapshot.as_ref())
                                    .context("no snapshot")?
                                    .id
                                    .clone(),
                                ..Default::default()
                            };
                            let (id, idem) = action_pair();
                            if let Err(error) = client
                                .message_send(
                                    id,
                                    idem,
                                    fence,
                                    MessageSendParameters {
                                        to: peer.header.id.clone(),
                                        content: value.clone(),
                                        title: None,
                                        in_reply_to: None,
                                        session_id: peer.current_session_id.clone(),
                                        tags: vec![],
                                    },
                                )
                                .await
                            {
                                app.input = value;
                                app.remember_chat_draft();
                                app.model.status = format!("Send failed: {error}");
                                app.dirty = true;
                                return Ok(false);
                            }
                            if let Some(key) = app.chat_draft_key() {
                                app.chat_draft_cache.remove(&key);
                            }
                            if let Err(error) = app.model.reload(client).await {
                                app.model.status = format!("Sent; refresh failed: {error}");
                            }
                        }
                        app.mode = Mode::Normal;
                    }
                    Mode::Title => {
                        app.launch[0] = value;
                        app.mode = Mode::Request;
                    }
                    Mode::Request => {
                        app.launch[1] = value;
                        app.mode = Mode::Mission;
                        app.input = format!("mission/{}", uuid::Uuid::now_v7());
                    }
                    Mode::Mission => {
                        app.launch[2] = value;
                        app.mode = Mode::Workspace;
                    }
                    Mode::Workspace => {
                        if !app.live_ready {
                            app.input = value;
                            app.model.status = "Reconnect before creating a launch".into();
                            app.dirty = true;
                            return Ok(false);
                        }
                        app.launch[3] = value;
                        let fence = Fence {
                            snapshot_id: app
                                .model
                                .launches
                                .snapshot
                                .as_ref()
                                .or(app.model.missions.snapshot.as_ref())
                                .context("no snapshot")?
                                .id
                                .clone(),
                            ..Default::default()
                        };
                        let (id, idem) = action_pair();
                        client
                            .launch_create(
                                id,
                                idem,
                                fence,
                                LaunchCreateParameters {
                                    title: app.launch[0].clone(),
                                    request: app.launch[1].clone(),
                                    target: LaunchTarget::NewMission {
                                        mission_id: app.launch[2].clone(),
                                        workspace: app.launch[3].clone(),
                                    },
                                    provider: None,
                                    model: None,
                                    effort: None,
                                },
                            )
                            .await?;
                        app.model.reload(client).await?;
                        app.mode = Mode::Normal;
                    }
                    Mode::Normal => {}
                    Mode::Confirm => unreachable!(),
                    Mode::ImportConfirm => unreachable!(),
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && app.input.len() < 4096 =>
            {
                app.input.push(c);
            }
            _ => {}
        }
        app.dirty = true;
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Char(d @ '1'..='4') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.tab = d as usize - '1' as usize;
            app.selected[app.tab] = app.selected[app.tab].min(app.count().saturating_sub(1));
        }
        KeyCode::Char('s') => app.sidebar = !app.sidebar,
        KeyCode::Char('i') => app.status_details = !app.status_details,
        KeyCode::Char('v') => toggle_text_selection(app)?,
        KeyCode::Char('h') if app.tab == 1 => {
            app.history_open = !app.history_open;
            app.history_scroll = 0;
        }
        KeyCode::Char('o') if app.tab == 1 && app.history_open && app.model.timeline_truncated => {
            load_older_history(app);
        }
        KeyCode::Char(c) if app.tab == 0 && "arjdm".contains(c) => {
            if let Some(attention) = app.model.attention().nth(app.selected[0]) {
                if let Some(action) = attention
                    .actions
                    .iter()
                    .find(|action| action_key(action) == Some(c))
                {
                    app.pending_action = Some((attention.header.id.clone(), action.clone()));
                    app.pending_reason = None;
                    app.mode = if action.starts_with("review.") {
                        Mode::ActionReason
                    } else {
                        Mode::Confirm
                    };
                    app.input.clear();
                    app.scroll[0] = 0;
                }
            }
        }
        KeyCode::Char('x') if app.tab == 2 => {
            app.show_system_missions = !app.show_system_missions;
            app.selected[2] = app.selected[2].min(app.count_for(2).saturating_sub(1));
        }
        KeyCode::Up => {
            if app.tab == 1 {
                app.remember_chat_scroll();
            }
            app.selected[app.tab] = app.selected[app.tab].saturating_sub(1);
            if app.tab == 1 {
                app.restore_chat_scroll();
            } else {
                app.scroll[app.tab] = 0;
            }
        }
        KeyCode::Down => {
            if app.tab == 1 {
                app.remember_chat_scroll();
            }
            app.selected[app.tab] = (app.selected[app.tab] + 1).min(app.count().saturating_sub(1));
            if app.tab == 1 {
                app.restore_chat_scroll();
            } else {
                app.scroll[app.tab] = 0;
            }
        }
        KeyCode::PageUp => {
            if app.tab == 1 && app.scroll[1] == u16::MAX {
                app.scroll[1] = app.chat_max_scroll.get();
            }
            app.scroll[app.tab] = app.scroll[app.tab].saturating_sub(10);
            if app.tab == 1 {
                app.remember_chat_scroll();
            }
        }
        KeyCode::PageDown => {
            app.scroll[app.tab] = app.scroll[app.tab].saturating_add(10);
            if app.tab == 1 {
                if app.scroll[1] >= app.chat_max_scroll.get() {
                    app.scroll[1] = u16::MAX;
                }
                app.remember_chat_scroll();
            }
        }
        KeyCode::End if app.tab == 1 => {
            app.scroll[1] = u16::MAX;
            app.remember_chat_scroll();
        }
        KeyCode::Char('c') if app.tab == 1 => {
            if !app.live_ready {
                app.model.status = "Reconnect before composing".into();
            } else if app.peer().is_some() {
                app.start_chat_composer();
            } else {
                app.model.status = "Undeclared sessions are read-only".into();
            }
        }
        KeyCode::Char('m') if app.tab == 1 => {
            if !app.live_ready {
                app.model.status = "Reconnect before importing a session".into();
            } else if let Some(session) = app.undeclared_session() {
                if session_is_importable(session) {
                    app.pending_import = Some(session.header.id.clone());
                    app.mode = Mode::ImportConfirm;
                } else {
                    app.model.status = session
                        .extra
                        .get("import_reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Import requires an exact native session ID and importability")
                        .to_owned();
                }
            }
        }
        KeyCode::Char('c') if app.tab == 2 => {
            if app.live_ready {
                app.mode = Mode::Title;
                app.input.clear();
            } else {
                app.model.status = "Reconnect before creating a launch".into();
            }
        }
        KeyCode::Enter if app.tab == 1 => {
            if app.live_ready {
                if let Err(error) = attach(app, client).await {
                    app.notice = Some(format!(
                        "Terminal: {error} · select another agent or retry Enter"
                    ));
                }
            } else {
                app.notice = Some("Reconnect before attaching a terminal".into());
            }
        }
        _ => {}
    }
    app.dirty = true;
    Ok(false)
}

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("stui needs an interactive terminal");
    }
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    let path =
        st3_client::discover_unix_endpoint(std::env::var_os("ST3_ENDPOINT").map(PathBuf::from))?;
    let person = configured_person()?;
    anyhow::ensure!(
        person
            .as_deref()
            .is_some_and(|person| person.starts_with("person/") && person.len() > 7),
        "stui needs ST3_PERSON=person/NAME or person = \"person/NAME\" in the st3 config"
    );
    let cache_path = person
        .as_deref()
        .and_then(|actor| cache::path(&path, actor));
    let client = match person.as_deref() {
        Some(person) => Client::unix_as(&path, person),
        None => Client::unix(&path),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (updates, incoming) = mpsc::channel::<Update>();
    let background_client = client.clone();
    let background_updates = updates.clone();
    let background_cache_path = cache_path.clone();
    let background_actor = person.clone();
    runtime.spawn(async move {
        let mut model = loop {
            match Model::bootstrap(&background_client).await {
                Ok(model) => break model,
                Err(error) => {
                    if background_updates
                        .send(Update::Error(format!("Initial load: {error}")))
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        let _ = background_updates.send(Update::Partial(Box::new(model.clone())));
        loop {
            match model.reload(&background_client).await {
                Ok(()) => break,
                Err(error) => {
                    if background_updates
                        .send(Update::Error(format!("Initial details: {error}")))
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        if let (Some(path), Some(actor)) = (&background_cache_path, &background_actor) {
            let _ = cache::save(path, actor, &model);
        }
        let _ = background_updates.send(Update::Model(Box::new(model.clone())));
        let mut last_external_scan = Instant::now() - Duration::from_secs(60);
        let mut last_cache_save = Instant::now();
        let mut last_full_reload = Instant::now();
        let mut was_offline = false;
        loop {
            let mut changed = match model.sync(&background_client).await {
                Ok((changed, invalidated_sessions, cursor_gap)) => {
                    if cursor_gap && background_updates.send(Update::TimelineCursorGap).is_err() {
                        return;
                    }
                    for id in invalidated_sessions {
                        if background_updates
                            .send(Update::TimelineInvalidated(id))
                            .is_err()
                        {
                            return;
                        }
                    }
                    let recovered = was_offline;
                    was_offline = false;
                    if recovered {
                        model.status = "Connected".into();
                    }
                    changed || recovered
                }
                Err(error) => {
                    was_offline = true;
                    if background_updates
                        .send(Update::Error(format!("Sync: {error}")))
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    false
                }
            };
            if last_external_scan.elapsed() >= Duration::from_secs(60) {
                match model.refresh_sessions(&background_client).await {
                    Ok(sessions_changed) => changed |= sessions_changed,
                    Err(error) => {
                        if background_updates
                            .send(Update::Error(format!("Session discovery: {error}")))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                last_external_scan = Instant::now();
            }
            if last_full_reload.elapsed() >= Duration::from_secs(120) {
                match model.reload(&background_client).await {
                    Ok(()) => changed = true,
                    Err(error) => {
                        let _ = background_updates
                            .send(Update::Error(format!("Periodic refresh: {error}")));
                    }
                }
                last_full_reload = Instant::now();
            }
            if changed && last_cache_save.elapsed() >= Duration::from_secs(60) {
                if let (Some(path), Some(actor)) = (&background_cache_path, &background_actor) {
                    let _ = cache::save(path, actor, &model);
                }
                last_cache_save = Instant::now();
            }
            if changed
                && background_updates
                    .send(Update::Model(Box::new(model.clone())))
                    .is_err()
            {
                break;
            }
        }
    });
    let mut app = App::new(Model::default());
    app.model.status = "Loading…".into();
    let mut guard = TerminalGuard::enter()?;
    #[cfg(target_os = "macos")]
    watch_terminal_hangup();
    #[cfg(debug_assertions)]
    if std::env::var_os("STUI_TEST_PANIC_AFTER_ENTER").is_some() {
        panic!("terminal restoration probe");
    }
    guard.terminal.draw(|frame| app.render(frame))?;
    app.dirty = false;
    if let (Some(path), Some(actor)) = (&cache_path, &person)
        && let Some(cached) = cache::load(path, actor)
    {
        app.model = cached;
        app.dirty = true;
    }
    while !stopping.load(Ordering::Relaxed) && !stdin_hung_up() {
        while !stopping.load(Ordering::Relaxed) && !stdin_hung_up() {
            let Ok(update) = incoming.try_recv() else {
                break;
            };
            match update {
                Update::Partial(model) => {
                    if !app.live_ready {
                        if app.model.status.starts_with("Cached") && app.model.actor == model.actor
                        {
                            app.model.now = model.now;
                            app.model.agents = model.agents;
                            app.model.sessions = model.sessions;
                            app.model.messages = model.messages;
                            app.model.status = "Cached · loading details…".into();
                        } else {
                            app.model = *model;
                        }
                        app.dirty = true;
                    }
                }
                Update::Model(mut model) => {
                    if app.model.actor == model.actor {
                        model.timeline = std::mem::take(&mut app.model.timeline);
                        model.timeline_truncated = app.model.timeline_truncated;
                        model.messages = std::mem::take(&mut app.model.messages);
                    } else {
                        app.timeline_cache.clear();
                        app.chat_scroll_cache.clear();
                        app.chat_draft_cache.clear();
                        app.timeline_requested = None;
                        app.messages_requested = None;
                    }
                    app.model = *model;
                    app.live_ready = true;
                    if app
                        .notice
                        .as_deref()
                        .is_some_and(|notice| notice.starts_with("Reconnect before"))
                    {
                        app.notice = None;
                    }
                    for tab in 0..4 {
                        app.selected[tab] =
                            app.selected[tab].min(app.count_for(tab).saturating_sub(1));
                    }
                    app.dirty = true;
                }
                Update::Timeline(id, timeline, truncated, pages) => {
                    if app.selected_session_id().as_deref() != Some(&id)
                        || app.timeline_requested_pages != pages
                    {
                        continue;
                    }
                    app.timeline_cache
                        .insert(id.clone(), (timeline.clone(), truncated));
                    while app.timeline_cache.len() > 32 {
                        if let Some(oldest) = app.timeline_cache.keys().next().cloned() {
                            app.timeline_cache.remove(&oldest);
                        }
                    }
                    if app.selected_session_id().as_deref() == Some(&id) {
                        app.model.timeline = timeline;
                        app.model.timeline_truncated = truncated;
                        app.dirty = true;
                    }
                }
                Update::Messages(peer, messages) => {
                    if app
                        .peer()
                        .is_some_and(|selected| selected.header.id == peer)
                    {
                        app.model.messages = messages;
                        app.dirty = true;
                    }
                }
                Update::TimelineInvalidated(id) => {
                    if app.selected_session_id().as_deref() == Some(&id) {
                        app.last_timeline = Instant::now() - TIMELINE_REFRESH;
                    }
                }
                Update::TimelineCursorGap => {
                    app.invalidate_timelines();
                    app.dirty = true;
                }
                Update::Error(error) => {
                    if error.starts_with("Sync:") || error.starts_with("Initial load:") {
                        app.live_ready = false;
                    }
                    if app.model.status != error {
                        app.model.status = error;
                        app.dirty = true;
                    }
                }
            }
        }
        if app.tab == 1 {
            let selected_id = app.selected_session_id();
            if selected_id != app.timeline_requested {
                app.history_page_limit = model::MAX_PAGES;
                app.history_scroll = 0;
            }
            let page_limit = if app.history_open {
                app.history_page_limit
            } else {
                model::MAX_PAGES
            };
            if selected_id != app.timeline_requested
                || app.last_timeline.elapsed() >= TIMELINE_REFRESH
                || page_limit != app.timeline_requested_pages
            {
                if selected_id != app.timeline_requested {
                    let cached = selected_id
                        .as_ref()
                        .and_then(|id| app.timeline_cache.get(id))
                        .cloned();
                    (app.model.timeline, app.model.timeline_truncated) = cached.unwrap_or_default();
                    app.dirty = true;
                }
                app.timeline_requested = selected_id.clone();
                app.timeline_requested_pages = page_limit;
                app.last_timeline = Instant::now();
                if let Some(id) = selected_id {
                    let timeline_client = client.clone();
                    let timeline_updates = updates.clone();
                    runtime.spawn(async move {
                        let mut model = Model::default();
                        match model
                            .load_timeline_with_pages(&timeline_client, &id, page_limit)
                            .await
                        {
                            Ok(()) => {
                                let _ = timeline_updates.send(Update::Timeline(
                                    id,
                                    model.timeline,
                                    model.timeline_truncated,
                                    page_limit,
                                ));
                            }
                            Err(error) => {
                                let _ = timeline_updates
                                    .send(Update::Error(format!("Conversation: {error}")));
                            }
                        }
                    });
                } else {
                    app.model.timeline.clear();
                    app.dirty = true;
                }
            }
            let selected_peer = app.peer().map(|peer| peer.header.id.clone());
            if selected_peer != app.messages_requested
                || app.last_messages.elapsed() >= TIMELINE_REFRESH
            {
                app.messages_requested = selected_peer.clone();
                app.last_messages = Instant::now();
                app.model.messages = model::Collection::default();
                if let Some(peer) = selected_peer {
                    let message_client = client.clone();
                    let message_updates = updates.clone();
                    runtime.spawn(async move {
                        let mut model = Model::default();
                        match model.load_messages_for_peer(&message_client, &peer).await {
                            Ok(()) => {
                                let _ =
                                    message_updates.send(Update::Messages(peer, model.messages));
                            }
                            Err(error) => {
                                let _ = message_updates
                                    .send(Update::Error(format!("Recent messages: {error}")));
                            }
                        }
                    });
                }
            }
        }
        if app.dirty && (!app.selection_mode || !app.selection_frame_drawn) {
            guard.terminal.draw(|frame| app.render(frame))?;
            app.dirty = false;
            app.selection_frame_drawn = app.selection_mode;
        }
        if !poll_terminal()? {
            break;
        }
        if event::poll(Duration::ZERO)? && !stopping.load(Ordering::Relaxed) && !stdin_hung_up() {
            match event::read()? {
                Event::Key(key) => match runtime.block_on(handle_key(&mut app, &client, key)) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        app.model.status = error.to_string();
                        app.dirty = true;
                    }
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(_))
                        && app.attached.is_some()
                        && mouse.row == 0 =>
                {
                    if let Err(error) = runtime.block_on(detach(&mut app, &client)) {
                        app.model.status = format!("Detach failed: {error}");
                        app.dirty = true;
                    }
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(_))
                        && app.attached.is_none()
                        && app.mode == Mode::Normal =>
                {
                    if point_in(app.select_control.get(), mouse.column, mouse.row) {
                        if let Err(error) = toggle_text_selection(&mut app) {
                            app.model.status = error.to_string();
                        }
                    } else if point_in(app.history_control.get(), mouse.column, mouse.row) {
                        app.history_open = !app.history_open;
                        app.history_scroll = 0;
                        app.dirty = true;
                    } else if point_in(app.history_load_control.get(), mouse.column, mouse.row) {
                        load_older_history(&mut app);
                    } else if let Some(index) =
                        sidebar_item_at(&app, mouse.column, mouse.row, guard.terminal.size()?.width)
                    {
                        if app.tab == 1 {
                            app.remember_chat_scroll();
                        }
                        app.selected[app.tab] = index;
                        if app.tab == 1 {
                            app.restore_chat_scroll();
                        } else {
                            app.scroll[app.tab] = 0;
                        }
                        app.dirty = true;
                    }
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) && app.attached.is_none()
                        && app.mode == Mode::Normal =>
                {
                    let size = guard.terminal.size()?;
                    let sidebar_width = sidebar_width(&app, size.width);
                    let history_width = history_width(&app, size.width);
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if mouse.row > 0 && mouse.row < size.height.saturating_sub(1) {
                        if mouse.column < sidebar_width {
                            if app.tab == 1 {
                                app.remember_chat_scroll();
                            }
                            app.selected[app.tab] = if up {
                                app.selected[app.tab].saturating_sub(1)
                            } else {
                                (app.selected[app.tab] + 1).min(app.count().saturating_sub(1))
                            };
                            if app.tab == 1 {
                                app.restore_chat_scroll();
                            }
                            app.dirty = true;
                        } else if history_width > 0 && mouse.column >= size.width - history_width {
                            app.history_scroll = if up {
                                app.history_scroll.saturating_sub(3)
                            } else {
                                app.history_scroll
                                    .saturating_add(3)
                                    .min(app.history_max_scroll.get())
                            };
                            app.dirty = true;
                        } else {
                            scroll_detail(&mut app, up);
                        }
                    }
                }
                Event::Resize(_, _) => app.dirty = true,
                _ => {}
            }
        }
        if let Some(attached) = app.attached.as_mut()
            && app.last_terminal.elapsed() >= Duration::from_millis(400)
        {
            if let Ok(screen) = runtime.block_on(client.terminal_screen(&attached.terminal_id))
                && attached.screen != screen.value
            {
                attached.screen = screen.value;
                app.dirty = true;
            }
            app.last_terminal = Instant::now();
        }
    }
    if app.attached.is_some() {
        let _ = runtime.block_on(detach(&mut app, &client));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    fn local_machine(model: &mut Model) {
        model.machines.items.push(serde_json::from_str(r#"{"kind":"machine","id":"machine/hetz","revision":"one","updated_at":"2026-09-25T08:00:00Z","host_id":"host/hetz","name":"hetz","state":"local","fleet_id":null,"capacity":{"state":"available","reason":""},"occupancy":{"running_runtimes":1}}"#).unwrap());
        model.sessions.snapshot = Some(st3_client::Snapshot {
            id: "snapshot/hetz/1".into(),
            host_id: "host/hetz".into(),
            store_index: 1,
            projection_version: "v0".into(),
            created_at: "2026-09-25T08:00:00Z".into(),
        });
    }
    #[test]
    fn regression_person_falls_back_to_local_config() {
        let dir = std::env::temp_dir().join(format!("stui-person-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "person = \"person/nathan\"\n").unwrap();
        assert_eq!(
            person_from_config(&path).unwrap().as_deref(),
            Some("person/nathan")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn regression_ready_work_and_agent_observation_show_age() {
        assert!(age_label("2026-09-24T11:56:00Z", "2026-09-25T08:56:00Z").contains("21h"));
    }
    #[test]
    fn regression_agent_header_shows_harness_state() {
        let mut model = Model::default();
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/st3","revision":"a","updated_at":"2026-09-25T08:00:00Z","name":"ST3","state":"running","reachability":"reachable","driver":"claude","harness_state":"ready"}"#).unwrap());
        let mut app = App::new(model);
        app.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Harness: claude · ready"));
        assert!(content.contains("observed"));
    }

    #[test]
    fn regression_mission_labels_are_unique() {
        let first: st3_client::Resource = serde_json::from_str(r#"{"kind":"mission","id":"mission/a/issue-triage","revision":"a","updated_at":"2026-09-25T08:00:00Z","title":"Issue Triage","state":"running","mission_revision":"a"}"#).unwrap();
        let second: st3_client::Resource = serde_json::from_str(r#"{"kind":"mission","id":"mission/b/issue-triage","revision":"b","updated_at":"2026-09-25T08:00:00Z","title":"Issue Triage","state":"running","mission_revision":"b"}"#).unwrap();
        let (st3_client::Resource::Mission(a), st3_client::Resource::Mission(b)) = (first, second)
        else {
            panic!()
        };
        assert_eq!(mission_display_label(&a), "a · Issue Triage");
        assert_eq!(mission_display_label(&b), "b · Issue Triage");
    }

    #[test]
    fn regression_now_action_has_a_useful_label_and_key() {
        assert_eq!(action_label("attention.resolve"), "Resolve [r]");
    }

    #[test]
    fn regression_fleet_agent_work_is_scoped_to_selected_host() {
        let agent: st3_client::Resource = serde_json::from_str(r#"{"kind":"agent","id":"agent/worker","revision":"a","updated_at":"2026-09-25T08:00:00Z","name":"Worker","state":"running","reachability":"reachable","runtime_ids":["runtime/worker"],"active_work_count":1}"#).unwrap();
        let runtime: st3_client::Resource = serde_json::from_str(r#"{"kind":"runtime","id":"runtime/worker","revision":"a","updated_at":"2026-09-25T08:00:00Z","runtime_kind":"agent","owner_id":"agent/worker","owner_host_id":"host/Silber","state":"running","runtime_id":"worker","incarnation_id":null,"desired_revision":"a"}"#).unwrap();
        let mut model = Model::default();
        model.agents.items.push(agent);
        model.runtimes.items.push(runtime);
        let app = App::new(model);
        assert!(!app.agent_on_host("agent/worker", "host/hetz"));
        assert!(app.agent_on_host("agent/worker", "host/Silber"));
    }

    #[test]
    fn regression_device_rows_identify_each_device() {
        let device: st3_client::Resource = serde_json::from_str(r#"{"kind":"device","id":"device/iphone-15","revision":"a","updated_at":"2026-09-25T08:00:00Z","person_id":"person/nathan","session_actor":"person/nathan/session/abc","state":"active","expires_at":"2026-10-01T00:00:00Z"}"#).unwrap();
        let st3_client::Resource::Device(device) = device else {
            panic!()
        };
        assert!(device_label(&device).contains("iphone-15"));
    }
    #[test]
    fn regression_history_is_an_explicit_control_not_inline_chat_text() {
        let mut model = Model::default();
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/cos","revision":"a","updated_at":"2026-09-25T08:00:00Z","name":"cos","state":"running","reachability":"reachable"}"#).unwrap());
        model.timeline_truncated = true;
        model.timeline.push(serde_json::from_str(r#"{"id":"timeline/one","sequence":1,"revision":1,"timestamp":"2026-09-25T08:00:00Z","role":"assistant","type":"content","final":true,"body":{"media_type":"text/plain","text":"Visible newest message"}}"#).unwrap());
        let mut app = App::new(model);
        app.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Visible newest message"));
        assert!(!content.contains("[More history beyond bounded view]"));
        assert!(content.contains("History"));
    }
    #[test]
    fn regression_status_only_timeline_has_an_explanation() {
        let mut model = Model::default();
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/app-apple","revision":"a","updated_at":"2026-09-25T08:00:00Z","name":"App Apple","state":"running","reachability":"reachable","current_session_id":"session/apple"}"#).unwrap());
        let mut app = App::new(model);
        app.tab = 1;
        app.timeline_cache
            .insert("session/apple".into(), (Vec::new(), false));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("No conversation in recent timeline."));
        app.messages_requested = Some("agent/app-apple".into());
        terminal.draw(|frame| app.render(frame)).unwrap();
        let loading = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(loading.contains("Loading ST3 messages…"));
    }

    #[test]
    fn status_only_chat_keeps_graph_messages_at_the_bottom() {
        let mut model = Model::default();
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/omp","revision":"a","updated_at":"2026-09-25T08:00:00Z","name":"OMP","state":"running","reachability":"reachable","current_session_id":"session/omp"}"#).unwrap());
        for number in 0..6 {
            model.messages.items.push(serde_json::from_value(serde_json::json!({
                "kind":"message", "id":format!("message/{number}"), "revision":"one",
                "updated_at":"2026-09-25T08:00:00Z", "from":"agent/cos", "to":"agent/omp",
                "title":null, "content":format!("message {number}: {}", "a long message ".repeat(40)),
                "state":"closed", "sent_at":"2026-09-25T08:00:00Z", "in_reply_to":null, "session_id":null
            })).unwrap());
        }
        let mut app = App::new(model);
        app.tab = 1;
        app.timeline_cache
            .insert("session/omp".into(), (Vec::new(), false));
        let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            content.contains("ST3 messages · transcript unavailable"),
            "{content}"
        );
        assert!(content.contains("message 5:"), "{content}");
    }
    #[test]
    fn recent_message_preserves_line_breaks_without_return_glyphs() {
        let mut model = Model::default();
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/st3","revision":"one","updated_at":"2026-09-25T08:00:00Z","name":"ST3","state":"running","reachability":"reachable"}"#).unwrap());
        model.messages.items.push(serde_json::from_str(r#"{"kind":"message","id":"message/two-lines","revision":"one","updated_at":"2026-09-25T08:00:00Z","from":"agent/cos","to":"agent/st3","title":null,"content":"first line\nsecond line","state":"closed","sent_at":"2026-09-25T08:00:00Z","in_reply_to":null,"session_id":null}"#).unwrap());
        let mut app = App::new(model);
        app.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("first line"));
        assert!(content.contains("second line"));
        assert!(!content.contains('⏎'));
    }
    #[test]
    fn all_views_render_at_normal_and_narrow_width() {
        let mut app = App::new(Model::default());
        for width in [80, 40] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            for (tab, label) in TABS.iter().enumerate() {
                app.tab = tab;
                terminal.draw(|frame| app.render(frame)).unwrap();
                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains(label));
            }
        }
    }
    #[test]
    fn terminal_key_encoding() {
        assert_eq!(
            key_input(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)).as_deref(),
            Some("C-x")
        );
    }

    #[test]
    fn terminal_detach_accepts_both_crossterm_encodings() {
        assert!(is_detach_key(KeyEvent::new(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL
        )));
        assert!(is_detach_key(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_detach_key(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::NONE
        )));
    }

    #[tokio::test]
    async fn ctrl_four_does_not_switch_tabs_outside_terminal() {
        let mut app = App::new(Model::default());
        app.tab = 1;
        let client = Client::unix("/nonexistent-stui-test.sock");
        assert!(
            !handle_key(
                &mut app,
                &client,
                KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL)
            )
            .await
            .unwrap()
        );
        assert_eq!(app.tab, 1);
    }

    #[test]
    fn terminal_fence_uses_owner_sequence_for_relayed_screen() {
        let mut screen: st3_client::Envelope<st3_client::TerminalScreen> = serde_json::from_str(
            include_str!("../../../docs/st3/client-v0/fixtures/terminal-screen.json"),
        )
        .unwrap();
        screen.snapshot.store_index = 10;
        screen.value.next_sequence = 42;
        let incarnation = screen.value.runtime_incarnation.clone();
        let fence = terminal_screen_fence(&screen, &incarnation).unwrap();
        assert_eq!(fence.terminal_sequence, Some(42));
        assert_eq!(fence.snapshot_id, screen.snapshot.id);
    }

    #[test]
    fn agent_relationships_render_as_a_tree_and_selection_tracks_it() {
        let parent: st3_client::Resource = serde_json::from_str(r#"{"kind":"agent","id":"agent/root","revision":"one","updated_at":"2026-09-24T09:00:00Z","name":"Root","state":"running","reachability":"local","runtime_ids":[],"under":[]}"#).unwrap();
        let child: st3_client::Resource = serde_json::from_str(r#"{"kind":"agent","id":"agent/child","revision":"one","updated_at":"2026-09-24T09:00:00Z","name":"Child","state":"running","reachability":"local","runtime_ids":[],"under":[{"agent_id":"agent/root","reason":"delegated"}]}"#).unwrap();
        let mut model = Model::default();
        model.agents.items.extend([child, parent]);
        let mut app = App::new(model);
        assert_eq!(
            app.agent_tree()
                .iter()
                .map(|(agent, depth)| (agent.name.as_str(), *depth))
                .collect::<Vec<_>>(),
            vec![("Root", 0), ("Child", 1)]
        );
        app.selected[1] = 1;
        assert_eq!(app.peer().unwrap().name, "Child");
        assert_eq!(agent_label(app.peer().unwrap()), "Child");
    }

    #[test]
    fn omp_agent_label_names_the_seat_and_driver() {
        let agent: st3_client::Agent = serde_json::from_str(r#"{"kind":"agent","id":"agent/fleet/pty-rust/omp","revision":"one","updated_at":"2026-09-25T08:00:00Z","name":"fleet/pty-rust/omp","state":"running","reachability":"local","runtime_ids":[],"under":[]}"#).unwrap();
        assert_eq!(agent_label(&agent), "PTY Rust · OMP");
    }

    #[test]
    fn chat_opens_on_active_work_before_stopped_history() {
        let mut model = Model::default();
        for (id, name, state, active) in [
            ("agent/diagnostic", "Diagnostic", "stopped", 0),
            ("agent/available", "Available", "running", 0),
            ("agent/working", "Working", "running", 1),
        ] {
            model.agents.items.push(
                serde_json::from_value(serde_json::json!({
                    "kind":"agent", "id":id, "revision":"one", "updated_at":"2026-09-25T08:00:00Z",
                    "name":name, "state":state, "reachability":"reachable",
                    "active_work_count":active
                }))
                .unwrap(),
            );
        }
        let app = App::new(model);
        assert_eq!(
            app.agent_tree()
                .iter()
                .map(|(agent, _)| agent.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Working", "Available"]
        );
    }

    #[test]
    fn st3_descendants_nest_and_top_level_omp_shows_its_work() {
        let mut model = Model::default();
        for (id, name, driver, active) in [
            ("agent/fleet/st3/standing/st3", "ST3", "codex", 0),
            (
                "agent/st3/tui-ios-fixes/2026-09-25/st3-tui-fixer",
                "TUI fixer",
                "codex",
                1,
            ),
            (
                "agent/fleet/st3/delivery-soak/recipient",
                "Recipient",
                "codex",
                0,
            ),
            ("agent/fleet/pty-rust/omp", "OMP", "omp", 1),
        ] {
            model.agents.items.push(
                serde_json::from_value(serde_json::json!({
                    "kind":"agent", "id":id, "revision":"one", "updated_at":"2026-09-25T08:00:00Z",
                    "name":name, "driver":driver, "state":"running", "reachability":"reachable",
                    "active_work_count":active
                }))
                .unwrap(),
            );
        }
        let mut app = App::new(model);
        app.tab = 1;
        let tree = app.agent_tree();
        assert_eq!(
            tree.iter()
                .find(|(agent, _)| agent.name == "TUI fixer")
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            tree.iter()
                .find(|(agent, _)| agent.name == "Recipient")
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            tree.iter()
                .find(|(agent, _)| agent.name == "OMP")
                .unwrap()
                .1,
            0
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("1 work"));
    }
    #[tokio::test]
    async fn exact_undeclared_session_offers_confirmed_migration() {
        let session: Resource = serde_json::from_str(r#"{"kind":"session","id":"session/external","revision":"native-rev","updated_at":"2026-09-25T08:00:00Z","owner_id":"external-session/codex/one","state":"running","started_at":"2026-09-25T07:00:00Z","ended_at":null,"timeline_cursor":"cursor/one","managed":false,"driver":"codex","native_session_id":"one","importable":true}"#).unwrap();
        let mut model = Model::default();
        model.sessions.items.push(session);
        let mut app = App::new(model);
        app.tab = 1;
        app.live_ready = true;
        let client = Client::unix("/nonexistent-stui-test.sock");
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("m import"));
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::ImportConfirm);
        assert_eq!(app.pending_import.as_deref(), Some("session/external"));
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::ImportConfirm);
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert!(app.pending_import.is_none());
        let unresolved: Resource = serde_json::from_str(r#"{"kind":"session","id":"session/unresolved","revision":"one","updated_at":"2026-09-25T08:00:00Z","owner_id":"external-process/claude/42","state":"running","started_at":"2026-09-25T07:00:00Z","ended_at":null,"timeline_cursor":"cursor/two","managed":false,"driver":"claude","native_session_id":null}"#).unwrap();
        app.model.sessions.items.push(unresolved);
        app.selected[1] = 1;
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.model.status.contains("exact native session ID"));
    }

    #[test]
    fn chat_and_fleet_show_agent_work_queue() {
        let agent: st3_client::Resource = serde_json::from_value(serde_json::json!({
            "kind": "agent", "id": "agent/worker", "revision": "one",
            "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
            "state": "running", "reachability": "local", "runtime_ids": [],
            "current_work_ids": ["step-run/old/work"], "active_work_count": 1,
            "next_work_id": "step-run/new/review", "queued_work_count": 2
        }))
        .unwrap();
        let mut model = Model::default();
        model.agents.items.push(agent);
        model.work.items.push(serde_json::from_str(r#"{"kind":"work","id":"step-run/new/review","revision":"one","updated_at":"2026-09-24T11:56:00Z","mission_run_id":"mission-run/new","generation_id":"run-generation/new","definition_id":"one","path":"review","state":"ready","attempt":1,"readiness_epoch":1,"claimant":null,"claim_incarnation":null,"blocked_reason":null}"#).unwrap());
        model.runtimes.items.push(serde_json::from_str(r#"{"kind":"runtime","id":"runtime/worker","revision":"one","updated_at":"2026-09-25T08:00:00Z","runtime_kind":"agent","owner_id":"agent/worker","owner_host_id":"host/hetz","state":"running","runtime_id":"worker","incarnation_id":null,"desired_revision":"one"}"#).unwrap());
        local_machine(&mut model);
        let mut app = App::new(model);
        let mut terminal = Terminal::new(TestBackend::new(100, 35)).unwrap();
        for (tab, expected) in [(1, "Next: step-run/new/review"), (3, "2 queued")] {
            app.tab = tab;
            terminal.draw(|frame| app.render(frame)).unwrap();
            let rows = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rows.contains(expected), "tab {tab} did not show {expected}");
            assert!(
                rows.contains("Ready "),
                "tab {tab} did not show queued work age"
            );
        }
    }

    #[test]
    fn chat_and_fleet_label_undeclared_sessions() {
        let session: st3_client::Resource = serde_json::from_str(
            r#"{"kind":"session","id":"session/external","revision":"one","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-session/codex/one","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/one","managed":false,"driver":"codex","native_session_id":"one"}"#,
        )
        .unwrap();
        let mut model = Model::default();
        model.sessions.items.push(session);
        local_machine(&mut model);
        let mut app = App::new(model);
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        for tab in [1, 3] {
            app.tab = tab;
            terminal.draw(|frame| app.render(frame)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.to_lowercase().contains("undeclared"));
        }
    }

    #[test]
    fn chat_finds_running_session_when_agent_field_is_absent() {
        let agent: st3_client::Resource = serde_json::from_str(r#"{"kind":"agent","id":"agent/cos","revision":"one","updated_at":"2026-09-24T09:00:00Z","name":"cos","state":"running","reachability":"reachable"}"#).unwrap();
        let session: st3_client::Resource = serde_json::from_str(r#"{"kind":"session","id":"session/cos-current","revision":"one","updated_at":"2026-09-24T09:00:00Z","owner_id":"agent/cos","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/one"}"#).unwrap();
        let mut model = Model::default();
        model.agents.items.push(agent);
        model.sessions.items.push(session);
        let app = App::new(model);
        assert_eq!(
            app.selected_session_id().as_deref(),
            Some("session/cos-current")
        );
    }

    #[test]
    fn chat_restores_each_sessions_scroll_position() {
        let mut model = Model::default();
        for (name, session) in [("alpha", "session/alpha"), ("beta", "session/beta")] {
            let agent: st3_client::Resource = serde_json::from_value(serde_json::json!({
                "kind":"agent", "id":format!("agent/{name}"), "revision":"one",
                "updated_at":"2026-09-24T09:00:00Z", "name":name, "state":"running",
                "reachability":"reachable", "current_session_id":session
            }))
            .unwrap();
            model.agents.items.push(agent);
        }
        let mut app = App::new(model);
        app.tab = 1;
        app.scroll[1] = 18;
        app.remember_chat_scroll();
        app.selected[1] = 1;
        app.restore_chat_scroll();
        assert_eq!(app.scroll[1], u16::MAX);
        app.scroll[1] = 4;
        app.remember_chat_scroll();
        app.selected[1] = 0;
        app.restore_chat_scroll();
        assert_eq!(app.scroll[1], 18);
        app.input = "unfinished alpha".into();
        app.remember_chat_draft();
        app.input.clear();
        app.selected[1] = 1;
        app.start_chat_composer();
        assert!(app.input.is_empty());
        app.input = "unfinished beta".into();
        app.remember_chat_draft();
        app.selected[1] = 0;
        app.start_chat_composer();
        assert_eq!(app.input, "unfinished alpha");
        assert_eq!(app.chat_draft_cache.len(), 2);
        app.timeline_cache
            .insert("session/alpha".into(), (Vec::new(), true));
        app.model.timeline_truncated = true;
        app.timeline_requested = Some("session/alpha".into());
        app.invalidate_timelines();
        assert!(app.timeline_cache.is_empty());
        assert!(app.model.timeline_truncated);
        assert_eq!(app.timeline_requested.as_deref(), Some("session/alpha"));
        assert_eq!(app.chat_draft_cache.len(), 2);
        assert_eq!(app.chat_scroll_cache.get("session/alpha"), Some(&18));
    }

    #[test]
    fn layout_has_top_tabs_compact_connection_and_selection_hint() {
        let mut app = App::new(Model::default());
        app.live_ready = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let first = (0..100)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert!(first.contains("Now") && first.contains("Chat"));
        assert!(first.contains("Online"));
        assert!(!first.contains("Smalltalk"));
        assert!(buffer[(1, 0)].bg != Color::Reset);
    }
    #[test]
    fn connection_error_is_available_on_demand_not_in_footer() {
        let mut model = Model::default();
        model.status = "Sync: long cache error with useful detail".into();
        let mut app = App::new(model);
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let top = (0..100)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        let footer = (0..100)
            .map(|x| buffer[(x, 24)].symbol())
            .collect::<String>();
        assert!(top.contains("Offline"));
        assert!(!footer.contains("cache error"));
        app.status_details = true;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Sync: long cache error"));
    }
    #[test]
    fn chat_follows_newest_message_and_click_targets_agent_row() {
        let mut model = Model::default();
        for name in ["alpha", "beta"] {
            model.agents.items.push(serde_json::from_value(serde_json::json!({"kind":"agent","id":format!("agent/{name}"),"revision":"one","updated_at":"2026-09-25T08:00:00Z","name":name,"state":"running","reachability":"reachable"})).unwrap());
        }
        for sequence in 1..=20 {
            model.timeline.push(serde_json::from_value(serde_json::json!({"id":format!("timeline/{sequence}"),"sequence":sequence,"revision":1,"timestamp":"2026-09-25T08:00:00Z","role":"assistant","type":"content","final":true,"body":{"media_type":"text/plain","text":format!("Message {sequence}")}})).unwrap());
        }
        let mut app = App::new(model);
        app.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Message 20"));
        assert_eq!(sidebar_item_at(&app, 2, 2, 90), Some(0));
        // Ratatui discards the trailing newline in each Chat item, so the
        // second rendered row is row 3, not row 4.
        assert_eq!(sidebar_item_at(&app, 2, 3, 90), Some(1));
        assert_eq!(sidebar_item_at(&app, 2, 3, 60), None);
        app.selected[1] = sidebar_item_at(&app, 2, 3, 90).unwrap();
        assert_eq!(app.peer().unwrap().header.id, "agent/beta");
        app.selected[1] = 0;
        app.sidebar_offsets[1].set(1);
        assert_eq!(sidebar_item_at(&app, 2, 2, 90), Some(1));
        let normal_header = (0..90)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(normal_header.contains("Select text"));
        app.selection_mode = true;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let header = (0..90)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(header.contains("SELECT"));
    }

    #[test]
    fn chat_wheel_scroll_leaves_and_returns_to_newest() {
        let mut app = App::new(Model::default());
        app.tab = 1;
        app.chat_max_scroll.set(20);
        scroll_detail(&mut app, true);
        assert_eq!(app.scroll[1], 17);
        scroll_detail(&mut app, false);
        assert_eq!(app.scroll[1], u16::MAX);
    }

    #[test]
    fn history_panel_and_text_selection_controls_fit_a_normal_terminal() {
        let mut app = App::new(Model::default());
        app.tab = 1;
        app.history_open = true;
        app.model.timeline_truncated = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("History & details"));
        assert!(content.contains("Load older pages"));
        assert!(!content.contains("Browse"));
        let history = app.history_control.get();
        let select = app.select_control.get();
        assert!(point_in(history, history.x + 1, 0));
        assert!(point_in(select, select.x + 1, 0));
    }

    #[test]
    fn now_card_lists_actions_and_confirmation_keys() {
        let mut model = Model::default();
        model.actor = "person/nathan".into();
        model.now.items.push(serde_json::from_str(r#"{"kind":"attention","id":"attention/a","revision":"one","updated_at":"2026-09-25T08:00:00Z","attention_kind":"human-gate","source_id":"step-run/a","person_id":"person/nathan","title":"Review deployment","detail":"Approve the release?","priority":"high","state":"open","requested_at":"2026-09-25T08:00:00Z","actions":["review.approve","review.reject"]}"#).unwrap());
        let app = App::new(model);
        let mut terminal = Terminal::new(TestBackend::new(110, 35)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Approve [a]"));
        assert!(content.contains("Reject [j]"));
        assert!(content.contains("Approve the release?"));
    }
    #[tokio::test]
    async fn attention_key_requires_explicit_confirmation() {
        let mut model = Model::default();
        model.actor = "person/nathan".into();
        model.now.items.push(serde_json::from_str(r#"{"kind":"attention","id":"attention/a","revision":"one","updated_at":"2026-09-25T08:00:00Z","attention_kind":"human-gate","source_id":"step-run/a","person_id":"person/nathan","title":"Review","detail":"Approve?","priority":"high","state":"open","requested_at":"2026-09-25T08:00:00Z","actions":["review.approve","review.reject"]}"#).unwrap());
        let mut app = App::new(model);
        let client = Client::unix("/nonexistent-stui-test.sock");
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::ActionReason);
        assert_eq!(app.pending_action.as_ref().unwrap().1, "review.approve");
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::ActionReason);
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        )
        .await
        .unwrap();
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(app.pending_reason.as_deref(), Some("y"));
        handle_key(
            &mut app,
            &client,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_action.is_none());
    }

    #[test]
    fn mission_progress_counts_terminal_and_active_steps() {
        let mut model = Model::default();
        model.missions.items.push(serde_json::from_str(r#"{"kind":"mission","id":"mission/a","revision":"one","updated_at":"2026-09-25T08:00:00Z","title":"Release","state":"running","mission_revision":"a","runs":["mission-run/old","mission-run/a"],"run_generations":{"mission-run/a":"generation/a"}}"#).unwrap());
        for (path, state) in [
            ("build", "completed"),
            ("review", "claimed"),
            ("deploy", "waiting"),
        ] {
            model.work.items.push(serde_json::from_value(serde_json::json!({"kind":"work","id":format!("step-run/a/{path}"),"revision":"one","updated_at":"2026-09-25T08:00:00Z","mission_run_id":"mission-run/a","generation_id":"generation/a","definition_id":"def/a","path":path,"state":state,"attempt":1,"readiness_epoch":1})).unwrap());
        }
        if let Some(Resource::Work(work)) = model
            .work
            .items
            .iter_mut()
            .find(|item| item.header().id == "step-run/a/review")
        {
            work.claimant = Some("agent/reviewer".into());
        }
        model.work.items.push(serde_json::from_str(r#"{"kind":"work","id":"step-run/old/ship","revision":"one","updated_at":"2026-09-24T08:00:00Z","mission_run_id":"mission-run/old","generation_id":"generation/old","definition_id":"def/old","path":"ship","state":"completed","attempt":1,"readiness_epoch":1}"#).unwrap());
        model.work.items.push(serde_json::from_str(r#"{"kind":"work","id":"step-run/superseded/review","revision":"one","updated_at":"2026-09-24T08:00:00Z","mission_run_id":"mission-run/a","generation_id":"generation/superseded","definition_id":"def/old","path":"review","state":"cancelled","attempt":1,"readiness_epoch":1}"#).unwrap());
        model.agents.items.push(serde_json::from_str(r#"{"kind":"agent","id":"agent/queue","revision":"one","updated_at":"2026-09-25T08:00:00Z","name":"queue","state":"running","reachability":"reachable","next_work_id":"step-run/a/deploy"}"#).unwrap());
        let mission = model.missions().next().unwrap();
        assert_eq!(mission_progress(&model, mission), (1, 3));
        assert_eq!(
            mission_current_work(&model, mission).unwrap().path,
            "review"
        );
        assert_eq!(
            next_action_label(mission_current_work(&model, mission).unwrap()),
            "Agent working"
        );
        assert_eq!(App::new(model.clone()).mission_group(mission), "Running");
        model.work.snapshot = Some(st3_client::Snapshot {
            id: "snapshot/one".into(),
            host_id: "host/one".into(),
            store_index: 1,
            projection_version: "v0".into(),
            created_at: "2026-09-25T08:00:00Z".into(),
        });
        model.work.truncated = true;
        assert_eq!(
            mission_progress_label(&model, model.missions().next().unwrap(), 1, 3),
            "1/3+ recent steps"
        );
        let mut app = App::new(model);
        app.tab = 2;
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("NEXT"));
        assert!(content.contains("Agent working"));
        assert!(content.contains("Owner"));
        assert!(content.contains("agent/reviewer"));
        if let Some(Resource::Work(work)) = app
            .model
            .work
            .items
            .iter_mut()
            .find(|item| item.header().id == "step-run/a/deploy")
        {
            work.state = "blocked".into();
            work.blocked_reason = Some("Needs owner review".into());
        }
        let mission = app.model.missions().next().unwrap();
        assert_eq!(app.mission_group(mission), "Blocked");
        assert_eq!(
            mission_current_work(&app.model, mission).unwrap().path,
            "deploy"
        );
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Resolve blocker"));
        assert!(content.contains("Needs owner review"));
        assert!(content.contains("agent/queue"));
    }
}
