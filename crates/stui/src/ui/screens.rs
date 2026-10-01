//! What each tab lists and what its detail pane says.
//!
//! A screen produces sidebar items and a detail `Doc`. It never draws cells itself; the
//! shell owns layout, scrolling, selection and clicks.

use super::doc::{Doc, Hit, meter};
use super::text::{self, run};
use super::theme;
use super::view::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The tabs everyone sees. Worktrees stays hidden until the graph models worktrees: its screens
/// only have invented data to show (Nathan, 2026-09-28).
pub const TABS: [&str; 4] = ["Home", "Agents", "Missions", "Fleet"];

pub enum Item {
    Header {
        title: String,
        count: usize,
        color: Color,
    },
    Row {
        index: usize,
        first: Vec<Span<'static>>,
        right: Vec<Span<'static>>,
        second: Vec<Span<'static>>,
    },
    Note(Line<'static>),
    /// A folder line in a tree: no spacing around it.
    Folder(Line<'static>),
}

/// Sidebar rows. `index` in each row is the position in the tab's selectable order.
pub struct Listing {
    pub items: Vec<Item>,
    /// Selectable ids in display order.
    pub ids: Vec<String>,
    pub state: ListState,
    pub legend: Vec<Line<'static>>,
}

pub enum ListState {
    Loading(&'static str),
    Empty(&'static str),
    Failed(String),
    Ready,
}

fn state_of<T>(load: &Load<Vec<T>>, loading: &'static str, empty: &'static str) -> ListState {
    match load {
        Load::Loading => ListState::Loading(loading),
        Load::Failed(error) => ListState::Failed(error.clone()),
        Load::Ready(items) if items.is_empty() => ListState::Empty(empty),
        Load::Ready(_) => ListState::Ready,
    }
}

// ------------------------------------------------------------------- glyphs

pub fn agent_glyph(state: AgentState, spinner: &'static str) -> (&'static str, Color) {
    match state {
        AgentState::NeedsYou => ("◆", theme::PERSON),
        AgentState::Fault => ("✕", theme::FAULT),
        AgentState::Working => (spinner, theme::WORKING),
        AgentState::Idle => ("●", theme::IDLE),
        AgentState::Starting => ("◌", theme::WAITING),
        AgentState::Stopped => ("○", theme::QUIET),
        AgentState::Unknown => ("?", theme::QUIET),
    }
}

pub fn agent_word(state: AgentState) -> &'static str {
    match state {
        AgentState::NeedsYou => "needs you",
        AgentState::Fault => "broken",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Starting => "starting",
        AgentState::Stopped => "stopped",
        AgentState::Unknown => "not managed",
    }
}

pub fn word_style(word: Word, spinner: &'static str) -> (&'static str, Color) {
    match word {
        Word::Decision => ("◆", theme::PERSON),
        Word::Stalled => ("▲", theme::FAULT),
        Word::Unstaffed => ("◇", theme::WAITING),
        Word::Unclaimed => ("◇", theme::OVERLAY1),
        Word::Queued => ("◌", theme::OVERLAY1),
        Word::Working => (spinner, theme::WORKING),
        Word::Watching => ("◉", theme::SAPPHIRE),
        Word::Held => ("◐", theme::SAPPHIRE),
        Word::Idle => ("●", theme::IDLE),
        Word::Done => ("✓", theme::DONE),
        Word::Failed => ("✕", theme::FAULT),
        Word::Cancelled => ("⊘", theme::QUIET),
        Word::NotStarted => ("○", theme::QUIET),
    }
}

fn step_style(state: StepState, spinner: &'static str) -> (&'static str, Color, &'static str) {
    match state {
        StepState::Done => ("▰", theme::DONE, "done"),
        StepState::Cancelled => ("−", theme::SURFACE2, "cancelled"),
        StepState::Working => (spinner, theme::WORKING, "working"),
        StepState::Ready => ("▱", theme::WAITING, "ready"),
        StepState::Waiting => ("◐", theme::SAPPHIRE, "waiting"),
        StepState::NeedsYou => ("◆", theme::PERSON, "needs you"),
        StepState::Failed => ("✕", theme::FAULT, "failed"),
        StepState::Pending => ("▱", theme::SURFACE2, "later"),
    }
}

fn attention_style(kind: &AttentionKind) -> (&'static str, Color) {
    match kind {
        AttentionKind::Review { .. }
        | AttentionKind::Feedback { .. }
        | AttentionKind::Launch { .. }
        | AttentionKind::Revision { .. }
        | AttentionKind::Request { .. } => ("◆", theme::PERSON),
        AttentionKind::Fault { .. } => ("✕", theme::FAULT),
        AttentionKind::Message { .. } => ("✉", theme::SAPPHIRE),
    }
}

fn harness_color(harness: Harness) -> Color {
    match harness {
        Harness::Claude => theme::PEACH,
        Harness::Codex => theme::BLUE,
        Harness::Omp => theme::TEAL,
        Harness::Pi => theme::LAVENDER,
        Harness::Unknown => theme::OVERLAY0,
    }
}

fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn legend(entries: &[(&str, Color, &str)]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    for (index, (glyph, color, word)) in entries.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            lines.push(Line::from(std::mem::take(&mut spans)));
        }
        spans.push(span(format!("{glyph} "), theme::fg(*color)));
        spans.push(span(format!("{word:<10}"), theme::dim()));
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

// --------------------------------------------------------------------- home

pub fn home_list(world: &World, snoozed: &std::collections::HashSet<String>) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let mut attention = world
        .attention
        .items()
        .iter()
        .filter(|item| !snoozed.contains(&item.id))
        .collect::<Vec<_>>();
    attention.sort_by_key(|item| item.tier);
    let mut current = None;
    for item in attention {
        if current != Some(item.tier) {
            current = Some(item.tier);
            let count = world
                .attention
                .items()
                .iter()
                .filter(|other| other.tier == item.tier)
                .count();
            items.push(Item::Header {
                title: item.tier.title().into(),
                count,
                color: if item.tier == Tier::Stopped {
                    theme::PERSON
                } else {
                    theme::OVERLAY1
                },
            });
        }
        let (glyph, color) = attention_style(&item.kind);
        let mut second = vec![span("   ", theme::dim())];
        if let Some(waiting) = &item.waiting {
            second.push(span(format!("{waiting} · "), theme::dim()));
        }
        second.push(span(format!("waited {}", item.age), theme::dim()));
        items.push(Item::Row {
            index: ids.len(),
            first: vec![
                span(format!(" {glyph} "), theme::fg(color)),
                span(format!("{:<9}", item.kind.word()), theme::fg(color)),
                span(item.title.clone(), theme::bold()),
            ],
            right: vec![],
            second,
        });
        ids.push(item.id.clone());
    }
    let later = world
        .attention
        .items()
        .iter()
        .filter(|item| snoozed.contains(&item.id))
        .count();
    if later > 0 {
        items.push(Item::Note(Line::from(span(
            format!(" {later} put off until later · demo, this machine only"),
            theme::dim(),
        ))));
    }
    if world.attention.ready().is_some() && world.quiet_missions > 0 {
        items.push(Item::Note(Line::from(span(
            format!(
                " not shown: {} missions that don't need you",
                world.quiet_missions
            ),
            theme::dim(),
        ))));
    }
    Listing {
        items,
        ids,
        state: state_of(
            &world.attention,
            "Checking what needs you…",
            "Nothing needs you.",
        ),
        legend: legend(&[
            ("◆", theme::PERSON, "decide"),
            ("✕", theme::FAULT, "fault"),
            ("✉", theme::SAPPHIRE, "message"),
        ]),
    }
}

pub struct Drafts<'a> {
    pub text: Option<&'a str>,
    pub editing: bool,
    pub confirm: Option<char>,
    /// "Chat about this": who it goes to, the draft, and the thread so far.
    pub chat: Option<Chat<'a>>,
}

pub struct Chat<'a> {
    pub to: String,
    pub text: &'a str,
    pub editing: bool,
    pub thread: Vec<Line<'static>>,
}

/// A clickable reference to a graph subject; clicking it opens a popover.
pub fn link(doc: &mut Doc, prefix: &str, label: &str, subject: &str) {
    doc.targets.push(super::doc::Target {
        line: doc.lines.len(),
        column: text::width(prefix) as u16,
        width: text::width(label) as u16,
        hit: Hit::Peek(subject.to_owned()),
    });
    doc.line(Line::from(vec![
        span(prefix.to_owned(), theme::dim()),
        span(
            label.to_owned(),
            theme::fg(theme::BLUE).add_modifier(Modifier::UNDERLINED),
        ),
    ]));
}

