//! Context menus reuse the actions registered by the rendered subject and its card.

use super::glass::{ContextMenu, MenuAction};
use super::*;

type Items = Vec<(String, MenuAction)>;
type Menu = (String, Items);

pub(super) enum PointMenu {
    Content(Menu),
    Glass,
}

impl MenuAction {
    fn shortcut(&self) -> Option<KeyEvent> {
        let (code, modifiers) = match self {
            Self::Agent { key, .. } => (KeyCode::Char(*key), KeyModifiers::NONE),
            Self::Activate { hit, .. } => match hit.as_ref() {
                Hit::Key(key) => (KeyCode::Char(*key), KeyModifiers::NONE),
                _ => return None,
            },
            Self::Declaration(_) => (KeyCode::Char('k'), KeyModifiers::NONE),
            Self::CloseTab(..) => (KeyCode::Char('w'), KeyModifiers::CONTROL),
            Self::SplitGroup { right, .. } => (
                KeyCode::Char(if *right { 'v' } else { 'x' }),
                KeyModifiers::CONTROL,
            ),
            Self::NewTab(_) => (KeyCode::Char('t'), KeyModifiers::CONTROL),
            _ => return None,
        };
        Some(KeyEvent::new(code, modifiers))
    }
}

fn activate(subject: Option<&str>, hit: Hit) -> MenuAction {
    MenuAction::Activate {
        subject: subject.map(str::to_owned),
        hit: Box::new(hit),
    }
}

fn entry_text(entry: &Entry) -> String {
    match &entry.body {
        Body::User(text) | Body::Assistant(text) | Body::Thinking(text) | Body::Event(text) => {
            text.clone()
        }
        Body::Mail { body, .. } => body.clone(),
        Body::Pending { text, .. } => text.clone(),
        Body::Tool { output, .. } => output.join("\n"),
    }
}

impl Ui {
    /// Resolve against the painted layer, rather than selecting or focusing the thing clicked.
    pub(crate) fn open_context_menu(&mut self, column: u16, row: u16) {
        match self.point_menu(column, row) {
            Some(PointMenu::Content((title, items))) => {
                self.cancel_drag();
                self.context = Some(ContextMenu {
                    column,
                    row,
                    title,
                    items,
                    selected: 0,
                });
            }
            Some(PointMenu::Glass) => self.open_glass_context_menu(column, row),
            None => {}
        }
    }

    /// Rendering the pointer hint uses the same resolver as opening the menu.
    pub(super) fn point_menu(&self, column: u16, row: u16) -> Option<PointMenu> {
        if self.help || self.palette_open() {
            return None;
        }
        let (hit, subject, entry, pane_subject) = {
            let info = self.frame.borrow();
            if !contains(info.area, column, row) {
                return None;
            }
            let cover = info
                .covers
                .iter()
                .enumerate()
                .rev()
                .find(|(_, (rect, _))| contains(*rect, column, row));
            let (layer, first_hit) =
                cover.map_or((0, 0), |(index, (_, first))| (index + 1, *first));
            let hit = info
                .hits
                .iter()
                .skip(first_hit)
                .rev()
                .find(|(rect, _)| contains(*rect, column, row))
                .map(|(_, hit)| hit.clone());
            let subject = info
                .context_rows
                .iter()
                .rev()
                .find(|(rect, _, first)| *first >= layer && contains(*rect, column, row))
                .map(|(_, subject, _)| subject.clone());
            let pane = info
                .panes
                .iter()
                .rev()
                .find(|pane| pane.layer >= layer && contains(pane.rect, column, row));
            let entry = pane.and_then(|pane| {
                let agent = pane.key.strip_prefix("chat:")?;
                let line = pane.top + usize::from(row - pane.rect.y);
                pane.entries
                    .iter()
                    .rev()
                    .find(|(_, start)| *start <= line)
                    .map(|(entry, _)| (agent.to_owned(), entry.clone()))
            });
            let pane_subject = pane.and_then(|pane| match Pane::parse(&pane.key) {
                Some(Pane::Home(id) | Pane::Mission(id) | Pane::Declaration(id)) => id,
                _ => None,
            });
            (hit, subject, entry, pane_subject)
        };
        let menu = if matches!(hit, Some(Hit::GlassTab(..))) {
            // Tab structure has its own existing actions, rather than the subject's actions.
            return Some(PointMenu::Glass);
        } else if let Some((agent, entry)) = entry {
            self.message_menu(&agent, &entry)
        } else if let Some(subject) = subject {
            self.subject_menu(&subject)
        } else if let Some(Hit::Peek(subject) | Hit::Open(subject) | Hit::Actions(subject)) = &hit {
            self.subject_menu(subject)
        } else if let Some(hit) = hit.as_ref().filter(|_| pane_subject.is_none()) {
            self.hit_menu(hit)
        } else if let Some(subject) = pane_subject {
            self.subject_menu(&subject)
        } else {
            None
        };
        menu.map(PointMenu::Content).or_else(|| {
            let on_split = self.frame.borrow().glass_leaves.iter().any(|rect| {
                contains(*rect, column, row)
                    || (row.saturating_add(1) == rect.y
                        && column >= rect.x
                        && column < rect.right())
            });
            (self.popover.is_none() && !self.home_open() && on_split).then_some(PointMenu::Glass)
        })
    }

