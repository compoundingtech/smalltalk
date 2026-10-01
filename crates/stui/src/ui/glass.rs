//! Glasses: stui as named workspaces of tabs and split panes with a palette, an experiment
//! beside the sidebar layout (`stui --glasses`, mission fleet/stui/glass, design
//! doc/fleet/stui/glass). Plain stui never reaches this module.
//!
//! The focused pane points stui's own tab and selection at its subject, so every key and action
//! the sidebar layout has works the same inside a glass.

use super::glass_store::{self, Stored, StoredGlass, StoredTab};
use super::layout::{Layout, Side};
use super::*;
use ratatui::style::Color;
use std::path::PathBuf;

/// The palette's sections, in order; a digit key opens the palette at one.
const SECTIONS: [&str; 5] = ["needs you", "agents", "missions", "fleet", "glasses"];
const GLASSES: usize = 4;

/// Every glass this window knows, and the one it shows.
pub(crate) struct Glasses {
    all: Vec<Glass>,
    shown: usize,
    palette: Option<Palette>,
    /// Where they are kept on this device; none in the demo.
    store: Option<PathBuf>,
}

/// A named workspace: Home, then tabs of panes.
#[derive(Clone)]
struct Glass {
    /// Stable across renames; the graph will keep the glass under it.
    id: String,
    name: String,
    /// The tabs after Home, which is always first and cannot be closed.
    tabs: Vec<Tab>,
    /// The tab shown: 0 is Home.
    current: usize,
}

#[derive(Clone)]
struct Tab {
    /// A name the person gave the tab; otherwise it is named after its focused pane.
    title: Option<String>,
    layout: Layout,
    /// The focused leaf.
    focus: usize,
}

impl Glass {
    fn new(name: String) -> Self {
        Self {
            id: new_id(),
            name,
            tabs: Vec::new(),
            current: 0,
        }
    }

    fn tab(&self) -> Option<&Tab> {
        self.current
            .checked_sub(1)
            .and_then(|index| self.tabs.get(index))
    }

    fn tab_mut(&mut self) -> Option<&mut Tab> {
        self.current
            .checked_sub(1)
            .and_then(|index| self.tabs.get_mut(index))
    }

    /// Where a pane key already shows: (tab, leaf).
    fn find(&self, key: &str) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.layout
                .leaves()
                .iter()
                .position(|leaf| *leaf == key)
                .map(|leaf| (index + 1, leaf))
        })
    }
}

impl Glasses {
    /// Open `wanted` (or the last glass used here, or `main`) from what this device keeps.
    pub(crate) fn open(wanted: Option<String>, store: Option<PathBuf>) -> Self {
        let stored = store.as_deref().map(glass_store::load).unwrap_or_default();
        let mut all = stored
            .glasses
            .into_iter()
            .map(|glass| Glass {
                id: if glass.id.is_empty() {
                    new_id()
                } else {
                    glass.id
                },
                name: glass.name,
                tabs: glass
                    .tabs
                    .into_iter()
                    .map(|tab| Tab {
                        title: tab.title,
                        layout: tab.layout,
                        focus: 0,
                    })
                    .collect(),
                current: 0,
            })
            .collect::<Vec<_>>();
        let name = wanted.or(stored.last).unwrap_or_else(|| "main".to_owned());
        let shown = match all.iter().position(|glass| glass.name == name) {
            Some(index) => index,
            None => {
                all.push(Glass::new(name));
                all.len() - 1
            }
        };
        Self {
            all,
            shown,
            palette: None,
            store,
        }
    }

    fn glass(&self) -> &Glass {
        &self.all[self.shown]
    }

    fn glass_mut(&mut self) -> &mut Glass {
        &mut self.all[self.shown]
    }