fn text_box(doc: &mut Doc, title: &str, drafts: &Drafts<'_>, placeholder: &str, width: usize) {
    let mut inner = Doc::new();
    let body = drafts.text.unwrap_or("");
    if body.is_empty() && !drafts.editing {
        inner.line(Line::from(span(placeholder.to_owned(), theme::dim())));
    } else {
        let mut runs = vec![run(body.to_owned(), theme::text())];
        if drafts.editing {
            runs.push(run("█", theme::fg(theme::ACCENT)));
        }
        let lines = text::wrap(&runs, width.saturating_sub(4), &[], &[], None);
        inner.lines(lines);
    }
    let start = doc.lines.len();
    doc.card(
        title,
        if drafts.editing {
            theme::ACCENT
        } else {
            theme::OVERLAY1
        },
        false,
        inner,
        width,
    );
    let end = doc.lines.len();
    for line in start..end {
        doc.targets.push(super::doc::Target {
            line,
            column: 0,
            width: width as u16,
            hit: Hit::Composer,
        });
    }
}

fn confirm_row(doc: &mut Doc, drafts: &Drafts<'_>, label: &str) -> bool {
    if drafts.confirm.is_some() {
        doc.line(Line::from(vec![span(
            format!("{label}? "),
            theme::strong(theme::YELLOW),
        )]));
        doc.buttons(&[
            ("y", "Yes, do it", Hit::Key('y'), theme::GREEN),
            ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
        ]);
        true
    } else {
        false
    }
}

pub fn home_detail(world: &World, id: Option<&str>, width: usize, drafts: &Drafts<'_>) -> Doc {
    let mut doc = Doc::new();
    let Some(item) = id.and_then(|id| world.attention.items().iter().find(|item| item.id == id))
    else {
        match &world.attention {
            Load::Loading => doc.line(Line::from(span("Checking what needs you…", theme::dim()))),
            Load::Failed(error) => doc.wrap(
                &[run(
                    format!("Could not read what needs you: {error}"),
                    theme::fg(theme::RED),
                )],
                width,
            ),
            Load::Ready(_) => {
                doc.blank();
                doc.line(Line::from(vec![
                    span("✓ ", theme::strong(theme::GREEN)),
                    span("Nothing needs you right now.", theme::bold()),
                ]));
                doc.blank();
                doc.wrap(
                    &[run(
                        format!(
                            "{} missions are running without you. They are on the Missions tab.",
                            world.quiet_missions
                        ),
                        theme::dim(),
                    )],
                    width,
                );
            }
        }
        return doc;
    };
    let (glyph, color) = attention_style(&item.kind);
    let heavy = color == theme::PERSON;
    let mut card = Doc::new();
    let inner = width.saturating_sub(4);
    card.blank();
    card.wrap(&text::inline(&item.title, theme::bold()), inner);
    let mut meta = vec![span(
        format!("{glyph} {} ", item.kind.word()),
        theme::fg(color),
    )];
    if let Some(waiting) = &item.waiting {
        meta.push(span(format!("· {waiting} is waiting · "), theme::dim()));
    } else {
        meta.push(span("· ", theme::dim()));
    }
    meta.push(span(format!("{} ago", item.age), theme::dim()));
    card.line(Line::from(meta));
    if let Some(mission) = &item.mission {
        let label = world
            .missions
            .items()
            .iter()
            .find(|candidate| &candidate.id == mission)
            .map(|candidate| candidate.title.clone())
            .unwrap_or_else(|| mission.trim_start_matches("mission/").to_owned());
        link(&mut card, "mission  ", &label, mission);
    }
    if let Some(agent) = &item.agent {
        let label = world
            .agents
            .items()
            .iter()
            .find(|candidate| &candidate.id == agent)
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| agent.trim_start_matches("agent/").to_owned());
        link(&mut card, "agent    ", &label, agent);
    }
    card.blank();
    match &item.kind {
        AttentionKind::Review {
            question,
            because,
            look_at,
            step,
        } => {
            card.wrap(&text::inline(question, theme::text()), inner);
            card.blank();
            card.field("because", because, inner, theme::soft());
            for (label, value) in look_at {
                card.field(label, value, inner, theme::text());
            }
            if !step.is_empty() {
                card.field("step", step, inner, theme::soft());
            }
            card.blank();
            if !confirm_row(&mut card, drafts, "Approve the cut-over") {
                if drafts.editing || drafts.text.is_some_and(|text| !text.is_empty()) {
                    text_box(
                        &mut card,
                        "what should change · the agent reads this",
                        drafts,
                        "",
                        inner,
                    );
                    card.buttons(&[
                        (
                            "enter",
                            "Send back with these notes",
                            Hit::Enter,
                            theme::YELLOW,
                        ),
                        ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
                    ]);
                } else {
                    card.buttons(&[
                        ("a", "Approve", Hit::Key('a'), theme::GREEN),
                        ("c", "Request changes", Hit::Key('c'), theme::YELLOW),
                    ]);
                }
            }
        }
        AttentionKind::Feedback {
            question,
            subject,
            excerpt,
            link,
        } => {
            card.wrap(&text::inline(question, theme::text()), inner);
            card.blank();
            let mut preview = Doc::new();
            preview.lines(text::markdown(
                &excerpt.join("\n"),
                inner.saturating_sub(4),
                theme::soft(),
            ));
            card.card(subject, theme::OVERLAY1, false, preview, inner);
            if let Some(link) = link {
                card.line(Line::from(vec![
                    span("open  ", theme::dim()),
                    span(
                        link.clone(),
                        theme::fg(theme::BLUE).add_modifier(Modifier::UNDERLINED),
                    ),
                ]));
            }
            card.blank();
            text_box(
                &mut card,
                "your feedback · the agent reads this",
                drafts,
                "Click here or press c to write feedback",
                inner,
            );
            if drafts.editing {
                card.buttons(&[
                    ("enter", "Send feedback", Hit::Enter, theme::ACCENT),
                    ("esc", "Stop editing", Hit::Escape, theme::OVERLAY1),
                ]);
            } else if !confirm_row(&mut card, drafts, "Say it looks good") {
                card.buttons(&[
                    ("c", "Write feedback", Hit::Key('c'), theme::ACCENT),
                    ("a", "Looks good as is", Hit::Key('a'), theme::GREEN),
                ]);
            }
        }
        AttentionKind::Launch {
            planner,
            name,
            preview,
        } => {
            card.wrap(
                &[run(
                    format!("{planner} proposes a new mission. Nothing runs until you approve it."),
                    theme::text(),
                )],
                inner,
            );
            card.blank();
            let preview = match preview {
                Load::Ready(preview) => Some(preview),
                Load::Loading => {
                    card.field("mission", name, inner, theme::bold());
                    card.blank();
                    card.line(Line::from(span(
                        "Loading the proposed mission…",
                        theme::dim(),
                    )));
                    card.blank();
                    None
                }
                Load::Failed(reason) => {
                    card.field("mission", name, inner, theme::bold());
                    card.blank();
                    let mut why = Doc::new();
                    for line in reason.lines() {
                        why.wrap(&text::inline(line, theme::text()), inner.saturating_sub(4));
                    }
                    card.card("nothing to approve yet", theme::YELLOW, false, why, inner);
                    card.blank();
                    None
                }
            };
            if let Some(preview) = preview {
                card.field("mission", &preview.name, inner, theme::bold());
                if !preview.workspace.is_empty() {
                    card.field("in", &preview.workspace, inner, theme::soft());
                }
                let asks = preview.steps.iter().filter(|step| step.asks_you).count();
                card.field(
                    "asks you",
                    &format!(
                        "{asks} time{} before it finishes",
                        if asks == 1 { "" } else { "s" }
                    ),
                    inner,
                    theme::fg(theme::PERSON),
                );
                card.blank();
                card.section("goals", None, inner);
                for goal in &preview.goals {
                    card.lines(text::wrap(
                        &text::inline(goal, theme::text()),
                        inner,
                        &[run("◆ ", theme::fg(theme::LAVENDER))],
                        &[run("  ", theme::dim())],
                        None,
                    ));
                }
                card.blank();
                card.section("steps", Some(preview.steps.len()), inner);
                card.lines(flow(
                    preview.steps.iter().map(|step| {
                        (
                            step.name.as_str(),
                            step.after.as_slice(),
                            if step.asks_you {
                                theme::PERSON
                            } else {
                                theme::SUBTEXT1
                            },
                        )
                    }),
                    inner,
                ));
                for step in &preview.steps {
                    card.line(Line::from(vec![
                        span(format!("  {:<12}", step.name), theme::text()),
                        span(
                            format!("{:<18}", step.assignee),
                            if step.asks_you {
                                theme::fg(theme::PERSON)
                            } else {
                                theme::soft()
                            },
                        ),
                        span(
                            if step.after.is_empty() {
                                "first".to_owned()
                            } else {
                                format!("after {}", step.after.join(", "))
                            },
                            theme::dim(),
                        ),
                    ]));
                }
                card.blank();
                card.section("agents", Some(preview.agents.len()), inner);
                for agent in &preview.agents {
                    card.line(Line::from(vec![
                        span(
                            format!("  {:<18} ", text::truncate(&agent.name, 18)),
                            theme::text(),
                        ),
                        span(
                            format!("{:<8}", agent.harness.name()),
                            theme::fg(harness_color(agent.harness)),
                        ),
                        span(format!("on {}", agent.host), theme::dim()),
                    ]));
                }
                card.blank();
            }
            if drafts.editing || drafts.text.is_some_and(|text| !text.is_empty()) {
                text_box(
                    &mut card,
                    "what the planner should change",
                    drafts,
                    "",
                    inner,
                );
                card.buttons(&[
                    ("enter", "Send to the planner", Hit::Enter, theme::YELLOW),
                    ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
                ]);
            } else if !confirm_row(
                &mut card,
                drafts,
                if drafts.confirm == Some('a') {
                    "Approve and start this mission"
                } else {
                    "Cancel this launch"
                },
            ) {
                if preview.is_some() {
                    card.buttons(&[
                        ("a", "Approve launch", Hit::Key('a'), theme::GREEN),
                        ("c", "Ask for changes", Hit::Key('c'), theme::YELLOW),
                        ("d", "Cancel launch", Hit::Key('d'), theme::RED),
                    ]);
                } else {
                    card.buttons(&[
                        ("c", "Ask the planner", Hit::Key('c'), theme::YELLOW),
                        ("d", "Cancel launch", Hit::Key('d'), theme::RED),
                    ]);
                }
            }
        }
        AttentionKind::Revision { reason, changes } => {
            card.wrap(&text::inline(reason, theme::text()), inner);
            card.blank();
            card.section("changes to the plan", Some(changes.len()), inner);
            for (sign, change) in changes {
                let color = match sign {
                    '+' => theme::GREEN,
                    '-' => theme::RED,
                    _ => theme::YELLOW,
                };
                card.lines(text::wrap(
                    &[run(change.clone(), theme::fg(color))],
                    inner,
                    &[run(format!("{sign} "), theme::strong(color))],
                    &[run("  ", theme::dim())],
                    None,
                ));
            }
            card.blank();
            if drafts.editing || drafts.text.is_some_and(|text| !text.is_empty()) {
                text_box(
                    &mut card,
                    "what should change · the proposing agent reads this",
                    drafts,
                    "",
                    inner,
                );
                card.buttons(&[
                    ("enter", "Ask for these changes", Hit::Enter, theme::YELLOW),
                    ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
                ]);
            } else if !confirm_row(
                &mut card,
                drafts,
                if drafts.confirm == Some('a') {
                    "Approve the revision"
                } else {
                    "Reject the revision"
                },
            ) {
                card.buttons(&[
                    ("a", "Approve revision", Hit::Key('a'), theme::GREEN),
                    ("c", "Ask for changes", Hit::Key('c'), theme::YELLOW),
                    ("j", "Reject", Hit::Key('j'), theme::RED),
                ]);
            }
        }
        AttentionKind::Fault {
            what,
            because,
            fix,
            source,
        } => {
            card.wrap(&text::inline(what, theme::text()), inner);
            card.blank();
            card.field("because", because, inner, theme::soft());
            if !source.is_empty() {
                card.field("source", source, inner, theme::soft());
            }
            if let Some(fix) = fix {
                card.blank();
                let mut suggestion = Doc::new();
                suggestion.wrap(&text::inline(fix, theme::text()), inner.saturating_sub(4));
                card.card("suggested fix", theme::GREEN, false, suggestion, inner);
            }
            card.blank();
            card.wrap(
                &text::inline("Act on the source to clear this fault.", theme::soft()),
                inner,
            );
        }
        AttentionKind::Request { from, question, .. } => {
            card.line(Line::from(vec![
                span("asks  ", theme::dim()),
                span(from.clone(), theme::strong(theme::PERSON)),
            ]));
            card.blank();
            card.lines(text::markdown(question, inner, theme::text()));
            card.blank();
            if drafts.editing || drafts.text.is_some_and(|text| !text.is_empty()) {
                text_box(&mut card, &format!("answer {from}"), drafts, "", inner);
                card.buttons(&[
                    ("enter", "Complete step", Hit::Enter, theme::ACCENT),
                    ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
                ]);
            } else if !confirm_row(&mut card, drafts, "Mark this request answered") {
                card.buttons(&[("c", "Complete step", Hit::Key('c'), theme::ACCENT)]);
            }
        }
        AttentionKind::Message { from, body } => {
            card.line(Line::from(vec![
                span("from  ", theme::dim()),
                span(from.clone(), theme::strong(theme::SAPPHIRE)),
            ]));
            card.blank();
            card.lines(text::markdown(body, inner, theme::text()));
            card.blank();
            if drafts.editing || drafts.text.is_some_and(|text| !text.is_empty()) {
                text_box(&mut card, &format!("reply to {from}"), drafts, "", inner);
                card.buttons(&[
                    ("enter", "Send reply", Hit::Enter, theme::ACCENT),
                    ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
                ]);
            } else {
                card.buttons(&[
                    ("c", "Reply", Hit::Key('c'), theme::ACCENT),
                    ("m", "Mark read", Hit::Key('m'), theme::GREEN),
                    (
                        "l",
                        "Remind me later · demo",
                        Hit::Key('l'),
                        theme::OVERLAY1,
                    ),
                ]);
            }
        }
    }
    card.blank();
    // What the item is about, as links wherever st names a graph subject.
    card.section("related", None, inner);
    if let Some(who) = &item.raised_by {
        card.line(Line::from(vec![
            span(format!("{:<10}", "raised by"), theme::dim()),
            span(who.clone(), theme::soft()),
        ]));
    }
    if item.related.is_empty() && item.mission.is_none() && item.agent.is_none() {
        card.line(Line::from(span(
            "st names no agent, mission or step for this item.",
            theme::dim(),
        )));
    }
    for (target, state) in &item.related {
        let prefix = format!("{:<10}", target.split('/').next().unwrap_or("item"));
        if target.starts_with("agent/")
            || target.starts_with("mission/")
            || target.starts_with("attention/")
        {
            link(&mut card, &prefix, target, target);
        } else {
            card.line(Line::from(vec![
                span(prefix, theme::dim()),
                span(target.clone(), theme::soft()),
            ]));
        }
        if let Some(state) = state {
            card.line(Line::from(span(format!("{:<10}{state}", ""), theme::dim())));
        }
    }
    card.blank();
    // Every item: talk to whoever can act on it, or go to what it is about.
    if let Some(chat) = &drafts.chat {
        card.section(&format!("chat with {} about this", chat.to), None, inner);
        if chat.thread.is_empty() {
            card.wrap(
                &[run(
                    format!(
                        "{} gets this item as context: its title, mission and question.",
                        chat.to
                    ),
                    theme::dim(),
                )],
                inner,
            );
        } else {
            card.lines(chat.thread.iter().cloned());
        }
        card.blank();
        let box_drafts = Drafts {
            text: Some(chat.text),
            editing: chat.editing,
            confirm: None,
            chat: None,
        };
        text_box(
            &mut card,
            &format!("message {}", chat.to),
            &box_drafts,
            "",
            inner,
        );
        card.buttons(&[
            ("enter", "Send", Hit::Enter, theme::ACCENT),
            ("esc", "Close chat", Hit::Escape, theme::OVERLAY1),
        ]);
    } else if !drafts.editing && drafts.confirm.is_none() {
        let mut buttons = vec![("t", "Chat about this", Hit::Key('t'), theme::SAPPHIRE)];
        if item.mission.is_some() || item.agent.is_some() {
            buttons.push((
                "g",
                if item.mission.is_some() {
                    "Go to the mission"
                } else {
                    "Go to the agent"
                },
                Hit::Key('g'),
                theme::OVERLAY1,
            ));
        }
        card.buttons(&buttons);
    }
    card.blank();
    let title = format!("{} · waiting {}", item.kind.word(), item.age);
    doc.card(
        &title,
        if heavy { theme::PERSON } else { color },
        heavy,
        card,
        width,
    );
    doc
}

