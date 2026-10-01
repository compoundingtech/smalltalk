//! Glasses: stui as named workspaces of splits, each with its own tabs, and a palette, an experiment
//! beside the sidebar layout (`stui --glasses`, mission fleet/stui/glass, design
//! doc/fleet/stui/glass). Plain stui never reaches this module.
//!
//! The focused pane points stui's own tab and selection at its subject, so every key and action
//! the sidebar layout has works the same inside a glass.

use super::glass_store::{self, Stored, StoredGlass, StoredView};
use super::layout::{Group, Layout, Side, Tab};
use super::*;
use ratatui::style::Color;
use st3_client::{GlassBody, GlassLayout, GlassSplit, GlassTab};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The palette's sections, in order; a digit key opens the palette at one.
const SECTIONS: [&str; 6] = [
    "needs you",
    "agents",
    "missions",
    "fleet",
    "glasses",
    "start",
];
const GLASSES: usize = 4;
/// Where each section ranks while a query is typed: agents, missions, then the rest in order.
const RANK: [usize; 6] = [2, 0, 1, 3, 4, 5];
const START: usize = 5;

/// Every glass this window knows, and the one it shows.
pub(crate) struct Glasses {
    all: Vec<Glass>,
    shown: usize,
    palette: Option<Palette>,
    /// Where they are kept on this device; none in the demo.
    store: Option<PathBuf>,
    /// st keeps them too: it granted glasses and sent the person's set.
    graph: bool,
    /// Changes st has not confirmed, newest per glass id, each with the key a retry reuses.
    pending: BTreeMap<String, GlassWrite>,
    /// The focused split fills the glass for now (Ctrl+O), as tmux zooms a pane; this window
    /// only, never stored.
    zoomed: bool,
    /// The glass made at start only because this device had none by the name asked for. If it
    /// is still empty when st first sends the person's glasses, st's glass of that name is
    /// shown instead of keeping both.
    placeholder: Option<String>,
}

/// A change for st to keep. The body travels as JSON text and the idempotency key goes with
/// it, so a retry is the same request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GlassWrite {
    Put {
        id: String,
        body: String,
        base: Option<String>,
        key: String,
    },
    Delete {
        id: String,
        base: Option<String>,
        key: String,
    },
}

impl GlassWrite {
    pub(crate) fn id(&self) -> &str {
        match self {
            GlassWrite::Put { id, .. } | GlassWrite::Delete { id, .. } => id,
        }
    }

    pub(crate) fn key(&self) -> &str {
        match self {
            GlassWrite::Put { key, .. } | GlassWrite::Delete { key, .. } => key,
        }
    }
}

/// A named workspace: splits, each with its own tabs. Home is always the first tab of the
/// first group and cannot be closed.
#[derive(Clone)]
struct Glass {
    /// Stable across renames; the graph will keep the glass under it.
    id: String,
    /// The graph's revision this glass last matched; `None` until st has kept it.
    revision: Option<String>,
    name: String,
    /// Each group's `current` counts Home in group 0: there, 0 is Home.
    layout: Layout,
    /// The focused group.
    focus: usize,
}

/// Where a group's tabs start among what its strip shows: group 0 shows Home first.
fn offset(group: usize) -> usize {
    usize::from(group == 0)
}

impl Glass {
    /// The glass as st keeps it: structure only.
    fn body(&self) -> GlassBody {
        GlassBody {
            name: self.name.clone(),
            layout: to_wire(&self.layout),
        }
    }

    /// Take st's structure, keeping which group and tabs this window shows where they still are.
    fn take(&mut self, body: GlassBody, revision: Option<String>) {
        let current = self
            .layout
            .groups()
            .iter()
            .map(|group| group.current)
            .collect::<Vec<_>>();
        self.name = body.name;
        self.layout = from_wire(body.layout);
        let count = self.layout.groups().len();
        for index in 0..count {
            let shown = self.count(index);
            if let Some(group) = self.layout.group_mut(index) {
                group.current = current
                    .get(index)
                    .copied()
                    .unwrap_or(0)
                    .min(shown.saturating_sub(1));
            }
        }
        self.focus = self.focus.min(count - 1);
        self.revision = revision;
    }

    fn new(name: String) -> Self {
        Self {
            id: new_id(),
            revision: None,
            name,
            layout: Layout::default(),
            focus: 0,
        }
    }

    /// Where this window is in the glass, for this device to remember.
    fn view(&self) -> StoredView {
        StoredView {
            focus: self.focus,
            current: self
                .layout
                .groups()
                .iter()
                .map(|group| group.current)
                .collect(),
        }
    }

    /// The glass as this device last left it, where its structure still allows.
    fn viewed(mut self, view: Option<StoredView>) -> Self {
        let Some(view) = view else { return self };
        let count = self.layout.groups().len();
        for (index, current) in view.current.into_iter().enumerate().take(count) {
            let shown = self.count(index);
            if let Some(group) = self.layout.group_mut(index) {
                group.current = current.min(shown.saturating_sub(1));
            }
        }
        self.focus = view.focus.min(count.saturating_sub(1));
        self
    }

    /// How many tabs group `index`'s strip shows, Home included.
    fn count(&self, index: usize) -> usize {
        self.layout
            .groups()
            .get(index)
            .map_or(0, |group| group.tabs.len() + offset(index))
    }

    /// The tab group `index` shows: `None` on Home or in an empty group.
    fn shown(&self, index: usize) -> Option<&Tab> {
        let groups = self.layout.groups();
        let group = groups.get(index)?;
        group
            .current
            .checked_sub(offset(index))
            .and_then(|tab| group.tabs.get(tab))
    }

    /// The pane key the focused group shows.
    fn focused(&self) -> Option<&str> {
        self.shown(self.focus).map(|tab| tab.pane.as_str())
    }

    fn on_home(&self) -> bool {
        self.focus == 0 && self.layout.groups()[0].current == 0
    }

    /// Where a pane key already shows: (group, tab as its strip counts it).
    fn find(&self, key: &str) -> Option<(usize, usize)> {
        self.layout
            .find(key)
            .map(|(group, tab)| (group, tab + offset(group)))
    }

    /// Group 0 and its Home always stay; this says whether anything else is open.
    fn is_empty(&self) -> bool {
        self.layout == Layout::default()
    }
}

