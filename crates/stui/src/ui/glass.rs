//! Glasses: stui as named tabs of panes with a palette, an experiment beside the sidebar layout
//! (`stui --glasses`, mission fleet/stui/glass, design doc/fleet/stui/glass). Plain stui never
//! reaches this module.
//!
//! A glass tab shows one pane. Showing a tab points stui's own tab and selection at the pane's
//! subject, so every key and action the sidebar layout has works the same inside a glass.

use super::*;
use ratatui::style::Color;

/// The palette's sections, in order; a digit key opens the palette at one.
const SECTIONS: [&str; 4] = ["needs you", "agents", "missions", "fleet"];

pub(crate) struct Glass {
    pub(crate) name: String,
    /// The tabs after Home, which is always first and cannot be closed.
    tabs: Vec<Pane>,
    /// The tab shown: 0 is Home.
    current: usize,
    palette: Option<Palette>,
}

impl Glass {
    pub(crate) fn new(name: String) -> Self {
        Self {
            name,
            tabs: Vec::new(),
            current: 0,
            palette: None,
        }
    }
}

#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
    /// Only this section, when opened with a digit.
    section: Option<usize>,
}

/// One row the palette can open.
#[derive(Clone)]
struct Choice {
    section: usize,
    glyph: (&'static str, Color),
    label: String,
    detail: String,
    pane: Pane,
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
        self.glass.as_ref().is_some_and(|glass| glass.current == 0)
    }

    /// Every subject the palette offers, section by section.
    fn choices(&self) -> Vec<Choice> {
        let spinner = self.spinner();
        let mut choices = Vec::new();
        for item in self.world.attention.items() {
            if self.snoozed.contains(&item.id) {
                continue;
            }
            choices.push(Choice {
                section: 0,
                glyph: screens::attention_style(&item.kind),
                label: item.title.clone(),
                detail: item.kind.word().to_owned(),
                pane: Pane::Home(Some(item.id.clone())),
            });
        }
        for agent in screens::agent_order(&self.world) {
            choices.push(Choice {
                section: 1,
                glyph: screens::agent_glyph(agent.state, spinner),
                label: agent.name.clone(),
                detail: format!("{} · {}", agent.harness.name(), agent.host),
                pane: Pane::Agent(Some(agent.id.clone())),
            });
        }
        for mission in screens::mission_order(&self.world, true) {
            choices.push(Choice {
                section: 2,
                glyph: screens::word_style(mission.word, spinner),
                label: mission.title.clone(),
                detail: mission.word.name().to_owned(),
                pane: Pane::Mission(Some(mission.id.clone())),
            });
        }
        for machine in self.world.machines.items() {
            choices.push(Choice {
                section: 3,
                glyph: screens::reach_style(machine.reach),
                label: machine.name.clone(),
                detail: machine.reach.word().to_owned(),
                pane: Pane::Machine(Some(format!("machine/{}", machine.name))),
            });
        }
        choices
    }

    /// What the palette shows for its query: sections in order, best matches first in each.
    fn matches(&self, palette: &Palette) -> Vec<Choice> {
        let mut scored = self
            .choices()
            .into_iter()
            .filter(|choice| {
                palette
                    .section
                    .is_none_or(|section| choice.section == section)
            })
            .filter_map(|choice| {
                let haystack = format!("{} {}", choice.label, choice.pane.key());
                fuzzy(&palette.query, &haystack).map(|score| (choice, score))
            })
            .collect::<Vec<_>>();
        if !palette.query.is_empty() {
            scored.sort_by_key(|(choice, score)| (choice.section, -score));
        }
        scored.into_iter().map(|(choice, _)| choice).collect()
    }