/// Steps drawn as a flow: each layer after the one it depends on.
fn flow<'a>(
    steps: impl Iterator<Item = (&'a str, &'a [String], Color)>,
    width: usize,
) -> Vec<Line<'static>> {
    let steps = steps.collect::<Vec<_>>();
    let mut depth = vec![0usize; steps.len()];
    for _ in 0..steps.len() {
        for (index, (_, after, _)) in steps.iter().enumerate() {
            for dependency in after.iter() {
                if let Some(position) = steps.iter().position(|(name, _, _)| name == dependency) {
                    depth[index] = depth[index].max(depth[position] + 1);
                }
            }
        }
    }
    let layers = depth.iter().max().map(|max| max + 1).unwrap_or(0);
    let mut runs = vec![];
    for layer in 0..layers {
        if layer > 0 {
            runs.push(run(" → ", theme::fg(theme::SURFACE2)));
        }
        let names = steps
            .iter()
            .enumerate()
            .filter(|(index, _)| depth[*index] == layer)
            .map(|(_, step)| *step)
            .collect::<Vec<_>>();
        // Steps that can run side by side read as a list: `a, b → c`.
        for (index, (name, _, color)) in names.iter().enumerate() {
            if index > 0 {
                runs.push(run(", ", theme::dim()));
            }
            runs.push(run(name.to_string(), theme::fg(*color)));
        }
    }
    text::wrap(
        &runs,
        width,
        &[run("  ", theme::dim())],
        &[run("  ", theme::dim())],
        None,
    )
}