impl Glasses {
    /// The agents whose conversations show in the shown glass: each group's shown tab, the
    /// focused group first.
    pub(crate) fn shown_agents(&self) -> Vec<String> {
        let glass = self.glass();
        let count = glass.layout.groups().len();
        std::iter::once(glass.focus)
            .chain((0..count).filter(|index| *index != glass.focus))
            .filter_map(|index| glass.shown(index))
            .filter_map(|tab| match Pane::parse(&tab.pane) {
                Some(Pane::Agent(Some(id))) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// Open `wanted` (or the last glass used here, or `main`) from what this device keeps.
    pub(crate) fn open(wanted: Option<String>, store: Option<PathBuf>) -> Self {
        let stored = store.as_deref().map(glass_store::load).unwrap_or_default();
        let mut all = stored
            .glasses
            .into_iter()
            .map(|glass| {
                Glass {
                    id: if glass.id.is_empty() {
                        new_id()
                    } else {
                        glass.id
                    },
                    revision: glass.revision,
                    name: glass.name,
                    layout: glass.layout,
                    focus: 0,
                }
                .viewed(glass.view)
            })
            .collect::<Vec<_>>();
        let name = wanted.or(stored.last).unwrap_or_else(|| "main".to_owned());
        let mut placeholder = None;
        let shown = match all.iter().position(|glass| glass.name == name) {
            Some(index) => index,
            None => {
                let glass = Glass::new(name);
                placeholder = Some(glass.id.clone());
                all.push(glass);
                all.len() - 1
            }
        };
        Self {
            all,
            shown,
            palette: None,
            store,
            graph: false,
            zoomed: false,
            pending: BTreeMap::new(),
            placeholder,
        }
    }

    /// A write keeping glass `id` as it is now, when st keeps glasses; it replaces any write
    /// for that glass st has not confirmed.
    fn write(&mut self, id: &str) -> Option<GlassWrite> {
        if !self.graph {
            return None;
        }
        let glass = self.all.iter().find(|glass| glass.id == id)?;
        let write = GlassWrite::Put {
            id: glass.id.clone(),
            body: serde_json::to_string(&glass.body()).ok()?,
            base: glass.revision.clone(),
            key: new_id(),
        };
        self.pending.insert(glass.id.clone(), write.clone());
        Some(write)
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
                    revision: glass.revision.clone(),
                    name: glass.name.clone(),
                    layout: glass.layout.clone(),
                    view: Some(glass.view()),
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
    /// In the focused group's shown tab (or a new tab, from Home or in an empty group).
    #[default]
    Here,
    /// In a new group to the right of the focused one.
    Right,
    /// In a new group below the focused one.
    Below,
    /// In a new tab of the focused group.
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
    /// The first row shown once the wheel moved the list, as content scrolls; `None` keeps the
    /// selection in view, as keys do.
    top: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Open(Pane),
    ShowGlass(usize),
    NewGlass(String),
    RenameGlass(String),
    DuplicateGlass(String),
    CloseGlass,
    /// The new agent form, with what it should do when the query says it.
    NewAgent(Option<String>),
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
    /// Whether a glass is showing its Home tab in the focused group.
    pub(crate) fn on_home(&self) -> bool {
        self.glasses
            .as_ref()
            .is_some_and(|glasses| glasses.glass().on_home())
    }

    /// Whether the glass is split into more than one group.
    pub(crate) fn split_shown(&self) -> bool {
        self.glasses
            .as_ref()
            .is_some_and(|glasses| glasses.glass().layout.groups().len() > 1)
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
        // Starting things: an agent from the query, or the empty form.
        let name = query.trim();
        let start = |label: String, detail: &str, search: String, action| Choice {
            section: START,
            glyph: ("＋", theme::GREEN),
            label,
            detail: detail.to_owned(),
            search,
            action,
        };
        choices.push(start(
            "New agent".into(),
            "ctrl+n",
            "new agent start".into(),
            Action::NewAgent(None),
        ));
        if !name.is_empty() {
            choices.push(start(
                format!("Start an agent: “{name}”"),
                "its first message",
                String::new(),
                Action::NewAgent(Some(name.to_owned())),
            ));
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
                "ctrl+g switches",
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
        // While something is typed, a matching agent ranks first, then missions, then the rest.
        if !palette.query.is_empty() {
            scored.sort_by_key(|(choice, score)| (RANK[choice.section], -score));
        }
        scored.into_iter().map(|(choice, _)| choice).collect()
    }

    pub(crate) fn render_glass(&self, buf: &mut Buffer, area: Rect) {
        let Some(glasses) = &self.glasses else { return };
        let glass = glasses.glass();
        self.status_line(buf, Rect { height: 1, ..area }, glass);
        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(2),
            ..area
        };
        // Zoomed, the focused split takes the whole glass and the others wait unseen.
        let (rects, dividers) = if glasses.zoomed {
            let groups = glass.layout.groups().len();
            let mut rects = vec![Rect::default(); groups];
            rects[glass.focus] = body;
            (rects, Vec::new())
        } else {
            glass.layout.rects(body)
        };
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
        // Each group's content, under its tab strip: where clicks and Alt+arrows find it.
        let contents = rects
            .iter()
            .map(|rect| Rect {
                y: rect.y + 1,
                height: rect.height.saturating_sub(1),
                ..*rect
            })
            .collect::<Vec<_>>();
        self.frame.borrow_mut().glass_leaves = contents.clone();
        // The focused group draws first, through stui's own tab and selection, so it is the
        // pane scrolling keys reach; the others draw from their keys.
        let order = std::iter::once(glass.focus)
            .chain((0..rects.len()).filter(|index| *index != glass.focus));
        for index in order {
            let (Some(rect), Some(content)) = (rects.get(index), contents.get(index)) else {
                continue;
            };
            if rect.is_empty() {
                continue;
            }
            self.tab_strip(buf, Rect { height: 1, ..*rect }, glass, index);
            self.draw_group(buf, *content, glass, index);
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

    /// What group `index` shows: Home, its shown tab's pane, or how to fill it.
    fn draw_group(&self, buf: &mut Buffer, area: Rect, glass: &Glass, index: usize) {
        let focused = index == glass.focus;
        let Some(tab) = glass.shown(index) else {
            if glass.layout.groups()[index].current == 0 && index == 0 {
                // Home is today's Home under a bar for starting things: its list and the
                // selected card.
                self.launcher_bar(buf, Rect { height: 1, ..area });
                let area = Rect {
                    y: area.y + 1,
                    height: area.height.saturating_sub(1),
                    ..area
                };
                if focused {
                    self.draw_body(buf, area);
                } else {
                    self.draw_home(buf, area);
                }
            } else {
                buf.set_stringn(
                    area.x + 2,
                    area.y + 1,
                    "Empty. Ctrl+K opens something here; Ctrl+W closes this split.",
                    area.width.saturating_sub(3) as usize,
                    theme::dim(),
                );
            }
            return;
        };
        // A pane keeps a column clear on each side, as the main area does.
        let content = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        };
        let key = &tab.pane;
        match Pane::parse(key) {
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
                    format!("{key} is gone. Ctrl+W closes this tab."),
                    content.width.saturating_sub(2) as usize,
                    theme::dim(),
                );
            }
            Some(_) if focused => self.draw_main(buf, content),
            Some(pane) => self.draw_pane(buf, content, &pane),
        }
    }

    /// Home's bar: start an agent, split the glass.
    fn launcher_bar(&self, buf: &mut Buffer, area: Rect) {
        let mut x = area.x + 1;
        for (glyph, label, key, hit) in [
            ("＋", "New agent", "ctrl+n", Hit::NewAgent),
            ("⇥", "Split right", "ctrl+v", Hit::Split(true)),
            ("⤓", "Split below", "ctrl+x", Hit::Split(false)),
        ] {
            let line = Line::from(vec![
                Span::styled(
                    format!(" {glyph} "),
                    theme::fg(theme::GREEN).bg(theme::SURFACE0),
                ),
                Span::styled(format!("{label} "), theme::text().bg(theme::SURFACE0)),
                Span::styled(format!("{key} "), theme::dim().bg(theme::SURFACE0)),
            ]);
            let width = line.width() as u16;
            if x + width > area.x + area.width {
                break;
            }
            buf.set_line(x, area.y, &line, width);
            self.hit(Rect { x, width, ..area }, hit);
            x += width + 2;
        }
    }

    /// Home while another group has the focus: its list and card, from Home's own selection.
    fn draw_home(&self, buf: &mut Buffer, area: Rect) {
        let selected = self.listing_for(0, 40).ids.get(self.selected[0]).cloned();
        let side = if self.sidebar && area.width >= 70 {
            (area.width * 3 / 10).clamp(30, 46)
        } else {
            0
        };
        if side > 0 {
            self.draw_pane(
                buf,
                Rect {
                    width: side,
                    ..area
                },
                &Pane::List(0),
            );
            for y in area.y..area.y + area.height {
                buf[(area.x + side, y)]
                    .set_symbol("│")
                    .set_style(Style::default().fg(theme::SURFACE0).bg(theme::BASE));
            }
        }
        let main = Rect {
            x: area.x + side + if side > 0 { 2 } else { 1 },
            width: area
                .width
                .saturating_sub(side + if side > 0 { 3 } else { 2 }),
            ..area
        };
        self.draw_pane(buf, main, &Pane::Home(selected));
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
        // Each count opens the palette at what it counts.
        let mut x = area.x + Line::from(spans.clone()).width() as u16;
        let need_text = if need > 0 {
            format!("◆ {need} need you")
        } else {
            "nothing needs you".to_owned()
        };
        self.hit(
            Rect {
                x,
                width: text::width(&need_text) as u16,
                ..area
            },
            Hit::PaletteSection(0),
        );
        spans.push(if need > 0 {
            Span::styled(need_text, bar(theme::strong(theme::PERSON)))
        } else {
            Span::styled(need_text, bar(theme::dim()))
        });
        let working = self
            .world
            .agents
            .items()
            .iter()
            .filter(|agent| agent.state == AgentState::Working)
            .count();
        spans.push(Span::styled(" · ", bar(theme::dim())));
        x = area.x + Line::from(spans.clone()).width() as u16;
        let working_text = format!("{} {working} working", self.spinner());
        self.hit(
            Rect {
                x,
                width: text::width(&working_text) as u16,
                ..area
            },
            Hit::PaletteSection(1),
        );
        spans.push(Span::styled(working_text, bar(theme::fg(theme::WORKING))));
        let machines = self.world.machines.items();
        let machines_shown = !machines.is_empty();
        if machines_shown {
            let count = |reach: &[Reach]| {
                machines
                    .iter()
                    .filter(|machine| reach.contains(&machine.reach))
                    .count()
            };
            spans.push(Span::styled(" · ", bar(theme::dim())));
            x = area.x + Line::from(spans.clone()).width() as u16;
            spans.push(Span::styled("fleet ", bar(theme::dim())));
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
        let end = area.x + Line::from(spans.clone()).width() as u16;
        if machines_shown {
            self.hit(
                Rect {
                    x,
                    width: end.saturating_sub(x),
                    ..area
                },
                Hit::PaletteSection(3),
            );
        }
        buf.set_line(area.x, area.y, &Line::from(spans), area.width);
        // ▢═▢: two panes and a bridge, a pair of glasses.
        let name = format!(" ▢═▢ {} ▾ ", glass.name);
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

    /// A group's tabs, Home first in group 0; a tab whose subject needs the person shows `◆`.
    fn tab_strip(&self, buf: &mut Buffer, area: Rect, glass: &Glass, index: usize) {
        let group = glass.layout.groups()[index];
        let focused = index == glass.focus;
        // With more than one split, the focused one's strip is lit and marked, so it is plain
        // where the palette and new tabs will open.
        let split = glass.layout.groups().len() > 1;
        let strip = if split && focused {
            theme::SURFACE0
        } else {
            theme::MANTLE
        };
        buf.set_style(area, Style::default().bg(strip));
        if split && focused {
            buf.set_stringn(area.x, area.y, "▌", 1, theme::fg(theme::ACCENT).bg(strip));
        }
        let mut x = area.x + 1;
        let home = (index == 0).then(|| ("Home".to_owned(), false));
        let tabs = group.tabs.iter().map(|tab| {
            let pane = Pane::parse(&tab.pane);
            let title = tab.title.clone().unwrap_or_else(|| {
                pane.as_ref()
                    .map(|pane| self.pane_title(pane))
                    .unwrap_or_else(|| tab.pane.clone())
            });
            let needs = pane.is_some_and(|pane| self.pane_needs_person(&pane));
            (title, needs)
        });
        for (shown, (title, needs)) in home.into_iter().chain(tabs).enumerate() {
            let label = format!(" {}{title} ", if needs { "◆ " } else { "" });
            let label = text::truncate(&label, 28);
            let style = match (shown == group.current, focused) {
                (true, true) => Style::default()
                    .fg(theme::CRUST)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
                (true, false) => theme::text().bg(theme::SURFACE1),
                _ if needs => theme::fg(theme::PERSON).bg(strip),
                (_, true) => theme::fg(theme::SUBTEXT0).bg(strip),
                _ => theme::fg(theme::OVERLAY0).bg(strip),
            };
            let width = text::width(&label) as u16;
            if x + width > area.x + area.width {
                break;
            }
            buf.set_stringn(x, area.y, &label, width as usize, style);
            self.hit(Rect { x, width, ..area }, Hit::GlassTab(index, shown));
            x += width + 1;
        }
        if x + 3 <= area.x + area.width {
            buf.set_stringn(x, area.y, " + ", 3, theme::dim().bg(strip));
            self.hit(
                Rect {
                    x,
                    width: 3,
                    ..area
                },
                Hit::GlassAdd(index),
            );
        }
        let zoomed = self.glasses.as_ref().is_some_and(|glasses| glasses.zoomed);
        if zoomed && focused {
            let label = " ⤢ zoomed · ctrl+o ";
            let width = text::width(label) as u16;
            if x + width < area.x + area.width {
                buf.set_stringn(
                    area.x + area.width - width,
                    area.y,
                    label,
                    width as usize,
                    theme::fg(theme::YELLOW).bg(strip),
                );
            }
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

    /// Where Enter will open the choice, said on the palette's edge.
    fn palette_target(&self, enter: Open) -> String {
        let Some(glass) = self.glasses.as_ref().map(Glasses::glass) else {
            return " open ".to_owned();
        };
        let shown = glass.shown(glass.focus).map(|tab| {
            tab.title.clone().unwrap_or_else(|| {
                Pane::parse(&tab.pane)
                    .map(|pane| self.pane_title(&pane))
                    .unwrap_or_else(|| tab.pane.clone())
            })
        });
        let split = glass.layout.groups().len() > 1;
        let target = match (enter, shown) {
            (Open::Here, Some(title)) => format!("open in place of “{title}”"),
            (Open::Here | Open::Tab, None) if glass.on_home() => "open in a new tab".to_owned(),
            (Open::Here | Open::Tab, None) => "open in this empty split".to_owned(),
            (Open::Tab, Some(title)) => format!("open in a new tab beside “{title}”"),
            (Open::Right, _) => "open in a new split to the right".to_owned(),
            (Open::Below, _) => "open in a new split below".to_owned(),
            (Open::Glass, _) => "open in a new glass".to_owned(),
        };
        let place = if split { " · focused split" } else { "" };
        format!(" {} ", text::truncate(&format!("{target}{place}"), 60))
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
        let title = self.palette_target(palette.enter);
        buf.set_stringn(
            rect.x + 2,
            rect.y,
            &title,
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
        let top = palette
            .top
            .unwrap_or_else(|| selected_row.saturating_sub(list_height.saturating_sub(1)))
            .min(rows.len().saturating_sub(list_height));
        self.frame.borrow_mut().palette_top = top;
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
            "enter open · ctrl+v split right · ctrl+x split below · ctrl+t tab · ctrl+g glass · esc",
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
            palette.top = None;
            match key.code {
                KeyCode::Esc => glasses.palette = None,
                KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
                KeyCode::Down => palette.selected += 1,
                KeyCode::Backspace if palette.query.is_empty() => palette.section = None,
                KeyCode::Backspace | KeyCode::Char('w' | 'u') => {
                    super::edit_text(&mut palette.query, key);
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
                // Ctrl (or Alt, or ⌘) and a digit show only that section; the same again shows all.
                KeyCode::Char(digit @ '1'..='5') if control || alt || command => {
                    let section = digit as usize - '1' as usize;
                    palette.section = (palette.section != Some(section)).then_some(section);
                    palette.selected = 0;
                    palette.top = None;
                }
                KeyCode::Char(letter) if !control && !command => {
                    palette.query.push(letter);
                    palette.selected = 0;
                }
                _ => {}
            }
            self.clamp_palette();
            return true;
        }
        // While the person types, every editing key is the input's (Ctrl+W deletes a word);
        // Esc leaves the input and glasses keys work again.
        if self.editing
            || self.find.is_some()
            || (self.agent_form && self.tab == 1 && self.new_agent.is_some())
            || self.new_mission.is_some()
            || self.chat.as_ref().is_some_and(|chat| chat.editing)
        {
            return false;
        }
        // In an attached terminal every key is the agent's; Ctrl+\ leaves it first.
        if self.terminal.is_some() && self.tab == 1 {
            return false;
        }
        let glass = glasses.glass();
        let tabs = glass.count(glass.focus).max(1);
        let current = glass.layout.groups()[glass.focus].current;
        let subject = glass.focused().is_some();
        let quiet = !self.editing && self.new_mission.is_none() && self.chat.is_none();
        match key.code {
            KeyCode::Char('k') if control || command => self.open_palette(None, Open::Here),
            KeyCode::Char('t') if control => self.open_palette(None, Open::Tab),
            KeyCode::Char('v') if control => self.split_group(Side::Right),
            KeyCode::Char('x') if control => self.split_group(Side::Below),
            KeyCode::Char('w') if control => self.close_tab(),
            KeyCode::Char('n') if control => self.open_new_agent(None),
            KeyCode::Char('o') if control => {
                glasses.zoomed = !glasses.zoomed && glasses.glass().layout.groups().len() > 1;
            }
            KeyCode::Char('g') if control => self.next_glass(),
            KeyCode::Char(digit @ '1'..='9') if alt => self.show_tab(digit as usize - '1' as usize),
            // Next and previous tab in the focused split: Ctrl+PgDn/PgUp, Ctrl+Tab where the
            // terminal reports it, and ] and [ whenever nothing is being typed.
            KeyCode::PageDown | KeyCode::Tab if control => self.show_tab((current + 1) % tabs),
            KeyCode::PageUp | KeyCode::BackTab if control => {
                self.show_tab((current + tabs - 1) % tabs)
            }
            KeyCode::Char(']') if quiet && key.modifiers.is_empty() => {
                self.show_tab((current + 1) % tabs)
            }
            KeyCode::Char('[') if quiet && key.modifiers.is_empty() => {
                self.show_tab((current + tabs - 1) % tabs)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if alt => {
                self.move_focus(key.code)
            }
            // Outside a draft, the sidebar's tab digits open the palette at that section.
            KeyCode::Char(digit @ '1'..='5') if quiet && key.modifiers.is_empty() => {
                self.open_palette(Some(digit as usize - '1' as usize), Open::Here)
            }
            // A mission's k shows its whole declaration, as its card says; Up still scrolls.
            KeyCode::Char('k') if quiet && subject && self.tab == 2 && key.modifiers.is_empty() => {
                self.action_key('k')
            }
            // A subject's tab has no list to move through: these scroll its pane.
            KeyCode::Up | KeyCode::Char('k') if quiet && subject && !control => {
                self.scroll_main(-1)
            }
            KeyCode::Down | KeyCode::Char('j') if quiet && subject && !control => {
                self.scroll_main(1)
            }
            _ => return false,
        }
        true
    }

    /// Pasted text goes into an open palette's query.
    pub(crate) fn paste_into_palette(&mut self, text: &str) -> bool {
        let Some(palette) = self
            .glasses
            .as_mut()
            .and_then(|glasses| glasses.palette.as_mut())
        else {
            return false;
        };
        palette.query.push_str(text);
        palette.selected = 0;
        palette.top = None;
        true
    }

    pub(crate) fn palette_open(&self) -> bool {
        self.glasses
            .as_ref()
            .is_some_and(|glasses| glasses.palette.is_some())
    }

    /// The wheel scrolls the palette's list as content, three rows a step; the selection
    /// stays where it is until a key moves it.
    pub(crate) fn scroll_palette(&mut self, up: bool) {
        let shown = self.frame.borrow().palette_top;
        if let Some(palette) = self
            .glasses
            .as_mut()
            .and_then(|glasses| glasses.palette.as_mut())
        {
            let top = palette.top.unwrap_or(shown);
            palette.top = Some(if up { top.saturating_sub(3) } else { top + 3 });
        }
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
            Action::NewAgent(task) => self.open_new_agent(task),
            Action::NewGlass(name) => {
                let name = glasses.unused_name(&name);
                let glass = Glass::new(name);
                let id = glass.id.clone();
                glasses.all.push(glass);
                let last = glasses.all.len() - 1;
                self.show_glass(last);
                self.glass_changed(&id);
            }
            Action::RenameGlass(name) => {
                if glasses.all.iter().any(|glass| glass.name == name) {
                    self.flash(format!("A glass is already called “{name}”"));
                } else {
                    glasses.glass_mut().name = name;
                    let id = glasses.glass().id.clone();
                    self.glass_changed(&id);
                }
            }
            Action::DuplicateGlass(name) => {
                let mut copy = glasses.glass().clone();
                copy.id = new_id();
                copy.revision = None;
                copy.name = glasses.unused_name(&name);
                let id = copy.id.clone();
                glasses.all.push(copy);
                let last = glasses.all.len() - 1;
                self.show_glass(last);
                self.glass_changed(&id);
            }
            Action::CloseGlass => {
                if glasses.all.len() > 1 {
                    let closed = glasses.all.remove(glasses.shown);
                    let next = glasses.shown.saturating_sub(1);
                    if glasses.graph {
                        let write = GlassWrite::Delete {
                            id: closed.id.clone(),
                            base: closed.revision.clone(),
                            key: new_id(),
                        };
                        glasses.pending.insert(closed.id.clone(), write.clone());
                        self.effects.push(Effect::SaveGlass(write));
                    }
                    self.show_glass(next);
                    self.flash(format!("Closed the glass “{}”", closed.name));
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
            self.show_in(0, 0);
            self.focus_pane(&pane);
            return;
        }
        let key = pane.key();
        if how == Open::Glass {
            let name = glasses.unused_name(&title);
            let mut glass = Glass::new(name);
            glass.layout = Layout::Group(Group::of(Tab::pane(&key)));
            let id = glass.id.clone();
            glasses.all.push(glass);
            let last = glasses.all.len() - 1;
            self.show_glass(last);
            self.show_in(0, 1);
            self.glass_changed(&id);
            return;
        }
        if let Some((group, tab)) = glasses.glass().find(&key) {
            self.show_in(group, tab);
            return;
        }
        let glass = glasses.glass_mut();
        let focus = glass.focus;
        let replace = how == Open::Here && glass.shown(focus).is_some();
        let (group, tab) = match how {
            Open::Right | Open::Below => {
                let side = if how == Open::Right {
                    Side::Right
                } else {
                    Side::Below
                };
                (
                    glass.layout.split(focus, side, Group::of(Tab::pane(&key))),
                    0,
                )
            }
            _ => {
                let Some(group) = glass.layout.group_mut(focus) else {
                    return;
                };
                if replace {
                    let index = group.current - offset(focus);
                    group.tabs[index] = Tab::pane(&key);
                } else {
                    group.tabs.push(Tab::pane(&key));
                }
                let tab = if replace {
                    group.current
                } else {
                    group.tabs.len() - 1 + offset(focus)
                };
                (focus, tab)
            }
        };
        let id = glass.id.clone();
        self.glass_changed(&id);
        self.show_in(group, tab);
    }

    /// Split the focused group: a new empty group beside or below it takes the focus, and the
    /// palette opens to fill it.
    /// Split from a button: right or below.
    pub(crate) fn split(&mut self, right: bool) {
        self.split_group(if right { Side::Right } else { Side::Below });
    }

    fn split_group(&mut self, side: Side) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        glasses.zoomed = false;
        let glass = glasses.glass_mut();
        let group = glass.layout.split(glass.focus, side, Group::default());
        let id = glass.id.clone();
        self.glass_changed(&id);
        self.show_in(group, 0);
        self.open_palette(None, Open::Here);
    }

    /// Keep a changed glass: on this device, and in st when st keeps glasses.
    fn glass_changed(&mut self, id: &str) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        glasses.save();
        if let Some(write) = glasses.write(id) {
            self.effects.push(Effect::SaveGlass(write));
        }
    }

    /// The person's glasses as st has them now. A glass st changed takes st's structure, a
    /// glass made elsewhere appears, and one deleted elsewhere goes, except while a change of
    /// ours is on its way. The first time, glasses kept only on this device move into st.
    pub(crate) fn glasses_from_graph(&mut self, remote: Vec<st3_client::Glass>) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let first = !glasses.graph;
        glasses.graph = true;
        // A glass made only because this device had none by that name gives way to st's.
        if let Some(placeholder) = glasses.placeholder.take()
            && let Some(index) = glasses
                .all
                .iter()
                .position(|glass| glass.id == placeholder && glass.is_empty())
        {
            let name = glasses.all[index].name.clone();
            let theirs = remote.iter().find(|item| {
                !item.deleted && item.body.as_ref().is_some_and(|body| body.name == name)
            });
            if let Some(theirs) = theirs {
                let id = theirs
                    .header
                    .id
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                glasses.all.remove(index);
                let mut glass = Glass::new(name);
                glass.id = id;
                glasses.all.insert(index, glass);
            }
        }
        let shown = glasses.glass().id.clone();
        let mut present = BTreeSet::new();
        for item in remote {
            let (Some(body), false) = (item.body, item.deleted) else {
                continue;
            };
            let id = item
                .header
                .id
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            present.insert(id.clone());
            if glasses.pending.contains_key(&id) {
                continue;
            }
            let revision = Some(item.header.revision);
            match glasses.all.iter_mut().find(|glass| glass.id == id) {
                Some(glass) if glass.revision == revision => {}
                Some(glass) => glass.take(body, revision),
                None => {
                    let mut glass = Glass::new(String::new());
                    glass.id = id;
                    glass.take(body, revision);
                    glasses.all.push(glass);
                }
            }
        }
        let Glasses { all, pending, .. } = glasses;
        let before = all.len();
        all.retain(|glass| {
            present.contains(&glass.id)
                || glass.revision.is_none()
                || pending.contains_key(&glass.id)
        });
        let gone = before - all.len();
        if all.is_empty() {
            all.push(Glass::new("main".into()));
        }
        glasses.shown = glasses
            .all
            .iter()
            .position(|glass| glass.id == shown)
            .unwrap_or(0);
        let unsent = glasses
            .all
            .iter()
            .filter(|glass| glass.revision.is_none() && !glasses.pending.contains_key(&glass.id))
            .filter(|_| first)
            .map(|glass| glass.id.clone())
            .collect::<Vec<_>>();
        for id in unsent {
            if let Some(write) = glasses.write(&id) {
                self.effects.push(Effect::SaveGlass(write));
            }
        }
        if let Some(glasses) = self.glasses.as_ref() {
            glasses.save();
        }
        if gone > 0 {
            self.flash(match gone {
                1 => "A glass was closed elsewhere".to_owned(),
                n => format!("{n} glasses were closed elsewhere"),
            });
        }
        self.show_focused();
    }

    /// st answered a write. A newer write for the same glass waits for its own answer; a
    /// failed one stays pending and goes again when st is reachable.
    pub(crate) fn glass_saved(
        &mut self,
        id: &str,
        key: &str,
        outcome: Result<Option<String>, String>,
    ) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let current = glasses
            .pending
            .get(id)
            .is_some_and(|write| write.key() == key);
        match outcome {
            Ok(revision) => {
                if current {
                    glasses.pending.remove(id);
                }
                if let Some(glass) = glasses.all.iter_mut().find(|glass| glass.id == id)
                    && revision.is_some()
                {
                    glass.revision = revision;
                }
                glasses.save();
            }
            Err(error) if current => self.flash(format!(
                "st did not keep a glass yet ({error}); stui will try again"
            )),
            Err(_) => {}
        }
    }

    /// Writes st has not confirmed, to send again once it is reachable.
    pub(crate) fn unsent_glass_writes(&self) -> Vec<GlassWrite> {
        self.glasses
            .as_ref()
            .map(|glasses| glasses.pending.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Show tab `index` of the focused group.
    pub(crate) fn show_tab(&mut self, index: usize) {
        let focus = self
            .glasses
            .as_ref()
            .map(|glasses| glasses.glass().focus)
            .unwrap_or_default();
        self.show_in(focus, index);
    }

    /// Focus group `group` and show its tab `tab` (as its strip counts them). A tab it does
    /// not have leaves the group showing what it showed.
    pub(crate) fn show_in(&mut self, group: usize, tab: usize) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let glass = glasses.glass_mut();
        let count = glass.count(group);
        let Some(shown) = glass.layout.group_mut(group) else {
            return;
        };
        if tab < count {
            shown.current = tab;
        }
        glass.focus = group;
        // Where the person is, remembered on this device for the next start.
        glasses.save();
        self.show_focused();
    }

    pub(crate) fn focus_group(&mut self, group: usize) {
        self.show_in(group, usize::MAX);
    }

    /// Bring stui's own tab and selection back to the focused pane's subject when they drifted:
    /// at start, before the lists arrive, the subject cannot be selected yet.
    pub(crate) fn resync_focus(&mut self) {
        let Some(pane) = self
            .glasses
            .as_ref()
            .and_then(|glasses| glasses.glass().focused())
            .and_then(Pane::parse)
        else {
            return;
        };
        let Some((tab, subject)) = pane_subject(&pane) else {
            return;
        };
        if self.tab != tab || subject.is_some_and(|subject| self.selected_id() != Some(subject)) {
            self.focus_pane(&pane);
        }
    }

    /// Point stui's own tab and selection at what the focused group shows.
    pub(crate) fn show_focused(&mut self) {
        let Some(glasses) = self.glasses.as_ref() else {
            return;
        };
        let glass = glasses.glass();
        let focused = glass.focused().and_then(Pane::parse);
        match focused {
            Some(pane) => self.focus_pane(&pane),
            None => {
                self.tab = 0;
                self.terminal = None;
                self.kdl = false;
            }
        }
    }

    /// Focus the group nearest in the arrow's direction, on screen; a zoomed split comes back
    /// to its place first.
    fn move_focus(&mut self, direction: KeyCode) {
        if let Some(glasses) = self.glasses.as_mut()
            && glasses.zoomed
        {
            glasses.zoomed = false;
            return;
        }
        let Some(focus) = self.glasses.as_ref().map(|glasses| glasses.glass().focus) else {
            return;
        };
        let rects = self.frame.borrow().glass_leaves.clone();
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
                // A group's strip sits above its content, so compare whole groups.
                KeyCode::Up => rect.y + rect.height <= from.y.saturating_sub(1),
                _ => rect.y > from.y + from.height,
            })
            .min_by_key(|(_, rect)| {
                let (x, y) = center(rect);
                (x - fx).abs() + (y - fy).abs()
            })
            .map(|(index, _)| index);
        if let Some(index) = best {
            self.focus_group(index);
        }
    }

    /// A click inside a group that is not focused focuses it, and does nothing else. While the
    /// palette is open, only its rows take clicks; a click elsewhere closes it.
    pub(crate) fn glass_click(&mut self, column: u16, row: u16) -> bool {
        if self.palette_open() {
            let on_palette = self.frame.borrow().hits.iter().any(|(rect, hit)| {
                matches!(hit, Hit::PaletteChoice(_)) && contains(*rect, column, row)
            });
            if !on_palette && let Some(glasses) = self.glasses.as_mut() {
                glasses.palette = None;
            }
            return !on_palette;
        }
        let Some(focus) = self.glasses.as_ref().map(|glasses| glasses.glass().focus) else {
            return false;
        };
        let group = self
            .frame
            .borrow()
            .glass_leaves
            .iter()
            .position(|rect| contains(*rect, column, row));
        match group {
            Some(group) if group != focus => {
                // A click on another split's message box focuses the split and the box.
                let composer = self.frame.borrow().hits.iter().rev().any(|(rect, hit)| {
                    matches!(hit, Hit::Composer) && contains(*rect, column, row)
                });
                self.focus_group(group);
                !composer
            }
            _ => false,
        }
    }

    /// Close the focused tab when it is a form that was just finished or cancelled.
    pub(crate) fn close_form_tab(&mut self) {
        let form = self
            .glasses
            .as_ref()
            .and_then(|glasses| glasses.glass().focused())
            .is_some_and(|key| key == Pane::NewAgent.key());
        if form {
            self.close_tab();
        }
    }

    /// Close the focused group's shown tab; an empty group other than the first goes with it.
    /// Home stays.
    fn close_tab(&mut self) {
        let Some(glasses) = self.glasses.as_mut() else {
            return;
        };
        let glass = glasses.glass_mut();
        if glass.on_home() {
            self.flash("Home stays open");
            return;
        }
        let focus = glass.focus;
        let count = glass.layout.groups().len();
        let Some(group) = glass.layout.group_mut(focus) else {
            return;
        };
        match group.current.checked_sub(offset(focus)) {
            Some(index) if index < group.tabs.len() => {
                group.tabs.remove(index);
                let shown = group.tabs.len() + offset(focus);
                group.current = group.current.min(shown.saturating_sub(1));
            }
            _ => {}
        }
        if group.tabs.is_empty() && focus > 0 && count > 1 {
            let layout = std::mem::take(&mut glass.layout);
            glass.layout = layout.remove(focus).unwrap_or_default();
            glass.focus = focus - 1;
        }
        let id = glass.id.clone();
        self.glass_changed(&id);
        self.show_focused();
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
        self.show_focused();
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
        let Some((tab, subject)) = pane_subject(pane) else {
            return;
        };
        self.tab = tab;
        self.terminal = self.terminal.take().filter(|_| tab == 1);
        self.kdl = matches!(pane, Pane::Declaration(_));
        self.agent_form = matches!(pane, Pane::NewAgent);
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

/// The sidebar tab and the selected id a pane's subject is, where it has one.
fn pane_subject(pane: &Pane) -> Option<(usize, Option<String>)> {
    Some(match pane {
        Pane::Home(id) => (0, id.clone()),
        Pane::Agent(id) => (1, id.clone()),
        Pane::Mission(id) | Pane::Declaration(id) => (2, id.clone()),
        Pane::NewAgent => (1, None),
        Pane::Machine(id) => (
            3,
            id.as_deref()
                .map(|id| id.trim_start_matches("machine/").to_owned()),
        ),
        _ => return None,
    })
}

/// A layout as st's client types spell it. stui splits two at a time; a longer split nests.
fn to_wire(layout: &Layout) -> GlassLayout {
    match layout {
        Layout::Group(group) => GlassLayout::Group {
            tabs: group
                .tabs
                .iter()
                .map(|tab| GlassTab {
                    title: tab.title.clone(),
                    pane: tab.pane.clone(),
                })
                .collect(),
        },
        Layout::Split { split, children } => match children.as_slice() {
            [] => GlassLayout::Group { tabs: Vec::new() },
            [only] => to_wire(only),
            [first, rest @ ..] => GlassLayout::Split {
                split: match split {
                    Side::Right => GlassSplit::Right,
                    Side::Below => GlassSplit::Below,
                },
                children: [
                    Box::new(to_wire(first)),
                    Box::new(to_wire(&Layout::Split {
                        split: *split,
                        children: rest.to_vec(),
                    })),
                ],
            },
        },
    }
}

fn from_wire(layout: GlassLayout) -> Layout {
    match layout {
        GlassLayout::Group { tabs } => Layout::Group(Group {
            tabs: tabs
                .into_iter()
                .map(|tab| Tab {
                    title: tab.title,
                    pane: tab.pane,
                })
                .collect(),
            current: 0,
        }),
        GlassLayout::Split { split, children } => {
            let [first, second] = children;
            Layout::Split {
                split: match split {
                    GlassSplit::Right => Side::Right,
                    GlassSplit::Below => Side::Below,
                },
                children: vec![from_wire(*first), from_wire(*second)],
            }
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

    /// The focused group, the tab it shows (Home is group 0's 0), and every group's pane keys.
    fn tabs(ui: &Ui) -> (usize, usize, Vec<Vec<String>>) {
        let glass = ui.glasses.as_ref().unwrap().glass();
        (
            glass.focus,
            glass.layout.groups()[glass.focus].current,
            glass
                .layout
                .groups()
                .iter()
                .map(|group| group.tabs.iter().map(|tab| tab.pane.clone()).collect())
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
        assert_eq!(tabs(&ui), (0, 1, vec![vec![ATLAS.to_owned()]]));
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
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned(), WEEKLY.to_owned()]]);
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(tabs(&ui).1, 1);
        assert_eq!(tabs(&ui).2[0].len(), 2);

        // Here replaces the shown tab.
        ctrl(&mut ui, 'k');
        typed(&mut ui, "harbor");
        let harbor = ui
            .matches(ui.glasses.as_ref().unwrap().palette.as_ref().unwrap())
            .iter()
            .position(|choice| choice.section == 3)
            .unwrap();
        ui.open_choice(Some(harbor), Open::Here);
        assert_eq!(tabs(&ui).2[0][0], "machine:machine/harbor");
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);

        press(&mut ui, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!((tabs(&ui).1, ui.tab), (0, 0));
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).2[0].len(), 2, "Home cannot be closed");
        typed(&mut ui, "]");
        assert_eq!(tabs(&ui).1, 1);
        typed(&mut ui, "[");
        assert_eq!(tabs(&ui).1, 0);
        press(&mut ui, KeyCode::PageDown, KeyModifiers::CONTROL);
        assert_eq!(tabs(&ui).1, 1);
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).2, vec![vec![WEEKLY.to_owned()]]);
    }

    #[test]
    fn splits_each_have_their_own_tabs_and_alt_arrows_move_between_them() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        ctrl(&mut ui, 'k');
        typed(&mut ui, "weekly release");
        ctrl(&mut ui, 'v');
        assert_eq!(
            tabs(&ui),
            (1, 0, vec![vec![ATLAS.to_owned()], vec![WEEKLY.to_owned()]])
        );
        assert_eq!(ui.tab, 2, "the new split has the focus");
        let shown = screen(&ui);
        let strip = shown.lines().nth(1).unwrap();
        assert!(
            strip.contains("Home") && strip.contains("Weekly release") && strip.contains("▌"),
            "each split has its own strip, the focused one marked: {strip}"
        );
        assert!(shown.contains("│"), "a divider between the splits");

        // A new tab opens in the focused split, and the palette says so.
        ctrl(&mut ui, 't');
        assert!(screen(&ui).contains("open in a new tab beside “Weekly release” · focused split"));
        typed(&mut ui, "harbor");
        let harbor = ui
            .matches(ui.glasses.as_ref().unwrap().palette.as_ref().unwrap())
            .iter()
            .position(|choice| choice.section == 3)
            .unwrap();
        ui.open_choice(Some(harbor), Open::Tab);
        assert_eq!(
            tabs(&ui),
            (
                1,
                1,
                vec![
                    vec![ATLAS.to_owned()],
                    vec![WEEKLY.to_owned(), "machine:machine/harbor".to_owned()]
                ]
            )
        );

        press(&mut ui, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(
            ui.selected_id().as_deref(),
            Some("agent/example/atlas/builder")
        );
        press(&mut ui, KeyCode::Right, KeyModifiers::ALT);
        assert_eq!((tabs(&ui).0, ui.tab), (1, 3));

        // Ctrl+X splits below right away: an empty split, focused, with the palette open.
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        ctrl(&mut ui, 'x');
        assert_eq!(tabs(&ui).0, 2);
        assert!(ui.glasses.as_ref().unwrap().palette.is_some());
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen(&ui).contains("Empty. Ctrl+K opens something here"));
        press(&mut ui, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(tabs(&ui).0, 1);
        press(&mut ui, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(tabs(&ui).0, 2);

        // Closing: the empty split goes, then tabs, then their split with its last tab.
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui).2.len(), 2);
        assert_eq!(tabs(&ui).0, 1);
        ctrl(&mut ui, 'w');
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui), (0, 1, vec![vec![ATLAS.to_owned()]]));
        ctrl(&mut ui, 'w');
        assert_eq!(tabs(&ui), (0, 0, vec![vec![]]));
    }

