//! Arrangement projection, explicit edits and this device's sidebar choices.
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

pub(super) fn folder_name(name: &str) -> Result<&str, String> {
    if name.trim().is_empty() || name.trim().len() > 256 || name.chars().any(char::is_control) {
        return Err("Use a folder name of at most 256 bytes without control characters.".into());
    }
    Ok(name.trim())
}
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PaletteMode {
    New {
        subject: Option<String>,
    },
    Rename {
        id: String,
    },
    Manage {
        id: String,
    },
    Move {
        subject: String,
    },
    Add {
        folder: String,
    },
    ConfirmPlace {
        subject: String,
        folder: Option<String>,
    },
    ConfirmCreate {
        subject: String,
        name: String,
    },
    Delete {
        id: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    pub subject: String,
    pub owner: String,
    pub operations: Vec<st3_client::ArrangementOperation>,
    pub key: String,
    pub creating: bool,
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
    inventory_partial: Vec<String>,
    pub observations: Vec<st3_client::ResourceObservation>,
    pub observations_has_more: bool,
    pub observations_issue: Option<String>,
    pub writable: bool,
    pub writing: bool,
    draft: Option<(String, bool)>,
    /// st refused to create the reserved Sidebar because the person retired it. Retired
    /// arrangements leave the window, so that refusal is how a client learns it.
    pub reserved_retired: bool,
    seen: BTreeMap<String, ResourceRow>,
    choices: Choices,
    path: Option<PathBuf>,
    pub selected: usize,
    pub filter: String,
    pub filtering: bool,
    pub focused: bool,
}
/// What a person reads when their Sidebar was removed and no older one exists to edit.
pub(super) const SIDEBAR_RETIRED: &str =
    "This Sidebar was removed; restoring a removed Sidebar is not available yet.";

impl Sidebar {
    /// The person's one Sidebar: the reserved subject whatever its name (#2043), else the oldest
    /// live arrangement named "Sidebar", which clients created before the reserved id. Every
    /// client picks the same one, so their edits land in the same arrangement.
    pub(super) fn arrangement(&self) -> Option<&st3_client::Arrangement> {
        let reserved = format!("/{}", st3_client::SIDEBAR_ARRANGEMENT_UUID);
        let live = || self.arrangements.iter().filter(|a| !a.deleted);
        live()
            .find(|a| a.header.id.ends_with(&reserved))
            .or_else(|| {
                live()
                    .filter(|a| a.body.name.value == "Sidebar")
                    .min_by(|a, b| a.header.id.cmp(&b.header.id))
            })
    }
    pub(super) fn location(&self, subject: &str) -> Option<&str> {
        let a = self.arrangement()?;
        let placement = a.body.placements.get(subject)?;
        let folder = a
            .resolved
            .as_ref()
            .and_then(|r| r.folders.get(subject))
            .unwrap_or(&placement.value.folder);
        folder.as_deref().filter(|id| {
            a.body
                .folders
                .get(*id)
                .is_some_and(|f| f.tombstone.is_none())
        })
    }
    pub(super) fn resources(&self) -> Vec<ResourceRow> {
        let mut rows = self.seen.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
        rows
    }
    pub(super) fn folders(&self) -> Vec<(String, String)> {
        let Some(a) = self.arrangement() else {
            return Vec::new();
        };
        let mut folders = a
            .body
            .folders
            .iter()
            .filter(|(_, f)| f.tombstone.is_none())
            .collect::<Vec<_>>();
        folders.sort_by_key(|(id, f)| (&f.position.value.key, *id));
        folders
            .into_iter()
            .map(|(id, f)| (id.clone(), f.name.value.clone()))
            .collect()
    }
    pub(super) fn placement(
        &self,
        subject: &str,
        folder: Option<&str>,
        before: Option<&str>,
    ) -> Result<Vec<st3_client::ArrangementOperation>, String> {
        use st3_client::ArrangementOperation as Op;
        let mut siblings = self
            .arrangement()
            .into_iter()
            .flat_map(|a| a.body.placements.iter())
            .filter(|(id, _)| id.as_str() != subject && self.location(id) == folder)
            .map(|(id, p)| (id.clone(), p.value.key.clone()))
            .collect::<Vec<_>>();
        siblings.sort_by(|(ai, ak), (bi, bk)| ak.cmp(bk).then(ai.cmp(bi)));
        let mut operations = BTreeMap::new();
        if let Some(before) = before
            && folder.is_none()
            && !siblings.iter().any(|(id, _)| id == before)
        {
            let key = super::arrangement_keys::between(
                siblings.last().map(|(_, key)| key.as_str()),
                None,
            )?;
            siblings.push((before.into(), key.clone()));
            operations.insert(
                before.to_owned(),
                Op::SubjectPlace {
                    subject: before.into(),
                    folder: None,
                    key,
                },
            );
        }
        let index = match before {
            Some(id) => siblings
                .iter()
                .position(|(subject, _)| subject == id)
                .ok_or("The destination changed; choose it again.")?,
            None => siblings.len(),
        };
        let (key, rekeys) = super::arrangement_keys::insertion(
            &siblings
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>(),
            index,
        )?;
        for (at, key) in rekeys {
            let id = siblings[at].0.clone();
            operations.insert(
                id.clone(),
                Op::SubjectPlace {
                    subject: id,
                    folder: folder.map(str::to_owned),
                    key,
                },
            );
        }
        operations.insert(
            subject.into(),
            Op::SubjectPlace {
                subject: subject.into(),
                folder: folder.map(str::to_owned),
                key,
            },
        );
        Ok(operations.into_values().collect())
    }
    pub(super) fn folder_move(
        &self,
        id: &str,
        before: Option<&str>,
    ) -> Result<Vec<st3_client::ArrangementOperation>, String> {
        use st3_client::ArrangementOperation as Op;
        let a = self
            .arrangement()
            .ok_or("The sidebar has not loaded yet.")?;
        let folder = a
            .body
            .folders
            .get(id)
            .ok_or("That folder no longer exists.")?;
        let mut siblings = a
            .body
            .folders
            .iter()
            .filter(|(other, f)| other.as_str() != id && f.tombstone.is_none())
            .collect::<Vec<_>>();
        siblings.sort_by_key(|(id, f)| (&f.position.value.key, *id));
        let index = match before {
            Some(id) => siblings
                .iter()
                .position(|(other, _)| other.as_str() == id)
                .ok_or("The destination changed; choose it again.")?,
            None => siblings.len(),
        };
        let (key, rekeys) = super::arrangement_keys::insertion(
            &siblings
                .iter()
                .map(|(_, f)| f.position.value.key.as_str())
                .collect::<Vec<_>>(),
            index,
        )?;
        let mut operations = rekeys
            .into_iter()
            .map(|(at, key)| Op::FolderMove {
                id: siblings[at].0.clone(),
                parent: siblings[at].1.position.value.parent.clone(),
                key,
            })
            .collect::<Vec<_>>();
        operations.push(Op::FolderMove {
            id: id.into(),
            parent: folder.position.value.parent.clone(),
            key,
        });
        Ok(operations)
    }
    pub(super) fn saved(&mut self, write: &Write, done: bool) {
        self.writing = false;
        if write.creating
            && done
            && let Some((id, created)) = self.draft.as_mut()
            && id == &write.subject
        {
            *created = true;
        }
    }
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
        self.inventory_partial = world.inventory_partial.iter()
            .filter(|name| ["machines", "sessions"].contains(&name.as_str())).cloned().collect();
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
        rows.extend(self.observations.iter().map(|row| ResourceRow {
            id: row.id.clone(),
            kind: super::observed_resources::kind(row).into(),
            title: super::observed_resources::title(row),
            open: row.id.clone(),
            missing: false,
        }));
        for row in rows {
            self.seen.insert(row.id.clone(), row);
        }
        if self.seen != before {
            self.save();
        }
    }
    pub fn rows(&self) -> Vec<Row> {
        let arrangement = self.arrangement();
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
        for name in &self.inventory_partial {
            rows.push(Row::Notice(format!("More {name} exist beyond this inventory window.")));
        }
        if let Some(issue) = &self.observations_issue {
            rows.push(Row::Notice(format!("Resources: {issue}")));
        }
        if self.observations_has_more {
            rows.push(Row::Notice(
                "More graph resources exist beyond this window.".into(),
            ));
        }
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
        let mut unfiled = catalog
            .values()
            .filter(|row| location(&row.id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        unfiled.sort_by_key(|row| {
            (
                arrangement
                    .and_then(|a| a.body.placements.get(&row.id))
                    .map(|p| p.value.key.clone())
                    .unwrap_or_else(|| "~".into()),
                row.title.clone(),
                row.id.clone(),
            )
        });
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
            for kind in [
                "Agents",
                "Missions",
                "Terminals",
                "Machines",
                "Pull requests",
                "Repositories",
                "Resources",
                "Unavailable",
            ] {
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
    pub(super) fn sidebar_edit(&mut self, operations: Vec<st3_client::ArrangementOperation>) {
        if self.resource_sidebar.writing {
            self.flash("Sidebar edit is still being saved.");
            return;
        }
        if self.live && !self.resource_sidebar.writable {
            self.flash("Sidebar editing is not available on this connection.");
            return;
        }
        let existing = self
            .resource_sidebar
            .arrangement()
            .map(|a| a.header.id.clone());
        if existing.is_none() && self.resource_sidebar.reserved_retired {
            self.flash(SIDEBAR_RETIRED);
            return;
        }
        let mut operations = operations;
        let (subject, creating) = match existing {
            Some(id) => (id, false),
            None => {
                let draft = self.resource_sidebar.draft.get_or_insert_with(|| {
                    (
                        format!(
                            "arrangement/{}/{}",
                            self.world.person,
                            st3_client::SIDEBAR_ARRANGEMENT_UUID
                        ),
                        false,
                    )
                });
                (draft.0.clone(), !draft.1)
            }
        };
        if creating {
            operations.insert(
                0,
                st3_client::ArrangementOperation::Create {
                    name: "Sidebar".into(),
                },
            );
        }
        let write = Write {
            subject,
            owner: self.world.person.clone(),
            operations,
            key: uuid::Uuid::now_v7().to_string(),
            creating,
        };
        self.resource_sidebar.writing = true;
        self.effects.push(super::Effect::EditArrangement(write));
    }
    pub(super) fn sidebar_place(
        &mut self,
        subject: &str,
        folder: Option<&str>,
        before: Option<&str>,
    ) {
        if subject.starts_with("session/") || subject.starts_with("pty/") {
            self.flash("Sessions only navigate; file a stable graph subject instead.");
            return;
        }
        match self.resource_sidebar.placement(subject, folder, before) {
            Ok(operations) => self.sidebar_edit(operations),
            Err(error) => self.flash(error),
        }
    }
    pub(super) fn sidebar_move_folder(&mut self, id: &str, before: Option<&str>) {
        match self.resource_sidebar.folder_move(id, before) {
            Ok(operations) => self.sidebar_edit(operations),
            Err(error) => self.flash(error),
        }
    }
    pub(super) fn sidebar_create_folder(&mut self, name: &str, subject: Option<&str>) {
        let id = uuid::Uuid::now_v7().to_string();
        let last = self
            .resource_sidebar
            .arrangement()
            .into_iter()
            .flat_map(|a| a.body.folders.values())
            .filter(|f| f.tombstone.is_none())
            .map(|f| f.position.value.key.as_str())
            .max();
        let key = match super::arrangement_keys::between(last, None) {
            Ok(key) => key,
            Err(error) => {
                self.flash(error);
                return;
            }
        };
        let mut operations = vec![st3_client::ArrangementOperation::FolderCreate {
            id: id.clone(),
            name: name.trim().into(),
            parent: None,
            key,
        }];
        if let Some(subject) = subject {
            operations.push(st3_client::ArrangementOperation::SubjectPlace {
                subject: subject.into(),
                folder: Some(id),
                key: "a0".into(),
            });
        }
        self.sidebar_edit(operations);
    }
    pub(crate) fn draw_resource_sidebar(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        self.frame.borrow_mut().glass_sidebar = area;
        let filter = format!(" / Filter: {}", self.resource_sidebar.filter);
        buf.set_stringn(area.x, area.y, filter, area.width as usize, theme::dim());
        self.hit(Rect { height: 1, ..area }, Hit::ResourceFilter);
        if area.width >= 8 {
            let rect = Rect::new(area.right() - 5, area.y, 5, 1);
            buf.set_stringn(rect.x, rect.y, " +New", 5, theme::fg(theme::ACCENT));
            self.hit(rect, Hit::ResourceNewFolder);
        }
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
            if let Row::Group { id, .. } = row
                && let Some(folder) = id.strip_prefix("folder/")
                && area.width >= 12
            {
                let x = area.right() - 9;
                buf.set_stringn(x, y, "+  …  ≡", 8, theme::fg(theme::ACCENT));
                self.hit(Rect::new(x, y, 2, 1), Hit::ResourceAdd(folder.into()));
                self.hit(
                    Rect::new(x + 3, y, 2, 1),
                    Hit::ResourceFolderMenu(folder.into()),
                );
                self.hit(
                    Rect::new(x + 6, y, 3, 1),
                    Hit::ResourceFolderDrag(folder.into()),
                );
            }
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
            KeyCode::Char('n') if !self.resource_sidebar.filtering => {
                self.open_sidebar_palette(PaletteMode::New { subject: None })
            }
            KeyCode::Char('m') if !self.resource_sidebar.filtering => {
                if let Some(Row::Resource(row)) = self
                    .resource_sidebar
                    .rows()
                    .get(self.resource_sidebar.selected)
                    .cloned()
                {
                    self.open_sidebar_palette(PaletteMode::Move { subject: row.id });
                }
            }
            KeyCode::Char('r' | 'd') if !self.resource_sidebar.filtering => {
                if let Some(Row::Group { id, .. }) = self
                    .resource_sidebar
                    .rows()
                    .get(self.resource_sidebar.selected)
                    .cloned()
                    && let Some(id) = id.strip_prefix("folder/")
                {
                    self.open_sidebar_palette(if key.code == KeyCode::Char('r') {
                        PaletteMode::Rename { id: id.into() }
                    } else {
                        PaletteMode::Delete { id: id.into() }
                    });
                }
            }
            KeyCode::Char('+') if !self.resource_sidebar.filtering => {
                if let Some(Row::Group { id, .. }) = self
                    .resource_sidebar
                    .rows()
                    .get(self.resource_sidebar.selected)
                    .cloned()
                    && let Some(id) = id.strip_prefix("folder/")
                {
                    self.open_sidebar_palette(PaletteMode::Add { folder: id.into() });
                }
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
    fn the_reserved_subject_is_the_sidebar_and_legacy_ones_pick_the_oldest_by_name() {
        let named = |uuid: &str, name: &str| {
            let mut a = arrangement();
            a.header.id = format!("arrangement/person/avery/{uuid}");
            a.body.name.value = name.into();
            a
        };
        let picked = |sidebar: &Sidebar| sidebar.arrangement().map(|a| a.header.id.clone());
        let reserved = format!(
            "arrangement/person/avery/{}",
            st3_client::SIDEBAR_ARRANGEMENT_UUID
        );
        let oldest = "arrangement/person/avery/019a0000-0000-7000-8000-000000000002";
        let mut sidebar = Sidebar {
            arrangements: vec![named("019a0000-0000-7000-8000-000000000009", "Desk")],
            ..Default::default()
        };
        assert_eq!(picked(&sidebar), None, "only a Sidebar is the Sidebar");
        for uuid in [
            "019a0000-0000-7000-8000-000000000003",
            "019a0000-0000-7000-8000-000000000002",
        ] {
            sidebar.arrangements.push(named(uuid, "Sidebar"));
        }
        assert_eq!(picked(&sidebar).as_deref(), Some(oldest));
        let renamed = named(st3_client::SIDEBAR_ARRANGEMENT_UUID, "Renamed");
        sidebar.arrangements.push(renamed);
        assert_eq!(
            picked(&sidebar),
            Some(reserved),
            "the reserved subject wins whatever its name"
        );
        sidebar.arrangements.last_mut().unwrap().deleted = true;
        assert_eq!(picked(&sidebar).as_deref(), Some(oldest));
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
    fn placements_rekey_collisions_and_can_anchor_unfiled_rows_without_losing_subjects() {
        let mut sidebar = Sidebar::default();
        let mut a = arrangement();
        a.body.placements.get_mut("agent/atlas").unwrap().value.key = "a0".into();
        sidebar.arrangements.push(a);
        let ops = sidebar
            .placement("agent/new", Some("first"), Some("mission/build"))
            .unwrap();
        let keys = ops
            .iter()
            .filter_map(|op| {
                if let st3_client::ArrangementOperation::SubjectPlace { subject, key, .. } = op {
                    Some((subject.as_str(), key.as_str()))
                } else {
                    None
                }
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(keys.len(), ops.len());
        assert!(
            keys["agent/atlas"] < keys["agent/new"] && keys["agent/new"] < keys["mission/build"]
        );
        assert!(
            keys.contains_key("mission/build"),
            "a dormant placement is kept in an atomic rekey"
        );
        assert!(
            keys.values()
                .all(|key| st3_schema::arrangements::valid_key(key))
        );
        let ops = sidebar
            .placement("agent/new", None, Some("agent/unplaced"))
            .unwrap();
        assert_eq!(ops.len(), 2);
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