// ------------------------------------------------------------------- agents

pub fn agent_order(world: &World) -> Vec<&Agent> {
    let mut agents = world.agents.items().iter().collect::<Vec<_>>();
    agents.sort_by(|a, b| {
        (a.unmanaged, a.state, a.name.to_lowercase()).cmp(&(
            b.unmanaged,
            b.state,
            b.name.to_lowercase(),
        ))
    });
    agents
}

fn agent_group(agent: &Agent) -> &'static str {
    if agent.unmanaged {
        return "found running · not started by st";
    }
    match agent.state {
        AgentState::NeedsYou => "waiting on you",
        AgentState::Fault => "broken",
        AgentState::Working => "working",
        AgentState::Idle | AgentState::Starting => "idle",
        AgentState::Stopped | AgentState::Unknown => "stopped",
    }
}

pub fn agents_list(world: &World, spinner: &'static str, width: usize) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let agents = agent_order(world);
    let mut current = "";
    for agent in &agents {
        let group = agent_group(agent);
        if group != current {
            current = group;
            let count = agents
                .iter()
                .filter(|other| agent_group(other) == group)
                .count();
            items.push(Item::Header {
                title: group.into(),
                count,
                color: if agent.state == AgentState::NeedsYou && !agent.unmanaged {
                    theme::PERSON
                } else {
                    theme::OVERLAY1
                },
            });
        }
        let (glyph, color) = agent_glyph(agent.state, spinner);
        let path = agent.id.strip_prefix("agent/").unwrap_or(&agent.id);
        items.push(Item::Row {
            index: ids.len(),
            first: vec![
                span(format!(" {glyph} "), theme::strong(color)),
                span(agent.name.clone(), theme::bold()),
            ],
            right: vec![
                span(
                    agent.harness.name(),
                    theme::fg(harness_color(agent.harness)),
                ),
                span(format!(" {:>4}", agent.activity), theme::dim()),
            ],
            second: vec![span(
                format!("   {}", text::truncate(path, width.saturating_sub(4))),
                theme::dim(),
            )],
        });
        ids.push(agent.id.clone());
    }
    Listing {
        items,
        ids,
        state: state_of(&world.agents, "Loading agents…", "No agents yet."),
        legend: legend(&[
            ("◆", theme::PERSON, "needs you"),
            (spinner, theme::WORKING, "working"),
            ("●", theme::IDLE, "idle"),
            ("✕", theme::FAULT, "broken"),
            ("○", theme::QUIET, "stopped"),
            ("?", theme::QUIET, "unmanaged"),
        ]),
    }
}

/// The header strip above an agent's conversation.
pub fn agent_header(world: &World, agent: &Agent, width: usize, spinner: &'static str) -> Doc {
    let mut doc = Doc::new();
    let (glyph, color) = agent_glyph(agent.state, spinner);
    let mut first = vec![
        span(format!(" {glyph} "), theme::strong(color)),
        span(agent.name.clone(), theme::bold()),
        span(format!("  {}", agent_word(agent.state)), theme::fg(color)),
    ];
    let right = format!("{} · {} ", agent.harness.name(), agent.host);
    let used = first.iter().map(Span::width).sum::<usize>();
    first.push(span(
        " ".repeat(width.saturating_sub(used + text::width(&right))),
        theme::dim(),
    ));
    first.push(span(
        agent.harness.name(),
        theme::fg(harness_color(agent.harness)),
    ));
    first.push(span(format!(" · {} ", agent.host), theme::dim()));
    doc.line(Line::from(first));
    let mut second = vec![span(format!("   {}", agent.id), theme::dim())];
    if let Some(tree) = &agent.worktree {
        second.push(span(format!("  ·  {tree}"), theme::dim()));
    }
    doc.line(Line::from(second));
    if let (Some(mission), Some(step)) = (&agent.mission, &agent.step) {
        let title = world
            .missions
            .items()
            .iter()
            .find(|candidate| &candidate.id == mission)
            .map(|candidate| candidate.title.clone())
            .unwrap_or_else(|| mission.trim_start_matches("mission/").to_owned());
        link(
            &mut doc,
            "   mission ",
            &format!("{title} › {step}"),
            mission,
        );
    }
    doc
}

// ----------------------------------------------------------------- missions

pub fn mission_order(world: &World, system: bool) -> Vec<&Mission> {
    let mut missions = world
        .missions
        .items()
        .iter()
        .filter(|mission| system || !hidden_by_default(mission))
        .collect::<Vec<_>>();
    missions.sort_by_key(|mission| (mission.word, mission.title.to_lowercase()));
    missions
}

/// Missions a person rarely looks for: st's own, and ones nobody has started. `x` shows them.
fn hidden_by_default(mission: &Mission) -> bool {
    mission.system || mission.word == Word::NotStarted
}

pub fn missions_list(world: &World, spinner: &'static str, system: bool) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let missions = mission_order(world, system);
    let mut current = None;
    for mission in &missions {
        if current != Some(mission.word) {
            current = Some(mission.word);
            let (_, color) = word_style(mission.word, spinner);
            items.push(Item::Header {
                title: mission.word.name().into(),
                count: missions
                    .iter()
                    .filter(|other| other.word == mission.word)
                    .count(),
                color,
            });
        }
        let (glyph, color) = word_style(mission.word, spinner);
        let (done, total) = mission.progress();
        // st reports only current steps for many runs, so a 0/1 meter would claim too much.
        let right = if mission.word == Word::NotStarted {
            vec![]
        } else if done > 0 {
            let mut right = meter(done, total, 5, color);
            right.push(span(format!(" {done}/{total}"), theme::dim()));
            right
        } else {
            vec![span(
                format!("{total} step{}", if total == 1 { "" } else { "s" }),
                theme::dim(),
            )]
        };
        items.push(Item::Row {
            index: ids.len(),
            first: vec![
                span(format!(" {glyph} "), theme::strong(color)),
                span(mission.title.clone(), theme::bold()),
            ],
            right,
            second: vec![span(
                format!(
                    "   {} · {}",
                    mission.id.trim_start_matches("mission/"),
                    mission.age
                ),
                theme::dim(),
            )],
        });
        ids.push(mission.id.clone());
    }
    if !system {
        let count = |hidden: fn(&Mission) -> bool| {
            world
                .missions
                .items()
                .iter()
                .filter(|mission| hidden(mission))
                .count()
        };
        let kinds = [
            (count(|mission| mission.system), "from st"),
            (
                count(|mission| !mission.system && mission.word == Word::NotStarted),
                "not started",
            ),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, kind)| format!("{count} {kind}"))
        .collect::<Vec<_>>();
        if !kinds.is_empty() {
            items.push(Item::Note(Line::from(span(
                format!(" hidden: {} · x shows them", kinds.join(", ")),
                theme::dim(),
            ))));
        }
    }
    Listing {
        items,
        ids,
        state: state_of(&world.missions, "Loading missions…", "No missions yet."),
        legend: legend(&[
            ("◆", theme::PERSON, "needs you"),
            ("▲", theme::FAULT, "stalled"),
            ("◇", theme::WAITING, "unstaffed"),
            ("◌", theme::OVERLAY1, "queued"),
            ("◉", theme::SAPPHIRE, "watching"),
            (spinner, theme::WORKING, "working"),
            ("●", theme::IDLE, "idle"),
            ("✓", theme::DONE, "done"),
        ]),
    }
}