    fn subject_menu(&self, subject: &str) -> Option<Menu> {
        if let Some(agent) = self
            .world
            .agents
            .items()
            .iter()
            .find(|agent| agent.id == subject)
        {
            let mut items = vec![(
                "Open conversation [Enter]".into(),
                activate(None, Hit::Open(subject.into())),
            )];
            if !self
                .world
                .conversations
                .get(subject)
                .is_none_or(|entries| entries.items().is_empty())
            {
                items.push((
                    "Message actions…".into(),
                    MenuAction::Messages(subject.into()),
                ));
            }
            items.extend(
                screens::agent_actions(agent)
                    .into_iter()
                    .filter(|action| {
                        !agent.unmanaged || !matches!(action.key, 'i' | 'r' | 'p' | 'x' | 's' | 'u')
                    })
                    .map(|action| {
                        (
                            format!(
                                "{} [{}]",
                                if action.key == 'c' {
                                    "Copy its path"
                                } else {
                                    action.label
                                },
                                action.key
                            ),
                            MenuAction::Agent {
                                agent: subject.into(),
                                key: action.key,
                            },
                        )
                    }),
            );
            return Some((agent.name.clone(), items));
        }
        if let Some(mission) = self
            .world
            .missions
            .items()
            .iter()
            .find(|mission| mission.id == subject)
        {
            return Some((
                mission.title.clone(),
                vec![
                    (
                        "Open mission [Enter]".into(),
                        activate(None, Hit::Open(subject.into())),
                    ),
                    (
                        "Show declaration [k]".into(),
                        MenuAction::Declaration(subject.into()),
                    ),
                    ("Copy its path".into(), MenuAction::CopyPath(subject.into())),
                ],
            ));
        }
        let item = self
            .world
            .attention
            .items()
            .iter()
            .find(|item| item.id == subject)?;
        // Build the same card in its resting state. Never turn a pending confirmation or
        // an unfinished draft's Send button into a menu action.
        let drafts = Drafts {
            text: None,
            cursor: 0,
            editing: false,
            confirm: None,
            answering: None,
            needs_words: None,
            chat: None,
        };
        let doc = screens::home_detail(&self.world, Some(subject), 120, &drafts);
        let mut items = vec![(
            "Open item [Enter]".into(),
            activate(None, Hit::Open(subject.into())),
        )];
        for target in &doc.targets {
            let Hit::Key(key) = target.hit else { continue };
            let label = Selection {
                pane: String::new(),
                anchor: (target.line, target.column),
                head: (target.line, target.column + target.width.saturating_sub(1)),
            }
            .text(&doc.lines);
            let label = label
                .trim()
                .strip_prefix(&format!("{key} "))
                .unwrap_or(label.trim());
            let action = activate(Some(subject), Hit::Key(key));
            if !items.iter().any(|(_, existing)| *existing == action) {
                items.push((format!("{label} [{key}]"), action));
            }
        }
        Some((item.title.clone(), items))
    }