    /// Keep the glasses on this device. A failure is not worth interrupting the person for.
    fn save(&self) {
        let Some(path) = &self.store else { return };
        let stored = Stored {
            version: 0,
            last: Some(self.glass().name.clone()),
            glasses: self
                .all
                .iter()
                .map(|glass| StoredGlass {
                    id: glass.id.clone(),
                    name: glass.name.clone(),
                    tabs: glass
                        .tabs
                        .iter()
                        .map(|tab| StoredTab {
                            title: tab.title.clone(),
                            layout: tab.layout.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        let _ = glass_store::save(path, &stored);
    }

    /// `base`, or `base 2`, `base 3`… whichever no glass is called yet.
    fn unused_name(&self, base: &str) -> String {
        let taken = |name: &str| self.all.iter().any(|glass| glass.name == name);
        if !taken(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base} {n}"))
            .find(|name| !taken(name))
            .unwrap_or_default()
    }
}

/// Where the palette opens what is chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Open {
    /// In the focused pane (or a new tab, from Home).
    #[default]
    Here,
    Right,
    Below,
    Tab,
    Glass,
}

#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
    /// Only this section, when opened with a digit.
    section: Option<usize>,
    /// What Enter does: Ctrl+T opens the palette to make a tab.
    enter: Open,
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Open(Pane),
    ShowGlass(usize),
    NewGlass(String),
    RenameGlass(String),
    DuplicateGlass(String),
    CloseGlass,
}

/// One row the palette can open.
#[derive(Clone)]
struct Choice {
    section: usize,
    glyph: (&'static str, Color),
    label: String,
    detail: String,
    /// What the query is matched against; empty matches only a typed name.
    search: String,
    action: Action,
}

/// How well `query` matches `text`: higher is better, `None` is no match. A query matches as a
/// substring, or letter by letter where each letter either follows the last one or starts a
/// word ("ab" finds "Atlas Builder"); letters scattered inside words do not match.
fn fuzzy(query: &str, text: &str) -> Option<i64> {
    let query = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    if query.is_empty() {
        return Some(0);
    }
    let lower = text.to_lowercase();
    let starts_word =
        |chars: &[char], index: usize| index == 0 || !chars[index - 1].is_alphanumeric();
    let chars = lower.chars().collect::<Vec<_>>();
    if let Some(at) = lower.find(&query) {
        let index = lower[..at].chars().count();
        let bonus = if starts_word(&chars, index) { 50 } else { 0 };
        return Some(1000 + bonus - index as i64);
    }
    let mut score = 0;
    let mut at = 0;
    let mut last: Option<usize> = None;
    for wanted in query.chars() {
        let found = (at..chars.len()).find(|&index| {
            chars[index] == wanted
                && (last.is_some_and(|last| last + 1 == index) || starts_word(&chars, index))
        })?;
        score += if last.is_some_and(|last| last + 1 == found) {
            5
        } else {
            3
        };
        last = Some(found);
        at = found + 1;
    }
    Some(score * 10 - chars.len() as i64)
}

impl Ui {
    /// Whether a glass is showing its Home tab.
    pub(crate) fn on_home(&self) -> bool {
        self.glasses
            .as_ref()
            .is_some_and(|glasses| glasses.glass().current == 0)
    }

    /// Whether the shown tab has more than one pane.
    pub(crate) fn split_shown(&self) -> bool {
        self.glasses
            .as_ref()
            .and_then(|glasses| glasses.glass().tab())
            .is_some_and(|tab| tab.layout.leaves().len() > 1)
    }

    /// Every subject and glass action the palette offers, section by section.
    fn choices(&self, query: &str) -> Vec<Choice> {
        let spinner = self.spinner();
        let mut choices = Vec::new();
        let mut open = |section, glyph, label: String, detail: String, pane: Pane| {
            choices.push(Choice {
                section,
                glyph,
                search: format!("{label} {}", pane.key()),
                label,
                detail,
                action: Action::Open(pane),
            });
        };
        for item in self.world.attention.items() {
            if !self.snoozed.contains(&item.id) {
                open(
                    0,
                    screens::attention_style(&item.kind),
                    item.title.clone(),
                    item.kind.word().to_owned(),
                    Pane::Home(Some(item.id.clone())),
                );
            }
        }
        for agent in screens::agent_order(&self.world) {
            open(
                1,
                screens::agent_glyph(agent.state, spinner),
                agent.name.clone(),
                format!("{} · {}", agent.harness.name(), agent.host),
                Pane::Agent(Some(agent.id.clone())),
            );
        }
        for mission in screens::mission_order(&self.world, true) {
            open(
                2,
                screens::word_style(mission.word, spinner),
                mission.title.clone(),
                mission.word.name().to_owned(),
                Pane::Mission(Some(mission.id.clone())),
            );
        }
        for machine in self.world.machines.items() {
            open(
                3,
                screens::reach_style(machine.reach),
                machine.name.clone(),
                machine.reach.word().to_owned(),
                Pane::Machine(Some(format!("machine/{}", machine.name))),
            );
        }
        let Some(glasses) = &self.glasses else {
            return choices;
        };
        let glass =
            |glyph: &'static str, label: String, detail: &str, search: String, action| Choice {
                section: GLASSES,
                glyph: (glyph, theme::LAVENDER),
                label,
                detail: detail.to_owned(),
                search,
                action,
            };
        for (index, each) in glasses.all.iter().enumerate() {
            let shown = index == glasses.shown;
            choices.push(glass(
                if shown { "●" } else { "○" },
                each.name.clone(),
                if shown { "shown" } else { "switch" },
                each.name.clone(),
                Action::ShowGlass(index),
            ));
        }
        let name = query.trim();
        if !name.is_empty() {
            choices.push(glass(
                "+",
                format!("New glass “{name}”"),
                "ctrl+o switches",
                String::new(),
                Action::NewGlass(name.to_owned()),
            ));
            choices.push(glass(
                "✎",
                format!("Rename “{}” to “{name}”", glasses.glass().name),
                "",
                String::new(),
                Action::RenameGlass(name.to_owned()),
            ));
            choices.push(glass(
                "⧉",
                format!("Duplicate “{}” as “{name}”", glasses.glass().name),
                "",
                String::new(),
                Action::DuplicateGlass(name.to_owned()),
            ));
        }
        if glasses.all.len() > 1 {
            choices.push(glass(
                "×",
                format!("Close “{}”", glasses.glass().name),
                "its tabs go with it",
                "close glass".to_owned(),
                Action::CloseGlass,
            ));
        }
        choices
    }

    /// What the palette shows for its query: sections in order, best matches first in each.
    fn matches(&self, palette: &Palette) -> Vec<Choice> {
        let mut scored = self
            .choices(&palette.query)
            .into_iter()
            .filter(|choice| {
                palette
                    .section
                    .is_none_or(|section| choice.section == section)
            })
            .filter_map(|choice| {
                // Glass actions named by the query always show while something is typed.
                let score = if choice.search.is_empty() {
                    Some(0)
                } else {
                    fuzzy(&palette.query, &choice.search)
                };
                score.map(|score| (choice, score))
            })
            .collect::<Vec<_>>();
        if !palette.query.is_empty() {
            scored.sort_by_key(|(choice, score)| (choice.section, -score));
        }
        scored.into_iter().map(|(choice, _)| choice).collect()
    }

    pub(crate) fn render_glass(&self, buf: &mut Buffer, area: Rect) {
        let Some(glasses) = &self.glasses else { return };
        let glass = glasses.glass();
        self.status_line(buf, Rect { height: 1, ..area }, glass);
        self.tab_strip(
            buf,
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
            glass,
        );
        let body = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(3),
            ..area
        };
        match glass.tab() {
            // Home is today's Home: its list and the selected card.
            None => self.draw_body(buf, body),
            Some(tab) => self.draw_tab(
                buf,
                Rect {
                    x: body.x + 1,
                    width: body.width.saturating_sub(2),
                    ..body
                },
                tab,
            ),
        }
        self.footer(
            buf,
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
        if let Some(palette) = &glasses.palette {
            self.draw_palette(buf, area, palette);
        }
    }

    /// A tab's panes. The focused one draws through stui's own tab and selection, first, so it
    /// is the pane scrolling keys reach; the others draw from their keys.
    fn draw_tab(&self, buf: &mut Buffer, area: Rect, tab: &Tab) {
        let leaves = tab.layout.leaves();
        let (rects, dividers) = tab.layout.rects(area);
        for (rect, side) in dividers {
            let symbol = match side {
                Side::Right => "│",
                Side::Below => "─",
            };
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    buf[(x, y)]
                        .set_symbol(symbol)
                        .set_style(Style::default().fg(theme::SURFACE1).bg(theme::BASE));
                }
            }
        }
        self.frame.borrow_mut().glass_leaves = rects.clone();
        let titled = leaves.len() > 1;
        let order =
            std::iter::once(tab.focus).chain((0..leaves.len()).filter(|index| *index != tab.focus));
        for index in order {
            let (Some(rect), Some(key)) = (rects.get(index), leaves.get(index)) else {
                continue;
            };
            let focused = index == tab.focus;
            let pane = Pane::parse(key);
            // A pane right of a divider keeps a column clear of it, as the main area does.
            let rect = &if rect.x > area.x {
                Rect {
                    x: rect.x + 1,
                    width: rect.width.saturating_sub(1),
                    ..*rect
                }
            } else {
                *rect
            };
            let content = if titled {
                let title = pane
                    .as_ref()
                    .map(|pane| self.pane_title(pane))
                    .unwrap_or_else(|| key.to_string());
                let style = if focused {
                    Style::default()
                        .fg(theme::CRUST)
                        .bg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::fg(theme::SUBTEXT0).bg(theme::SURFACE0)
                };
                buf.set_style(Rect { height: 1, ..*rect }, style);
                buf.set_stringn(
                    rect.x,
                    rect.y,
                    format!(" {title}"),
                    rect.width as usize,
                    style,
                );
                Rect {
                    y: rect.y + 1,
                    height: rect.height.saturating_sub(1),
                    ..*rect
                }
            } else {
                *rect
            };
            match pane {
                None => {
                    buf.set_stringn(
                        content.x + 1,
                        content.y + 1,
                        format!("This stui does not know {key}. Ctrl+W closes it."),
                        content.width.saturating_sub(2) as usize,
                        theme::dim(),
                    );
                }
                Some(pane) if self.subject_gone(&pane) => {
                    buf.set_stringn(
                        content.x + 1,
                        content.y + 1,
                        format!("{key} is gone. Ctrl+W closes this pane."),
                        content.width.saturating_sub(2) as usize,
                        theme::dim(),
                    );
                }
                Some(_) if focused => self.draw_main(buf, content),
                Some(pane) => self.draw_pane(buf, content, &pane),
            }
        }
    }

    /// A pane whose subject st no longer lists. Unknown while its list is still loading.
    fn subject_gone(&self, pane: &Pane) -> bool {
        match pane {
            Pane::Agent(Some(id)) | Pane::Terminal(id) => self
                .world
                .agents
                .ready()
                .is_some_and(|agents| !agents.iter().any(|agent| &agent.id == id)),
            Pane::Mission(Some(id)) | Pane::Declaration(Some(id)) => self
                .world
                .missions
                .ready()
                .is_some_and(|missions| !missions.iter().any(|mission| &mission.id == id)),
            Pane::Machine(Some(id)) => {
                let name = id.trim_start_matches("machine/");
                self.world
                    .machines
                    .ready()
                    .is_some_and(|machines| !machines.iter().any(|machine| machine.name == name))
            }
            _ => false,
        }
    }

    /// Connection, what needs the person, what is working, and the fleet, always in view.
    fn status_line(&self, buf: &mut Buffer, area: Rect, glass: &Glass) {
        buf.set_style(area, Style::default().bg(theme::CRUST));
        let bar = |style: Style| style.bg(theme::CRUST);
        let mut spans = vec![Span::styled(" ≡ st  ", bar(theme::strong(theme::ACCENT)))];
        let (glyph, word, color) = match &self.world.link {
            Link::Live if !self.world.diverged.is_empty() => ("⚠", "diverged", theme::RED),
            Link::Live => ("●", "live", theme::GREEN),
            Link::Connecting => (self.spinner(), "connecting", theme::YELLOW),
            Link::Offline(_) => ("○", "offline", theme::RED),
        };
        spans.push(Span::styled(
            format!("{glyph} {word}"),
            bar(theme::fg(color)),
        ));
        let need = self
            .world
            .attention
            .items()
            .iter()
            .filter(|item| !self.snoozed.contains(&item.id))
            .count();
        spans.push(Span::styled(" · ", bar(theme::dim())));
        spans.push(if need > 0 {
            Span::styled(
                format!("◆ {need} need you"),
                bar(theme::strong(theme::PERSON)),
            )
        } else {
            Span::styled("nothing needs you", bar(theme::dim()))
        });
        let working = self
            .world
            .agents
            .items()
            .iter()
            .filter(|agent| agent.state == AgentState::Working)
            .count();
        spans.push(Span::styled(" · ", bar(theme::dim())));
        spans.push(Span::styled(
            format!("{} {working} working", self.spinner()),
            bar(theme::fg(theme::WORKING)),
        ));
        let machines = self.world.machines.items();
        if !machines.is_empty() {
            let count = |reach: &[Reach]| {
                machines
                    .iter()
                    .filter(|machine| reach.contains(&machine.reach))
                    .count()
            };
            spans.push(Span::styled(" · fleet ", bar(theme::dim())));
            for (glyph, color, n) in [
                ("●", theme::GREEN, count(&[Reach::Here, Reach::Direct])),
                ("◐", theme::SAPPHIRE, count(&[Reach::Indirect])),
                ("○", theme::RED, count(&[Reach::Offline, Reach::Unknown])),
            ] {
                if n > 0 {
                    spans.push(Span::styled(format!("{glyph}{n} "), bar(theme::fg(color))));
                }
            }
        }
        buf.set_line(area.x, area.y, &Line::from(spans), area.width);
        let name = format!(" {} ▾ ", glass.name);
        let width = text::width(&name) as u16;
        let x = area.x + area.width.saturating_sub(width);
        buf.set_stringn(
            x,
            area.y,
            &name,
            width as usize,
            bar(theme::strong(theme::LAVENDER)),
        );
        self.hit(Rect { x, width, ..area }, Hit::GlassMenu);
    }

    /// Home, then the glass's tabs; a tab where something needs the person shows `◆`.
    fn tab_strip(&self, buf: &mut Buffer, area: Rect, glass: &Glass) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        let mut x = area.x + 1;
        for index in 0..=glass.tabs.len() {
            let (title, needs) = match index {
                0 => ("Home".to_owned(), false),
                _ => {
                    let tab = &glass.tabs[index - 1];
                    let panes = tab
                        .layout
                        .leaves()
                        .iter()
                        .filter_map(|key| Pane::parse(key))
                        .collect::<Vec<_>>();
                    let first = tab
                        .layout
                        .leaves()
                        .get(tab.focus)
                        .and_then(|key| Pane::parse(key))
                        .map(|pane| self.pane_title(&pane))
                        .unwrap_or_default();
                    let more = panes.len().saturating_sub(1);
                    (
                        match (&tab.title, more) {
                            (Some(title), _) => title.clone(),
                            (None, 0) => first,
                            (None, more) => format!("{first} +{more}"),
                        },
                        panes.iter().any(|pane| self.pane_needs_person(pane)),
                    )
                }
            };
            let label = format!(" {}{title} ", if needs { "◆ " } else { "" });
            let label = text::truncate(&label, 28);
            let style = if index == glass.current {
                Style::default()
                    .fg(theme::CRUST)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else if needs {
                theme::fg(theme::PERSON).bg(theme::MANTLE)
            } else {
                theme::fg(theme::SUBTEXT0).bg(theme::MANTLE)
            };
            let width = text::width(&label) as u16;
            if x + width > area.x + area.width {
                break;
            }
            buf.set_stringn(x, area.y, &label, width as usize, style);
            self.hit(Rect { x, width, ..area }, Hit::GlassTab(index));
            x += width + 1;
        }
        if x + 3 <= area.x + area.width {
            buf.set_stringn(x, area.y, " + ", 3, theme::dim().bg(theme::MANTLE));
            self.hit(
                Rect {
                    x,
                    width: 3,
                    ..area
                },
                Hit::Palette,
            );
        }
    }

    /// A pane's title: the name of what it shows.
    fn pane_title(&self, pane: &Pane) -> String {
        let find = |id: &Option<String>| id.clone().unwrap_or_default();
        match pane {
            Pane::Agent(Some(id)) | Pane::Terminal(id) => self
                .world
                .agents
                .items()
                .iter()
                .find(|agent| &agent.id == id)
                .map(|agent| agent.name.clone())
                .unwrap_or_else(|| id.clone()),
            Pane::Mission(id) | Pane::Declaration(id) => {
                let id = find(id);
                self.world
                    .missions
                    .items()
                    .iter()
                    .find(|mission| mission.id == id)
                    .map(|mission| mission.title.clone())
                    .unwrap_or(id)
            }
            Pane::Machine(id) => find(id).trim_start_matches("machine/").to_owned(),
            other => other.key(),
        }
    }

    fn pane_needs_person(&self, pane: &Pane) -> bool {
        match pane {
            Pane::Agent(Some(id)) => self
                .world
                .agents
                .items()
                .iter()
                .any(|agent| &agent.id == id && agent.state == AgentState::NeedsYou),
            Pane::Mission(Some(id)) => self.mission_decision(id).is_some(),
            _ => false,
        }
    }

    fn draw_palette(&self, buf: &mut Buffer, area: Rect, palette: &Palette) {
        let width = area.width.saturating_sub(8).clamp(20, 84);
        let height = area.height.saturating_sub(6).clamp(6, 26);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + 2,
            width,
            height,
        };
        let edge = Style::default().fg(theme::SURFACE2).bg(theme::MANTLE);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let top = y == rect.y;
                let bottom = y == rect.y + rect.height - 1;
                let left = x == rect.x;
                let right = x == rect.x + rect.width - 1;
                let symbol = match (top, bottom, left, right) {
                    (true, _, true, _) => "╭",
                    (true, _, _, true) => "╮",
                    (_, true, true, _) => "╰",
                    (_, true, _, true) => "╯",
                    (true, _, _, _) | (_, true, _, _) => "─",
                    (_, _, true, _) | (_, _, _, true) => "│",
                    _ => " ",
                };
                buf[(x, y)].set_symbol(symbol).set_style(edge);
            }
        }
        let inner = width.saturating_sub(4) as usize;
        let title = match palette.enter {
            Open::Tab => " open in a new tab ",
            _ => " open ",
        };
        buf.set_stringn(
            rect.x + 2,
            rect.y,
            title,
            inner,
            theme::strong(theme::ACCENT).bg(theme::MANTLE),
        );
        let scope = palette
            .section
            .map(|section| format!(" {} ", SECTIONS[section]))
            .unwrap_or_default();
        let input = Line::from(vec![
            Span::styled(" › ", theme::strong(theme::ACCENT).bg(theme::MANTLE)),
            Span::styled(scope, theme::fg(theme::LAVENDER).bg(theme::MANTLE)),
            Span::styled(palette.query.clone(), theme::text().bg(theme::MANTLE)),
            Span::styled("▏", theme::fg(theme::ACCENT).bg(theme::MANTLE)),
        ]);
        buf.set_line(rect.x + 1, rect.y + 1, &input, rect.width - 2);
        let matches = self.matches(palette);
        let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
        let mut current = None;
        for (index, choice) in matches.iter().enumerate() {
            if current != Some(choice.section) {
                current = Some(choice.section);
                rows.push((
                    None,
                    Line::from(Span::styled(
                        format!(" {}", SECTIONS[choice.section]),
                        theme::strong(theme::OVERLAY1).bg(theme::MANTLE),
                    )),
                ));
            }
            let selected = index == palette.selected;
            let bg = if selected {
                theme::SURFACE0
            } else {
                theme::MANTLE
            };
            let detail_width = text::width(&choice.detail).min(inner / 3);
            let label = text::truncate(&choice.label, inner.saturating_sub(detail_width + 6));
            let fill = inner.saturating_sub(text::width(&label) + detail_width + 4);
            rows.push((
                Some(index),
                Line::from(vec![
                    Span::styled(
                        if selected { " ▌" } else { "  " },
                        theme::fg(theme::ACCENT).bg(bg),
                    ),
                    Span::styled(
                        format!("{} ", choice.glyph.0),
                        theme::fg(choice.glyph.1).bg(bg),
                    ),
                    Span::styled(label, theme::text().bg(bg)),
                    Span::styled(" ".repeat(fill), Style::default().bg(bg)),
                    Span::styled(
                        text::truncate(&choice.detail, detail_width),
                        theme::dim().bg(bg),
                    ),
                ]),
            ));
        }
        if matches.is_empty() {
            rows.push((
                None,
                Line::from(Span::styled(
                    "  Nothing matches.",
                    theme::dim().bg(theme::MANTLE),
                )),
            ));
        }
        let list_height = rect.height.saturating_sub(4) as usize;
        let selected_row = rows
            .iter()
            .position(|(index, _)| *index == Some(palette.selected))
            .unwrap_or(0);
        let top = selected_row.saturating_sub(list_height.saturating_sub(1));
        for (offset, (index, line)) in rows.iter().skip(top).take(list_height).enumerate() {
            let y = rect.y + 2 + offset as u16;
            buf.set_line(rect.x + 1, y, line, rect.width - 2);
            if let Some(index) = index {
                self.hit(
                    Rect {
                        x: rect.x,
                        y,
                        width: rect.width,
                        height: 1,
                    },
                    Hit::PaletteChoice(*index),
                );
            }
        }
        buf.set_stringn(
            rect.x + 2,
            rect.y + rect.height - 2,
            "enter open · ctrl+v right · ctrl+x below · ctrl+t tab · ctrl+g glass · esc",
            inner,
            theme::dim().bg(theme::MANTLE),
        );
    }

    /// Keys glasses own. Returns whether the key was used here.
    pub(crate) fn glass_key(&mut self, key: KeyEvent) -> bool {
        let Some(glasses) = self.glasses.as_mut() else {
            return false;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(palette) = glasses.palette.as_mut() {
            match key.code {
                KeyCode::Esc => glasses.palette = None,
                KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
                KeyCode::Down => palette.selected += 1,
                KeyCode::Backspace if palette.query.is_empty() => palette.section = None,
                KeyCode::Backspace => {
                    palette.query.pop();
                    palette.selected = 0;
                }
                KeyCode::Enter => {
                    let enter = palette.enter;
                    self.open_choice(None, enter);
                }
                KeyCode::Char('v') if control => self.open_choice(None, Open::Right),
                KeyCode::Char('x') if control => self.open_choice(None, Open::Below),
                KeyCode::Char('t') if control => self.open_choice(None, Open::Tab),
                KeyCode::Char('g') if control => self.open_choice(None, Open::Glass),
                KeyCode::Char('k') if control || command => glasses.palette = None,
                KeyCode::Char(letter) if !control && !command => {
                    palette.query.push(letter);
                    palette.selected = 0;
                }
                _ => {}
            }
            self.clamp_palette();
            return true;
        }
        // In an attached terminal every key is the agent's; Ctrl+\ leaves it first.
        if self.terminal.is_some() && self.tab == 1 {
            return false;
        }
        let tabs = glasses.glass().tabs.len() + 1;
        let current = glasses.glass().current;
        let quiet = !self.editing && self.new_mission.is_none() && self.chat.is_none();
        match key.code {
            KeyCode::Char('k') if control || command => self.open_palette(None, Open::Here),
            KeyCode::Char('t') if control => self.open_palette(None, Open::Tab),
            KeyCode::Char('w') if control => self.close_pane(),
            KeyCode::Char('o') if control => self.next_glass(),
            KeyCode::Char(digit @ '1'..='9') if alt => self.show_tab(digit as usize - '1' as usize),
            KeyCode::PageDown if control => self.show_tab((current + 1) % tabs),
            KeyCode::PageUp if control => self.show_tab((current + tabs - 1) % tabs),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if alt => {
                self.move_focus(key.code)
            }
            // Outside a draft, the sidebar's tab digits open the palette at that section.
            KeyCode::Char(digit @ '1'..='5') if quiet && key.modifiers.is_empty() => {
                self.open_palette(Some(digit as usize - '1' as usize), Open::Here)
            }
            // A subject's tab has no list to move through: these scroll its pane.
            KeyCode::Up | KeyCode::Char('k') if quiet && current > 0 && !control => {
                self.scroll_main(-1)
            }
            KeyCode::Down | KeyCode::Char('j') if quiet && current > 0 && !control => {
                self.scroll_main(1)
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn open_palette(&mut self, section: Option<usize>, enter: Open) {
        if let Some(glasses) = self.glasses.as_mut() {
            glasses.palette = Some(Palette {
                section,
                enter,
                ..Palette::default()
            });
        }
    }

    fn clamp_palette(&mut self) {
        let count = match self
            .glasses
            .as_ref()
            .and_then(|glasses| glasses.palette.as_ref())
        {
            Some(palette) => self.matches(palette).len(),
            None => return,
        };
        if let Some(palette) = self
            .glasses
            .as_mut()
            .and_then(|glasses| glasses.palette.as_mut())
        {
            palette.selected = palette.selected.min(count.saturating_sub(1));
        }
    }

    /// Act on the chosen row (or `index`).
    pub(crate) fn open_choice(&mut self, index: Option<usize>, how: Open) {
        let Some(palette) = self
            .glasses
            .as_ref()
            .and_then(|glasses| glasses.palette.as_ref())
        else {
            return;
        };
        let index = index.unwrap_or(palette.selected);
        let Some(choice) = self.matches(palette).into_iter().nth(index) else {
            return;
        };
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        glasses.palette = None;
        match choice.action {
            Action::Open(pane) => self.open_in_glass(pane, how),
            Action::ShowGlass(index) => self.show_glass(index),
            Action::NewGlass(name) => {
                let name = glasses.unused_name(&name);
                glasses.all.push(Glass::new(name));
                let last = glasses.all.len() - 1;
                self.show_glass(last);
            }
            Action::RenameGlass(name) => {
                if glasses.all.iter().any(|glass| glass.name == name) {
                    self.flash(format!("A glass is already called “{name}”"));
                } else {
                    glasses.glass_mut().name = name;
                    glasses.save();
                }
            }
            Action::DuplicateGlass(name) => {
                let mut copy = glasses.glass().clone();
                copy.id = new_id();
                copy.name = glasses.unused_name(&name);
                glasses.all.push(copy);
                let last = glasses.all.len() - 1;
                self.show_glass(last);
            }
            Action::CloseGlass => {
                if glasses.all.len() > 1 {
                    let closed = glasses.all.remove(glasses.shown).name;
                    let next = glasses.shown.saturating_sub(1);
                    self.show_glass(next);
                    self.flash(format!("Closed the glass “{closed}”"));
                }
            }
        }
    }

    /// Show `pane` in this glass: where it already shows, else as `how` says. Home stays Home,
    /// so what opens from Home gets its own tab, and a Home item opens on Home itself.
    pub(crate) fn open_in_glass(&mut self, pane: Pane, how: Open) {
        let title = self.pane_title(&pane);
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        if matches!(pane, Pane::Home(_)) {
            self.show_tab(0);
            self.focus_pane(&pane);
            return;
        }
        let key = pane.key();
        if how == Open::Glass {
            let name = glasses.unused_name(&title);
            let mut glass = Glass::new(name);
            glass.tabs.push(Tab {
                title: None,
                layout: Layout::pane(&key),
                focus: 0,
            });
            glasses.all.push(glass);
            let last = glasses.all.len() - 1;
            self.show_glass(last);
            self.show_tab(1);
            return;
        }
        if let Some((tab, leaf)) = glasses.glass().find(&key) {
            self.show_tab(tab);
            self.focus_leaf(leaf);
            return;
        }
        let glass = glasses.glass_mut();
        match (how, glass.tab_mut()) {
            (Open::Here, Some(tab)) => {
                tab.layout.replace(tab.focus, &key);
            }
            (Open::Right | Open::Below, Some(tab)) => {
                let side = if how == Open::Right {
                    Side::Right
                } else {
                    Side::Below
                };
                tab.focus = tab.layout.split(tab.focus, side, &key);
            }
            _ => {
                glass.tabs.push(Tab {
                    title: None,
                    layout: Layout::pane(&key),
                    focus: 0,
                });
                glass.current = glass.tabs.len();
            }
        }
        glasses.save();
        let current = glasses.glass().current;
        self.show_tab(current);
    }

    pub(crate) fn show_tab(&mut self, index: usize) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let glass = glasses.glass_mut();
        if index > glass.tabs.len() {
            return;
        }
        glass.current = index;
        let focused = glass.tab().and_then(|tab| {
            tab.layout
                .leaves()
                .get(tab.focus)
                .map(|key| key.to_string())
        });
        match focused.and_then(|key| Pane::parse(&key)) {
            Some(pane) => self.focus_pane(&pane),
            None => self.tab = 0,
        }
    }

    fn focus_leaf(&mut self, leaf: usize) {
        let Some(tab) = self
            .glasses
            .as_mut()
            .and_then(|glasses| glasses.glass_mut().tab_mut())
        else {
            return;
        };
        if leaf < tab.layout.leaves().len() {
            tab.focus = leaf;
        }
        let current = self
            .glasses
            .as_ref()
            .map(|glasses| glasses.glass().current)
            .unwrap_or_default();
        self.show_tab(current);
    }

    /// Focus the pane nearest in the arrow's direction, on screen.
    fn move_focus(&mut self, direction: KeyCode) {
        let (rects, focus) = {
            let Some(tab) = self
                .glasses
                .as_ref()
                .and_then(|glasses| glasses.glass().tab())
            else {
                return;
            };
            (self.frame.borrow().glass_leaves.clone(), tab.focus)
        };
        let Some(from) = rects.get(focus) else { return };
        let center = |rect: &Rect| {
            (
                rect.x as i32 + rect.width as i32 / 2,
                rect.y as i32 + rect.height as i32 / 2,
            )
        };
        let (fx, fy) = center(from);
        let best = rects
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != focus)
            .filter(|(_, rect)| match direction {
                KeyCode::Left => rect.x + rect.width <= from.x,
                KeyCode::Right => rect.x >= from.x + from.width,
                KeyCode::Up => rect.y + rect.height <= from.y,
                _ => rect.y >= from.y + from.height,
            })
            .min_by_key(|(_, rect)| {
                let (x, y) = center(rect);
                (x - fx).abs() + (y - fy).abs()
            })
            .map(|(index, _)| index);
        if let Some(index) = best {
            self.focus_leaf(index);
        }
    }

    /// A click inside a pane that is not focused focuses it, and does nothing else.
    pub(crate) fn glass_click(&mut self, column: u16, row: u16) -> bool {
        let Some(tab) = self
            .glasses
            .as_ref()
            .and_then(|glasses| glasses.glass().tab())
        else {
            return false;
        };
        let focus = tab.focus;
        let leaf = self
            .frame
            .borrow()
            .glass_leaves
            .iter()
            .position(|rect| contains(*rect, column, row));
        match leaf {
            Some(leaf) if leaf != focus => {
                self.focus_leaf(leaf);
                true
            }
            _ => false,
        }
    }

    /// Close the focused pane, and its tab with its last pane. Home stays.
    fn close_pane(&mut self) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let glass = glasses.glass_mut();
        let Some(index) = glass.current.checked_sub(1) else {
            self.flash("Home stays open");
            return;
        };
        let tab = glass.tabs.remove(index);
        match tab.layout.remove(tab.focus) {
            Some(layout) => {
                let focus = tab.focus.min(layout.leaves().len() - 1);
                glass.tabs.insert(
                    index,
                    Tab {
                        title: tab.title,
                        layout,
                        focus,
                    },
                );
            }
            None => glass.current = glass.current.min(glass.tabs.len()),
        }
        glasses.save();
        let current = glasses.glass().current;
        self.show_tab(current);
    }

    fn show_glass(&mut self, index: usize) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        if index >= glasses.all.len() {
            return;
        }
        glasses.shown = index;
        glasses.save();
        let current = glasses.glass().current;
        self.show_tab(current);
    }

    fn next_glass(&mut self) {
        let Some(glasses) = self.glasses.as_ref() else {
            return;
        };
        if glasses.all.len() < 2 {
            self.flash("One glass so far: ctrl+k, type a name, “New glass”");
            return;
        }
        let next = (glasses.shown + 1) % glasses.all.len();
        self.show_glass(next);
        let name = self
            .glasses
            .as_ref()
            .map(|glasses| glasses.glass().name.clone())
            .unwrap_or_default();
        self.flash(format!("Glass “{name}”"));
    }

    /// Point stui's own tab and selection at a pane's subject, so its keys act on it.
    fn focus_pane(&mut self, pane: &Pane) {
        let (tab, subject) = match pane {
            Pane::Home(id) => (0, id.clone()),
            Pane::Agent(id) => (1, id.clone()),
            Pane::Mission(id) | Pane::Declaration(id) => (2, id.clone()),
            Pane::Machine(id) => (
                3,
                id.as_deref()
                    .map(|id| id.trim_start_matches("machine/").to_owned()),
            ),
            _ => return,
        };
        self.tab = tab;
        self.terminal = self.terminal.take().filter(|_| tab == 1);
        self.kdl = matches!(pane, Pane::Declaration(_));
        let Some(subject) = subject else { return };
        let mut position = self
            .listing_for(tab, 40)
            .ids
            .iter()
            .position(|id| *id == subject);
        if position.is_none() && tab == 2 {
            // st's own missions are listed only while shown.
            self.system = true;
            position = self
                .listing_for(tab, 40)
                .ids
                .iter()
                .position(|id| *id == subject);
        }
        if let Some(position) = position {
            self.selected[tab] = position;
        }
    }
}