pub fn mission_detail(
    world: &World,
    id: Option<&str>,
    width: usize,
    spinner: &'static str,
    decision_card: Option<Doc>,
    expanded: &std::collections::HashSet<String>,
) -> Doc {
    let mut doc = Doc::new();
    let Some(mission) = id.and_then(|id| {
        world
            .missions
            .items()
            .iter()
            .find(|mission| mission.id == id)
    }) else {
        doc.line(Line::from(span(
            match world.missions {
                Load::Loading => "Loading missions…",
                _ => "Select a mission.",
            },
            theme::dim(),
        )));
        return doc;
    };
    let (glyph, color) = word_style(mission.word, spinner);
    let inner = width.saturating_sub(4);
    doc.line(Line::from(vec![
        span(
            format!(" {glyph} {} ", mission.word.name()),
            Style::default()
                .fg(theme::CRUST)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        span(format!("  {}", mission.title), theme::bold()),
        span(
            if mission.host.is_empty() {
                format!("   {}", mission.age)
            } else {
                format!("   on {} · {}", mission.host, mission.age)
            },
            theme::dim(),
        ),
    ]));
    doc.line(Line::from(vec![
        span(format!(" {}", mission.id), theme::dim()),
        span(format!("  ·  {}", mission.word.explain()), theme::fg(color)),
    ]));
    if let Some(outcome) = &mission.outcome {
        doc.lines(text::wrap(
            &text::inline(outcome, theme::text()),
            inner,
            &[run(" outcome ", theme::dim())],
            &[run("         ", theme::dim())],
            None,
        ));
    }
    doc.blank();
    let mut goals = Doc::new();
    for goal in &mission.goals {
        goals.lines(text::wrap(
            &text::inline(goal, theme::text()),
            inner,
            &[run("◆ ", theme::fg(theme::LAVENDER))],
            &[run("  ", theme::dim())],
            None,
        ));
    }
    if !goals.lines.is_empty() {
        doc.card("goals", theme::LAVENDER, false, goals, width);
    }
    if let Some(card) = decision_card {
        // The same card Home shows, so the answer can be given right here.
        doc.append(card, 0);
    }
    what_you_can_do(&mut doc, world, mission, width, spinner);
    let (done, total) = mission.progress();
    let mut steps = Doc::new();
    let mut bar = meter(done, total, 20, theme::DONE);
    bar.push(span(format!("  {done}/{total} done"), theme::dim()));
    steps.line(Line::from(bar));
    // The order only says something when a step waits for another.
    if mission.steps.iter().any(|step| !step.after.is_empty()) {
        steps.lines(flow(
            mission.steps.iter().map(|step| {
                (
                    step.name.as_str(),
                    step.after.as_slice(),
                    step_style(step.state, spinner).1,
                )
            }),
            inner,
        ));
    }
    let name_width = mission
        .steps
        .iter()
        .map(|step| text::width(&step.name))
        .max()
        .unwrap_or(8)
        .clamp(8, inner / 3);
    steps.blank();
    for step in &mission.steps {
        let (glyph, color, word) = step_style(step.state, spinner);
        let key = format!("step:{}:{}", mission.id, step.name);
        let open = expanded.contains(&key);
        steps.targets.push(super::doc::Target {
            line: steps.lines.len(),
            column: 0,
            width: inner as u16,
            hit: Hit::ToggleTool(key),
        });
        steps.line(Line::from(vec![
            span(if open { "▾ " } else { "▸ " }, theme::dim()),
            span(format!("{glyph} "), theme::strong(color)),
            span(
                format!(
                    "{:<w$} ",
                    text::truncate(&step.name, name_width),
                    w = name_width
                ),
                theme::text(),
            ),
            span(format!("{:<11}", word), theme::fg(color)),
            span(
                format!("{:<18}", step.owner.as_deref().unwrap_or("nobody")),
                if step.owner.is_some() {
                    theme::soft()
                } else {
                    theme::fg(theme::WAITING)
                },
            ),
            span(step.age.clone(), theme::dim()),
        ]));
        if let Some(note) = &step.note {
            steps.lines(text::wrap(
                &text::inline(note, theme::dim()),
                inner,
                &[run("      ", theme::dim())],
                &[run("      ", theme::dim())],
                None,
            ));
        }
        if open {
            let pad = "      ";
            let mut field = |label: &str, values: &[String], style: Style| {
                for (index, value) in values.iter().enumerate() {
                    let head = if index == 0 {
                        format!("{pad}{label:<12}")
                    } else {
                        format!("{pad}{:<12}", "")
                    };
                    steps.lines(text::wrap(
                        &text::inline(value, style),
                        inner,
                        &[run(head, theme::dim())],
                        &[run(format!("{pad}{:<12}", ""), theme::dim())],
                        None,
                    ));
                }
            };
            field("goal", &step.goals, theme::text());
            field("constraint", &step.constraints, theme::soft());
            field("gate", &step.gates, theme::fg(theme::PERSON));
            field("blocked by", &step.blockers, theme::fg(theme::RED));
            if !step.after.is_empty() {
                field("after", &[step.after.join(", ")], theme::soft());
            }
            field("attempt", &[step.attempt.to_string()], theme::soft());
            steps.blank();
        }
    }
    steps.line(Line::from(span("click a step to open it", theme::dim())));
    doc.card("steps", theme::OVERLAY1, false, steps, width);
    let mut agents = Doc::new();
    for id in &mission.agents {
        if let Some(agent) = world.agents.items().iter().find(|agent| &agent.id == id) {
            let (glyph, color) = agent_glyph(agent.state, spinner);
            let label = format!("{glyph} {:<18} ", text::truncate(&agent.name, 18));
            agents.targets.push(super::doc::Target {
                line: agents.lines.len(),
                column: 0,
                width: text::width(&label) as u16,
                hit: Hit::Peek(agent.id.clone()),
            });
            agents.line(Line::from(vec![
                span(label, theme::fg(color)),
                span(format!("{:<11}", agent_word(agent.state)), theme::fg(color)),
                span(
                    format!("{:<8}", agent.harness.name()),
                    theme::fg(harness_color(agent.harness)),
                ),
                span(
                    format!("on {} · {}", agent.host, agent.activity),
                    theme::dim(),
                ),
            ]));
        }
    }
    if agents.lines.is_empty() {
        agents.line(Line::from(span("No agents right now.", theme::dim())));
    }
    doc.card(
        &format!("agents · {}", mission.agents.len()),
        theme::OVERLAY1,
        false,
        agents,
        width,
    );
    if let Some(tree) = &mission.worktree {
        let mut place = Doc::new();
        place.line(Line::from(vec![
            span(tree.clone(), theme::text()),
            span(format!("  on {}", mission.host), theme::dim()),
        ]));
        doc.card("worktree", theme::OVERLAY1, false, place, width);
    }
    doc.buttons(&[("k", "The whole declaration", Hit::Key('k'), theme::OVERLAY1)]);
    doc
}

/// The mission as written. st does not send the declaration to clients yet.
pub fn mission_kdl(world: &World, id: Option<&str>, width: usize) -> Doc {
    let mut doc = Doc::new();
    let mission = id.and_then(|id| {
        world
            .missions
            .items()
            .iter()
            .find(|mission| mission.id == id)
    });
    doc.line(Line::from(vec![
        span(
            " declaration ",
            Style::default()
                .fg(theme::CRUST)
                .bg(theme::OVERLAY1)
                .add_modifier(Modifier::BOLD),
        ),
        span(
            format!(
                "  {}",
                mission.map(|mission| mission.id.as_str()).unwrap_or("")
            ),
            theme::dim(),
        ),
    ]));
    doc.buttons(&[("k", "Back to the mission", Hit::Key('k'), theme::ACCENT)]);
    doc.blank();
    match mission.and_then(|mission| mission.kdl.as_deref()) {
        Some(kdl) => {
            for line in text::sanitize(kdl).lines() {
                let trimmed = line.trim_start();
                let style = if trimmed.starts_with("//") {
                    theme::dim()
                } else if trimmed.starts_with("goal") || trimmed.starts_with("constraint") {
                    theme::fg(theme::LAVENDER)
                } else if trimmed.starts_with("gate") || trimmed.starts_with("reviewer") || trimmed.starts_with("question") {
                    theme::fg(theme::PERSON)
                } else if trimmed.starts_with("step") || trimmed.starts_with("mission") {
                    theme::strong(theme::PEACH)
                } else {
                    theme::fg(theme::SUBTEXT1)
                };
                doc.lines(text::wrap(&[run(line.to_owned(), style)], width, &[], &[run("    ", theme::dim())], None));
            }
        }
        None => doc.wrap(
            &[run(
                "st does not send mission declarations to clients yet. Until it does, read it with: st missions show",
                theme::dim(),
            )],
            width,
        ),
    }
    doc
}

/// For a mission that is not moving: why, and what a person can do about it.
fn what_you_can_do(
    doc: &mut Doc,
    world: &World,
    mission: &Mission,
    width: usize,
    spinner: &'static str,
) {
    let inner = width.saturating_sub(4);
    let mut card = Doc::new();
    let broken = mission
        .agents
        .iter()
        .filter_map(|id| world.agents.items().iter().find(|agent| &agent.id == id))
        .find(|agent| matches!(agent.state, AgentState::Fault | AgentState::Stopped));
    let stuck = mission.steps.iter().find(|step| {
        matches!(
            step.state,
            StepState::Failed | StepState::Ready | StepState::Waiting
        )
    });
    let (title, color) = match mission.word {
        Word::Stalled | Word::Failed => ("what you can do", theme::FAULT),
        Word::Unstaffed => ("what you can do", theme::WAITING),
        Word::Queued => ("nothing for you to do", theme::OVERLAY1),
        _ => return,
    };
    card.blank();
    if mission.word == Word::Queued {
        let note = stuck
            .and_then(|step| step.note.clone())
            .unwrap_or_else(|| "It is waiting its turn.".into());
        card.wrap(&text::inline(&note, theme::text()), inner);
        card.blank();
        card.wrap(
            &[run(
                "It starts by itself when the agent is free.",
                theme::dim(),
            )],
            inner,
        );
        card.blank();
        doc.card(title, color, false, card, width);
        return;
    }
    if let Some(step) = stuck {
        let (glyph, step_color, word) = step_style(step.state, spinner);
        card.line(Line::from(vec![
            span(format!("{glyph} {} ", step.name), theme::strong(step_color)),
            span(
                match step.state {
                    StepState::Failed => "failed".to_owned(),
                    _ => format!("is {word}"),
                },
                theme::fg(step_color),
            ),
            span(
                if step.attempt > 1 {
                    format!(" after {} attempts", step.attempt)
                } else {
                    String::new()
                },
                theme::dim(),
            ),
        ]));
        for blocker in &step.blockers {
            card.wrap(&[run(format!("because {blocker}"), theme::soft())], inner);
        }
        card.blank();
    }
    if let Some(agent) = broken {
        card.wrap(
            &[run(
                format!(
                    "{} is {}. Fix the agent and the step can run again.",
                    agent.name,
                    screens_word(agent.state)
                ),
                theme::text(),
            )],
            inner,
        );
        card.blank();
        card.buttons(&[
            ("R", "Restart the agent", Hit::Key('R'), theme::GREEN),
            (
                "t",
                "Chat with it",
                Hit::Peek(agent.id.clone()),
                theme::SAPPHIRE,
            ),
        ]);
    }
    card.buttons(&[
        ("r", "Retry the step", Hit::Key('r'), theme::YELLOW),
        ("X", "Cancel this run", Hit::Key('X'), theme::RED),
    ]);
    card.blank();
    doc.card(title, color, true, card, width);
}

fn screens_word(state: AgentState) -> &'static str {
    agent_word(state)
}

