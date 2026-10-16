//! Alerts in the conversation of the agent they belong to: what waits on the person, above the
//! message box, with the answers that one tap can send and a way into the whole card in Now.
//!
//! An alert is anything that blocks or waits on an answer: an ask, a human gate, a launch or
//! revision approval, a native harness prompt or a harness login. It clears itself when it is
//! answered anywhere, so the band shows exactly what st lists and nothing it remembers.

use super::doc::AlertTap;
use super::*;
use ratatui::style::Color;

/// The most alerts the band lists before it says how many more wait in Now.
const SHOWN: usize = 2;
/// The lines of a pending call the band shows. A call that is longer is cut with an ellipsis, and
/// then `allow` is withheld: a person allows only what they can read in full.
const CALL_LINES: usize = 4;

/// One row of the band: its line, and the buttons on it by column.
pub(super) struct BandRow {
    pub(super) line: Line<'static>,
    pub(super) hits: Vec<(u16, u16, Hit)>,
}

impl BandRow {
    fn text(spans: Vec<Span<'static>>) -> Self {
        Self {
            line: Line::from(spans),
            hits: Vec::new(),
        }
    }

    /// The buttons in as few rows as fit `width`.
    fn button_rows(buttons: Vec<(String, Color, Hit)>, width: usize) -> Vec<Self> {
        let mut rows = Vec::new();
        let mut row: Vec<(String, Color, Hit)> = Vec::new();
        let mut used = 1;
        for button in buttons {
            let needed = text::width(&button.0) + 4;
            if !row.is_empty() && used + needed > width {
                rows.push(Self::buttons(std::mem::take(&mut row)));
                used = 1;
            }
            used += needed;
            row.push(button);
        }
        if !row.is_empty() {
            rows.push(Self::buttons(row));
        }
        rows
    }

    fn buttons(buttons: Vec<(String, Color, Hit)>) -> Self {
        let mut spans = vec![Span::raw(" ")];
        let mut hits = Vec::new();
        let mut column = 1u16;
        for (label, color, hit) in buttons {
            let shown = format!(" {label} ");
            let width = text::width(&shown) as u16;
            hits.push((column, width, hit));
            spans.push(Span::styled(
                shown,
                Style::default().fg(theme::CRUST).bg(color),
            ));
            spans.push(Span::raw("  "));
            column += width + 2;
        }
        Self {
            line: Line::from(spans),
            hits,
        }
    }
}

/// A dim note under an alert's title, wrapped to the pane instead of cut at its edge.
fn note_rows(note: &str, width: usize) -> Vec<BandRow> {
    text::wrap(
        &[text::run(note.to_owned(), theme::dim())],
        width,
        &[text::run("   ", theme::dim())],
        &[text::run("   ", theme::dim())],
        None,
    )
    .into_iter()
    .map(|line| BandRow {
        line,
        hits: Vec::new(),
    })
    .collect()
}

impl Ui {
    /// The open alerts that show in `agent`'s conversation, in the order Now lists them.
    pub(super) fn alerts_in(&self, agent: &str) -> Vec<&Attention> {
        self.world
            .attention
            .items()
            .iter()
            .filter(|item| {
                item.is_alert()
                    && item.is_in(agent)
                    && !self.snoozed.contains(&item.id)
                    && !self.closed.contains(&item.id)
            })
            .collect()
    }