    fn message_menu(&self, agent: &str, id: &str) -> Option<Menu> {
        let entry = self
            .world
            .conversations
            .get(agent)?
            .items()
            .iter()
            .find(|entry| entry.id == id)?;
        let text = entry_text(entry);
        let mut items = vec![
            ("Copy its text".into(), MenuAction::CopyText(text)),
            ("Copy its id".into(), MenuAction::CopyPath(id.into())),
        ];
        if self
            .world
            .agents
            .items()
            .iter()
            .any(|a| a.id == agent && !a.unmanaged)
        {
            // Mail between others is readable context, rather than a thread the person owns.
            let personal =
                matches!(&entry.body, Body::Mail { from, to, .. } if from == "you" || to == "you");
            if personal
                || matches!(
                    entry.body,
                    Body::User(_) | Body::Assistant(_) | Body::Pending { .. }
                )
            {
                items.push((
                    "Reply".into(),
                    MenuAction::Reply {
                        agent: agent.into(),
                        message: (personal && id.starts_with("message/")).then(|| id.to_owned()),
                    },
                ));
            }
        }
        Some(("message".into(), items))
    }

    fn hit_menu(&self, hit: &Hit) -> Option<Menu> {
        let label = match hit {
            Hit::Connection => "Show connection",
            Hit::Home => "Show or hide Now [Ctrl+H]",
            Hit::Usage => "Show or hide Usage",
            Hit::PaletteSection(0) => "Open needs you [Ctrl+1]",
            Hit::PaletteSection(1) => "Open agents [Ctrl+2]",
            Hit::PaletteSection(2) => "Open missions [Ctrl+3]",
            Hit::PaletteSection(3) => "Open fleet [Ctrl+4]",
            Hit::GlassMenu => "Open spaces [Ctrl+G]",
            Hit::NewTerminal => "New terminal",
            Hit::Split(true) => "Split right [Ctrl+V]",
            Hit::Split(false) => "Split below [Ctrl+X]",
            Hit::Help => "Show help [F1]",
            Hit::Key('s') => "Show or hide the list [s]",
            Hit::Tab(index) => {
                return Some((
                    "section".into(),
                    vec![(
                        format!("Open {} [{}]", TABS.get(*index)?, index + 1),
                        activate(None, hit.clone()),
                    )],
                ));
            }
            Hit::Link(_) => "Copy link",
            _ => return None,
        };
        Some((
            "actions".into(),
            vec![(label.into(), activate(None, hit.clone()))],
        ))
    }

    fn replace_context_menu(&mut self, title: String, items: Items) {
        if items.is_empty() {
            return;
        }
        let anchor = self.frame.borrow().menu.unwrap_or(Rect::new(2, 3, 1, 1));
        self.context = Some(ContextMenu {
            column: anchor.x,
            row: anchor.y.saturating_sub(1),
            title,
            items,
            selected: 0,
        });
    }

    /// The same message actions are reachable without a pointer, through the subject menu.
    pub(super) fn open_message_choices(&mut self, agent: &str) {
        let Some(entries) = self.world.conversations.get(agent) else {
            return;
        };
        let items = entries
            .items()
            .iter()
            .rev()
            .map(|entry| {
                (
                    text::truncate(
                        &text::sanitize(&format!(
                            "{} {}",
                            entry.at,
                            entry_text(entry).lines().next().unwrap_or("message")
                        )),
                        72,
                    ),
                    MenuAction::Message {
                        agent: agent.into(),
                        entry: entry.id.clone(),
                    },
                )
            })
            .collect();
        self.replace_context_menu("messages".into(), items);
    }

    pub(super) fn open_message_actions(&mut self, agent: &str, entry: &str) {
        if let Some((title, items)) = self.message_menu(agent, entry) {
            self.replace_context_menu(title, items);
        }
    }