// -------------------------------------------------------------------- fleet

pub fn fleet_list(world: &World) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    for machine in world.machines.items() {
        let (glyph, color) = reach_style(machine.reach);
        let agents = world
            .agents
            .items()
            .iter()
            .filter(|agent| agent.host == machine.name)
            .count();
        items.push(Item::Row {
            index: ids.len(),
            first: vec![
                span(format!(" {glyph} "), theme::strong(color)),
                span(machine.name.clone(), theme::bold()),
                span(
                    if machine.you_are_here {
                        "  you are here".to_owned()
                    } else {
                        String::new()
                    },
                    theme::fg(theme::ACCENT),
                ),
            ],
            right: vec![span(
                format!("{agents} agent{}", if agents == 1 { "" } else { "s" }),
                theme::dim(),
            )],
            second: vec![span(format!("   {}", machine_line(machine)), theme::dim())],
        });
        ids.push(machine.name.clone());
    }
    Listing {
        items,
        ids,
        state: state_of(&world.machines, "Loading machines…", "No machines yet."),
        legend: legend(&[
            ("●", theme::GREEN, "connected"),
            ("◐", theme::SAPPHIRE, "through another"),
            ("○", theme::RED, "offline"),
        ]),
    }
}

fn reach_style(reach: Reach) -> (&'static str, Color) {
    match reach {
        Reach::Here | Reach::Direct => ("●", theme::GREEN),
        Reach::Indirect => ("◐", theme::SAPPHIRE),
        Reach::Offline => ("○", theme::RED),
        Reach::Unknown => ("◌", theme::OVERLAY1),
    }
}

/// How a machine is reached and when it was last heard from, in words.
fn machine_line(machine: &Machine) -> String {
    let mut parts = vec![machine.reach.word().to_owned()];
    if !machine.platform.is_empty() {
        parts.push(machine.platform.clone());
    }
    match (machine.reach, machine.seen.as_str()) {
        (Reach::Here, _) => {}
        (_, "never") => parts.push("never heard from".into()),
        (_, seen) => parts.push(format!("heard {seen}")),
    }
    parts.join(" · ")
}

pub fn fleet_detail(world: &World, id: Option<&str>, width: usize, spinner: &'static str) -> Doc {
    let mut doc = Doc::new();
    let Some(machine) = id.and_then(|id| {
        world
            .machines
            .items()
            .iter()
            .find(|machine| machine.name == id)
    }) else {
        doc.line(Line::from(span("Select a machine.", theme::dim())));
        return doc;
    };
    let (glyph, color) = reach_style(machine.reach);
    doc.line(Line::from(vec![
        span(format!(" {glyph} "), theme::strong(color)),
        span(machine.name.clone(), theme::bold()),
        span(format!("  {}", machine_line(machine)), theme::dim()),
    ]));
    if let Some(load) = &machine.load {
        doc.line(Line::from(span(format!("   {load}"), theme::soft())));
    }
    doc.blank();
    let mut links = Doc::new();
    for (peer, up, detail) in &machine.links {
        links.line(Line::from(vec![
            span(
                if *up { "● " } else { "○ " },
                theme::fg(if *up { theme::GREEN } else { theme::RED }),
            ),
            span(format!("{peer:<12}"), theme::text()),
            span(detail.clone(), theme::dim()),
        ]));
    }
    if links.lines.is_empty() {
        links.line(Line::from(span(
            if machine.you_are_here {
                "This is the machine stui is talking to."
            } else {
                "st reports no link to this machine."
            },
            theme::dim(),
        )));
    }
    doc.card("reaches", theme::OVERLAY1, false, links, width);
    let mut agents = Doc::new();
    for agent in agent_order(world)
        .into_iter()
        .filter(|agent| agent.host == machine.name)
    {
        let (glyph, color) = agent_glyph(agent.state, spinner);
        let label = format!("{glyph} {:<20} ", text::truncate(&agent.name, 20));
        agents.targets.push(super::doc::Target {
            line: agents.lines.len(),
            column: 0,
            width: text::width(&label) as u16,
            hit: Hit::Peek(agent.id.clone()),
        });
        agents.line(Line::from(vec![
            span(label, theme::fg(color)),
            span(
                format!("{:<8}", agent.harness.name()),
                theme::fg(harness_color(agent.harness)),
            ),
            span(agent.worktree.clone().unwrap_or_default(), theme::dim()),
        ]));
    }
    if agents.lines.is_empty() {
        agents.line(Line::from(span("No agents on this machine.", theme::dim())));
    }
    doc.card("agents here", theme::OVERLAY1, false, agents, width);
    if machine.you_are_here {
        doc.append(devices_card(world, width), 0);
    }
    doc
}

// ---------------------------------------------------------------- worktrees

pub fn worktrees_list(world: &World) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    items.push(Item::Note(Line::from(vec![
        span(
            " DEMO ",
            Style::default()
                .fg(theme::CRUST)
                .bg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
        span(" invented worktrees", theme::fg(theme::YELLOW)),
    ])));
    let mut trees = world.worktrees.items().iter().collect::<Vec<_>>();
    trees.sort_by(|a, b| (&a.host, &a.path).cmp(&(&b.host, &b.path)));
    let mut current = "";
    for tree in trees {
        if tree.host != current {
            current = &tree.host;
            items.push(Item::Header {
                title: format!("on {}", tree.host),
                count: world
                    .worktrees
                    .items()
                    .iter()
                    .filter(|other| other.host == tree.host)
                    .count(),
                color: theme::OVERLAY1,
            });
        }
        let mut right = Vec::new();
        if tree.ahead > 0 {
            right.push(span(format!("↑{} ", tree.ahead), theme::fg(theme::GREEN)));
        }
        if tree.behind > 0 {
            right.push(span(format!("↓{} ", tree.behind), theme::fg(theme::RED)));
        }
        if tree.dirty > 0 {
            right.push(span(format!("±{}", tree.dirty), theme::fg(theme::YELLOW)));
        }
        items.push(Item::Row {
            index: ids.len(),
            first: vec![
                span("  ", theme::dim()),
                span(tree.branch.clone(), theme::strong(theme::LAVENDER)),
            ],
            right,
            second: vec![span(
                format!("  {} · {} agents", tree.path, tree.agents.len()),
                theme::dim(),
            )],
        });
        ids.push(format!("{}:{}", tree.host, tree.path));
    }
    Listing {
        items,
        ids,
        state: state_of(
            &world.worktrees,
            "Loading worktrees…",
            "No worktrees known.",
        ),
        legend: legend(&[
            ("↑", theme::GREEN, "ahead"),
            ("↓", theme::RED, "behind"),
            ("±", theme::YELLOW, "changed"),
        ]),
    }
}

pub fn worktree_detail(
    world: &World,
    id: Option<&str>,
    width: usize,
    spinner: &'static str,
) -> Doc {
    let mut doc = Doc::new();
    demo_banner(&mut doc, width);
    let Some(tree) = id.and_then(|id| {
        world
            .worktrees
            .items()
            .iter()
            .find(|tree| format!("{}:{}", tree.host, tree.path) == id)
    }) else {
        doc.line(Line::from(span("Select a worktree.", theme::dim())));
        return doc;
    };
    doc.line(Line::from(vec![
        span(format!(" {}", tree.branch), theme::strong(theme::LAVENDER)),
        span(format!("  {} on {}", tree.path, tree.host), theme::dim()),
    ]));
    doc.line(Line::from(span(
        format!(
            "   {} ahead · {} behind · {} changed files",
            tree.ahead, tree.behind, tree.dirty
        ),
        theme::soft(),
    )));
    doc.blank();
    let mut agents = Doc::new();
    for id in &tree.agents {
        if let Some(agent) = world.agents.items().iter().find(|agent| &agent.id == id) {
            let (glyph, color) = agent_glyph(agent.state, spinner);
            let label = format!("{glyph} {:<20} ", text::truncate(&agent.name, 20));
            agents.targets.push(super::doc::Target {
                line: agents.lines.len(),
                column: 0,
                width: text::width(&label) as u16,
                hit: Hit::Peek(agent.id.clone()),
            });
            agents.line(Line::from(vec![
                span(label, theme::fg(color)),
                span(agent_word(agent.state), theme::fg(color)),
            ]));
        }
    }
    if agents.lines.is_empty() {
        agents.line(Line::from(span("Nobody is working here.", theme::dim())));
    }
    doc.card("agents here", theme::OVERLAY1, false, agents, width);
    let mut missions = Doc::new();
    for id in &tree.missions {
        if let Some(mission) = world
            .missions
            .items()
            .iter()
            .find(|mission| &mission.id == id)
        {
            let (glyph, color) = word_style(mission.word, spinner);
            let label = format!("{glyph} {:<26}", mission.title);
            missions.targets.push(super::doc::Target {
                line: missions.lines.len(),
                column: 0,
                width: text::width(&label) as u16,
                hit: Hit::Peek(mission.id.clone()),
            });
            missions.line(Line::from(vec![
                span(label, theme::fg(color)),
                span(mission.word.name(), theme::fg(color)),
            ]));
        }
    }
    if missions.lines.is_empty() {
        missions.line(Line::from(span(
            "No missions use this worktree.",
            theme::dim(),
        )));
    }
    doc.card("missions here", theme::OVERLAY1, false, missions, width);
    doc
}