    #[test]
    fn typing_owns_the_editing_keys_and_enter_keeps_the_input() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.live = true;
        typed(&mut ui, "c");
        assert!(ui.editing);
        typed(&mut ui, "ship it now");
        ctrl(&mut ui, 'w');
        assert_eq!(
            tabs(&ui).2,
            vec![vec![ATLAS.to_owned()]],
            "Ctrl+W kept the tab"
        );
        let draft = |ui: &Ui| {
            ui.conversation_state
                .drafts
                .get("agent/example/atlas/builder")
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(draft(&ui), "ship it ");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert!(ui.editing, "a send keeps the input focused");
        assert_eq!(draft(&ui), "");
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        ctrl(&mut ui, 'w');
        assert_eq!(
            tabs(&ui).2,
            vec![Vec::<String>::new()],
            "after Esc, Ctrl+W closes"
        );
    }

    #[test]
    fn every_agent_on_screen_is_followed_and_only_the_focused_box_takes_keys() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.open_in_glass(
            Pane::Mission(Some("mission/fleet/release/weekly".into())),
            Open::Right,
        );
        assert_eq!(ui.live_conversations(), ["agent/example/atlas/builder"]);
        let other = ui
            .world
            .agents
            .items()
            .iter()
            .map(|agent| agent.id.clone())
            .find(|id| id != "agent/example/atlas/builder")
            .unwrap();
        ui.open_in_glass(Pane::Agent(Some(other.clone())), Open::Below);
        assert_eq!(
            ui.live_conversations(),
            [other.clone(), "agent/example/atlas/builder".to_owned()],
            "the focused one first"
        );
        ui.live = true;
        typed(&mut ui, "c");
        let shown = screen(&ui);
        assert_eq!(shown.matches("c or click").count(), 0, "{shown}");
        assert_eq!(shown.matches('█').count(), 1, "one cursor: {shown}");
        assert!(
            shown.contains("· click"),
            "the other box says how to reach it"
        );
    }

    #[test]
    fn a_message_that_did_not_arrive_can_be_sent_again_or_cleared() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.live = true;
        if let Some(Load::Ready(entries)) = ui
            .world
            .conversations
            .get_mut("agent/example/atlas/builder")
        {
            entries.push(Entry {
                id: "pending:token-1".into(),
                at: "17:35".into(),
                body: Body::Pending {
                    text: "hello".into(),
                    failed: Some("deadline exceeded".into()),
                    unconfirmed: true,
                },
            });
        }
        assert!(screen(&ui).contains("unconfirmed, st did not answer"));
        typed(&mut ui, "r");
        typed(&mut ui, "x");
        assert_eq!(
            std::mem::take(&mut ui.effects),
            [
                Effect::Resend {
                    entry: "pending:token-1".into()
                },
                Effect::Forget {
                    entry: "pending:token-1".into()
                }
            ]
        );
    }

    #[test]
    fn a_missions_k_shows_its_declaration_and_ctrl_digits_pick_a_palette_section() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Mission(Some("mission/fleet/release/weekly".into())),
            Open::Tab,
        );
        typed(&mut ui, "k");
        assert!(ui.kdl, "k shows the whole declaration");
        typed(&mut ui, "k");
        assert!(!ui.kdl);
        ctrl(&mut ui, 'k');
        press(&mut ui, KeyCode::Char('3'), KeyModifiers::CONTROL);
        let section = |ui: &Ui| {
            ui.glasses
                .as_ref()
                .unwrap()
                .palette
                .as_ref()
                .unwrap()
                .section
        };
        assert_eq!(section(&ui), Some(2));
        press(&mut ui, KeyCode::Char('3'), KeyModifiers::CONTROL);
        assert_eq!(section(&ui), None, "the same again shows every section");
        // The status line's counts open the palette at what they count.
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        screen(&ui);
        let need = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::PaletteSection(0)))
            .map(|(rect, _)| *rect)
            .unwrap();
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: need.x + 1,
            row: need.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(section(&ui), Some(0));
    }

    #[test]
    fn slash_finds_text_in_a_conversation_newest_first() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        let said = |ui: &Ui| {
            let Some(Load::Ready(entries)) =
                ui.world.conversations.get("agent/example/atlas/builder")
            else {
                panic!("the demo agent has a conversation")
            };
            entries.clone()
        };
        // A word the demo conversation says more than once.
        let word = "the";
        assert!(said(&ui).len() > 1);
        typed(&mut ui, "/");
        typed(&mut ui, word);
        let shown = screen(&ui);
        let find = ui.find.as_ref().unwrap();
        let count = find.count.get();
        assert!(count > 1, "{shown}");
        assert!(shown.contains(&format!("1 of {count}")), "{shown}");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        screen(&ui);
        assert!(screen(&ui).contains(&format!("2 of {count}")));
        press(&mut ui, KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(screen(&ui).contains(&format!("1 of {count}")));
        // Typing goes to the find bar, not to glasses.
        ctrl(&mut ui, 'w');
        assert_eq!(ui.find.as_ref().unwrap().query, "");
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned()]]);
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.find.is_none());
        assert!(screen(&ui).contains("Message Atlas Builder"));
    }

    #[test]
    fn ctrl_o_zooms_the_focused_split_and_back() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.open_in_glass(
            Pane::Mission(Some("mission/fleet/release/weekly".into())),
            Open::Right,
        );
        assert!(screen(&ui).contains("Atlas Builder"));
        ctrl(&mut ui, 'o');
        let zoomed = screen(&ui);
        assert!(zoomed.contains("zoomed · ctrl+o"), "{zoomed}");
        assert!(
            !zoomed.lines().nth(1).unwrap().contains("Atlas Builder"),
            "only the focused split shows"
        );
        ctrl(&mut ui, 'o');
        assert!(
            screen(&ui)
                .lines()
                .nth(1)
                .unwrap()
                .contains("Atlas Builder")
        );
        // A move between splits brings a zoomed one back first.
        ctrl(&mut ui, 'o');
        press(&mut ui, KeyCode::Left, KeyModifiers::ALT);
        assert!(!ui.glasses.as_ref().unwrap().zoomed);
        assert_eq!(tabs(&ui).0, 1);
    }

    #[test]
    fn a_paste_lands_whole_and_a_pasted_image_path_attaches_to_the_message() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.live = true;
        // A paste over the conversation starts a message; its newlines never send it.
        ui.paste("first line\r\nsecond line".into());
        assert!(ui.editing);
        assert!(ui.effects.is_empty());
        assert_eq!(
            ui.conversation_state.drafts["agent/example/atlas/builder"],
            "first line\nsecond line"
        );
        // A dropped image file becomes an attachment, shown as a chip.
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        ui.paste(image.display().to_string());
        assert!(screen(&ui).contains("▣ 1 image"), "{}", screen(&ui));
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        let sent = std::mem::take(&mut ui.effects);
        assert!(
            matches!(&sent[..], [Effect::Send { text, .. }]
                if text == &format!("first line\nsecond line\n\n[image: {}]", image.display())),
            "{sent:?}"
        );
        assert!(
            !screen(&ui).contains("▣ 1 image"),
            "sent images leave the box"
        );
    }

    #[test]
    fn an_attached_image_shows_a_thumbnail_where_the_terminal_can_draw_one() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.live = true;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("red.png");
        // Stripes, so half blocks must draw both halves of a cell.
        image::RgbImage::from_fn(32, 32, |_, y| {
            if (y / 4) % 2 == 0 {
                image::Rgb([243, 139, 168])
            } else {
                image::Rgb([137, 180, 250])
            }
        })
        .save(&path)
        .unwrap();
        ui.paste(path.display().to_string());
        let without = screen(&ui);
        assert!(without.contains("▣ 1 image 32×32"), "{without}");
        // Half blocks stand in for any terminal; kitty and sixel draw the real image.
        ui.picker = Some(ratatui_image::picker::Picker::halfblocks());
        let with = screen(&ui);
        assert!(with.contains('▀') || with.contains('▄'), "{with}");
    }

    #[test]
    fn a_new_agent_starts_from_a_form_in_its_own_tab() {
        let mut ui = glass();
        ui.live = true;
        // Home's bar and Ctrl+N both open the form; Ctrl+K can start one from a sentence.
        assert!(screen(&ui).contains("New agent ctrl+n"));
        ctrl(&mut ui, 'k');
        typed(&mut ui, "fix the login test");
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        let start = ui
            .matches(palette)
            .iter()
            .position(|choice| choice.action == Action::NewAgent(Some("fix the login test".into())))
            .unwrap();
        ui.open_choice(Some(start), Open::Here);
        assert_eq!(tabs(&ui).2, vec![vec!["new-agent:".to_owned()]]);
        let shown = screen(&ui);
        assert!(
            shown.contains("fix the login test") && shown.contains("claude"),
            "{shown}"
        );
        let name = ui.new_agent.as_ref().unwrap().name.clone();
        assert!(name.contains('-'), "a name is made up: {name}");
        // Typing goes to the form; Tab moves on; ← → choose.
        typed(&mut ui, ", then open a PR");
        press(&mut ui, KeyCode::Tab, KeyModifiers::NONE);
        ctrl(&mut ui, 'u');
        typed(&mut ui, "login fixer");
        press(&mut ui, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut ui, KeyCode::Right, KeyModifiers::NONE);
        press(&mut ui, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut ui, KeyCode::Right, KeyModifiers::NONE);
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            std::mem::take(&mut ui.effects),
            [Effect::CreateAgent {
                name: "loginfixer".into(),
                harness: "codex".into(),
                model: Some("gpt-6-sol".into()),
                effort: None,
                host: None,
                message: Some("fix the login test, then open a PR".into()),
            }],
            "a name keeps to letters, digits, dots and dashes"
        );
        // Once st starts it, its conversation takes the form's place.
        ui.agent_started("agent/example/atlas/builder".into());
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned()]]);
        assert!(ui.new_agent.is_none());
        // Ctrl+N again opens a fresh form; Esc closes it and its tab.
        ctrl(&mut ui, 'n');
        assert_eq!(tabs(&ui).2[0].len(), 2);
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned()]]);
    }

    #[test]
    fn the_wheel_moves_the_palette_and_nothing_behind_it() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        let wheel = |ui: &mut Ui, kind| {
            ui.mouse(MouseEvent {
                kind,
                column: 60,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })
        };
        let before = screen(&ui);
        wheel(&mut ui, MouseEventKind::ScrollDown);
        wheel(&mut ui, MouseEventKind::ScrollDown);
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        assert_eq!(
            (palette.selected, palette.top),
            (0, Some(6)),
            "content scrolls"
        );
        assert_ne!(screen(&ui), before);
        wheel(&mut ui, MouseEventKind::ScrollUp);
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        assert_eq!(palette.top, Some(3));
        assert_eq!(ui.list_top.borrow()[0], 0, "the list behind did not scroll");
        // A key brings the selection back into view.
        press(&mut ui, KeyCode::Down, KeyModifiers::NONE);
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        assert_eq!((palette.selected, palette.top), (1, None));
    }

    #[test]
    fn a_click_in_another_split_focuses_it() {
        let mut ui = glass();
        ui.open_in_glass(
            Pane::Agent(Some("agent/example/atlas/builder".into())),
            Open::Tab,
        );
        ui.open_in_glass(
            Pane::Mission(Some("mission/fleet/release/weekly".into())),
            Open::Right,
        );
        screen(&ui);
        let left = ui.frame.borrow().glass_leaves[0];
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: left.x + 2,
            row: left.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!((tabs(&ui).0, ui.tab), (0, 1));
    }

    #[test]
    fn glasses_are_named_kept_on_the_device_and_switched_with_ctrl_g() {
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
        assert_eq!(
            tabs(&ui),
            (0, 0, vec![vec![]]),
            "a new glass starts on Home"
        );
        assert!(screen(&ui).lines().next().unwrap().contains("review ▾"));

        ctrl(&mut ui, 'g');
        assert_eq!(ui.glasses.as_ref().unwrap().glass().name, "main");
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned()]]);

        // Another window on this device opens the last glass used, with its tabs, on the tab
        // it was showing.
        let again = Glasses::open(None, Some(store.clone()));
        assert_eq!(again.glass().name, "main");
        assert_eq!(again.all.len(), 2);
        assert_eq!(again.glass().focused(), Some(ATLAS));
        assert_eq!(again.glass().layout.groups()[0].tabs, [Tab::pane(ATLAS)]);
        assert_eq!(
            Glasses::open(Some("review".into()), Some(store))
                .glass()
                .name,
            "review"
        );
    }

    /// A glass as st sends it.
    fn graph_glass(id: &str, revision: &str, name: &str, panes: &[&str]) -> st3_client::Glass {
        serde_json::from_value(serde_json::json!({
            "kind": "glass", "id": format!("glass/person/avery/{id}"), "revision": revision,
            "updated_at": "2026-10-01T09:00:00Z", "deleted": false,
            "base_revision": null, "replaced_revision": null,
            "body": {"name": name, "layout": {"tabs": panes.iter().map(|pane| serde_json::json!({"pane": pane})).collect::<Vec<_>>()}},
        }))
        .unwrap()
    }

    /// The glass writes stui asked to send, leaving other effects.
    fn writes(ui: &mut Ui) -> Vec<GlassWrite> {
        std::mem::take(&mut ui.effects)
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::SaveGlass(write) => Some(write),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn layouts_cross_the_wire_unchanged() {
        let mut layout = Layout::Group(Group::of(Tab::pane("agent:a")));
        layout.split(0, Side::Right, Group::of(Tab::pane("mission:m")));
        layout.split(1, Side::Below, Group::default());
        layout.group_mut(2).unwrap().tabs.push(Tab {
            title: Some("hosts".into()),
            pane: "machine:h".into(),
        });
        assert_eq!(from_wire(to_wire(&layout)), layout);
        // A split of three nests as two.
        let three = Layout::Split {
            split: Side::Right,
            children: vec![Layout::default(), Layout::default(), Layout::default()],
        };
        assert_eq!(from_wire(to_wire(&three)).groups().len(), 3);
    }

    #[test]
    fn st_keeps_glasses_once_it_sends_them_and_device_glasses_move_in_once() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            writes(&mut ui).is_empty(),
            "nothing is written before st sends glasses"
        );
        let local = ui.glasses.as_ref().unwrap().glass().id.clone();

        // st has one glass made elsewhere; this device's glass moves in.
        ui.glasses_from_graph(vec![graph_glass("0190-a", "r1", "review", &[WEEKLY])]);
        let sent = writes(&mut ui);
        assert_eq!(sent.len(), 1);
        let GlassWrite::Put {
            id,
            body,
            base,
            key,
        } = &sent[0]
        else {
            panic!("a put")
        };
        assert_eq!((id, base), (&local, &None));
        let body: GlassBody = serde_json::from_str(body).unwrap();
        assert_eq!(body.name, "main");
        assert_eq!(
            body.layout,
            GlassLayout::Group {
                tabs: vec![GlassTab {
                    title: None,
                    pane: ATLAS.into()
                }]
            }
        );
        let names = ui
            .glasses
            .as_ref()
            .unwrap()
            .all
            .iter()
            .map(|glass| glass.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, ["main", "review"]);
        assert_eq!(
            ui.glasses.as_ref().unwrap().glass().name,
            "main",
            "still showing main"
        );

        // Until st answers, the write is pending, and a snapshot without it does not drop it.
        ui.glasses_from_graph(vec![graph_glass("0190-a", "r1", "review", &[WEEKLY])]);
        assert!(writes(&mut ui).is_empty(), "no glass moves in twice");
        assert_eq!(
            ui.unsent_glass_writes(),
            sent,
            "a retry reuses the same key"
        );
        ui.glass_saved(&local, key, Ok(Some("r7".into())));
        assert!(ui.unsent_glass_writes().is_empty());
        assert_eq!(
            ui.glasses.as_ref().unwrap().glass().revision.as_deref(),
            Some("r7")
        );

        // A change made elsewhere arrives.
        ui.glasses_from_graph(vec![
            graph_glass(&local, "r8", "main", &[ATLAS, WEEKLY]),
            graph_glass("0190-a", "r1", "review", &[WEEKLY]),
        ]);
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned(), WEEKLY.to_owned()]]);

        // A change here goes to st on top of what st last sent.
        press(&mut ui, KeyCode::Char('3'), KeyModifiers::ALT);
        ctrl(&mut ui, 'w');
        let sent = writes(&mut ui);
        assert!(
            matches!(&sent[..], [GlassWrite::Put { base, .. }] if base.as_deref() == Some("r8"))
        );
        // A failure keeps it for the next try.
        ui.glass_saved(&local, sent[0].key(), Err("member restarting".into()));
        assert_eq!(ui.unsent_glass_writes(), sent);
    }

    #[test]
    fn a_device_with_no_glasses_shows_sts_glass_of_that_name_instead_of_making_another() {
        let mut ui = glass();
        assert_eq!(ui.glasses.as_ref().unwrap().glass().name, "main");
        ui.glasses_from_graph(vec![graph_glass("0190-m", "r4", "main", &[ATLAS])]);
        assert!(writes(&mut ui).is_empty(), "nothing new goes to st");
        let glasses = ui.glasses.as_ref().unwrap();
        assert_eq!(glasses.all.len(), 1);
        assert_eq!(glasses.glass().id, "0190-m");
        assert_eq!(tabs(&ui).2, vec![vec![ATLAS.to_owned()]]);
    }

    #[test]
    fn a_glass_closed_elsewhere_goes_and_closing_one_here_deletes_it_in_st() {
        let mut ui = glass();
        let main = ui.glasses.as_ref().unwrap().glass().id.clone();
        ui.glasses_from_graph(vec![
            graph_glass(&main, "r1", "main", &[]),
            graph_glass("0190-b", "r2", "spare", &[]),
            graph_glass("0190-c", "r3", "old", &[]),
        ]);
        writes(&mut ui);
        assert_eq!(ui.glasses.as_ref().unwrap().all.len(), 3);
        ui.glasses_from_graph(vec![
            graph_glass(&main, "r1", "main", &[]),
            graph_glass("0190-b", "r2", "spare", &[]),
        ]);
        assert_eq!(
            ui.glasses.as_ref().unwrap().all.len(),
            2,
            "old was closed elsewhere"
        );

        // Close "spare" from here: st is told to delete it, against what st last sent.
        ui.show_glass(1);
        ui.open_palette(Some(GLASSES), Open::Here);
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        let close = ui
            .matches(palette)
            .iter()
            .position(|choice| choice.action == Action::CloseGlass)
            .unwrap();
        ui.open_choice(Some(close), Open::Here);
        let sent = writes(&mut ui);
        assert!(
            matches!(&sent[..], [GlassWrite::Delete { id, base, .. }] if id == "0190-b" && base.as_deref() == Some("r2")),
            "{sent:?}"
        );
    }

    #[test]
    fn a_pane_whose_subject_is_gone_says_so() {
        let mut ui = glass();
        ui.open_in_glass(Pane::Agent(Some("agent/example/retired".into())), Open::Tab);
        assert!(
            screen(&ui).contains("agent:agent/example/retired is gone. Ctrl+W closes this tab")
        );
    }

    #[test]
    fn a_typed_query_ranks_agents_then_missions_then_the_rest() {
        let mut ui = glass();
        ctrl(&mut ui, 'k');
        typed(&mut ui, "a");
        let palette = ui.glasses.as_ref().unwrap().palette.as_ref().unwrap();
        let sections = ui
            .matches(palette)
            .iter()
            .map(|choice| choice.section)
            .collect::<Vec<_>>();
        let first = |section| sections.iter().position(|each| *each == section);
        assert_eq!(sections.first(), Some(&1), "{sections:?}");
        let (missions, needs) = (first(2).unwrap(), first(0).unwrap());
        assert!(missions < needs, "missions before needs you: {sections:?}");
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