    /// The menu owns keys while open. Enter chooses an action; confirmations remain y-only.
    pub(super) fn context_key(&mut self, key: KeyEvent) -> bool {
        if let Some(menu) = &mut self.context {
            let action = match key.code {
                KeyCode::Esc => {
                    self.context = None;
                    return true;
                }
                KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.quit = true;
                    return true;
                }
                KeyCode::Up => {
                    menu.selected = menu.selected.saturating_sub(1);
                    None
                }
                KeyCode::Down => {
                    menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                    None
                }
                KeyCode::Home => {
                    menu.selected = 0;
                    None
                }
                KeyCode::End => {
                    menu.selected = menu.items.len().saturating_sub(1);
                    None
                }
                KeyCode::Enter => menu
                    .items
                    .get(menu.selected)
                    .map(|(_, action)| action.clone()),
                _ => menu
                    .items
                    .iter()
                    .find(|(_, action)| {
                        action.shortcut().is_some_and(|shortcut| {
                            shortcut.code == key.code && shortcut.modifiers == key.modifiers
                        })
                    })
                    .map(|(_, action)| action.clone()),
            };
            if let Some(action) = action {
                self.run_menu_action(action);
            }
            return true;
        }
        let subject_key = key.code == KeyCode::Menu
            || (key.code == KeyCode::F(10) && key.modifiers.contains(KeyModifiers::SHIFT));
        let bar_key = key.code == KeyCode::F(10) && key.modifiers.is_empty();
        if (subject_key || bar_key)
            && !self.terminal_focused()
            && !self.editing
            && !self.help
            && !self.palette_open()
            && self.confirm.is_none()
            && self.find.is_none()
            && self.new_mission.is_none()
        {
            if bar_key {
                let info = self.frame.borrow();
                let mut items = Items::new();
                for (_, hit) in info.hits.iter().filter(|(rect, _)| rect.y == info.area.y) {
                    if let Some((_, actions)) = self.hit_menu(hit) {
                        for (label, action) in actions {
                            if !items.iter().any(|(_, existing)| *existing == action) {
                                items.push((label, action));
                            }
                        }
                    }
                }
                drop(info);
                self.replace_context_menu("top bar".into(), items);
                return true;
            }
            let subject = self
                .popover
                .as_deref()
                .map(|id| id.trim_start_matches("actions:").to_owned())
                .or_else(|| {
                    self.glasses
                        .as_ref()
                        .filter(|glasses| glasses.sidebar.focused && glasses.sidebar.shown)
                        .and_then(|glasses| {
                            self.listing_for(glasses.sidebar.section, 40)
                                .ids
                                .get(glasses.sidebar.selected[glasses.sidebar.section])
                                .cloned()
                        })
                })
                .or_else(|| self.attention_focus())
                .or_else(|| self.selected_id());
            if let Some(subject) = subject
                && let Some((title, items)) = self.subject_menu(&subject)
            {
                self.context = Some(ContextMenu {
                    column: 2,
                    row: 2,
                    title,
                    items,
                    selected: 0,
                });
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(ui: &Ui, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn press(ui: &mut Ui, code: KeyCode) {
        ui.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn mouse(ui: &mut Ui, button: MouseButton, x: u16, y: u16) {
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    fn row(ui: &Ui, id: &str) -> Rect {
        ui.frame
            .borrow()
            .context_rows
            .iter()
            .find(|(_, subject, _)| subject == id)
            .unwrap()
            .0
    }

    fn choose(ui: &mut Ui, wanted: impl Fn(&MenuAction) -> bool) {
        draw(ui, 160, 48);
        let rect = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find_map(|(rect, hit)| match hit {
                Hit::Menu(action) if wanted(action) => Some(*rect),
                _ => None,
            })
            .unwrap();
        mouse(ui, MouseButton::Left, rect.x + 2, rect.y);
    }

    fn agent_rows(spaces: bool) -> Ui {
        let mut ui = Ui::new(demo::world());
        if spaces {
            ui.glasses = Some(glass::Glasses::open(None, None));
            ui.toggle_sidebar();
        } else {
            ui.switch_tab(1);
        }
        ui
    }

    #[test]
    fn a_right_click_targets_its_row_without_selecting_and_outside_clicks_only_close() {
        for spaces in [false, true] {
            let mut ui = agent_rows(spaces);
            draw(&ui, 160, 48);
            let before = ui.selected_id();
            let subject = ui
                .frame
                .borrow()
                .context_rows
                .iter()
                .find(|(_, id, _)| Some(id) != before.as_ref())
                .unwrap()
                .1
                .clone();
            let rect = row(&ui, &subject);
            let focus = ui.focus();
            mouse(&mut ui, MouseButton::Right, rect.x + 2, rect.y);
            assert_eq!(ui.selected_id(), before);
            assert_eq!(ui.focus(), focus);
            assert!(ui.effects.is_empty() && ui.conversation_state.selection.is_none());
            let menu = ui.context.as_ref().unwrap();
            assert!(menu.items[0].0.starts_with("Open conversation"));
            assert!(
                menu.items
                    .iter()
                    .any(|(label, _)| label.contains("Copy its path [c]"))
            );
            assert!(
                menu.items
                    .iter()
                    .any(|(label, _)| label.contains("Terminal [t]"))
            );
            draw(&ui, 160, 48);
            mouse(&mut ui, MouseButton::Left, 0, 47);
            assert!(ui.context.is_none());
            assert_eq!(ui.selected_id(), before);
            assert_eq!(ui.focus(), focus);
            draw(&ui, 160, 48);
            mouse(&mut ui, MouseButton::Right, rect.x + 2, rect.y);
            choose(
                &mut ui,
                |action| matches!(action, MenuAction::Activate { hit, .. } if hit.as_ref() == &Hit::Open(subject.clone())),
            );
            assert_eq!(ui.selected_id().as_deref(), Some(subject.as_str()));
            assert!(ui.context.is_none());
        }
    }

    #[test]
    fn agent_changes_and_interrupts_use_the_existing_y_only_confirmation() {
        let mut ui = agent_rows(true);
        ui.live = true;
        let subject = ui.world.agents.items()[0].id.clone();
        if let Load::Ready(agents) = &mut ui.world.agents {
            agents[0].state = AgentState::Working;
        }
        for key in ['r', 'p', 'x', 'i'] {
            draw(&ui, 160, 48);
            let rect = row(&ui, &subject);
            mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
            press(&mut ui, KeyCode::Char(key));
            assert!(ui.confirm.is_some(), "{key} must ask first");
            assert!(ui.effects.is_empty());
            press(&mut ui, KeyCode::Enter);
            assert!(ui.effects.is_empty(), "Enter must never confirm {key}");
            assert!(ui.confirm.is_none());
        }
        draw(&ui, 160, 48);
        let rect = row(&ui, &subject);
        mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
        press(&mut ui, KeyCode::Char('r'));
        press(&mut ui, KeyCode::Char('y'));
        assert!(
            matches!(ui.effects.as_slice(), [Effect::AgentControl { agent, control: AgentControl::Restart }] if agent == &subject)
        );
        ui.effects.clear();
        draw(&ui, 160, 48);
        mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
        press(&mut ui, KeyCode::Char('i'));
        press(&mut ui, KeyCode::Char('y'));
        assert!(
            matches!(ui.effects.as_slice(), [Effect::StopAgent { agent }] if agent == &subject)
        );
    }

    #[test]
    fn mission_rows_open_their_own_declaration_and_home_reuses_every_card_key() {
        let mut ui = Ui::new(demo::world());
        ui.switch_tab(2);
        draw(&ui, 160, 48);
        let subject = ui.frame.borrow().context_rows[1].1.clone();
        let rect = row(&ui, &subject);
        mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
        assert!(
            ui.context
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|(_, action)| *action == MenuAction::CopyPath(subject.clone()))
        );
        press(&mut ui, KeyCode::Char('k'));
        assert_eq!(ui.selected_id().as_deref(), Some(subject.as_str()));
        assert!(ui.kdl);
        for item in ui.world.attention.items() {
            let (_, items) = ui.subject_menu(&item.id).unwrap();
            assert!(items[0].0.starts_with("Open item"));
            assert!(
                items
                    .iter()
                    .any(|(_, action)| *action == activate(Some(&item.id), Hit::Key('t')))
            );
            let doc = screens::home_detail(
                &ui.world,
                Some(&item.id),
                120,
                &Drafts {
                    text: None,
                    cursor: 0,
                    editing: false,
                    confirm: None,
                    answering: None,
                    needs_words: None,
                    chat: None,
                },
            );
            for target in doc.targets {
                if let Hit::Key(key) = target.hit {
                    assert!(
                        items
                            .iter()
                            .any(|(_, action)| *action == activate(Some(&item.id), Hit::Key(key))),
                        "missing {key} for {}",
                        item.kind.word()
                    );
                }
            }
        }
        ui.switch_tab(0);
        ui.live = true;
        if let Load::Ready(items) = &mut ui.world.attention {
            for item in items {
                item.actions.push("review.approve".into());
            }
        }
        draw(&ui, 160, 48);
        let review = ui
            .world
            .attention
            .items()
            .iter()
            .find(|item| item.kind.word() == "review")
            .unwrap()
            .id
            .clone();
        let rect = row(&ui, &review);
        mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
        press(&mut ui, KeyCode::Char('a'));
        assert_eq!(ui.attention_focus().as_deref(), Some(review.as_str()));
        assert!(ui.confirm.is_some() && ui.effects.is_empty());
        press(&mut ui, KeyCode::Enter);
        assert!(ui.effects.is_empty());
        draw(&ui, 160, 48);
        mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
        press(&mut ui, KeyCode::Char('a'));
        press(&mut ui, KeyCode::Char('y'));
        assert!(
            matches!(ui.effects.as_slice(), [Effect::Attention { id, action, .. }] if id == &review && action == "review.approve")
        );
    }

    #[test]
    fn messages_copy_exact_text_and_ids_and_reply_in_the_existing_send_thread() {
        let mut ui = Ui::new(demo::world());
        let agent = ui.world.agents.items()[0].id.clone();
        let body = "First line: **copper**.\n\nSecond line: café 🐚.";
        let message = "message/copper";
        ui.world.conversations.insert(
            agent.clone(),
            Load::Ready(vec![Entry {
                id: message.into(),
                at: "12:00".into(),
                body: Body::Mail {
                    from: "keeper".into(),
                    to: "you".into(),
                    subject: "Copper update".into(),
                    body: body.into(),
                    delivered: true,
                    dictated: false,
                    signed: None,
                    images: Vec::new(),
                },
            }]),
        );
        ui.open(&agent);
        draw(&ui, 160, 48);
        let point = {
            let info = ui.frame.borrow();
            let pane = info
                .panes
                .iter()
                .find(|pane| pane.key == format!("chat:{agent}"))
                .unwrap();
            (
                pane.rect.x + 2,
                pane.rect.y + (pane.entries[0].1 - pane.top) as u16,
            )
        };
        mouse(&mut ui, MouseButton::Right, point.0, point.1);
        let menu = ui.context.as_ref().unwrap();
        assert_eq!(menu.items[0].1, MenuAction::CopyText(body.into()));
        assert_eq!(menu.items[1].1, MenuAction::CopyPath(message.into()));
        assert!(!menu.items.iter().any(|(label, _)| label.contains("unread")));
        ui.live = true;
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Enter);
        assert!(ui.editing);
        assert_eq!(ui.replies.get(&agent).map(String::as_str), Some(message));
        ui.conversation_state
            .drafts
            .insert(agent.clone(), "Thanks for the detail.".into());
        press(&mut ui, KeyCode::Enter);
        assert!(
            matches!(ui.effects.as_slice(), [Effect::Send { agent: to, in_reply_to: Some(id), text, .. }] if to == &agent && id == message && text == "Thanks for the detail.")
        );
        assert!(!ui.replies.contains_key(&agent));
        ui.effects.clear();
        ui.editing = false;
        ui.key(KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT));
        choose(&mut ui, |action| {
            *action == MenuAction::Messages(agent.clone())
        });
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            ui.context.as_ref().unwrap().items[0].1,
            MenuAction::CopyText(body.into()),
            "keyboard reaches the same message menu"
        );
    }

    #[test]
    fn all_bar_targets_reuse_their_click_and_a_small_menu_keeps_every_row_reachable() {
        for spaces in [false, true] {
            let mut ui = agent_rows(spaces);
            draw(&ui, 160, 48);
            let bar = ui
                .frame
                .borrow()
                .hits
                .iter()
                .filter(|(rect, _)| rect.y == 0)
                .cloned()
                .collect::<Vec<_>>();
            press(&mut ui, KeyCode::F(10));
            let menu = ui.context.as_ref().unwrap();
            for (_, hit) in &bar {
                assert!(
                    menu.items
                        .iter()
                        .any(|(_, action)| *action == activate(None, hit.clone())),
                    "the keyboard reaches bar target {hit:?}"
                );
            }
            press(&mut ui, KeyCode::Esc);
            for (rect, hit) in bar {
                mouse(&mut ui, MouseButton::Right, rect.x, rect.y);
                assert!(ui.context.is_some(), "bar target {hit:?} has a menu");
                let (_, action) = &ui.context.as_ref().unwrap().items[0];
                assert_eq!(*action, activate(None, hit));
                press(&mut ui, KeyCode::Esc);
                draw(&ui, 160, 48);
            }
        }
        let mut ui = agent_rows(false);
        ui.key(KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT));
        assert!(ui.context.as_ref().unwrap().items.len() > 4);
        press(&mut ui, KeyCode::End);
        let last = ui.context.as_ref().unwrap().items.last().unwrap().1.clone();
        let area = Rect::new(0, 0, 20, 6);
        *ui.frame.borrow_mut() = FrameInfo {
            area,
            ..FrameInfo::default()
        };
        ui.draw_context_menu(&mut Buffer::empty(area), area);
        let info = ui.frame.borrow();
        let menu = info.menu.unwrap();
        assert_eq!(menu.intersection(info.area), menu);
        assert!(
            info.hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Menu(action) if action == &last))
        );
    }

    #[test]
    fn terminal_body_right_clicks_reach_the_program_and_menu_rows_do_not() {
        use pty_core::protocol::{MessageType, PacketReader, encode_packet};
        use std::io::{Read as _, Write as _};
        use std::os::unix::net::UnixStream;
        let mut ui = Ui::new(demo::world());
        ui.glasses = Some(glass::Glasses::open(None, None));
        let shell = "terminal/copper-shell";
        ui.open_in_glass(Pane::Terminal(shell.into()), glass::Open::Tab);
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        ui.terminal = Some(TerminalView {
            agent: shell.into(),
            title: "Copper shell".into(),
            name: "Copper shell".into(),
            lines: Vec::new(),
            cursor: None,
            stale: None,
            ended: None,
            native: Some(pty::NativeTerminal::spawn(
                client,
                "copper-shell",
                "one".into(),
                24,
                80,
            )),
        });
        peer.write_all(&encode_packet(
            MessageType::Screen,
            b"\x1b[?1000h\x1b[?1006h",
        ))
        .unwrap();
        let start = Instant::now();
        while !ui
            .native_terminal()
            .unwrap()
            .mode()
            .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE)
        {
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(5));
        }
        draw(&ui, 120, 32);
        let body = ui.terminal_body.get().unwrap();
        mouse(&mut ui, MouseButton::Right, body.x + 2, body.y + 1);
        assert!(ui.context.is_none());
        let mut reader = PacketReader::new();
        'received: loop {
            let mut bytes = [0; 1024];
            let count = peer.read(&mut bytes).unwrap();
            for packet in reader.feed(&bytes[..count]).unwrap() {
                if packet.type_ == MessageType::Data {
                    assert_eq!(packet.payload, b"\x1b[<2;3;2M");
                    break 'received;
                }
            }
        }
        let tab = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find_map(|(rect, hit)| match hit {
                Hit::GlassTab(0, _) => Some(*rect),
                _ => None,
            })
            .unwrap();
        mouse(&mut ui, MouseButton::Right, tab.x, tab.y);
        draw(&ui, 120, 32);
        let copy = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find_map(|(rect, hit)| match hit {
                Hit::Menu(MenuAction::CopyPath(path)) if path == shell => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert!(copy.y >= body.y, "the menu overlays the terminal body");
        mouse(&mut ui, MouseButton::Left, copy.x + 2, copy.y);
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            ui.mouse(MouseEvent {
                kind,
                column: copy.x + 2,
                row: copy.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(ui.context.is_none());
        assert!(ui.flash.as_ref().unwrap().0.starts_with("Copied"));
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        loop {
            let mut bytes = [0; 1024];
            match peer.read(&mut bytes) {
                Ok(count) if count > 0 => {
                    for packet in reader.feed(&bytes[..count]).unwrap() {
                        assert_ne!(
                            packet.type_,
                            MessageType::Data,
                            "menu click leaked to the program"
                        );
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                other => panic!("unexpected terminal read {other:?}"),
            }
        }
    }
}