/// Every worktree screen starts with this: st does not track worktrees yet.
fn demo_banner(doc: &mut Doc, width: usize) {
    doc.lines(text::wrap(
        &[run(
            "st does not track worktrees yet. Everything on this tab is invented.",
            theme::fg(theme::YELLOW),
        )],
        width,
        &[
            run(
                " DEMO DATA ",
                Style::default()
                    .fg(theme::CRUST)
                    .bg(theme::YELLOW)
                    .add_modifier(Modifier::BOLD),
            ),
            run(" ", theme::dim()),
        ],
        &[run("            ", theme::dim())],
        None,
    ));
    doc.blank();
}

// ------------------------------------------------------------------ popovers

/// The small card a clicked reference opens: enough to recognise the subject, and a way
/// to go to it.
pub fn peek(world: &World, subject: &str, width: usize, spinner: &'static str) -> Doc {
    let mut doc = Doc::new();
    doc.blank();
    if let Some(agent) = world
        .agents
        .items()
        .iter()
        .find(|agent| agent.id == subject)
    {
        let (glyph, color) = agent_glyph(agent.state, spinner);
        doc.line(Line::from(vec![
            span(format!("{glyph} "), theme::strong(color)),
            span(agent.name.clone(), theme::bold()),
            span(format!("  {}", agent_word(agent.state)), theme::fg(color)),
        ]));
        doc.line(Line::from(span(agent.id.clone(), theme::dim())));
        doc.blank();
        doc.field(
            "harness",
            agent.harness.name(),
            width,
            theme::fg(harness_color(agent.harness)),
        );
        doc.field("host", &agent.host, width, theme::soft());
        if let Some(tree) = &agent.worktree {
            doc.field("worktree", tree, width, theme::soft());
        }
        if let (Some(mission), Some(step)) = (&agent.mission, &agent.step) {
            let title = world
                .missions
                .items()
                .iter()
                .find(|candidate| &candidate.id == mission)
                .map(|candidate| candidate.title.clone())
                .unwrap_or_else(|| mission.trim_start_matches("mission/").to_owned());
            doc.field("doing", &format!("{title} › {step}"), width, theme::text());
        }
        doc.field(
            "last seen",
            &format!("{} ago", agent.activity),
            width,
            theme::soft(),
        );
        doc.blank();
        if agent.unmanaged {
            doc.buttons(&[("g", "Go to agent", Hit::Key('g'), theme::ACCENT)]);
        } else {
            doc.buttons(&[
                ("g", "Go to agent", Hit::Key('g'), theme::ACCENT),
                ("t", "Message", Hit::Key('t'), theme::SAPPHIRE),
            ]);
        }
    } else if let Some(mission) = world
        .missions
        .items()
        .iter()
        .find(|mission| mission.id == subject)
    {
        let (glyph, color) = word_style(mission.word, spinner);
        doc.line(Line::from(vec![
            span(format!("{glyph} "), theme::strong(color)),
            span(mission.title.clone(), theme::bold()),
            span(format!("  {}", mission.word.name()), theme::fg(color)),
        ]));
        doc.line(Line::from(span(mission.id.clone(), theme::dim())));
        doc.line(Line::from(span(mission.word.explain(), theme::fg(color))));
        doc.blank();
        let (done, total) = mission.progress();
        doc.field(
            "steps",
            &format!("{done} of {total} done"),
            width,
            theme::soft(),
        );
        for step in mission
            .steps
            .iter()
            .filter(|step| {
                !matches!(
                    step.state,
                    StepState::Done | StepState::Cancelled | StepState::Pending
                )
            })
            .take(3)
        {
            let (glyph, color, word) = step_style(step.state, spinner);
            doc.line(Line::from(vec![
                span(format!("          {glyph} "), theme::fg(color)),
                span(format!("{} ", step.name), theme::text()),
                span(word, theme::fg(color)),
                span(
                    format!("  {}", step.owner.as_deref().unwrap_or("nobody")),
                    theme::dim(),
                ),
            ]));
        }
        let names = mission
            .agents
            .iter()
            .filter_map(|id| world.agents.items().iter().find(|agent| &agent.id == id))
            .map(|agent| agent.name.clone())
            .collect::<Vec<_>>();
        if !names.is_empty() {
            doc.field("agents", &names.join(", "), width, theme::soft());
        }
        doc.blank();
        doc.buttons(&[("g", "Go to mission", Hit::Key('g'), theme::ACCENT)]);
    } else if let Some(item) = world
        .attention
        .items()
        .iter()
        .find(|item| item.id == subject)
    {
        let (glyph, color) = attention_style(&item.kind);
        doc.line(Line::from(vec![
            span(format!("{glyph} {} ", item.kind.word()), theme::fg(color)),
            span(item.title.clone(), theme::bold()),
        ]));
        doc.line(Line::from(span(
            format!("waiting {}", item.age),
            theme::dim(),
        )));
        doc.blank();
        doc.buttons(&[("g", "Open on Home", Hit::Key('g'), theme::PERSON)]);
    } else {
        doc.wrap(
            &[run(
                format!("{subject} is not in the current view."),
                theme::dim(),
            )],
            width,
        );
    }
    doc.blank();
    doc
}

// -------------------------------------------------------------- agent details

/// The pane beside a conversation: what the agent holds, what is next, and how it runs.
pub fn agent_details(world: &World, agent: &Agent, width: usize, spinner: &'static str) -> Doc {
    let mut doc = Doc::new();
    let details = &agent.details;
    let unknown = || span("st has not said", theme::dim());
    // A close control on the pane itself, not only on the header rule.
    let close = "× close";
    doc.targets.push(super::doc::Target {
        line: 0,
        column: width.saturating_sub(text::width(close)) as u16,
        width: text::width(close) as u16,
        hit: Hit::Key('i'),
    });
    doc.line(Line::from(vec![
        span("details", theme::label()),
        span(
            " ".repeat(width.saturating_sub(7 + text::width(close))),
            theme::dim(),
        ),
        span(close, theme::fg(theme::OVERLAY1)),
    ]));
    if let Some(fault) = &details.fault {
        let mut inner = Doc::new();
        inner.wrap(&text::inline(fault, theme::text()), width.saturating_sub(4));
        doc.card("broken", theme::FAULT, true, inner, width);
        doc.blank();
    }
    doc.section("now", None, width);
    match (&agent.mission, &agent.step) {
        (Some(mission), Some(step)) => {
            let title = world
                .missions
                .items()
                .iter()
                .find(|candidate| &candidate.id == mission)
                .map(|candidate| candidate.title.clone())
                .unwrap_or_else(|| mission.trim_start_matches("mission/").to_owned());
            link(&mut doc, "", &format!("{title} › {step}"), mission);
            if let Some(goal) = &details.goal {
                doc.lines(text::wrap(
                    &text::inline(goal, theme::soft()),
                    width,
                    &[],
                    &[],
                    None,
                ));
            }
            if let Some(claimed) = &details.claimed {
                doc.line(Line::from(span(
                    format!("held since {claimed}"),
                    theme::dim(),
                )));
            }
        }
        _ => doc.line(Line::from(span("No step right now.", theme::dim()))),
    }
    doc.blank();
    doc.section("next", Some(details.queued as usize), width);
    match &details.next {
        Some(next) => doc.lines(text::wrap(
            &[run(next.clone(), theme::text())],
            width,
            &[run("› ", theme::fg(theme::ACCENT))],
            &[run("  ", theme::dim())],
            None,
        )),
        None => doc.line(Line::from(span("Nothing queued.", theme::dim()))),
    }
    for item in details
        .queue
        .iter()
        .filter(|item| Some(*item) != details.next.as_ref())
    {
        doc.lines(text::wrap(
            &[run(item.clone(), theme::soft())],
            width,
            &[run("· ", theme::dim())],
            &[run("  ", theme::dim())],
            None,
        ));
    }
    doc.blank();
    doc.section("runs as", None, width);
    let (glyph, color) = agent_glyph(agent.state, spinner);
    doc.line(Line::from(vec![
        span(format!("{glyph} "), theme::fg(color)),
        span(agent_word(agent.state), theme::fg(color)),
    ]));
    let field = |doc: &mut Doc, label: &str, value: Option<&str>| {
        let mut spans = vec![span(format!("{label:<9}"), theme::dim())];
        match value {
            Some(value) => spans.push(span(value.to_owned(), theme::soft())),
            None => spans.push(unknown()),
        }
        doc.line(Line::from(spans));
    };
    field(&mut doc, "harness", Some(agent.harness.name()));
    field(&mut doc, "state", details.harness_state.as_deref());
    field(&mut doc, "runtime", details.runtime.as_deref());
    field(&mut doc, "host", Some(&agent.host));
    field(&mut doc, "worktree", agent.worktree.as_deref());
    if let Some(under) = &details.under {
        field(&mut doc, "under", Some(under));
    }
    doc.blank();
    doc.lines(text::wrap(
        &[run(agent.id.clone(), theme::dim())],
        width,
        &[],
        &[],
        None,
    ));
    doc
}

