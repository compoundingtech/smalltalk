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
/// The lines of a plan the band shows before it is cut and `approve` is withheld likewise.
const PLAN_LINES: usize = 24;

/// A pending call's input as lines a person reads: each field on its own line, with a value's
/// further lines indented under it, so a multi-line command or an edit's text is there in full.
fn call_lines(call: &PendingCall) -> Vec<String> {
    let mut lines = Vec::new();
    match &call.input {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                let mut rest = text.lines();
                lines.push(format!("{key}: {}", rest.next().unwrap_or_default()));
                lines.extend(rest.map(|line| format!("  {line}")));
            }
        }
        serde_json::Value::Null => {}
        serde_json::Value::String(text) => lines.extend(text.lines().map(str::to_owned)),
        other => lines.extend(
            serde_json::to_string_pretty(other)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned),
        ),
    }
    lines
}

/// One question of a harness's pending `AskUserQuestion` call.
#[derive(Debug, Clone, PartialEq)]
struct Question {
    text: String,
    header: String,
    options: Vec<String>,
    multi: bool,
}

/// The questions an `AskUserQuestion` call asks, when it is one and they can be read.
fn questions(call: &PendingCall) -> Option<Vec<Question>> {
    if call.tool != "AskUserQuestion" {
        return None;
    }
    let asked = call.input.get("questions")?.as_array()?;
    let questions = asked
        .iter()
        .map(|question| {
            Some(Question {
                text: question.get("question")?.as_str()?.to_owned(),
                header: question
                    .get("header")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                options: question
                    .get("options")?
                    .as_array()?
                    .iter()
                    .map(|option| option.get("label")?.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?,
                multi: question
                    .get("multiSelect")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!questions.is_empty() && questions.iter().all(|question| !question.options.is_empty()))
        .then_some(questions)
}

/// How an answer a prompt offers reads on its button.
fn answer_label(answer: &str) -> String {
    match answer {
        "use-all" => "use all".into(),
        "approve" => "approve plan".into(),
        "answer" => "send".into(),
        "approve-once" => "allow once".into(),
        "approve-kind" => "allow this kind".into(),
        other => other.to_owned(),
    }
}

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

    /// The tool calls the conversation records with no result yet, oldest first, in full.
    fn calls_waiting(&self, agent: &str) -> &[PendingCall] {
        self.world
            .pending_calls
            .get(agent)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The calls to show beside a prompt's answers: the exact call it asks about, when st names
    /// it (none when its harness named none); from an older daemon, every unanswered call, of
    /// which only a lone one is certain.
    fn prompt_calls(&self, seat: &str, call: &Option<Option<String>>) -> Vec<&PendingCall> {
        let waiting = self.calls_waiting(seat);
        match call {
            None => waiting.iter().collect(),
            Some(named) => waiting
                .iter()
                .filter(|pending| named.as_deref() == Some(pending.id.as_str()))
                .collect(),
        }
    }

    /// What the band shows under a native prompt, and its buttons. `allow` and `approve` are
    /// offered only beside the one waiting call shown in full; a question's options are chosen
    /// here and sent together; a screen prompt's own answers (use, skip, trust, ...) and `deny`
    /// are always one tap.
    fn prompt_answer_rows(
        &self,
        id: &str,
        seat: &str,
        answers: &[String],
        prompt: &str,
        call: &Option<Option<String>>,
        width: usize,
    ) -> (Vec<BandRow>, Vec<(String, Color, Hit)>) {
        let mut rows = Vec::new();
        let mut buttons = Vec::new();
        let button = |answer: &str, color: Color| {
            (
                answer_label(answer),
                color,
                Hit::Alert(id.to_owned(), AlertTap::Answer(answer.to_owned())),
            )
        };
        if answers.is_empty() {
            // Answered where it is shown.
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
            return (rows, buttons);
        }
        let calls = self.prompt_calls(seat, call);
        let gated = answers
            .iter()
            .find(|answer| matches!(answer.as_str(), "allow" | "approve" | "answer"));
        match (gated.map(String::as_str), calls.as_slice()) {
            (Some("answer"), [only]) if let Some(asked) = questions(only) => {
                let chosen = self.question_choices.get(&question_key(id, &only.id));
                for (index, question) in asked.iter().enumerate() {
                    let header = if question.header.is_empty() {
                        question.text.clone()
                    } else {
                        format!("{} · {}", question.header, question.text)
                    };
                    rows.extend(note_rows(
                        &format!(
                            "{header}{}",
                            if question.multi { " (choose any)" } else { "" }
                        ),
                        width,
                    ));
                    let picked = chosen.and_then(|chosen| chosen.get(&index));
                    rows.extend(BandRow::button_rows(
                        question
                            .options
                            .iter()
                            .enumerate()
                            .map(|(option, label)| {
                                let on = picked.is_some_and(|picked| picked.contains(&option));
                                (
                                    format!("{}{}", if on { "✓ " } else { "" }, text::truncate(label, 32)),
                                    if on { theme::GREEN } else { theme::SURFACE2 },
                                    Hit::Alert(id.to_owned(), AlertTap::Choose(index, option)),
                                )
                            })
                            .collect(),
                        width,
                    ));
                }
                let complete = (0..asked.len()).all(|index| {
                    chosen
                        .and_then(|chosen| chosen.get(&index))
                        .is_some_and(|picked| !picked.is_empty())
                });
                if complete {
                    buttons.push(button("answer", theme::GREEN));
                }
            }
            (Some("answer"), _) => rows.extend(note_rows(
                "the question is not in the conversation yet: answer it in the terminal (Ctrl+]) or open it in Now",
                width,
            )),
            // Several calls wait and the prompt names none: show them all and offer no allow.
            (Some(_), [_, _, ..]) => {
                rows.extend(note_rows(
                    &format!(
                        "{} calls are unanswered and st cannot tell which one the harness asks about; answer it in the terminal (Ctrl+]):",
                        calls.len()
                    ),
                    width,
                ));
                for call in calls.iter().take(CALL_LINES) {
                    let line = call_lines(call).into_iter().next().unwrap_or_default();
                    rows.push(BandRow::text(vec![
                        Span::styled("   · ", theme::dim()),
                        Span::styled(
                            text::truncate(&format!("{} {line}", call.tool), width.saturating_sub(8)),
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
            (Some(gated), [only]) => {
                let limit = if gated == "approve" { PLAN_LINES } else { CALL_LINES };
                let mut runs = vec![text::run(format!("{}\n", only.tool), theme::bold())];
                runs.push(text::run(call_lines(only).join("\n"), theme::text()));
                let lines = text::wrap(
                    &runs,
                    width.saturating_sub(4),
                    &[text::run("   ", theme::dim())],
                    &[text::run("   ", theme::dim())],
                    None,
                );
                let cut = lines.len() > limit;
                let last = lines.len().min(limit);
                for (index, line) in lines.into_iter().take(limit).enumerate() {
                    let mut line = line;
                    if cut && index + 1 == last {
                        line.spans.push(Span::styled(" …", theme::dim()));
                    }
                    rows.push(BandRow { line, hits: Vec::new() });
                }
                // The whole call must be in view to allow it, and the whole plan to approve it.
                if !cut {
                    buttons.push(button(gated, theme::GREEN));
                } else {
                    rows.extend(note_rows(
                        "it is too long to show here in full: open it in Now, or answer it in the terminal (Ctrl+])",
                        width,
                    ));
                }
            }
            (Some(_), []) => rows.extend(note_rows(
                "the call it would make is not in the conversation yet: answer it in the terminal (Ctrl+]) or open it in Now",
                width,
            )),
            (None, _) => rows.extend(note_rows(first_line(prompt), width)),
        }
        // A screen prompt's own answers, then deny, are one tap each.
        for answer in answers {
            match answer.as_str() {
                "allow" | "approve" | "answer" | "deny" => {}
                other => buttons.push(button(other, theme::ACCENT)),
            }
        }
        if answers.iter().any(|answer| answer == "deny") {
            buttons.push((
                if gated.is_some_and(|gated| gated == "answer") {
                    "decline".into()
                } else {
                    "deny".into()
                },
                theme::RED,
                Hit::Alert(id.to_owned(), AlertTap::Answer("deny".into())),
            ));
        }
        (rows, buttons)
    }

    /// Choose an option of a harness question alert's question, or for a multi-select toggle
    /// it. Nothing is sent until the person sends every question's choice.
    pub(super) fn choose_answer(&mut self, id: &str, question: usize, option: usize) {
        let Some((seat, call)) = self.world.attention.items().iter().find_map(|item| match &item.kind {
            AttentionKind::Prompt { seat_id, call, .. } if item.id == id => {
                Some((seat_id.clone(), call.clone()))
            }
            _ => None,
        }) else {
            self.flash("That alert is gone");
            return;
        };
        let Some((asked, key)) = (match self.prompt_calls(&seat, &call).as_slice() {
            [only] => questions(only).map(|asked| (asked, question_key(id, &only.id))),
            _ => None,
        }) else {
            self.flash("The question is gone");
            return;
        };
        let Some(multi) = asked.get(question).map(|question| question.multi) else {
            return;
        };
        // Picks belong to this exact question call: a replaced question starts with none.
        let prefix = question_key(id, "");
        self.question_choices
            .retain(|other, _| *other == key || !other.starts_with(&prefix));
        let picked = self
            .question_choices
            .entry(key)
            .or_default()
            .entry(question)
            .or_default();
        if multi {
            if !picked.remove(&option) {
                picked.insert(option);
            }
        } else {
            picked.clear();
            picked.insert(option);
        }
    }

    /// The JSON a question alert sends: positions only, each question's position to the positions
    /// of the options chosen. The questions and labels stay in the conversation; the seat's hook
    /// turns positions into labels.
    fn question_answer(&self, id: &str, seat: &str, call: &Option<Option<String>>) -> Option<String> {
        let calls = self.prompt_calls(seat, call);
        let [only] = calls.as_slice() else {
            return None;
        };
        let asked = questions(only)?;
        let chosen = self.question_choices.get(&question_key(id, &only.id))?;
        let mut answers = serde_json::Map::new();
        for (index, question) in asked.iter().enumerate() {
            let options = chosen
                .get(&index)?
                .iter()
                .filter(|option| **option < question.options.len())
                .copied()
                .collect::<Vec<_>>();
            if options.is_empty() {
                return None;
            }
            answers.insert(index.to_string(), serde_json::json!(options));
        }
        Some(serde_json::Value::Object(answers).to_string())
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
                    call,
                    ..
                } => {
                    let (prompt_rows, prompt_buttons) =
                        self.prompt_answer_rows(&item.id, seat_id, answers, prompt, call, width);
                    rows.extend(prompt_rows);
                    buttons.extend(prompt_buttons);
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
        if self.glasses.is_some() {
            self.open_home();
        } else {
            self.switch_tab(0);
        }
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
            AttentionKind::Prompt { seat_id, call, .. }
                if item.actions.iter().any(|a| a == "prompt.respond") =>
            {
                // A question's answer is the choices made here.
                let sent = if answer == "answer" {
                    self.question_answer(id, seat_id, call)
                } else {
                    Some(answer.to_owned())
                };
                sent.map(|sent| {
                    (
                        Effect::Attention {
                            id: id.to_owned(),
                            action: "prompt.respond".into(),
                            reason: None,
                            answer: Some(sent),
                        },
                        format!("Answering {}…", answer_label(answer)),
                    )
                })
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
                let prefix = question_key(id, "");
                self.question_choices.retain(|key, _| !key.starts_with(&prefix));
                self.flash(notice);
            }
            Some((_, notice)) => self.flash(format!("{notice} demo: nothing was sent")),
            None => self.flash("st does not offer that answer here: open it in Now"),
        }
    }
}

/// Where the picks for a question alert's question call are kept: by the alert and the exact call,
/// so picks made for one question never answer another that replaced it.
fn question_key(alert: &str, call: &str) -> String {
    format!("{alert}\u{0}{call}")
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
                call: None,
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

    fn call(tool: &str, input: serde_json::Value) -> PendingCall {
        PendingCall {
            id: format!("call-{tool}"),
            tool: tool.into(),
            input,
        }
    }

    fn bash(command: &str) -> PendingCall {
        call("Bash", serde_json::json!({"command": command}))
    }

    fn waiting(ui: &mut Ui, calls: Vec<PendingCall>) {
        ui.world
            .pending_calls
            .insert("agent/example/atlas".into(), calls);
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
        waiting(&mut ui, vec![bash("rm -rf build/cache")]);
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
        waiting(
            &mut ui,
            vec![
                bash("rm -rf build/cache"),
                call("Write", serde_json::json!({"file_path": "notes.md", "content": "hi"})),
            ],
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
        waiting(&mut ui, vec![bash(&"word ".repeat(200))]);
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

    /// A harness question is answered here: each question with its options, chosen with a tap
    /// (any number for a multi-select), and sent together once every question has a choice.
    #[test]
    fn a_question_is_answered_with_the_options_chosen_here() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["answer", "deny"])]);
        ui.live = true;
        waiting(
            &mut ui,
            vec![call(
                "AskUserQuestion",
                serde_json::json!({"questions": [
                    {"question": "Which color?", "header": "Color", "multiSelect": false,
                     "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]},
                    {"question": "Which pets?", "header": "Pets", "multiSelect": true,
                     "options": [{"label": "Cat", "description": ""}, {"label": "Dog", "description": ""},
                                 {"label": "Fish", "description": ""}]},
                ]}),
            )],
        );
        let send = Hit::Alert("attention/one".into(), AlertTap::Answer("answer".into()));
        let rows = ui.expanded_band("agent/example/atlas", 80);
        let shown = texts(&rows);
        assert!(shown.contains("Color · Which color?"), "{shown}");
        assert!(shown.contains("Pets · Which pets? (choose any)"), "{shown}");
        assert!(shown.contains("Fish"), "{shown}");
        assert!(!taps(&rows).contains(&send), "nothing is chosen yet");
        assert!(taps(&rows).contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("deny".into())
        )));
        for (question, option) in [(0, 0), (0, 1), (1, 0), (1, 2), (1, 1), (1, 1)] {
            ui.click(Hit::Alert("attention/one".into(), AlertTap::Choose(question, option)));
        }
        let rows = ui.expanded_band("agent/example/atlas", 80);
        assert!(texts(&rows).contains("✓ Blue"), "{}", texts(&rows));
        assert!(taps(&rows).contains(&send));
        // Another question call replaces it: the picks made for the first never answer it.
        let asked = ui.world.pending_calls["agent/example/atlas"][0].clone();
        waiting(&mut ui, vec![PendingCall { id: "call-replaced".into(), ..asked.clone() }]);
        let rows = ui.expanded_band("agent/example/atlas", 80);
        assert!(!texts(&rows).contains("✓ Blue"), "{}", texts(&rows));
        assert!(!taps(&rows).contains(&send), "nothing is chosen for the new question");
        waiting(&mut ui, vec![asked]);
        ui.click(send);
        let [Effect::Attention { action, answer: Some(answer), .. }] = ui.effects.as_slice() else {
            panic!("{:?}", ui.effects);
        };
        assert_eq!(action, "prompt.respond");
        // Positions only: the questions' and options' words stay in the conversation.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(answer).unwrap(),
            serde_json::json!({"0": [1], "1": [0, 2]})
        );
    }

    /// A prompt that names its exact call is answered only beside that call: a parent's open
    /// Task call is never the call an allow lets through, and a prompt that named none has no
    /// allow at all.
    #[test]
    fn allow_is_offered_only_beside_the_exact_call_the_prompt_names() {
        let named = |call: Option<&str>| {
            let mut item = prompt("attention/one", &["allow", "deny"]);
            if let AttentionKind::Prompt { call: named, .. } = &mut item.kind {
                *named = Some(call.map(str::to_owned));
            }
            item
        };
        let allow = Hit::Alert("attention/one".into(), AlertTap::Answer("allow".into()));
        let task = call("Task", serde_json::json!({"prompt": "run the build"}));
        // The subagent's call is not in this conversation: only the parent's Task call is.
        let mut ui = with_alerts(vec![named(Some("call-Bash"))]);
        waiting(&mut ui, vec![task.clone()]);
        let rows = ui.expanded_band("agent/example/atlas", 80);
        assert!(!taps(&rows).contains(&allow));
        assert!(!texts(&rows).contains("run the build"), "{}", texts(&rows));
        // Beside the named call, among others, allow is offered and only it is shown.
        waiting(&mut ui, vec![task.clone(), bash("make")]);
        let rows = ui.expanded_band("agent/example/atlas", 80);
        assert!(taps(&rows).contains(&allow));
        assert!(texts(&rows).contains("command: make"));
        assert!(!texts(&rows).contains("run the build"));
        // Named none: no allow, whatever is pending.
        let mut ui = with_alerts(vec![named(None)]);
        waiting(&mut ui, vec![bash("make")]);
        assert!(!taps(&ui.expanded_band("agent/example/atlas", 80)).contains(&allow));
    }

    /// A plan is shown in full beside `approve`; a screen prompt's own answers are one tap each.
    #[test]
    fn a_plan_and_a_screen_prompt_offer_their_own_answers() {
        let mut ui = with_alerts(vec![prompt("attention/one", &["approve", "deny"])]);
        waiting(
            &mut ui,
            vec![call(
                "ExitPlanMode",
                serde_json::json!({"plan": "1. Create hello.txt\n2. Write hello in it"}),
            )],
        );
        let rows = ui.expanded_band("agent/example/atlas", 80);
        assert!(texts(&rows).contains("2. Write hello in it"), "{}", texts(&rows));
        assert!(taps(&rows).contains(&Hit::Alert(
            "attention/one".into(),
            AlertTap::Answer("approve".into())
        )));

        let ui = with_alerts(vec![prompt("attention/two", &["use", "use-all", "skip"])]);
        let rows = ui.expanded_band("agent/example/atlas", 80);
        let taps = taps(&rows);
        for answer in ["use", "use-all", "skip"] {
            assert!(
                taps.contains(&Hit::Alert(
                    "attention/two".into(),
                    AlertTap::Answer(answer.into())
                )),
                "{answer}"
            );
        }
        assert!(texts(&rows).contains(" use all "), "{}", texts(&rows));
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
        assert_eq!(ui.tab, 0);
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