    /// The tool calls the conversation shows as unanswered, oldest first, as the seat's harness
    /// recorded them. A permission prompt is for one of them, but `prompt.respond` names no
    /// call: only when exactly one is unanswered is it certain which one an allow lets through.
    fn unanswered_calls(&self, agent: &str) -> Vec<String> {
        let Some(Load::Ready(entries)) = self.world.conversations.get(agent) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| match &entry.body {
                Body::Tool {
                    title,
                    state: ToolState::Running,
                    ..
                } => Some(title.clone()),
                _ => None,
            })
            .collect()
    }

    /// The band for `agent` at `width` columns: empty when nothing waits on the person there,
    /// one line that says what waits until the person expands it, then the cards. It is only a
    /// click away from either: no key reaches it, so typing is never taken by an alert.
    pub(super) fn alert_band(&self, agent: &str, width: usize) -> Vec<BandRow> {
        let alerts = self.alerts_in(agent);
        let Some(first) = alerts.first() else {
            return Vec::new();
        };
        if self.alerts_open.contains(agent) {
            return self.alert_card(agent, width);
        }
        let count = alerts.len();
        let (glyph, color) = screens::attention_style(&first.kind);
        let lead = format!(" ◆ {count} alert{} · ", if count == 1 { "" } else { "s" });
        let tail = " ▸ show ";
        let room = width
            .saturating_sub(text::width(&lead) + text::width(tail) + 4)
            .max(8);
        let spans = vec![
            Span::styled(lead, theme::strong(theme::PERSON)),
            Span::styled(format!("{glyph} "), theme::fg(color)),
            Span::styled(
                text::truncate(first_line(&first.title), room),
                theme::text(),
            ),
            Span::styled(tail, theme::dim()),
        ];
        vec![BandRow {
            line: Line::from(spans),
            hits: vec![(
                0,
                width as u16,
                Hit::Alert(agent.to_owned(), AlertTap::Toggle),
            )],
        }]
    }

    /// Show or hide the cards of `agent`'s alerts.
    pub(super) fn toggle_alerts(&mut self, agent: &str) {
        if !self.alerts_open.remove(agent) {
            self.alerts_open.insert(agent.to_owned());
        }
    }

    /// The cards of `agent`'s alerts at `width` columns, under a line that hides them again.
    fn alert_card(&self, agent: &str, width: usize) -> Vec<BandRow> {
        let alerts = self.alerts_in(agent);
        if alerts.is_empty() {
            return Vec::new();
        }
        let full = width;
        let width = width.saturating_sub(2).max(10);
        let count = alerts.len();
        // The pane's title bar, filled across the width so the cards below read as one block
        // that belongs to the box under it; the whole bar hides it again.
        let left = format!(" ◆ {count} alert{} ", if count == 1 { "" } else { "s" });
        let right = " ▾ hide ";
        let gap = full.saturating_sub(text::width(&left) + text::width(right));
        let bar = |style: Style| style.bg(theme::SURFACE0);
        let mut rows = vec![BandRow {
            line: Line::from(vec![
                Span::styled(left, bar(theme::strong(theme::PERSON))),
                Span::styled(" ".repeat(gap), bar(Style::default())),
                Span::styled(right, bar(theme::dim())),
            ]),
            hits: vec![(
                0,
                full as u16,
                Hit::Alert(agent.to_owned(), AlertTap::Toggle),
            )],
        }];
        for (index, item) in alerts.iter().take(SHOWN).enumerate() {
            // A thin rule between alerts, so where one ends and the next begins is plain.
            if index > 0 {
                rows.push(BandRow::text(vec![Span::styled(
                    format!(" {}", "┄".repeat(full.saturating_sub(2))),
                    theme::fg(theme::SURFACE1),
                )]));
            }
            let first_row = rows.len();
            let (glyph, color) = screens::attention_style(&item.kind);
            let mut head = vec![
                text::run(format!(" {glyph} "), theme::fg(color)),
                text::run(item.title.clone(), theme::bold()),
            ];
            head.push(text::run(format!("  {} ago", item.age), theme::dim()));
            rows.extend(
                text::wrap(&head, width, &[], &[text::run("   ", theme::dim())], None)
                    .into_iter()
                    .take(2)
                    .map(|line| BandRow {
                        line,
                        hits: Vec::new(),
                    }),
            );
            // Room between the title and what it says, as between what it says and its buttons:
            // an alert is read in pieces, and a packed card read as one block (Nathan,
            // 2026-10-12: "still too compact when I expand it").
            rows.push(BandRow::text(Vec::new()));
            let mut buttons: Vec<(String, Color, Hit)> = Vec::new();
            match &item.kind {
                AttentionKind::Prompt {
                    seat_id,
                    answers,
                    text: prompt,
                    ..
                } => {
                    let calls = if answers.is_empty() {
                        Vec::new()
                    } else {
                        self.unanswered_calls(seat_id)
                    };
                    match calls.as_slice() {
                        // Several calls are unanswered and the prompt names none: show them all
                        // and offer only deny. Allowing the wrong one is not undoable.
                        [_, _, ..] => {
                            rows.extend(note_rows(
                                &format!(
                                    "{} calls are unanswered and st cannot tell which one Claude asks about; allow it in the terminal (Ctrl+]):",
                                    calls.len()
                                ),
                                width,
                            ));
                            for call in calls.iter().take(CALL_LINES) {
                                rows.push(BandRow::text(vec![
                                    Span::styled("   · ", theme::dim()),
                                    Span::styled(
                                        text::truncate(first_line(call), width.saturating_sub(8)),
                                        theme::text(),
                                    ),
                                ]));
                            }
                            if calls.len() > CALL_LINES {
                                rows.push(BandRow::text(vec![Span::styled(
                                    format!("   … and {} more", calls.len() - CALL_LINES),
                                    theme::dim(),
                                )]));
                            }
                        }
                        [call] => {
                            let lines = text::wrap(
                                &[text::run(call.clone(), theme::text())],
                                width.saturating_sub(4),
                                &[text::run("   ", theme::dim())],
                                &[text::run("   ", theme::dim())],
                                None,
                            );
                            let cut = lines.len() > CALL_LINES;
                            let last = lines.len().min(CALL_LINES);
                            for (index, line) in lines.into_iter().take(CALL_LINES).enumerate() {
                                let mut line = line;
                                if cut && index + 1 == last {
                                    line.spans.push(Span::styled(" …", theme::dim()));
                                }
                                rows.push(BandRow { line, hits: Vec::new() });
                            }
                            // The whole call must be in view to allow it.
                            if !cut && answers.iter().any(|answer| answer == "allow") {
                                buttons.push((
                                    "allow".into(),
                                    theme::GREEN,
                                    Hit::Alert(item.id.clone(), AlertTap::Answer("allow".into())),
                                ));
                            }
                        }
                        [] if answers.is_empty() => {
                            // Another harness's prompt: it is answered where it is shown.
                            rows.extend(
                                text::wrap(
                                    &[text::run(
                                        format!("{} · answer it in the terminal (Ctrl+])", first_line(prompt)),
                                        theme::dim(),
                                    )],
                                    width,
                                    &[text::run("   ", theme::dim())],
                                    &[text::run("   ", theme::dim())],
                                    None,
                                )
                                .into_iter()
                                .take(2)
                                .map(|line| BandRow { line, hits: Vec::new() }),
                            );
                        }
                        [] => rows.extend(note_rows(
                            "the call it would make is not in the conversation yet: answer it in the terminal (Ctrl+]) or open it in Now",
                            width,
                        )),
                    }
                    if answers.iter().any(|answer| answer == "deny") {
                        buttons.push((
                            "deny".into(),
                            theme::RED,
                            Hit::Alert(item.id.clone(), AlertTap::Answer("deny".into())),
                        ));
                    }
                }
                AttentionKind::Request {
                    structured: Some(request),
                    ..
                } => {
                    // A named answer is one tap, unless it needs the person's words: that one
                    // is written in Now.
                    for answer in request
                        .answers
                        .iter()
                        .filter(|answer| answer.outcome.as_deref() != Some("request_changes"))
                        .take(3)
                    {
                        buttons.push((
                            text::truncate(&answer.label, 28),
                            theme::ACCENT,
                            Hit::Alert(item.id.clone(), AlertTap::Answer(answer.id.clone())),
                        ));
                    }
                }
                AttentionKind::Request { question, .. } => {
                    rows.push(BandRow::text(vec![
                        Span::styled("   ", theme::dim()),
                        Span::styled(
                            text::truncate(first_line(question), width.saturating_sub(4)),
                            theme::soft(),
                        ),
                    ]));
                    if item.title.trim_end().ends_with('?')
                        && !item.actions.iter().any(|action| action == "custom.reply")
                    {
                        buttons.push((
                            "yes".into(),
                            theme::GREEN,
                            Hit::Alert(item.id.clone(), AlertTap::Answer("Yes".into())),
                        ));
                        buttons.push((
                            "no".into(),
                            theme::RED,
                            Hit::Alert(item.id.clone(), AlertTap::Answer("No".into())),
                        ));
                    }
                }
                AttentionKind::Login { text: login, seats } => {
                    rows.push(BandRow::text(vec![Span::styled(
                        format!(
                            "   {} · {} seat{} share this sign-in",
                            text::truncate(first_line(login), width.saturating_sub(30)),
                            seats.len().max(1),
                            if seats.len() > 1 { "s" } else { "" }
                        ),
                        theme::dim(),
                    )]));
                }
                _ => {}
            }
            buttons.push((
                "open in Now".into(),
                theme::SURFACE2,
                Hit::Alert(item.id.clone(), AlertTap::Open),
            ));
            rows.push(BandRow::text(Vec::new()));
            rows.extend(BandRow::button_rows(buttons, width));
            rows.push(BandRow::text(Vec::new()));
            // Each alert carries a bar down its left side in its own colour.
            for row in &mut rows[first_row..] {
                row.line
                    .spans
                    .insert(0, Span::styled("▌", theme::fg(color)));
                for hit in &mut row.hits {
                    hit.0 += 1;
                }
            }
        }
        if count > SHOWN {
            rows.push(BandRow {
                line: Line::from(Span::styled(
                    format!("   +{} more in Now", count - SHOWN),
                    theme::dim(),
                )),
                hits: Vec::new(),
            });
        }
        rows
    }

    /// Open an alert's whole card in Now, selected.
    pub(super) fn open_alert(&mut self, id: &str) {
        self.open_home();
        if let Some(index) = self
            .listing_for(0, 40)
            .ids
            .iter()
            .position(|item| item == id)
        {
            self.select(index);
        }
    }

    /// Send a typed answer to an alert from its band: a prompt's `allow` or `deny`, a structured
    /// request's named answer, or the yes or no of a question.
    pub(super) fn answer_alert(&mut self, id: &str, answer: &str) {
        let Some(item) = self
            .world
            .attention
            .items()
            .iter()
            .find(|item| item.id == id)
        else {
            self.flash("That alert is gone");
            return;
        };
        let effect = match &item.kind {
            AttentionKind::Prompt { .. } if item.actions.iter().any(|a| a == "prompt.respond") => {
                Some((
                    Effect::Attention {
                        id: id.to_owned(),
                        action: "prompt.respond".into(),
                        reason: None,
                        answer: Some(answer.to_owned()),
                    },
                    format!("Answering {answer}…"),
                ))
            }
            AttentionKind::Request {
                structured: Some(request),
                ..
            } => request
                .answers
                .iter()
                .find(|option| option.id == answer)
                .map(|option| {
                    (
                        Effect::Attention {
                            id: id.to_owned(),
                            action: "work.done".into(),
                            reason: Some(option.label.clone()),
                            answer: Some(option.id.clone()),
                        },
                        format!("Answering “{}”…", option.label),
                    )
                }),
            AttentionKind::Request {
                structured: None, ..
            } if matches!(answer, "Yes" | "No")
                && item.actions.iter().any(|a| a == "work.done") =>
            {
                Some((
                    Effect::Attention {
                        id: id.to_owned(),
                        action: "work.done".into(),
                        reason: Some(answer.to_owned()),
                        answer: None,
                    },
                    format!("Answering “{answer}”…"),
                ))
            }
            _ => None,
        };
        match effect {
            Some((effect, notice)) if self.live => {
                self.effects.push(effect);
                self.flash(notice);
            }
            Some((_, notice)) => self.flash(format!("{notice} demo: nothing was sent")),
            None => self.flash("st does not offer that answer here: open it in Now"),
        }
    }
}

