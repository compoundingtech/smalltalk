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

pub const TABS: [&str; 5] = ["Home", "Agents", "Missions", "Fleet", "Worktrees"];

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
        AgentState::Fault => "fault",
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
        Word::Unclaimed => ("◇", theme::WAITING),
        Word::Working => (spinner, theme::WORKING),
        Word::Held => ("◐", theme::SAPPHIRE),
        Word::Idle => ("●", theme::IDLE),
        Word::Done => ("✓", theme::DONE),
        Word::Failed => ("✕", theme::FAULT),
    }
}

fn step_style(state: StepState, spinner: &'static str) -> (&'static str, Color, &'static str) {
    match state {
        StepState::Done => ("▰", theme::DONE, "done"),
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
        | AttentionKind::Revision { .. } => ("◆", theme::PERSON),
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

pub fn home_list(world: &World, _spinner: &'static str) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let mut attention = world.attention.items().iter().collect::<Vec<_>>();
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
        let label = format!("↗ {mission}");
        card.targets.push(super::doc::Target {
            line: card.lines.len(),
            column: 0,
            width: text::width(&label) as u16,
            hit: Hit::Open(mission.clone()),
        });
        card.line(Line::from(span(label, theme::fg(theme::BLUE))));
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
        AttentionKind::Launch { planner, preview } => {
            card.wrap(
                &[run(
                    format!("{planner} proposes a new mission. Nothing runs until you approve it."),
                    theme::text(),
                )],
                inner,
            );
            card.blank();
            card.field("mission", &preview.name, inner, theme::bold());
            card.field("in", &preview.workspace, inner, theme::soft());
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
                    span(format!("  {:<18}", agent.name), theme::text()),
                    span(
                        format!("{:<8}", agent.harness.name()),
                        theme::fg(harness_color(agent.harness)),
                    ),
                    span(format!("on {}", agent.host), theme::dim()),
                ]));
            }
            card.blank();
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
                card.buttons(&[
                    ("a", "Approve launch", Hit::Key('a'), theme::GREEN),
                    ("c", "Ask for changes", Hit::Key('c'), theme::YELLOW),
                    ("d", "Cancel launch", Hit::Key('d'), theme::RED),
                ]);
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
            if !confirm_row(
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
            card.field("source", source, inner, theme::soft());
            if let Some(fix) = fix {
                card.blank();
                let mut suggestion = Doc::new();
                suggestion.wrap(&text::inline(fix, theme::text()), inner.saturating_sub(4));
                card.card("suggested fix", theme::GREEN, false, suggestion, inner);
            }
            card.blank();
            if !confirm_row(&mut card, drafts, "Mark this fault resolved") {
                card.buttons(&[
                    ("r", "Mark resolved", Hit::Key('r'), theme::GREEN),
                    ("g", "Open the agent", Hit::Key('g'), theme::ACCENT),
                ]);
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
                ]);
            }
        }
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
        if names.len() > 1 {
            runs.push(run("{", theme::dim()));
        }
        for (index, (name, _, color)) in names.iter().enumerate() {
            if index > 0 {
                runs.push(run(", ", theme::dim()));
            }
            runs.push(run(name.to_string(), theme::fg(*color)));
        }
        if names.len() > 1 {
            runs.push(run("}", theme::dim()));
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
        let label = format!("   ↗ {mission} › {step}");
        doc.targets.push(super::doc::Target {
            line: 2,
            column: 0,
            width: text::width(&label) as u16,
            hit: Hit::Open(format!("mission/fleet/{mission}")),
        });
        doc.line(Line::from(span(label, theme::fg(theme::BLUE))));
    }
    let _ = world;
    doc
}

// ----------------------------------------------------------------- missions