/// A glass's identity: made here, kept for good.
fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The pane a graph subject opens as, for links followed inside a glass.
pub(crate) fn pane_for(id: &str) -> Option<Pane> {
    let id = id.to_owned();
    if id.starts_with("attention/") {
        Some(Pane::Home(Some(id)))
    } else if id.starts_with("mission/") {
        Some(Pane::Mission(Some(id)))
    } else if id.starts_with("agent/") {
        Some(Pane::Agent(Some(id)))
    } else if id.starts_with("machine/") {
        Some(Pane::Machine(Some(id)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glass() -> Ui {
        let mut ui = Ui::new(demo::world());
        ui.glasses = Some(Glasses::open(None, None));
        ui
    }

    fn press(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers) {
        ui.key(KeyEvent::new(code, modifiers));
    }

    fn ctrl(ui: &mut Ui, letter: char) {
        press(ui, KeyCode::Char(letter), KeyModifiers::CONTROL);
    }

    fn typed(ui: &mut Ui, text: &str) {
        for letter in text.chars() {
            press(ui, KeyCode::Char(letter), KeyModifiers::NONE);
        }
    }

    fn screen(ui: &Ui) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The shown tab and every tab's pane keys.
    fn tabs(ui: &Ui) -> (usize, Vec<Vec<String>>) {
        let glass = ui.glasses.as_ref().unwrap().glass();
        (
            glass.current,
            glass
                .tabs
                .iter()
                .map(|tab| {
                    tab.layout
                        .leaves()
                        .iter()
                        .map(|key| key.to_string())
                        .collect()
                })
                .collect(),
        )
    }

    const ATLAS: &str = "agent:agent/example/atlas/builder";
    const WEEKLY: &str = "mission:mission/fleet/release/weekly";

    #[test]
    fn fuzzy_finds_substrings_and_word_starts_but_not_scattered_letters() {
        assert!(fuzzy("atl", "Atlas Builder").is_some());
        assert!(fuzzy("ab", "Atlas Builder").is_some(), "word starts");
        assert!(
            fuzzy("lease", "Release Captain").is_some(),
            "a substring anywhere"
        );
        assert!(fuzzy("rel", "Rekey Worker").is_none(), "l is inside a word");
        assert!(fuzzy("zz", "Atlas Builder").is_none());
        assert!(
            fuzzy("bu", "Atlas Builder") > fuzzy("bu", "Rebuild atlas"),
            "a word start wins"
        );
        assert!(fuzzy("atlas", "Atlas Builder") > fuzzy("ab", "Atlas Builder"));
    }

    #[test]
    fn a_glass_opens_on_home_with_the_status_line_and_plain_stui_has_neither() {
        let ui = glass();
        let shown = screen(&ui);
        let first = shown.lines().next().unwrap();
        assert!(
            first.contains("need you") && first.contains("working") && first.contains("main"),
            "{first}"
        );
        assert!(shown.lines().nth(1).unwrap().contains("Home"));
        assert!(shown.contains("ctrl+k open"));
        let plain = screen(&Ui::new(demo::world()));
        assert!(!plain.contains("ctrl+k") && !plain.lines().next().unwrap().contains("need you"));
    }

    #[test]
    fn the_palette_opens_subjects_in_tabs_and_home_stays() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        assert!(screen(&ui).contains("╭─ open"));
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(tabs(&ui), (1, vec![vec![ATLAS.to_owned()]]));
        // The tab points stui's own selection at the agent, so its keys act on it.
        assert_eq!(
            (ui.tab, ui.selected_id().as_deref()),
            (1, Some("agent/example/atlas/builder"))
        );
        assert!(
            screen(&ui)
                .lines()
                .nth(1)
                .unwrap()
                .contains("◆ Atlas Builder"),
            "it needs the person"
        );
        // Up and Down scroll the agent's pane; they never move to another agent.
        press(&mut ui, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            ui.selected_id().as_deref(),
            Some("agent/example/atlas/builder")
        );

        // Ctrl+T opens beside it; a subject already open goes to its tab.
        ctrl(&mut ui, 'k');
        typed(&mut ui, "weekly release");
        ctrl(&mut ui, 't');
        assert_eq!(tabs(&ui).1.len(), 2);
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(tabs(&ui).0, 1);
        assert_eq!(tabs(&ui).1.len(), 2);

        press(&mut ui, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!((tabs(&ui).0, ui.tab), (0, 0));
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).1.len(), 2, "Home cannot be closed");
        press(&mut ui, KeyCode::PageDown, KeyModifiers::CONTROL);
        assert_eq!(tabs(&ui).0, 1);
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).1, vec![vec![WEEKLY.to_owned()]]);
    }

    #[test]
    fn splits_open_beside_and_below_and_alt_arrows_move_between_them() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        ctrl(&mut ui, 'k');
        typed(&mut ui, "weekly release");
        ctrl(&mut ui, 'v');
        assert_eq!(tabs(&ui).1, vec![vec![ATLAS.to_owned(), WEEKLY.to_owned()]]);
        assert_eq!(ui.tab, 2, "the new pane has the focus");
        let shown = screen(&ui);
        assert!(shown.lines().nth(1).unwrap().contains("Weekly release +1"));
        assert!(shown.contains("│"), "a divider between the panes");

        press(&mut ui, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(
            ui.selected_id().as_deref(),
            Some("agent/example/atlas/builder")
        );
        press(&mut ui, KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(ui.tab, 2);

        ctrl(&mut ui, 'k');
        typed(&mut ui, "harbor");
        let harbor = ui
            .matches(ui.glasses.as_ref().unwrap().palette.as_ref().unwrap())
            .iter()
            .position(|choice| choice.section == 3)
            .unwrap();
        ui.open_choice(Some(harbor), Open::Below);
        assert_eq!(tabs(&ui).1[0].len(), 3);
        screen(&ui);

        // Closing panes keeps the tab until its last one goes.
        ctrl(&mut ui, 'w');
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).1, vec![vec![ATLAS.to_owned()]]);
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui), (0, vec![]));
    }

    #[test]
    fn glasses_are_named_kept_on_the_device_and_switched_with_ctrl_o() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("glasses.json");
        let mut ui = Ui::new(demo::world());
        ui.glasses = Some(Glasses::open(None, Some(store.clone())));
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);

        // A new glass by name, from the palette's glasses section.
        typed(&mut ui, "5");
        typed(&mut ui, "review");
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        let new = ui
            .matches(palette)
            .iter()
            .position(|choice| choice.action == Action::NewGlass("review".into()))
            .unwrap();
        ui.open_choice(Some(new), Open::Here);
        assert_eq!(ui.glasses.as_ref().unwrap().glass().name, "review");
        assert_eq!(tabs(&ui), (0, vec![]), "a new glass starts on Home");
        assert!(screen(&ui).lines().next().unwrap().contains("review ▾"));

        ctrl(&mut ui, 'o');
        assert_eq!(ui.glasses.as_ref().unwrap().glass().name, "main");
        assert_eq!(tabs(&ui).1, vec![vec![ATLAS.to_owned()]]);

        // Another window on this device opens the last glass used, with its tabs.
        let again = Glasses::open(None, Some(store.clone()));
        assert_eq!(again.glass().name, "main");
        assert_eq!(again.all.len(), 2);
        assert_eq!(again.glass().tabs[0].layout.leaves(), [ATLAS]);
        assert_eq!(
            Glasses::open(Some("review".into()), Some(store))
                .glass()
                .name,
            "review"
        );
    }

    #[test]
    fn a_pane_whose_subject_is_gone_says_so() {
        let mut ui = glass();
        ui.open_in_glass(Pane::Agent(Some("agent/example/retired".into())), Open::Tab);
        assert!(screen(&ui).contains("agent:agent/example/retired is gone"));
    }

    #[test]
    fn a_digit_opens_the_palette_at_its_section_and_escape_closes_it() {
        let mut ui = glass();
        typed(&mut ui, "3");
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        assert_eq!(palette.section, Some(2));
        assert!(ui.matches(palette).iter().all(|choice| choice.section == 2));
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.glasses.as_ref().unwrap().palette.is_none());
        assert_eq!(ui.tab, 0, "the sidebar's tabs do not change underneath");
    }
}
