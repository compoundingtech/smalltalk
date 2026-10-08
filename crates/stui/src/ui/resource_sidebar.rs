//! Read-only arrangement projection and this device's sidebar choices.
use super::{
    Ui,
    doc::Hit,
    glass::{Open, pane_for},
    pane::Pane,
    text, theme,
    view::World,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResourceRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub open: String,
    pub missing: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Group {
        id: String,
        title: String,
        count: usize,
        closed: bool,
    },
    Resource(ResourceRow),
    Notice(String),
    Welcome,
}
#[derive(Default, Serialize, Deserialize)]
struct Choices {
    collapsed: BTreeMap<String, bool>,
    #[serde(default)]
    resources: BTreeMap<String, ResourceRow>,
}
#[derive(Default)]
pub(crate) struct Sidebar {
    pub arrangements: Vec<st3_client::Arrangement>,
    pub terminals: Vec<ResourceRow>,
    pub terminals_has_more: bool,
    seen: BTreeMap<String, ResourceRow>,
    choices: Choices,
    path: Option<PathBuf>,
    pub selected: usize,
    pub filter: String,
    pub filtering: bool,
    pub focused: bool,
}
impl Sidebar {
    pub fn load(&mut self, person: &str) {
        self.path =
            super::glass_store::path(person).map(|path| path.with_extension("sidebar.json"));
        self.choices = self
            .path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        self.seen
            .extend(std::mem::take(&mut self.choices.resources));
    }
    fn save(&self) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &self.path else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&Choices {
            collapsed: self.choices.collapsed.clone(),
            resources: self.seen.clone(),
        }) else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
    pub fn observe(&mut self, world: &World) {
        // Retain missing subjects until the person explicitly clears them; never prune by age.
        let before = self.seen.clone();
        for row in self.seen.values_mut() {
            row.missing = true;
        }
        let mut rows = Vec::new();
        rows.extend(world.agents.items().iter().map(|agent| ResourceRow {
            id: agent.id.clone(),
            kind: "Agents".into(),
            title: agent.name.clone(),
            open: agent.id.clone(),
            missing: false,
        }));
        rows.extend(world.missions.items().iter().map(|mission| ResourceRow {
            id: mission.id.clone(),
            kind: "Missions".into(),
            title: mission.title.clone(),
            open: mission.id.clone(),
            missing: false,
        }));
        rows.extend(world.machines.items().iter().map(|machine| ResourceRow {
            id: format!("machine/{}", machine.name),
            kind: "Machines".into(),
            title: machine.name.clone(),
            open: format!("machine/{}", machine.name),
            missing: false,
        }));
        rows.extend(self.terminals.clone());
        for row in rows {
            self.seen.insert(row.id.clone(), row);
        }
        if self.seen != before {
            self.save();
        }
    }
    pub fn rows(&self) -> Vec<Row> {
        let arrangement = self
            .arrangements
            .iter()
            .filter(|a| !a.deleted)
            .min_by_key(|a| (a.body.name.value != "Sidebar", a.header.id.clone()));
        let mut catalog = self.seen.clone();
        if let Some(a) = arrangement {
            for id in a.body.placements.keys() {
                catalog.entry(id.clone()).or_insert_with(|| ResourceRow {
                    id: id.clone(),
                    kind: "Unavailable".into(),
                    title: id.clone(),
                    open: id.clone(),
                    missing: true,
                });
            }
        }
        let mut folders = arrangement
            .map(|a| {
                a.body
                    .folders
                    .iter()
                    .filter(|(_, f)| f.tombstone.is_none())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        folders.sort_by_key(|(id, folder)| (&folder.position.value.key, *id));
        // One level: nested folders remain visible, and their placements keep their real folder.
        let location = |id: &str| {
            arrangement.and_then(|a| {
                a.body.placements.get(id).and_then(|placement| {
                    let folder = a
                        .resolved
                        .as_ref()
                        .and_then(|r| r.folders.get(id))
                        .unwrap_or(&placement.value.folder);
                    folder.as_deref().filter(|folder| {
                        a.body
                            .folders
                            .get(*folder)
                            .is_some_and(|f| f.tombstone.is_none())
                    })
                })
            })
        };
        let mut rows = Vec::new();
        if self.terminals_has_more {
            rows.push(Row::Notice(
                "More terminals exist beyond this 200-item window.".into(),
            ));
        }
        let mut append =
            |id: String, title: String, resources: Vec<ResourceRow>, default_closed: bool| {
                let resources = resources
                    .into_iter()
                    .filter(|row| {
                        self.filter.is_empty()
                            || row
                                .title
                                .to_lowercase()
                                .contains(&self.filter.to_lowercase())
                            || row.id.to_lowercase().contains(&self.filter.to_lowercase())
                    })
                    .collect::<Vec<_>>();
                let count = resources.len();
                let closed = self.filter.is_empty()
                    && self
                        .choices
                        .collapsed
                        .get(&id)
                        .copied()
                        .unwrap_or(default_closed);
                rows.push(Row::Group {
                    id,
                    title,
                    count,
                    closed,
                });
                if !closed {
                    rows.extend(resources.into_iter().map(Row::Resource));
                }
            };
        for (id, folder) in folders {
            let mut resources = catalog
                .values()
                .filter(|row| location(&row.id) == Some(id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            if let Some(a) = arrangement {
                resources
                    .sort_by_key(|row| (&a.body.placements[&row.id].value.key, row.id.clone()));
            }
            append(
                format!("folder/{id}"),
                folder.name.value.clone(),
                resources,
                false,
            );
        }
        let unfiled = catalog
            .values()
            .filter(|row| location(&row.id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let count = unfiled.len();
        let closed = self.filter.is_empty()
            && self
                .choices
                .collapsed
                .get("everything")
                .copied()
                .unwrap_or(count > 30);
        rows.push(Row::Group {
            id: "everything".into(),
            title: "Everything else".into(),
            count,
            closed,
        });
        if !closed {
            for kind in ["Agents", "Missions", "Terminals", "Machines", "Unavailable"] {
                let resources = unfiled
                    .iter()
                    .filter(|row| {
                        row.kind == kind
                            && (self.filter.is_empty()
                                || row
                                    .title
                                    .to_lowercase()
                                    .contains(&self.filter.to_lowercase())
                                || row.id.to_lowercase().contains(&self.filter.to_lowercase()))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if resources.is_empty() {
                    continue;
                }
                let count = resources.len();
                let id = format!("kind/{kind}");
                let closed = self.filter.is_empty()
                    && self
                        .choices
                        .collapsed
                        .get(&id)
                        .copied()
                        .unwrap_or(count > 30);
                rows.push(Row::Group {
                    id,
                    title: kind.into(),
                    count,
                    closed,
                });
                if !closed {
                    rows.extend(resources.into_iter().map(Row::Resource));
                }
            }
        }
        if catalog.values().all(|row| row.kind == "Machines") && self.arrangements.is_empty() {
            rows.push(Row::Welcome);
        }
        rows
    }
    pub fn toggle(&mut self, index: usize) {
        if let Some(Row::Group { id, closed, .. }) = self.rows().get(index) {
            self.choices.collapsed.insert(id.clone(), !closed);
            self.save();
        }
    }
}
impl Ui {
    pub(crate) fn draw_resource_sidebar(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        self.frame.borrow_mut().glass_sidebar = area;
        let filter = format!(" / Filter: {}", self.resource_sidebar.filter);
        buf.set_stringn(area.x, area.y, filter, area.width as usize, theme::dim());
        self.hit(Rect { height: 1, ..area }, Hit::ResourceFilter);
        let rows = self.resource_sidebar.rows();
        let height = area.height.saturating_sub(1) as usize;
        let top = self
            .resource_sidebar
            .selected
            .saturating_sub(height.saturating_sub(1));
        for (index, row) in rows.iter().enumerate().skip(top).take(height) {
            let y = area.y + 1 + (index - top) as u16;
            let (label, style) = match row {
                Row::Group {
                    title,
                    count,
                    closed,
                    ..
                } => (
                    format!(" {} {title} {count}", if *closed { "▸" } else { "▾" }),
                    theme::label(),
                ),
                Row::Resource(row) => (
                    format!(
                        "   {}{}",
                        row.title,
                        if row.missing { " · unavailable" } else { "" }
                    ),
                    if row.missing {
                        theme::dim()
                    } else {
                        theme::fg(theme::TEXT)
                    },
                ),
                Row::Notice(message) => (format!(" {message}"), theme::dim()),
                Row::Welcome => (
                    " Welcome · Ctrl+K: New terminal".into(),
                    theme::fg(theme::ACCENT),
                ),
            };
            let style = if index == self.resource_sidebar.selected
                && (self.resource_sidebar.focused
                    || self
                        .glasses
                        .as_ref()
                        .is_some_and(|glasses| glasses.sidebar.focused))
            {
                Style::default().fg(theme::CRUST).bg(theme::ACCENT)
            } else {
                style
            };
            buf.set_stringn(
                area.x,
                y,
                text::sanitize(&label),
                area.width as usize,
                style,
            );
            let rect = Rect {
                y,
                height: 1,
                ..area
            };
            self.hit(rect, Hit::ResourceRow(index));
            if let Row::Resource(row) = row
                && !row.missing
            {
                let mut frame = self.frame.borrow_mut();
                let layer = frame.covers.len();
                frame.context_rows.push((rect, row.id.clone(), layer));
            }
        }
    }
    pub(crate) fn open_resource_row(&mut self, index: usize) {
        self.resource_sidebar.selected = index;
        match self.resource_sidebar.rows().get(index).cloned() {
            Some(Row::Group { .. }) => self.resource_sidebar.toggle(index),
            Some(Row::Resource(row)) if !row.missing => {
                let pane = if row.kind == "Terminals" {
                    Some(Pane::Terminal(row.open.clone()))
                } else {
                    pane_for(&row.open)
                };
                if let Some(pane) = pane {
                    self.resource_sidebar.focused = false;
                    if let Some(glasses) = self.glasses.as_mut() {
                        glasses.sidebar.focused = false;
                    }
                    if self.glasses.is_some() {
                        self.open_in_glass(pane, Open::Tab);
                    } else {
                        self.open(&row.open);
                    }
                    if row.kind == "Terminals" {
                        self.attach_terminal(&row.open);
                    }
                }
            }
            Some(Row::Welcome) => self.open_palette(None, Open::Tab),
            _ => {}
        }
    }
    pub(crate) fn resource_sidebar_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        let count = self.resource_sidebar.rows().len();
        match key.code {
            KeyCode::Esc => {
                if self.resource_sidebar.filter.is_empty() && !self.resource_sidebar.filtering {
                    self.resource_sidebar.focused = false;
                    if let Some(glasses) = self.glasses.as_mut() {
                        glasses.sidebar.focused = false;
                    }
                }
                self.resource_sidebar.filter.clear();
                self.resource_sidebar.filtering = false;
            }
            KeyCode::Char('/') if !self.resource_sidebar.filtering => {
                self.resource_sidebar.filtering = true
            }
            KeyCode::Char(ch) if self.resource_sidebar.filtering => {
                self.resource_sidebar.filter.push(ch);
                self.resource_sidebar.selected = 0;
            }
            KeyCode::Backspace if self.resource_sidebar.filtering => {
                self.resource_sidebar.filter.pop();
                self.resource_sidebar.selected = 0;
            }
            KeyCode::Up => {
                self.resource_sidebar.selected = self.resource_sidebar.selected.saturating_sub(1)
            }
            KeyCode::Down => {
                self.resource_sidebar.selected =
                    (self.resource_sidebar.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Home => self.resource_sidebar.selected = 0,
            KeyCode::End => self.resource_sidebar.selected = count.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.open_resource_row(self.resource_sidebar.selected)
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn arrangement() -> st3_client::Arrangement {
        serde_json::from_value(json!({
            "id":"arrangement/person/avery/019a0000-0000-7000-8000-000000000001",
            "revision":"claim/1", "updated_at":"2026-10-01T00:00:00Z", "owner":"person/avery", "deleted":false,
            "body":{"version":1, "name":{"value":"Sidebar", "revision":"claim/1"},
                "folders": {
                    "first":{"name":{"value":"First", "revision":"claim/1"}, "position":{"value":{"parent":null,"key":"a0"},"revision":"claim/1"}, "tombstone":null},
                    "second":{"name":{"value":"Second", "revision":"claim/1"}, "position":{"value":{"parent":"first","key":"a1"},"revision":"claim/1"}, "tombstone":null}
                },
                "placements":{
                    "agent/atlas":{"value":{"folder":"first","key":"a1"},"revision":"claim/1"},
                    "mission/build":{"value":{"folder":"first","key":"a0"},"revision":"claim/1"}
                }
            }
        })).unwrap()
    }
    fn resource(id: &str, kind: &'static str) -> ResourceRow {
        ResourceRow {
            id: id.into(),
            title: id.into(),
            kind: kind.into(),
            open: id.into(),
            missing: false,
        }
    }
    #[test]
    fn folders_follow_register_order_and_missing_placements_stay_marked() {
        let mut sidebar = Sidebar::default();
        sidebar.arrangements.push(arrangement());
        sidebar
            .seen
            .insert("agent/atlas".into(), resource("agent/atlas", "Agents"));
        let rows = sidebar.rows();
        assert!(matches!(&rows[0], Row::Group { title, closed: false, .. } if title == "First"));
        assert!(matches!(&rows[1], Row::Resource(row) if row.id == "mission/build" && row.missing));
        assert!(matches!(&rows[2], Row::Resource(row) if row.id == "agent/atlas" && !row.missing));
        assert!(matches!(&rows[3], Row::Group { title, .. } if title == "Second"));
        // A resolved null takes precedence over the raw placement, and a deleted folder unfiles.
        sidebar.arrangements[0].resolved = Some(st3_client::ArrangementResolved {
            parents: BTreeMap::new(),
            folders: BTreeMap::from([("agent/atlas".into(), None)]),
        });
        assert!(
            sidebar
                .rows()
                .iter()
                .any(|row| matches!(row, Row::Group { title, .. } if title == "Agents"))
        );
        sidebar.arrangements[0]
            .body
            .folders
            .get_mut("first")
            .unwrap()
            .tombstone = Some(st3_client::ArrangementRegister {
            value: true,
            revision: "claim/2".into(),
        });
        assert!(
            !sidebar
                .rows()
                .iter()
                .any(|row| matches!(row, Row::Group { title, .. } if title == "First"))
        );
    }
    #[test]
    fn busy_groups_collapse_search_opens_them_and_choices_stay_on_this_device() {
        let mut sidebar = Sidebar::default();
        for index in 0..31 {
            let id = format!("agent/{index}");
            sidebar.seen.insert(id.clone(), resource(&id, "Agents"));
        }
        assert!(matches!(
            &sidebar.rows()[0],
            Row::Group { closed: true, .. }
        ));
        sidebar.toggle(0);
        assert!(matches!(
            &sidebar.rows()[0],
            Row::Group { closed: false, .. }
        ));
        sidebar.filter = "agent/30".into();
        assert_eq!(
            sidebar
                .rows()
                .iter()
                .filter(|row| matches!(row, Row::Resource(_)))
                .count(),
            1
        );
        sidebar.filter.clear();
        let root = tempfile::tempdir().unwrap();
        sidebar.path = Some(root.path().join("choices.json"));
        sidebar.save();
        let choices: Choices =
            serde_json::from_slice(&std::fs::read(sidebar.path.unwrap()).unwrap()).unwrap();
        assert_eq!(choices.collapsed.get("everything"), Some(&false));
        assert!(sidebar.arrangements.is_empty());
    }
    #[test]
    fn saving_replaces_the_catalog_without_truncating_the_previous_file() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sidebar.json");
        let mut sidebar = Sidebar {
            path: Some(path.clone()),
            ..Default::default()
        };
        let mut row = resource("agent/atlas", "Agents");
        row.missing = true;
        sidebar.seen.insert(row.id.clone(), row);
        sidebar.save();
        let previous = std::fs::read(&path).unwrap();
        let mut old_file = std::fs::File::open(&path).unwrap();
        sidebar.choices.collapsed.insert("everything".into(), true);
        sidebar.save();
        let mut retained = Vec::new();
        old_file.read_to_end(&mut retained).unwrap();
        assert_eq!(retained, previous);
        let restored: Choices = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored.collapsed.get("everything"), Some(&true));
        assert!(restored.resources["agent/atlas"].missing);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[test]
    fn truncated_terminal_catalog_stays_visible_above_collapsed_groups() {
        let mut sidebar = Sidebar {
            terminals_has_more: true,
            ..Default::default()
        };
        sidebar.choices.collapsed.insert("everything".into(), true);
        assert!(
            matches!(&sidebar.rows()[0], Row::Notice(message) if message.contains("200-item window"))
        );
        sidebar.terminals_has_more = false;
        assert!(
            !sidebar
                .rows()
                .iter()
                .any(|row| matches!(row, Row::Notice(_)))
        );
    }
    #[test]
    fn opening_and_collapsing_only_navigate_and_do_not_edit_arrangements() {
        let mut ui = Ui::new(super::super::demo::world());
        ui.glasses = Some(super::super::glass::Glasses::open(None, None));
        ui.resource_sidebar.arrangements.push(arrangement());
        let before = ui.resource_sidebar.arrangements.clone();
        ui.open_resource_row(0);
        ui.open_resource_row(0);
        let missing = ui
            .resource_sidebar
            .rows()
            .iter()
            .position(|row| matches!(row, Row::Resource(resource) if resource.missing))
            .unwrap();
        ui.open_resource_row(missing);
        assert_eq!(ui.resource_sidebar.arrangements, before);
        assert!(ui.effects.is_empty());
        let agent = ui.resource_sidebar.rows().iter().position(|row| matches!(row, Row::Resource(resource) if resource.kind == "Agents" && !resource.missing)).unwrap();
        ui.open_resource_row(agent);
        assert!(matches!(ui.focused_pane(), Some(Pane::Agent(_))));
        assert_eq!(ui.resource_sidebar.arrangements, before);
    }
    #[test]
    fn a_fresh_install_shows_the_host_and_how_to_start() {
        let mut world = super::super::demo::world();
        world.agents = super::super::view::Load::Ready(Vec::new());
        world.missions = super::super::view::Load::Ready(Vec::new());
        world.attention = super::super::view::Load::Ready(Vec::new());
        world.machines = super::super::view::Load::Ready(Vec::new());
        world.host = "example-linux".into();
        let mut ui = Ui::new(world);
        ui.glasses = Some(super::super::glass::Glasses::open(None, None));
        ui.set_world(ui.world.clone());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("example-linux") && screen.contains("nothing running yet"));
        assert!(screen.contains("Ctrl+K: New terminal") && screen.contains("Welcome"));
    }
}