pub fn mission_order(world: &World, system: bool) -> Vec<&Mission> {
    let mut missions = world
        .missions
        .items()
        .iter()
        .filter(|mission| system || !mission.system)
        .collect::<Vec<_>>();
    missions.sort_by_key(|mission| (mission.word, mission.title.to_lowercase()));
    missions
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
        let right = if done > 0 {
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
    let hidden = world
        .missions
        .items()
        .iter()
        .filter(|mission| mission.system && !system)
        .count();
    if hidden > 0 {
        items.push(Item::Note(Line::from(span(
            format!(" {hidden} system missions hidden · x shows them"),
            theme::dim(),
        ))));
    }
    Listing {
        items,
        ids,
        state: state_of(&world.missions, "Loading missions…", "No missions yet."),
        legend: legend(&[
            ("◆", theme::PERSON, "needs you"),
            ("▲", theme::FAULT, "stalled"),
            ("◇", theme::WAITING, "unclaimed"),
            (spinner, theme::WORKING, "working"),
            ("●", theme::IDLE, "idle"),
            ("✓", theme::DONE, "done"),
        ]),
    }
}

pub fn mission_detail(world: &World, id: Option<&str>, width: usize, spinner: &'static str) -> Doc {
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
    if let Some(decision) = mission
        .decision
        .as_ref()
        .and_then(|id| world.attention.items().iter().find(|item| &item.id == id))
    {
        let mut card = Doc::new();
        card.blank();
        let question = match &decision.kind {
            AttentionKind::Review { question, .. } | AttentionKind::Feedback { question, .. } => {
                question.clone()
            }
            _ => decision.title.clone(),
        };
        card.wrap(&text::inline(&question, theme::text()), inner);
        card.blank();
        card.buttons(&[(
            "enter",
            "Answer on Home",
            Hit::Open(decision.id.clone()),
            theme::PERSON,
        )]);
        card.blank();
        doc.card(
            &format!("{} · waiting {}", decision.kind.word(), decision.age),
            theme::PERSON,
            true,
            card,
            width,
        );
    }
    let (done, total) = mission.progress();
    let available = mission
        .steps
        .iter()
        .filter(|step| matches!(step.state, StepState::Ready))
        .count();
    let mut steps = Doc::new();
    let mut bar = meter(done, total, 20, theme::DONE);
    bar.push(span(format!("  {done}/{total} done"), theme::dim()));
    if available > 0 {
        bar.push(span(
            format!(" · {available} ready, nobody has it"),
            theme::fg(theme::WAITING),
        ));
    }
    steps.line(Line::from(bar));
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
        steps.line(Line::from(vec![
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
                &[run("    ", theme::dim())],
                &[run("    ", theme::dim())],
                None,
            ));
        }
    }
    doc.card("steps", theme::OVERLAY1, false, steps, width);
    let mut agents = Doc::new();
    for id in &mission.agents {
        if let Some(agent) = world.agents.items().iter().find(|agent| &agent.id == id) {
            let (glyph, color) = agent_glyph(agent.state, spinner);
            let label = format!("{glyph} {:<18}", agent.name);
            agents.targets.push(super::doc::Target {
                line: agents.lines.len(),
                column: 0,
                width: text::width(&label) as u16,
                hit: Hit::Open(agent.id.clone()),
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
    doc
}

// -------------------------------------------------------------------- fleet

pub fn fleet_list(world: &World) -> Listing {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    for machine in world.machines.items() {
        let (glyph, color) = if machine.online {
            ("●", theme::GREEN)
        } else {
            ("○", theme::RED)
        };
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
            right: vec![span(format!("{agents} agents"), theme::dim())],
            second: vec![span(
                format!("   {} · seen {}", machine.platform, machine.seen),
                theme::dim(),
            )],
        });
        ids.push(machine.name.clone());
    }
    Listing {
        items,
        ids,
        state: state_of(&world.machines, "Loading machines…", "No machines yet."),
        legend: legend(&[("●", theme::GREEN, "online"), ("○", theme::RED, "offline")]),
    }
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
    let (glyph, color) = if machine.online {
        ("●", theme::GREEN)
    } else {
        ("○", theme::RED)
    };
    doc.line(Line::from(vec![
        span(format!(" {glyph} "), theme::strong(color)),
        span(machine.name.clone(), theme::bold()),
        span(
            format!("  {} · seen {}", machine.platform, machine.seen),
            theme::dim(),
        ),
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
            "Unknown while this machine is offline.",
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
        let label = format!("{glyph} {:<20}", agent.name);
        agents.targets.push(super::doc::Target {
            line: agents.lines.len(),
            column: 0,
            width: text::width(&label) as u16,
            hit: Hit::Open(agent.id.clone()),
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
            let label = format!("{glyph} {:<20}", agent.name);
            agents.targets.push(super::doc::Target {
                line: agents.lines.len(),
                column: 0,
                width: text::width(&label) as u16,
                hit: Hit::Open(agent.id.clone()),
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
                hit: Hit::Open(mission.id.clone()),
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