// ---------------------------------------------------------------- tree views

/// A listing laid out as the graph's path tree: folders from the id, one line per leaf.
fn tree_listing(
    mut leaves: Vec<(Vec<String>, String, Vec<Span<'static>>, Vec<Span<'static>>)>,
    state: ListState,
    legend: Vec<Line<'static>>,
) -> Listing {
    leaves.sort_by(|a, b| a.0.cmp(&b.0));
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let mut open: Vec<String> = Vec::new();
    for (path, id, first, right) in leaves {
        let folders = &path[..path.len().saturating_sub(1)];
        let shared = open.iter().zip(folders).take_while(|(a, b)| a == b).count();
        open.truncate(shared);
        for (depth, folder) in folders.iter().enumerate().skip(shared) {
            items.push(Item::Folder(Line::from(vec![
                span(
                    format!(" {}▾ ", "  ".repeat(depth)),
                    theme::fg(theme::SURFACE2),
                ),
                span(format!("{folder}/"), theme::fg(theme::OVERLAY1)),
            ])));
            open.push(folder.clone());
        }
        let mut row = vec![span("  ".repeat(folders.len()), theme::dim())];
        row.extend(first);
        items.push(Item::Row {
            index: ids.len(),
            first: row,
            right,
            second: vec![],
        });
        ids.push(id);
    }
    Listing {
        items,
        ids,
        state,
        legend,
    }
}

pub fn agents_tree(world: &World, spinner: &'static str) -> Listing {
    let leaves = agent_order(world)
        .into_iter()
        .map(|agent| {
            let (glyph, color) = agent_glyph(agent.state, spinner);
            let path = agent
                .id
                .trim_start_matches("agent/")
                .split('/')
                .map(str::to_owned)
                .collect::<Vec<_>>();
            (
                path,
                agent.id.clone(),
                vec![
                    span(format!("{glyph} "), theme::strong(color)),
                    span(agent.name.clone(), theme::bold()),
                ],
                vec![
                    span(
                        agent.harness.name(),
                        theme::fg(harness_color(agent.harness)),
                    ),
                    span(format!(" {:>4}", agent.activity), theme::dim()),
                ],
            )
        })
        .collect();
    let mut listing = tree_listing(
        leaves,
        state_of(&world.agents, "Loading agents…", "No agents yet."),
        vec![],
    );
    listing.legend = agents_list(world, spinner, 40).legend;
    listing
}

pub fn missions_tree(world: &World, spinner: &'static str, system: bool) -> Listing {
    let leaves = mission_order(world, system)
        .into_iter()
        .map(|mission| {
            let (glyph, color) = word_style(mission.word, spinner);
            let path = mission
                .id
                .trim_start_matches("mission/")
                .split('/')
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let (done, total) = mission.progress();
            (
                path.clone(),
                mission.id.clone(),
                vec![
                    span(format!("{glyph} "), theme::strong(color)),
                    span(path.last().cloned().unwrap_or_default(), theme::bold()),
                ],
                vec![span(
                    format!("{} {done}/{total}", mission.word.name()),
                    theme::fg(color),
                )],
            )
        })
        .collect();
    let mut listing = tree_listing(
        leaves,
        state_of(&world.missions, "Loading missions…", "No missions yet."),
        vec![],
    );
    listing.legend = missions_list(world, spinner, system).legend;
    listing
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    #[test]
    fn a_tree_has_one_line_per_folder_and_leaf_in_path_order() {
        let listing = agents_tree(&super::super::demo::world(), "⠋");
        let kinds = listing
            .items
            .iter()
            .map(|item| match item {
                Item::Folder(line) => format!("F {}", super::super::text::plain(line).trim()),
                Item::Row { first, .. } => format!(
                    "R {}",
                    first
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                        .trim()
                ),
                Item::Note(_) => "N".into(),
                Item::Header { .. } => "H".into(),
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds[0], "F ▾ example/", "{kinds:#?}");
        assert!(
            kinds
                .iter()
                .all(|kind| !kind.starts_with('N') && !kind.starts_with('H')),
            "{kinds:#?}"
        );
    }
}

// ------------------------------------------------------------- new mission

pub const NEW_MISSION_FIELDS: [(&str, &str); 4] = [
    ("title", "A short name, like 'Nightly dependency audit'"),
    (
        "what you want",
        "Describe the outcome; the planner turns it into a mission",
    ),
    (
        "mission id",
        "Where it lives in the graph, like fleet/harbor/nightly-audit",
    ),
    ("workspace", "The directory the agents work in"),
];

/// The form behind Missions' New mission: it creates a launch, which a planner turns into a
/// proposed mission on Home. Nothing runs until the person approves it there.
pub fn new_mission_form(fields: &[String; 4], focus: usize, width: usize) -> Doc {
    let mut inner = Doc::new();
    let w = width.saturating_sub(4);
    inner.blank();
    inner.wrap(
        &[run(
            "Say what you want. A planner turns it into a proposed mission, which appears on Home for you to approve; nothing runs before that.",
            theme::soft(),
        )],
        w,
    );
    inner.blank();
    for (index, (label, hint)) in NEW_MISSION_FIELDS.iter().enumerate() {
        let focused = index == focus;
        let mut body = Doc::new();
        let value = &fields[index];
        if value.is_empty() && !focused {
            body.line(Line::from(span(*hint, theme::dim())));
        } else {
            for (line_index, paragraph) in value.split('\n').enumerate() {
                let mut runs = vec![run(paragraph.to_owned(), theme::text())];
                if focused && line_index == value.split('\n').count() - 1 {
                    runs.push(run("█", theme::fg(theme::ACCENT)));
                }
                body.lines(text::wrap(&runs, w.saturating_sub(4), &[], &[], None));
            }
        }
        let start = inner.lines.len();
        inner.card(
            label,
            if focused {
                theme::ACCENT
            } else {
                theme::OVERLAY1
            },
            false,
            body,
            w,
        );
        for line in start..inner.lines.len() {
            inner.targets.push(super::doc::Target {
                line,
                column: 0,
                width: w as u16,
                hit: Hit::Field(index),
            });
        }
    }
    inner.blank();
    inner.buttons(&[
        ("tab", "Next field", Hit::Key('\t'), theme::OVERLAY1),
        ("enter", "Create the launch", Hit::Enter, theme::GREEN),
        ("esc", "Cancel", Hit::Escape, theme::OVERLAY1),
    ]);
    inner.blank();
    let mut doc = Doc::new();
    doc.card("new mission", theme::ACCENT, false, inner, width);
    doc
}

// ------------------------------------------------------------------ devices

pub fn devices_card(world: &World, width: usize) -> Doc {
    let mut inner = Doc::new();
    match &world.devices {
        Load::Loading => inner.line(Line::from(span("Loading your devices…", theme::dim()))),
        Load::Failed(error) => inner.wrap(
            &[run(
                format!("Could not read devices: {error}"),
                theme::fg(theme::RED),
            )],
            width.saturating_sub(4),
        ),
        Load::Ready(devices) if devices.is_empty() => {
            inner.line(Line::from(span("No paired devices.", theme::dim())))
        }
        Load::Ready(devices) => {
            for device in devices {
                inner.line(Line::from(vec![
                    span(
                        if device.state == "active" {
                            "● "
                        } else {
                            "○ "
                        },
                        theme::fg(if device.state == "active" {
                            theme::GREEN
                        } else {
                            theme::QUIET
                        }),
                    ),
                    span(format!("{:<22}", device.name), theme::text()),
                    span(format!("{:<24}", device.scopes.join(", ")), theme::soft()),
                    span(format!("expires {}", device.expires), theme::dim()),
                ]));
                if device.state == "active" {
                    inner.buttons(&[("", "Revoke", Hit::Revoke(device.id.clone()), theme::RED)]);
                }
            }
        }
    }
    let mut doc = Doc::new();
    doc.card("your devices", theme::OVERLAY1, false, inner, width);
    doc
}