/// The first non-empty line, without markdown's heading marks.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(id: &str, answers: &[&str]) -> Attention {
        Attention {
            id: id.into(),
            tier: Tier::Stopped,
            title: "agent/example/atlas is waiting for a permission".into(),
            waiting: None,
            age: "1m".into(),
            mission: None,
            agent: Some("agent/example/atlas".into()),
            kind: AttentionKind::Prompt {
                seat: "atlas".into(),
                seat_id: "agent/example/atlas".into(),
                text: "Claude asks to use Bash".into(),
                answers: answers.iter().map(|answer| (*answer).to_owned()).collect(),
                episode: "episode-1".into(),
            },
            actions: if answers.is_empty() {
                vec![]
            } else {
                vec!["prompt.respond".into()]
            },
            related: vec![],
            raised_by: None,
            blocked: None,
            conversations: vec!["agent/example/atlas".into()],
        }
    }

    fn with_alerts(items: Vec<Attention>) -> Ui {
        let mut world = demo::world();
        world.attention = Load::Ready(items);
        Ui::new(world)
    }

    fn running_call(title: &str) -> Entry {
        Entry {
            id: "tool-1".into(),
            at: "10:00".into(),
            body: Body::Tool {
                title: title.into(),
                state: ToolState::Running,
                output: vec![],
            },
        }
    }

    /// The cards, as a person sees them after expanding the band.
    impl Ui {
        fn expanded_band(&self, agent: &str, width: usize) -> Vec<BandRow> {
            self.alert_card(agent, width)
        }
    }

    fn texts(rows: &[BandRow]) -> String {
        rows.iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn taps(rows: &[BandRow]) -> Vec<Hit> {
        rows.iter()
            .flat_map(|row| row.hits.iter().map(|(_, _, hit)| hit.clone()))
            .collect()
    }

    #[test]
    fn nothing_waiting_shows_no_band() {
        let ui = with_alerts(vec![]);
        assert!(ui.expanded_band("agent/example/atlas", 80).is_empty());
    }

    #[test]
    fn an_alert_shows_only_in_the_conversation_it_belongs_to() {
        let ui = with_alerts(vec![prompt("attention/one", &["allow", "deny"])]);
        assert!(!ui.expanded_band("agent/example/atlas", 80).is_empty());
        assert!(ui.expanded_band("agent/example/other", 80).is_empty());
    }

    #[test]
    fn a_pending_claude_call_is_shown_in_full_beside_allow_and_deny() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["allow", "deny"])]);
        ui.world.conversations.insert(
            "agent/example/atlas".into(),
            Load::Ready(vec![running_call("Bash · rm -rf build/cache")]),
        );
        let rows = ui.expanded_band("agent/example/atlas", 80);
        let shown = texts(&rows);
        assert!(shown.contains("1 alert"), "{shown}");
        assert!(shown.contains("rm -rf build/cache"), "{shown}");
        let taps = taps(&rows);
        assert!(taps.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("allow".into())
        )));
        assert!(taps.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("deny".into())
        )));
    }

    #[test]
    fn with_several_calls_unanswered_every_one_is_listed_and_only_deny_is_offered() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["allow", "deny"])]);
        ui.world.conversations.insert(
            "agent/example/atlas".into(),
            Load::Ready(vec![
                running_call("Bash · rm -rf build/cache"),
                Entry {
                    id: "tool-2".into(),
                    ..running_call("Write · notes.md")
                },
            ]),
        );
        let rows = ui.expanded_band("agent/example/atlas", 80);
        let shown = texts(&rows);
        assert!(shown.contains("2 calls are unanswered"), "{shown}");
        assert!(
            shown.contains("rm -rf build/cache") && shown.contains("notes.md"),
            "{shown}"
        );
        let taps = taps(&rows);
        assert!(!taps.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("allow".into())
        )));
        assert!(taps.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("deny".into())
        )));
    }

    #[test]
    fn allow_is_withheld_when_the_call_cannot_be_shown() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["allow", "deny"])]);
        // No call in the conversation yet.
        let taps_without = taps(&ui.expanded_band("agent/example/atlas", 80));
        assert!(!taps_without.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("allow".into())
        )));
        assert!(taps_without.contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("deny".into())
        )));
        // A call too long to read in full is not allowed from here either.
        let long = format!("Bash · {}", "word ".repeat(200));
        ui.world.conversations.insert(
            "agent/example/atlas".into(),
            Load::Ready(vec![running_call(&long)]),
        );
        let rows = ui.expanded_band("agent/example/atlas", 60);
        assert!(!taps(&rows).contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("allow".into())
        )));
        assert!(texts(&rows).contains('…'));
    }

    #[test]
    fn another_harnesss_prompt_is_answered_in_the_terminal() {
        let ui = with_alerts(vec![prompt("attention/one", &[])]);
        let rows = ui.expanded_band("agent/example/atlas", 80);
        let shown = texts(&rows);
        assert!(shown.contains("answer it in the terminal"), "{shown}");
        assert_eq!(
            taps(&rows),
            vec![
                Hit::Alert("agent/example/atlas".into(), AlertTap::Toggle),
                Hit::Alert("attention/one".into(), AlertTap::Open)
            ]
        );
    }

    #[test]
    fn updates_and_messages_are_not_alerts() {
        let mut update = prompt("attention/update", &[]);
        update.kind = AttentionKind::Update {
            from: "atlas".into(),
            body: "done".into(),
            about: String::new(),
            subjects: vec![],
        };
        let mut message = prompt("attention/message", &[]);
        message.kind = AttentionKind::Message {
            from: "atlas".into(),
            body: "hi".into(),
        };
        let ui = with_alerts(vec![update, message]);
        assert!(ui.expanded_band("agent/example/atlas", 80).is_empty());
    }

    #[test]
    fn more_than_two_alerts_say_how_many_wait_in_now() {
        let ui = with_alerts(vec![
            prompt("attention/a", &[]),
            prompt("attention/b", &[]),
            prompt("attention/c", &[]),
        ]);
        let shown = texts(&ui.expanded_band("agent/example/atlas", 80));
        assert!(shown.contains("3 alerts"), "{shown}");
        assert!(shown.contains("+1 more in Now"), "{shown}");
    }

    #[test]
    fn tapping_allow_sends_prompt_respond_for_that_alert() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["allow", "deny"])]);
        ui.live = true;
        ui.click(Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("deny".into()),
        ));
        assert!(matches!(
            ui.effects.as_slice(),
            [Effect::Attention { id, action, answer: Some(answer), .. }]
                if id == "attention/one" && action == "prompt.respond" && answer == "deny"
        ));
    }

    #[test]
    fn a_tap_on_a_gone_alert_sends_nothing() {
        let mut ui = with_alerts(vec![]);
        ui.live = true;
        ui.click(Hit::Alert(
            "attention/gone".into(),
            AlertTap::Answer("allow".into()),
        ));
        assert!(ui.effects.is_empty());
    }

    #[test]
    fn open_selects_the_alert_in_now() {
        let mut ui = with_alerts(vec![prompt("attention/a", &[]), prompt("attention/b", &[])]);
        ui.click(Hit::Alert("attention/b".into(), AlertTap::Open));
        assert_eq!(ui.tab(), 0);
        assert_eq!(ui.attention_focus().as_deref(), Some("attention/b"));
    }

    #[test]
    fn the_band_is_one_line_until_it_is_expanded() {
        let mut ui = with_alerts(vec![
            prompt("attention/a", &["allow", "deny"]),
            prompt("attention/b", &[]),
        ]);
        let rows = ui.alert_band("agent/example/atlas", 80);
        assert_eq!(rows.len(), 1, "{}", texts(&rows));
        let shown = texts(&rows);
        assert!(shown.contains("2 alerts"), "{shown}");
        assert!(shown.contains("is waiting for a permission"), "{shown}");
        // Nothing on the line answers an alert by accident.
        assert_eq!(
            taps(&rows),
            vec![Hit::Alert("agent/example/atlas".into(), AlertTap::Toggle)]
        );
        ui.click(Hit::Alert("agent/example/atlas".into(), AlertTap::Toggle));
        let open = ui.alert_band("agent/example/atlas", 80);
        assert!(open.len() > 1);
        assert!(taps(&open).contains(&Hit::Alert(
            "attention/a".into(),
            AlertTap::Answer("deny".into())
        )));
        ui.click(Hit::Alert("agent/example/atlas".into(), AlertTap::Toggle));
        assert_eq!(ui.alert_band("agent/example/atlas", 80).len(), 1);
    }

    #[test]
    fn the_collapsed_line_fits_a_narrow_pane() {
        let ui = with_alerts(vec![prompt("attention/a", &[])]);
        for width in [12usize, 24, 40] {
            let rows = ui.alert_band("agent/example/atlas", width);
            assert_eq!(rows.len(), 1);
        }
    }
}