    pub(crate) fn render_glass(&self, buf: &mut Buffer, area: Rect) {
        let Some(glass) = &self.glass else { return };
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
        if glass.current == 0 {
            // Home is today's Home: its list and the selected card.
            self.draw_body(buf, body);
        } else {
            let main = Rect {
                x: body.x + 1,
                width: body.width.saturating_sub(2),
                ..body
            };
            self.draw_main(buf, main);
        }
        self.footer(
            buf,
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
        if let Some(palette) = &glass.palette {
            self.draw_palette(buf, area, palette);
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
        self.hit(Rect { x, width, ..area }, Hit::Palette);
    }

    /// Home, then the glass's tabs; a tab whose subject needs the person shows `◆`.
    fn tab_strip(&self, buf: &mut Buffer, area: Rect, glass: &Glass) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        let mut x = area.x + 1;
        for index in 0..=glass.tabs.len() {
            let (title, needs) = match index {
                0 => ("Home".to_owned(), false),
                _ => {
                    let pane = &glass.tabs[index - 1];
                    (self.pane_title(pane), self.pane_needs_person(pane))
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

    /// A tab's title: the name of what it shows.
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
        buf.set_stringn(
            rect.x + 2,
            rect.y,
            " open ",
            6,
            theme::strong(theme::ACCENT).bg(theme::MANTLE),
        );
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
            "enter open · ctrl+t new tab · ↑↓ choose · esc close",
            inner,
            theme::dim().bg(theme::MANTLE),
        );
    }

    /// Keys glasses own. Returns whether the key was used here.
    pub(crate) fn glass_key(&mut self, key: KeyEvent) -> bool {
        let Some(glass) = self.glass.as_mut() else {
            return false;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        if let Some(palette) = glass.palette.as_mut() {
            match key.code {
                KeyCode::Esc => glass.palette = None,
                KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
                KeyCode::Down => palette.selected += 1,
                KeyCode::Backspace if palette.query.is_empty() => palette.section = None,
                KeyCode::Backspace => {
                    palette.query.pop();
                    palette.selected = 0;
                }
                KeyCode::Enter => self.open_choice(None, false),
                KeyCode::Char('t') if control => self.open_choice(None, true),
                KeyCode::Char('k') if control || command => glass.palette = None,
                KeyCode::Char(letter) if !control => {
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
        match key.code {
            KeyCode::Char('k') if control || command => {
                self.open_palette(None);
                true
            }
            KeyCode::Char('t') if control => {
                self.open_palette(None);
                true
            }
            KeyCode::Char('w') if control => {
                self.close_tab();
                true
            }
            KeyCode::Char(digit @ '1'..='9') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.show_tab(digit as usize - '1' as usize);
                true
            }
            KeyCode::PageUp | KeyCode::PageDown if control => {
                let count = glass.tabs.len() + 1;
                let next = if key.code == KeyCode::PageDown {
                    (glass.current + 1) % count
                } else {
                    (glass.current + count - 1) % count
                };
                self.show_tab(next);
                true
            }
            // Outside a draft, the sidebar's tab digits open the palette at that section.
            KeyCode::Char(digit @ '1'..='4')
                if !self.editing && key.modifiers.is_empty() && self.new_mission.is_none() =>
            {
                self.open_palette(Some(digit as usize - '1' as usize));
                true
            }
            _ => false,
        }
    }

    pub(crate) fn open_palette(&mut self, section: Option<usize>) {
        if let Some(glass) = self.glass.as_mut() {
            glass.palette = Some(Palette {
                section,
                ..Palette::default()
            });
        }
    }

    fn clamp_palette(&mut self) {
        let count = match self.glass.as_ref().and_then(|glass| glass.palette.as_ref()) {
            Some(palette) => self.matches(palette).len(),
            None => return,
        };
        if let Some(palette) = self.glass.as_mut().and_then(|glass| glass.palette.as_mut()) {
            palette.selected = palette.selected.min(count.saturating_sub(1));
        }
    }

    /// Open the chosen row (or `index`): in this tab, or a new one. Home stays Home, so what is
    /// opened from Home gets its own tab; a Home item opens on Home itself.
    pub(crate) fn open_choice(&mut self, index: Option<usize>, new_tab: bool) {
        let Some(palette) = self.glass.as_ref().and_then(|glass| glass.palette.as_ref()) else {
            return;
        };
        let index = index.unwrap_or(palette.selected);
        let Some(choice) = self.matches(palette).into_iter().nth(index) else {
            return;
        };
        let Some(glass) = self.glass.as_mut() else {
            return;
        };
        glass.palette = None;
        if matches!(choice.pane, Pane::Home(_)) {
            self.show_tab(0);
            self.focus_pane(&choice.pane);
            return;
        }
        if let Some(existing) = glass.tabs.iter().position(|pane| *pane == choice.pane) {
            self.show_tab(existing + 1);
            return;
        }
        if new_tab || glass.current == 0 {
            glass.tabs.push(choice.pane);
            let last = glass.tabs.len();
            self.show_tab(last);
        } else {
            glass.tabs[glass.current - 1] = choice.pane;
            let current = glass.current;
            self.show_tab(current);
        }
    }

    pub(crate) fn show_tab(&mut self, index: usize) {
        let Some(glass) = self.glass.as_mut() else {
            return;
        };
        if index > glass.tabs.len() {
            return;
        }
        glass.current = index;
        match index {
            0 => self.tab = 0,
            _ => {
                let pane = glass.tabs[index - 1].clone();
                self.focus_pane(&pane);
            }
        }
    }

    fn close_tab(&mut self) {
        let Some(glass) = self.glass.as_mut() else {
            return;
        };
        if glass.current == 0 {
            self.flash("Home stays open");
            return;
        }
        glass.tabs.remove(glass.current - 1);
        let next = glass.current.min(glass.tabs.len());
        self.show_tab(next);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn glass() -> Ui {
        let mut ui = Ui::new(demo::world());
        ui.glass = Some(Glass::new("main".into()));
        ui
    }

    fn press(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers) {
        ui.key(KeyEvent::new(code, modifiers));
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

    fn tabs(ui: &Ui) -> (usize, Vec<String>) {
        let glass = ui.glass.as_ref().unwrap();
        (glass.current, glass.tabs.iter().map(Pane::key).collect())
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
        press(&mut ui, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(screen(&ui).contains("╭─ open"));
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        let (current, keys) = tabs(&ui);
        assert_eq!(
            (current, keys.as_slice()),
            (
                1,
                ["agent:agent/example/atlas/builder".to_owned()].as_slice()
            )
        );
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

        // Enter on a tab that is not Home replaces it; Ctrl+T opens beside it.
        press(&mut ui, KeyCode::Char('k'), KeyModifiers::CONTROL);
        typed(&mut ui, "weekly release");
        press(&mut ui, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(tabs(&ui).1.len(), 2);
        assert_eq!(ui.tab, 2);
        // Opening a subject a tab already shows goes to that tab.
        press(&mut ui, KeyCode::Char('k'), KeyModifiers::CONTROL);
        typed(&mut ui, "atlas builder");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(tabs(&ui), (1, tabs(&ui).1.clone()));
        assert_eq!(tabs(&ui).1.len(), 2);

        press(&mut ui, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!((tabs(&ui).0, ui.tab), (0, 0));
        press(&mut ui, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(tabs(&ui).1.len(), 2, "Home cannot be closed");
        press(&mut ui, KeyCode::PageDown, KeyModifiers::CONTROL);
        assert_eq!(tabs(&ui).0, 1);
        press(&mut ui, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(
            tabs(&ui).1,
            ["mission:mission/fleet/release/weekly".to_owned()]
        );
    }

    #[test]
    fn a_digit_opens_the_palette_at_its_section_and_escape_closes_it() {
        let mut ui = glass();
        typed(&mut ui, "3");
        let palette = ui.glass.as_ref().unwrap().palette.as_ref().unwrap();
        assert_eq!(palette.section, Some(2));
        assert!(ui.matches(palette).iter().all(|choice| choice.section == 2));
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.glass.as_ref().unwrap().palette.is_none());
        assert_eq!(ui.tab, 0, "the sidebar's tabs do not change underneath");
    }

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
}
