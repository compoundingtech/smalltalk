//! Alerts in the conversation of the agent they belong to: what waits on the person, above the
//! message box, folded to one line until the person expands it in place to the whole card.
//!
//! An alert is anything that blocks or waits on an answer: an ask, a human gate, a launch or
//! revision approval, a native harness prompt or a harness login. It clears itself when it is
//! answered anywhere, so the band shows exactly what st lists and nothing it remembers.
//!
//! A native prompt is a form that reads like the harness's own dialog: its question line, its
//! options numbered in its order with one marked, and the keys in a footer. The same form is the
//! prompt's card in Now and its card in the band, built by [`prompt_form`] from the shared model
//! in `st3_ui_model::prompts` (web, tui and ios offer the same answers in the same words).
//! Focus only focuses: arrows and Tab move the mark and answer nothing. An answer takes its own
//! number or a click on its own row, and Enter takes the marked one only when Enter may (the
//! call or plan shown in full, the person's own choices, or a safe way past); never a refusal.

use super::doc::{AlertTap, Target};
use super::*;
use ratatui::style::{Color, Modifier};
use std::collections::{BTreeMap, BTreeSet};
use st3_ui_model::prompts::{self, Call, Form, Kind, Question};

/// The most alerts the band shows expanded before it says how many more wait in Now.
const SHOWN: usize = 2;
/// The lines of a pending call the band shows. A call that is longer is cut with an ellipsis, and
/// then `allow` is withheld: a person allows only what they can read in full. Now shows it all.
const CALL_LINES: usize = 4;
/// The lines of a plan the band shows before it is cut and `approve` is withheld likewise.
const PLAN_LINES: usize = 24;

/// Where a prompt's form is drawn: its words for the keys differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The band above a conversation's message box: Ctrl+N gives it the keys, Esc folds it.
    Band,
    /// The prompt's card in Now: `a` gives it the keys.
    Now,
}

/// One numbered row of a prompt's form, in the order it is drawn and numbered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FormRow {
    /// An option of a harness question: its question's and its own place in the pending call.
    Choose(usize, usize),
    /// An answer st sends.
    Answer(String),
}

/// What the person has done on a form so far: the mark, whether it has the keys, and a question's
/// choices.
#[derive(Clone, Copy, Default)]
pub(crate) struct FormState<'a> {
    pub mark: Option<usize>,
    pub keys: bool,
    pub chosen: Option<&'a BTreeMap<usize, BTreeSet<usize>>>,
}

/// A native prompt read as a form: the calls it may be about, the shared model's form, and its
/// rows.
pub(super) struct PromptModel<'w> {
    harness: Harness,
    calls: Vec<&'w PendingCall>,
    form: Form,
    questions: Option<Vec<Question>>,
    pub rows: Vec<FormRow>,
}

impl PromptModel<'_> {
    /// The row marked when the person has not moved the mark: a question's first option, else
    /// the form's own mark.
    pub fn default_mark(&self) -> Option<usize> {
        match &self.questions {
            Some(_) => Some(0),
            None => self.form.mark,
        }
    }

    /// Every question has a choice, so the answers can be sent.
    fn complete(&self, chosen: Option<&BTreeMap<usize, BTreeSet<usize>>>) -> bool {
        self.questions.as_ref().is_some_and(|asked| {
            (0..asked.len()).all(|index| {
                chosen
                    .and_then(|chosen| chosen.get(&index))
                    .is_some_and(|picked| !picked.is_empty())
            })
        })
    }
}

/// The calls to show beside a prompt's answers: the exact call it asks about, when st names it
/// (none when its harness named none); from an older daemon, every unanswered call, of which only
/// a lone one is certain.
fn prompt_calls<'w>(world: &'w World, seat: &str, call: &Option<Option<String>>) -> Vec<&'w PendingCall> {
    let waiting = world.pending_calls.get(seat).map(Vec::as_slice).unwrap_or_default();
    match call {
        None => waiting.iter().collect(),
        Some(named) => waiting
            .iter()
            .filter(|pending| named.as_deref() == Some(pending.id.as_str()))
            .collect(),
    }
}

/// A native prompt as a form, when st lists answers for it.
pub(super) fn prompt_model<'w>(world: &'w World, item: &Attention) -> Option<PromptModel<'w>> {
    let AttentionKind::Prompt {
        seat_id,
        answers,
        text,
        call,
        ..
    } = &item.kind
    else {
        return None;
    };
    if answers.is_empty() {
        return None;
    }
    let harness = world
        .agents
        .items()
        .iter()
        .find(|agent| &agent.id == seat_id)
        .map_or(Harness::Unknown, |agent| agent.harness);
    let calls = prompt_calls(world, seat_id, call);
    let shared: Vec<Call<'_>> = calls
        .iter()
        .map(|call| Call {
            tool: &call.tool,
            input: &call.input,
        })
        .collect();
    let form = prompts::prompt_form(
        match harness {
            Harness::Claude => prompts::Harness::Claude,
            Harness::Codex => prompts::Harness::Codex,
            _ => prompts::Harness::Other,
        },
        answers,
        text,
        &shared,
    );
    let questions = match (form.kind, shared.as_slice()) {
        (Kind::Question, [only]) => prompts::questions(*only),
        _ => None,
    };
    let mut rows = Vec::new();
    for (index, question) in questions.iter().flatten().enumerate() {
        rows.extend((0..question.options.len()).map(|option| FormRow::Choose(index, option)));
    }
    rows.extend(
        form.options
            .iter()
            .map(|option| FormRow::Answer(option.answer.clone())),
    );
    Some(PromptModel {
        harness,
        calls,
        form,
        questions,
        rows,
    })
}

/// One option row: the mark, its number and its words, wrapped under themselves.
fn option_lines(marked: bool, keys: bool, number: usize, label: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let lead = if marked { "❯ " } else { "  " };
    let style = if marked {
        style.add_modifier(Modifier::BOLD).fg(theme::ACCENT)
    } else {
        style
    };
    let lead_style = if marked && keys {
        theme::strong(theme::ACCENT)
    } else if marked {
        theme::fg(theme::ACCENT)
    } else {
        theme::dim()
    };
    let number = format!("{number}. ");
    let indent = " ".repeat(text::width(lead) + text::width(&number));
    text::wrap(
        &[text::run(label.to_owned(), style)],
        width,
        &[text::run(lead, lead_style), text::run(number, if marked { lead_style } else { theme::dim() })],
        &[text::run(indent, theme::dim())],
        None,
    )
}

/// A prompt's form into `doc` at `width`: what it is about (the call or plan in full, or the
/// questions to choose from), the harness's question line, its numbered options with one marked,
/// and the keys. `limit` cuts a call (the band's few lines); a call that is cut withholds the
/// answers that would let it through. Returns whether the call or plan it is about is in view
/// in full, and how many of its lines are shown.
pub(super) fn prompt_form(
    doc: &mut Doc,
    world: &World,
    item: &Attention,
    state: FormState<'_>,
    width: usize,
    limit: Option<usize>,
    place: Place,
) -> (bool, usize) {
    let Some(model) = prompt_model(world, item) else {
        doc.wrap(
            &text::inline(
                "Answer it in the seat's terminal (Ctrl+]). This clears when the prompt is gone, however it was answered.",
                theme::dim(),
            ),
            width,
        );
        return (false, 0);
    };
    let id = &item.id;
    let harness = model.harness.name();
    let note = |doc: &mut Doc, words: &str| doc.wrap(&[text::run(words.to_owned(), theme::dim())], width);
    // What it is about.
    let mut full = true;
    let mut call_shown = 0;
    match model.calls.as_slice() {
        [_, _, ..] => {
            note(
                doc,
                &format!(
                    "{} calls are unanswered and st cannot tell which one {harness} asks about, so it offers no allow here:",
                    model.calls.len()
                ),
            );
            for call in model.calls.iter().take(CALL_LINES) {
                let line = prompts::call_lines(&call.input).into_iter().next().unwrap_or_default();
                doc.line(Line::from(vec![
                    Span::styled(" · ", theme::dim()),
                    Span::styled(
                        text::truncate(&format!("{} {line}", call.tool), width.saturating_sub(4)),
                        theme::text(),
                    ),
                ]));
            }
            if model.calls.len() > CALL_LINES {
                note(doc, &format!(" … and {} more", model.calls.len() - CALL_LINES));
            }
            doc.blank();
        }
        [only] if model.questions.is_none() => {
            let limit = limit.map(|limit| {
                limit.min(if model.form.kind == Kind::Plan { PLAN_LINES } else { CALL_LINES })
            });
            // What would run sits behind a bar, apart from the options: a plan as markdown, as the
            // harness shows it; any other call field by field.
            let lines = match only.input.get("plan").and_then(serde_json::Value::as_str) {
                Some(plan) if model.form.kind == Kind::Plan => text::markdown(plan, width.saturating_sub(2), theme::text())
                    .into_iter()
                    .map(|mut line| {
                        line.spans.insert(0, Span::styled("▏ ", theme::fg(theme::SURFACE2)));
                        line
                    })
                    .collect(),
                _ => text::wrap(
                    &[text::run(prompts::call_lines(&only.input).join("\n"), theme::text())],
                    width.saturating_sub(2),
                    &[text::run("▏ ", theme::fg(theme::SURFACE2))],
                    &[text::run("▏ ", theme::fg(theme::SURFACE2))],
                    None,
                ),
            };
            doc.line(Line::from(Span::styled(only.tool.clone(), theme::bold())));
            let shown = limit.map_or(lines.len(), |limit| limit.min(lines.len()));
            full = shown == lines.len();
            call_shown = shown;
            for (index, mut line) in lines.into_iter().take(shown).enumerate() {
                if !full && index + 1 == shown {
                    line.spans.push(Span::styled(" …", theme::dim()));
                }
                doc.line(line);
            }
            doc.blank();
        }
        [_] => {}
        [] if matches!(model.form.kind, Kind::Permission | Kind::Plan | Kind::Question) => {
            note(
                doc,
                "What it asks about is not in the conversation yet, so st offers only what needs no reading. Or answer it in the terminal (Ctrl+]).",
            );
            doc.blank();
        }
        [] => {}
    }
    let mark = state.mark.or(model.default_mark());
    let mut number = 0;
    // A question's options are chosen here, any number of them for a multi-select.
    for (index, question) in model.questions.iter().flatten().enumerate() {
        let header = if question.header.is_empty() {
            question.text.clone()
        } else {
            format!("{} · {}", question.header, question.text)
        };
        doc.lines(text::wrap(
            &[
                text::run(header, theme::bold()),
                text::run(if question.multi { "  (choose any)" } else { "" }, theme::dim()),
            ],
            width,
            &[],
            &[],
            None,
        ));
        let picked = state.chosen.and_then(|chosen| chosen.get(&index));
        for (option, choice) in question.options.iter().enumerate() {
            let on = picked.is_some_and(|picked| picked.contains(&option));
            let box_ = match (question.multi, on) {
                (true, true) => "[✓] ",
                (true, false) => "[ ] ",
                (false, true) => "✓ ",
                (false, false) => "",
            };
            let mut label = format!("{box_}{}", choice.label);
            if !choice.description.is_empty() {
                label.push_str(&format!(" · {}", choice.description));
            }
            let line = doc.lines.len();
            let style = if on { theme::fg(theme::GREEN) } else { theme::text() };
            doc.lines(option_lines(mark == Some(number), state.keys, number + 1, &label, style, width));
            doc.targets.push(Target {
                line,
                column: 0,
                width: width as u16,
                hit: Hit::Alert(id.clone(), AlertTap::Choose(index, option)),
            });
            number += 1;
        }
        doc.blank();
    }
    // The harness's question line, then its answers.
    doc.lines(text::wrap(&[text::run(model.form.question.clone(), theme::bold())], width, &[], &[], None));
    let complete = model.complete(state.chosen);
    for option in &model.form.options {
        let marked = mark == Some(number);
        // An answer is a row that answers only when it can: a call cut short is not let through,
        // and a question's answers go once every question has a choice.
        let withheld = match option.answer.as_str() {
            "allow" | "approve" if !full => Some("too long to show here in full"),
            "answer" if !complete => Some("choose an answer to each question first"),
            _ => None,
        };
        let line = doc.lines.len();
        match withheld {
            Some(why) => doc.lines(option_lines(
                marked,
                state.keys,
                number + 1,
                &format!("{} · {why}", option.label),
                theme::dim(),
                width,
            )),
            None => {
                doc.lines(option_lines(marked, state.keys, number + 1, &option.label, theme::text(), width));
                doc.targets.push(Target {
                    line,
                    column: 0,
                    width: width as u16,
                    hit: Hit::Alert(id.clone(), AlertTap::Answer(option.answer.clone())),
                });
            }
        }
        number += 1;
    }
    if !full {
        note(
            doc,
            match place {
                Place::Band => "The call is too long to show here in full: read it in Now (Ctrl+H), or answer it in the terminal (Ctrl+]).",
                Place::Now => "The call is too long to show here in full: answer it in the terminal (Ctrl+]).",
            },
        );
    }
    if let Some(elsewhere) = &model.form.elsewhere {
        note(doc, elsewhere);
    }
    doc.blank();
    // The keys, as the harness's dialog lists its own.
    let marked = mark.and_then(|mark| model.rows.get(mark));
    let enter = match marked {
        Some(FormRow::Choose(question, _)) => Some(
            if model.questions.as_ref().is_some_and(|asked| asked[*question].multi) {
                "Enter to toggle"
            } else {
                "Enter to choose"
            },
        ),
        Some(FormRow::Answer(answer))
            if prompts::enter_answers(answer)
                && !(matches!(answer.as_str(), "allow" | "approve") && !full)
                && !(answer == "answer" && !complete) =>
        {
            Some("Enter to select")
        }
        _ => None,
    };
    let count = model.rows.len();
    let numbers = if count > 1 {
        format!("1–{} to choose", count.min(9))
    } else {
        "1 to choose".into()
    };
    let hints = if state.keys {
        let mut hints: Vec<String> = enter.map(str::to_owned).into_iter().collect();
        hints.push("↑/↓ to move".into());
        hints.push(numbers);
        hints.push(match place {
            Place::Band => "Esc to fold".into(),
            Place::Now => "Esc to stop".into(),
        });
        hints.join(" · ")
    } else {
        match place {
            Place::Band => "Ctrl+N to answer with the keys · or click an option".into(),
            Place::Now => "a to answer with the keys · or click an option".into(),
        }
    };
    doc.wrap(&[text::run(hints, theme::dim())], width);
    (full, call_shown)
}

/// A band card kept between frames: a hash of what it was drawn from, the card, and how many
/// lines of its call it shows.
pub(super) type KeptCard = (u64, Doc, usize);

/// One row of the band: its line, and the buttons on it by column.
pub(super) struct BandRow {
    pub(super) line: Line<'static>,
    pub(super) hits: Vec<(u16, u16, Hit)>,
    /// A card's right edge, drawn at its column: the blank between is not written cell by cell.
    pub(super) edge: Option<(u16, Span<'static>)>,
}

impl BandRow {
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
            edge: None,
        }
    }

    /// A document's lines as rows, each with the targets on it.
    fn from_doc(doc: Doc) -> Vec<Self> {
        let mut rows: Vec<Self> = doc
            .lines
            .into_iter()
            .map(|mut line| {
                // A card's inner line ends in plain padding and its border: keep the border as
                // an edge at its column and leave the padding unwritten, since the frame starts
                // blank.
                let mut edge = None;
                let count = line.spans.len();
                if count >= 3
                    && line.spans[count - 2].style == Style::default()
                    && line.spans[count - 2].content.chars().all(|c| c == ' ')
                {
                    let column = line.width() - line.spans[count - 1].width();
                    let border = line.spans.pop().unwrap_or_default();
                    line.spans.pop();
                    edge = Some((column as u16, border));
                }
                Self {
                    line,
                    hits: Vec::new(),
                    edge,
                }
            })
            .collect();
        for target in doc.targets {
            if let Some(row) = rows.get_mut(target.line) {
                row.hits.push((target.column, target.width, target.hit));
            }
        }
        rows
    }
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

    /// What the person has done on alert `id`'s form.
    pub(super) fn form_state(&self, id: &str) -> FormState<'_> {
        FormState {
            mark: self.prompt_marks.get(id).copied(),
            keys: self.prompt_keys.as_deref() == Some(id),
            chosen: self
                .question_call_key(id)
                .and_then(|key| self.question_choices.get(&key)),
        }
    }

    /// Where alert `id`'s picks are kept: by the alert and its one question call.
    fn question_call_key(&self, id: &str) -> Option<String> {
        let item = self.world.attention.items().iter().find(|item| item.id == id)?;
        match prompt_model(&self.world, item)?.calls.as_slice() {
            [only] => Some(question_key(id, &only.id)),
            _ => None,
        }
    }

    /// Choose an option of a harness question alert's question, or for a multi-select toggle
    /// it. Nothing is sent until the person sends every question's choice.
    pub(super) fn choose_answer(&mut self, id: &str, question: usize, option: usize) {
        let Some(item) = self.world.attention.items().iter().find(|item| item.id == id) else {
            self.flash(GONE);
            return;
        };
        let Some((asked, key)) = prompt_model(&self.world, item).and_then(|model| {
            let key = match model.calls.as_slice() {
                [only] => question_key(id, &only.id),
                _ => return None,
            };
            Some((model.questions?, key))
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
    fn question_answer(&self, id: &str) -> Option<String> {
        let item = self.world.attention.items().iter().find(|item| item.id == id)?;
        let asked = prompt_model(&self.world, item)?.questions?;
        let chosen = self.question_choices.get(&self.question_call_key(id)?)?;
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

    /// The band for `agent` at `width` columns and at most `room` rows: empty when nothing waits
    /// on the person there, one line that says what waits until the person expands it, then the
    /// cards in place. The line is a click or Ctrl+N away from them; no other key reaches it, so
    /// typing is never taken by an alert.
    pub(super) fn alert_band(&self, agent: &str, width: usize, room: usize) -> Vec<BandRow> {
        let alerts = self.alerts_in(agent);
        let Some(first) = alerts.first() else {
            return Vec::new();
        };
        if self.alerts_open.contains(agent) {
            return self.alert_card(agent, width, room);
        }
        let count = alerts.len();
        let (glyph, color) = screens::attention_style(&first.kind);
        let lead = format!(" ◆ {count} alert{} · ", if count == 1 { "" } else { "s" });
        let tail = " ▸ show · Ctrl+N ";
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
            edge: None,
        }]
    }

    /// Show or hide the cards of `agent`'s alerts. Folding them takes the keys back from them.
    pub(super) fn toggle_alerts(&mut self, agent: &str) {
        if !self.alerts_open.remove(agent) {
            self.alerts_open.insert(agent.to_owned());
        } else if self
            .prompt_keys
            .as_ref()
            .is_some_and(|id| self.alerts_in(agent).iter().any(|item| &item.id == id))
        {
            self.prompt_keys = None;
        }
    }

    /// One alert's card for the band, kept from the last frame while nothing it reads changed:
    /// the alert, its seat's pending calls and harness, the form's state, the width and the
    /// limit. Typing redraws every key, and the card should cost it nothing.
    fn band_card(&self, item: &Attention, width: usize, limit: usize) -> (Doc, usize) {
        use std::fmt::Write as _;
        use std::hash::{Hash, Hasher};
        struct Feed<'a>(&'a mut std::collections::hash_map::DefaultHasher);
        impl std::fmt::Write for Feed<'_> {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0.write(text.as_bytes());
                Ok(())
            }
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (width, limit).hash(&mut hasher);
        let state = self.form_state(&item.id);
        let seat = match &item.kind {
            AttentionKind::Prompt { seat_id, .. } => Some(seat_id.as_str()),
            _ => None,
        };
        let calls = seat.and_then(|seat| self.world.pending_calls.get(seat));
        let harness = seat.and_then(|seat| {
            self.world
                .agents
                .items()
                .iter()
                .find(|agent| agent.id == seat)
                .map(|agent| agent.harness)
        });
        let _ = write!(
            Feed(&mut hasher),
            "{item:?}{calls:?}{harness:?}{:?}{}{:?}",
            state.mark,
            state.keys,
            state.chosen
        );
        let signature = hasher.finish();
        let key = (item.id.clone(), limit);
        if let Some((kept, doc, shown)) = self.band_cards.borrow().get(&key)
            && *kept == signature
        {
            return (doc.clone(), *shown);
        }
        let (doc, shown) = self.draw_band_card(item, width, limit);
        let mut cards = self.band_cards.borrow_mut();
        if cards.len() > 32 {
            cards.clear();
        }
        cards.insert(key, (signature, doc.clone(), shown));
        (doc, shown)
    }

    /// One alert's card for the band, as its card in Now draws it: its kind and age in the frame,
    /// its title, and for a prompt the same form, the call cut to `limit` lines. Also returns how
    /// many lines of the call it shows.
    fn draw_band_card(&self, item: &Attention, width: usize, limit: usize) -> (Doc, usize) {
        let (_, color) = screens::attention_style(&item.kind);
        let heavy = color == theme::PERSON;
        let inner = width.saturating_sub(4).max(10);
        let mut card = Doc::new();
        card.lines(text::wrap(
            &[text::run(item.title.clone(), theme::bold())],
            inner,
            &[],
            &[],
            None,
        ));
        let mut buttons: Vec<(String, Color, Hit)> = Vec::new();
        let mut call_shown = 0;
        match &item.kind {
            AttentionKind::Prompt { .. } => {
                card.blank();
                (_, call_shown) = prompt_form(
                    &mut card,
                    &self.world,
                    item,
                    self.form_state(&item.id),
                    inner,
                    Some(limit),
                    Place::Band,
                );
            }
            AttentionKind::Request {
                structured: Some(request),
                ..
            } => {
                // A named answer is one tap, unless it needs the person's words: that one is
                // written in Now.
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
                card.line(Line::from(Span::styled(
                    text::truncate(first_line(question), inner),
                    theme::soft(),
                )));
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
                card.wrap(
                    &[text::run(
                        format!(
                            "{} · {} seat{} share this sign-in. Sign in in the seat's terminal (Ctrl+]).",
                            first_line(login),
                            seats.len().max(1),
                            if seats.len() > 1 { "s" } else { "" }
                        ),
                        theme::dim(),
                    )],
                    inner,
                );
            }
            _ => {}
        }
        for row in BandRow::button_rows(buttons, inner) {
            let line = card.lines.len();
            for (column, width, hit) in row.hits {
                card.targets.push(Target {
                    line,
                    column: column.saturating_sub(1),
                    width,
                    hit,
                });
            }
            let mut row_line = row.line;
            row_line.spans.remove(0);
            card.line(row_line);
        }
        let mut doc = Doc::new();
        doc.card(
            &format!("{} · waiting {}", item.kind.word(), item.age),
            if heavy { theme::PERSON } else { color },
            heavy,
            card,
            width,
        );
        (doc, call_shown)
    }

    /// The cards of `agent`'s alerts at `width` columns in at most `room` rows, under a line that
    /// folds them again. A prompt's call is cut to what fits, never its answers.
    fn alert_card(&self, agent: &str, width: usize, room: usize) -> Vec<BandRow> {
        let alerts = self.alerts_in(agent);
        if alerts.is_empty() {
            return Vec::new();
        }
        let count = alerts.len();
        // The pane's title bar, filled across the width so the cards below read as one block
        // that belongs to the box under it; the whole bar folds it again.
        let left = format!(" ◆ {count} alert{} ", if count == 1 { "" } else { "s" });
        // Esc folds only while a form here has the keys; a click on the bar always does.
        let keys_here = self
            .prompt_keys
            .as_ref()
            .is_some_and(|keys| alerts.iter().any(|item| &item.id == keys));
        let right = if keys_here { " ▾ fold · Esc " } else { " ▾ fold " };
        let gap = width.saturating_sub(text::width(&left) + text::width(right));
        let bar = |style: Style| style.bg(theme::SURFACE0);
        let mut rows = vec![BandRow {
            line: Line::from(vec![
                Span::styled(left, bar(theme::strong(theme::PERSON))),
                Span::styled(" ".repeat(gap), bar(Style::default())),
                Span::styled(right, bar(theme::dim())),
            ]),
            hits: vec![(
                0,
                width as u16,
                Hit::Alert(agent.to_owned(), AlertTap::Toggle),
            )],
            edge: None,
        }];
        // The alert with the keys comes first, so its form is the one in view.
        let mut ordered = alerts.clone();
        if let Some(keys) = &self.prompt_keys
            && let Some(at) = ordered.iter().position(|item| &item.id == keys)
        {
            let item = ordered.remove(at);
            ordered.insert(0, item);
        }
        let mut shown = 0;
        for item in ordered.iter().take(SHOWN) {
            let left = room.saturating_sub(rows.len() + 1);
            let (mut card, call_shown) = self.band_card(item, width, PLAN_LINES);
            if card.lines.len() > left && call_shown > 0 {
                // Cut the call to what fits, keeping the form's answers and keys in view.
                let over = card.lines.len() - left;
                card = self.band_card(item, width, call_shown.saturating_sub(over)).0;
            }
            if shown > 0 && card.lines.len() > left {
                break;
            }
            rows.extend(BandRow::from_doc(card));
            shown += 1;
        }
        if count > shown {
            rows.push(BandRow {
                line: Line::from(Span::styled(
                    format!("   +{} more in Now (Ctrl+H)", count - shown),
                    theme::dim(),
                )),
                hits: Vec::new(),
                edge: None,
            });
        }
        rows
    }

    /// Now is what the person sees: its tab, or Home over the glass.
    pub(super) fn home_in_view(&self) -> bool {
        self.home_open() || (self.glasses.is_none() && self.tab == 0)
    }

    /// The agent whose conversation the person is in, with its band above the message box.
    pub(super) fn conversation_here(&self) -> Option<String> {
        if self.home_in_view() {
            return None;
        }
        if self.glasses.is_some() {
            self.composing_agent()
        } else if self.tab == 1 {
            self.selected_id()
        } else {
            None
        }
    }

    /// The prompt alert whose form takes the keys from here: in a conversation, the first of its
    /// prompts with answers; in Now, the one selected.
    fn form_here(&self) -> Option<String> {
        let answerable = |item: &&Attention| prompt_model(&self.world, item).is_some();
        if self.home_in_view() {
            let id = self.selected_id()?;
            self.world
                .attention
                .items()
                .iter()
                .filter(answerable)
                .find(|item| item.id == id)
                .map(|item| item.id.clone())
        } else {
            let agent = self.conversation_here()?;
            self.alerts_in(&agent)
                .into_iter()
                .find(answerable)
                .map(|item| item.id.clone())
        }
    }

    /// Ctrl+N, or `a` on a prompt in Now: give its form the keys (expanding the band in place
    /// in a conversation), or take them back and fold it.
    pub(super) fn toggle_form_keys(&mut self) {
        if self.prompt_keys.take().is_some() {
            if !self.home_in_view()
                && let Some(agent) = self.conversation_here()
            {
                self.alerts_open.remove(&agent);
            }
            return;
        }
        if !self.home_in_view()
            && let Some(agent) = self.conversation_here()
        {
            if self.alerts_in(&agent).is_empty() {
                self.flash("Nothing here waits on you");
                return;
            }
            self.alerts_open.insert(agent);
        }
        match self.form_here() {
            Some(id) => {
                self.editing = false;
                self.prompt_keys = Some(id);
            }
            None if self.home_in_view() => self.flash("This alert has no answers to choose from here"),
            None => {}
        }
    }

    /// A key while a prompt's form has the keys. Arrows and Tab move the mark; a number answers
    /// (or chooses) its row; Enter takes the marked row only when Enter may; Esc folds. Any other
    /// printable key goes back to typing, the key included. Returns whether the form took it.
    pub(super) fn form_key(&mut self, key: KeyEvent) -> bool {
        let Some(id) = self.prompt_keys.clone() else {
            return false;
        };
        let Some(item) = self.world.attention.items().iter().find(|item| item.id == id) else {
            self.prompt_keys = None;
            return false;
        };
        let Some(model) = prompt_model(&self.world, item) else {
            // Its answers went while it had the keys: Esc still folds it, and nothing else is taken.
            if key.code == KeyCode::Esc {
                self.toggle_form_keys();
                return true;
            }
            self.prompt_keys = None;
            return false;
        };
        let rows = model.rows.clone();
        let mark = self
            .prompt_marks
            .get(&id)
            .copied()
            .or(model.default_mark())
            .filter(|mark| *mark < rows.len());
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let count = rows.len().max(1);
        match key.code {
            KeyCode::Down | KeyCode::Tab | KeyCode::Right if plain => {
                self.prompt_marks
                    .insert(id, mark.map_or(0, |mark| (mark + 1) % count));
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Left if plain => {
                self.prompt_marks
                    .insert(id, mark.map_or(0, |mark| (mark + count - 1) % count));
            }
            KeyCode::Esc => self.toggle_form_keys(),
            KeyCode::Enter if plain => match mark.and_then(|mark| rows.get(mark)) {
                Some(FormRow::Choose(question, option)) => self.choose_answer(&id, *question, *option),
                Some(FormRow::Answer(answer)) if prompts::enter_answers(answer) => {
                    self.answer_shown(&id, answer)
                }
                Some(FormRow::Answer(_)) | None => self.flash(format!(
                    "Enter takes only an answer about what is shown in full: press its number{}",
                    if rows.len() > 1 { format!(" (1–{})", rows.len().min(9)) } else { String::new() }
                )),
            },
            KeyCode::Char(' ') if plain => {
                if let Some(FormRow::Choose(question, option)) = mark.and_then(|mark| rows.get(mark)) {
                    self.choose_answer(&id, *question, *option);
                }
            }
            KeyCode::Char(digit @ '1'..='9') if plain => {
                let index = digit as usize - '1' as usize;
                match rows.get(index) {
                    Some(FormRow::Choose(question, option)) => {
                        self.prompt_marks.insert(id.clone(), index);
                        self.choose_answer(&id, *question, *option);
                    }
                    Some(FormRow::Answer(answer)) => {
                        self.prompt_marks.insert(id.clone(), index);
                        self.answer_shown(&id, answer);
                    }
                    None => self.flash(format!("There is no {digit} here")),
                }
            }
            // Typing is never blocked: the key goes to the message box (or Now's own keys).
            KeyCode::Char(_) if plain => {
                self.prompt_keys = None;
                if !self.home_in_view() && self.conversation_here().is_some() {
                    self.editing = true;
                }
                return false;
            }
            _ if key.code == KeyCode::Char('n') && key.modifiers == KeyModifiers::CONTROL => {
                self.toggle_form_keys()
            }
            _ => {
                self.prompt_keys = None;
                return false;
            }
        }
        true
    }

    /// Answer `answer` on alert `id` from the keys, only when its row is on screen and answers:
    /// the same row a click would take. A call cut short or a question not yet chosen has none.
    fn answer_shown(&mut self, id: &str, answer: &str) {
        let wanted = Hit::Alert(id.to_owned(), AlertTap::Answer(answer.to_owned()));
        let shown = self.frame.borrow().hits.iter().any(|(_, hit)| *hit == wanted);
        if shown {
            self.answer_alert(id, answer);
        } else if answer == "answer" {
            self.flash("Choose an answer to each question first");
        } else {
            self.flash("That answer is not in view in full here: scroll to it, or answer it in the terminal (Ctrl+])");
        }
    }

    /// Send a typed answer to an alert: a prompt's answer, a structured request's named answer,
    /// or the yes or no of a question.
    pub(super) fn answer_alert(&mut self, id: &str, answer: &str) {
        let Some(item) = self
            .world
            .attention
            .items()
            .iter()
            .find(|item| item.id == id)
        else {
            self.flash(GONE);
            return;
        };
        let effect = match &item.kind {
            AttentionKind::Prompt { answers, .. }
                if item.actions.iter().any(|a| a == "prompt.respond")
                    && answers.iter().any(|offered| offered == answer) =>
            {
                // A question's answer is the choices made here.
                let sent = if answer == "answer" {
                    self.question_answer(id)
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
                        format!("Answering “{}”…", self.answer_words(item, answer)),
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
                self.prompt_marks.remove(id);
                if self.prompt_keys.as_deref() == Some(id) {
                    self.prompt_keys = None;
                }
                self.flash(notice);
            }
            Some((_, notice)) => self.flash(format!("{notice} demo: nothing was sent")),
            None => self.flash("st does not offer that answer here: answer it in the terminal (Ctrl+])"),
        }
    }

    /// The words a prompt's answer reads as on its form.
    fn answer_words(&self, item: &Attention, answer: &str) -> String {
        prompt_model(&self.world, item)
            .and_then(|model| {
                model
                    .form
                    .options
                    .into_iter()
                    .find(|option| option.answer == answer)
                    .map(|option| option.label)
            })
            .unwrap_or_else(|| answer.to_owned())
    }
}

/// What a client says when the alert it answers is no longer listed, in st's own words for a
/// prompt that is gone.
const GONE: &str = "That prompt is no longer waiting: it was answered or replaced";

/// The first non-empty line, without markdown's heading marks.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("")
}


/// Where the picks for a question alert's question call are kept: by the alert and the exact call,
/// so picks made for one question never answer another that replaced it.
fn question_key(alert: &str, call: &str) -> String {
    format!("{alert}\u{0}{call}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::glass::{Glasses, Open};
    use ratatui::{Terminal, backend::TestBackend};

    const SEAT: &str = "agent/example/atlas/builder";

    fn prompt(id: &str, answers: &[&str], text: &str) -> Attention {
        Attention {
            id: id.into(),
            tier: Tier::Stopped,
            title: "builder is waiting for permission".into(),
            waiting: None,
            age: "1m".into(),
            mission: None,
            agent: Some(SEAT.into()),
            kind: AttentionKind::Prompt {
                seat: "builder".into(),
                seat_id: SEAT.into(),
                text: text.into(),
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
            conversations: vec![SEAT.into()],
        }
    }

    fn call(tool: &str, input: serde_json::Value) -> PendingCall {
        PendingCall {
            id: format!("call-{tool}"),
            tool: tool.into(),
            input,
        }
    }

    /// One prompt kind with invented content: the answers st lists, the call it is about, and
    /// what its form must show.
    struct Case {
        name: &'static str,
        harness: Harness,
        answers: &'static [&'static str],
        text: &'static str,
        calls: Vec<PendingCall>,
        /// The options in order, as numbered rows.
        options: &'static [&'static str],
        /// The option marked at first.
        marked: Option<&'static str>,
        question: &'static str,
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "claude read outside the working directories",
                harness: Harness::Claude,
                answers: &["allow", "deny"],
                text: "Claude asks to use Read",
                calls: vec![call("Read", serde_json::json!({"file_path": "/srv/example/shared/notes.txt"}))],
                options: &["Yes, but ask again next time", "No, and ask again next time"],
                marked: Some("Yes, but ask again next time"),
                question: "Allow this read outside the working directories?",
            },
            Case {
                name: "claude command of several lines",
                harness: Harness::Claude,
                answers: &["allow", "deny"],
                text: "Claude asks to use Bash",
                calls: vec![call(
                    "Bash",
                    serde_json::json!({"command": "cd garden\nmake clean\nmake all", "description": "Rebuild"}),
                )],
                options: &["Yes", "No"],
                marked: Some("Yes"),
                question: "Do you want to proceed?",
            },
            Case {
                name: "claude plan",
                harness: Harness::Claude,
                answers: &["approve", "deny"],
                text: "Claude asks you to approve its plan",
                calls: vec![call(
                    "ExitPlanMode",
                    serde_json::json!({"plan": "1. Add a greeting to the garden page\n2. Test it"}),
                )],
                options: &["Yes", "No, keep planning"],
                marked: Some("Yes"),
                question: "Would you like to proceed?",
            },
            Case {
                name: "claude question with a multi-select",
                harness: Harness::Claude,
                answers: &["answer", "deny"],
                text: "Claude asks a question",
                calls: vec![call(
                    "AskUserQuestion",
                    serde_json::json!({"questions": [
                        {"question": "Which flowers?", "header": "Flowers", "multiSelect": true,
                         "options": [{"label": "Tulips", "description": "spring"},
                                     {"label": "Asters", "description": "autumn"}]}
                    ]}),
                )],
                options: &[
                    "[ ] Tulips · spring",
                    "[ ] Asters · autumn",
                    "Submit answers · choose an answer to each question first",
                    "No, decline to answer",
                ],
                marked: Some("[ ] Tulips · spring"),
                question: "Ready to submit your answers?",
            },
            Case {
                name: "claude new MCP server",
                harness: Harness::Claude,
                answers: &["use", "use-all", "skip"],
                text: "New MCP server found in this project: example-echo.",
                calls: vec![],
                options: &[
                    "Use this MCP server",
                    "Use this and all future MCP servers in this project",
                    "Continue without using this MCP server",
                ],
                marked: Some("Continue without using this MCP server"),
                question: "New MCP server found in this project: example-echo.",
            },
            Case {
                name: "codex approval",
                harness: Harness::Codex,
                answers: &["allow", "deny"],
                text: "Codex asks for approval",
                calls: vec![call("shell", serde_json::json!({"command": "npm test -- --watch=false"}))],
                options: &["Yes, proceed", "No"],
                marked: Some("Yes, proceed"),
                question: "Would you like to run the following command?",
            },
        ]
    }

    fn world_with(case: &Case) -> World {
        let mut world = demo::world();
        if let Load::Ready(agents) = &mut world.agents
            && let Some(agent) = agents.iter_mut().find(|agent| agent.id == SEAT)
        {
            agent.harness = case.harness;
        }
        world.attention = Load::Ready(vec![prompt("attention/one", case.answers, case.text)]);
        world.pending_calls.insert(SEAT.into(), case.calls.clone());
        world
    }

    /// The conversation, as a person in a glass sees it.
    fn in_conversation(world: World) -> Ui {
        let mut ui = Ui::new(world);
        ui.glasses = Some(Glasses::open(None, None));
        ui.glasses.as_mut().unwrap().sidebar.shown = false;
        ui.open_in_glass(Pane::Agent(Some(SEAT.into())), Open::Tab);
        ui
    }

    /// Now, with the alert's card selected.
    fn in_now(world: World) -> Ui {
        let mut ui = Ui::new(world);
        ui.switch_tab(0);
        let index = ui.ids().iter().position(|id| id == "attention/one").unwrap();
        ui.select(index);
        ui
    }

    fn render(ui: &Ui, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
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

    fn press(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers) {
        ui.key(KeyEvent::new(code, modifiers));
    }

    /// The numbered option rows on screen, in order, with whether each is marked.
    fn options(shown: &str) -> Vec<(bool, usize, String)> {
        shown
            .lines()
            .filter_map(|line| {
                // Inside a card's frame, an option row starts with its mark or two spaces.
                let line = match line.split_once('┃') {
                    Some((_, inside)) => inside,
                    None => line.split_once('│').map_or(line, |(_, inside)| inside),
                };
                let line = line.strip_prefix(' ').unwrap_or(line);
                let marked = line.starts_with("❯ ");
                let rest = line.strip_prefix("❯ ").or_else(|| line.strip_prefix("  "))?;
                if !rest.starts_with(|c: char| c.is_ascii_digit()) {
                    return None;
                }
                let (number, label) = rest.split_once(". ")?;
                let number = number.parse::<usize>().ok()?;
                let label = label.trim_end_matches(['│', '┃', ' ']).trim_end();
                Some((marked, number, label.to_owned()))
            })
            .collect()
    }

    /// The screen's words in one line, without card frames, so wrapped words read on.
    fn words_of(shown: &str) -> String {
        shown
            .replace(['┃', '│'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn check_form(case: &Case, shown: &str, width: u16, place: &str) {
        let rows = options(shown);
        let labels: Vec<&str> = rows.iter().map(|(_, _, label)| label.as_str()).collect();
        // A label too long for a narrow card wraps: its first line starts the same.
        assert_eq!(rows.len(), case.options.len(), "{} {place} at {width}: {labels:?}\n{shown}", case.name);
        for ((_, number, label), wanted) in rows.iter().zip(case.options) {
            assert!(wanted.starts_with(label.as_str()), "{} {place} at {width}: {label} for {wanted}\n{shown}", case.name);
            assert!(*number >= 1);
        }
        let numbers: Vec<usize> = rows.iter().map(|(_, number, _)| *number).collect();
        assert_eq!(numbers, (1..=rows.len()).collect::<Vec<_>>(), "{}", case.name);
        let marked: Vec<&str> = rows.iter().filter(|(marked, _, _)| *marked).map(|(_, _, label)| label.as_str()).collect();
        match case.marked {
            Some(wanted) => assert!(marked.len() == 1 && wanted.starts_with(marked[0]), "{} {place} at {width}: {marked:?}\n{shown}", case.name),
            None => assert!(marked.is_empty(), "{}", case.name),
        }
        let flat = words_of(shown);
        let question = case.question.split_whitespace().collect::<Vec<_>>().join(" ");
        let squeezed: String = flat.chars().filter(|c| c.is_alphanumeric()).collect();
        let wanted: String = question.chars().filter(|c| c.is_alphanumeric()).collect();
        assert!(squeezed.contains(&wanted), "{} {place} at {width}: question\n{shown}", case.name);
        assert!(!shown.contains("open in Now"), "{}\n{shown}", case.name);
    }

    /// Every prompt kind in Now and in its conversation, folded and expanded, at three widths:
    /// the same form, worded and numbered like the harness's, with its marked option and keys.
    #[test]
    fn every_prompt_kind_reads_like_its_harness_in_now_and_in_place() {
        for case in cases() {
            for width in [60u16, 80, 120] {
                // Now: the card carries the prompt's answers.
                let ui = in_now(world_with(&case));
                let shown = render(&ui, width, 60);
                check_form(&case, &shown, width, "Now");
                let flat = words_of(&shown);
                assert!(flat.contains("a to answer with the keys"), "{}\n{shown}", case.name);

                // The conversation, folded: one line, nothing to answer, a way in.
                let mut ui = in_conversation(world_with(&case));
                let shown = render(&ui, width, 60);
                assert!(shown.contains("◆ 1 alert") && shown.contains("Ctrl+N"), "{}\n{shown}", case.name);
                assert!(options(&shown).is_empty(), "{} folded\n{shown}", case.name);
                assert!(!shown.contains("open in Now"));

                // Expanded in place: the same card.
                ui.click(Hit::Alert(SEAT.into(), AlertTap::Toggle));
                let shown = render(&ui, width, 60);
                check_form(&case, &shown, width, "band");
                assert!(shown.contains("PROMPT · WAITING 1M"), "{}\n{shown}", case.name);
                let flat = words_of(&shown);
                assert!(flat.contains("Ctrl+N to answer with the keys"), "{}\n{shown}", case.name);

                // With the keys, the footer lists them as the harness does.
                ui.toggle_form_keys();
                let shown = render(&ui, width, 60);
                let flat = words_of(&shown);
                assert!(flat.contains("↑/↓ to move") && flat.contains("Esc to fold"), "{}\n{shown}", case.name);
                assert_eq!(
                    flat.contains("Enter to select") || flat.contains("Enter to toggle") || flat.contains("Enter to choose"),
                    case.marked.is_some(),
                    "{}\n{shown}",
                    case.name
                );
                // Esc folds it back to its line.
                press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
                let shown = render(&ui, width, 60);
                assert!(options(&shown).is_empty(), "{} folded again\n{shown}", case.name);
            }
        }
    }

    fn bash_world() -> World {
        let case = &cases()[1];
        world_with(case)
    }

    /// Focus only focuses: arrows and Tab move the mark and send nothing. Enter takes the marked
    /// allow, shown in full; a refusal takes its own number, never Enter.
    #[test]
    fn the_mark_moves_without_answering_and_enter_never_refuses() {
        let mut ui = in_conversation(bash_world());
        ui.live = true;
        render(&ui, 80, 40);
        press(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        render(&ui, 80, 40);
        for code in [KeyCode::Down, KeyCode::Tab, KeyCode::Up, KeyCode::BackTab, KeyCode::Down] {
            press(&mut ui, code, KeyModifiers::NONE);
        }
        assert!(ui.effects.is_empty(), "moving the mark answers nothing");
        let shown = render(&ui, 80, 40);
        assert!(options(&shown).iter().any(|(marked, _, label)| *marked && label == "No"), "{shown}");
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        assert!(ui.effects.is_empty(), "Enter never takes a refusal");
        press(&mut ui, KeyCode::Up, KeyModifiers::NONE);
        render(&ui, 80, 40);
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        let [Effect::Attention { action, answer: Some(answer), .. }] = ui.effects.as_slice() else {
            panic!("{:?}", ui.effects);
        };
        assert_eq!((action.as_str(), answer.as_str()), ("prompt.respond", "allow"));

        // A refusal is its own number.
        let mut ui = in_conversation(bash_world());
        ui.live = true;
        render(&ui, 80, 40);
        press(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        render(&ui, 80, 40);
        press(&mut ui, KeyCode::Char('2'), KeyModifiers::NONE);
        assert!(matches!(ui.effects.as_slice(), [Effect::Attention { answer: Some(answer), .. }] if answer == "deny"));
    }

    /// Typing is never blocked: a letter while the form has the keys goes to the message box.
    #[test]
    fn a_letter_while_the_form_has_the_keys_types_into_the_message_box() {
        let mut ui = in_conversation(bash_world());
        ui.live = true;
        render(&ui, 80, 40);
        press(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(ui.prompt_keys.is_some());
        for letter in "hi".chars() {
            press(&mut ui, KeyCode::Char(letter), KeyModifiers::NONE);
        }
        assert!(ui.effects.is_empty());
        assert!(ui.prompt_keys.is_none());
        assert_eq!(ui.conversation_state.drafts.get(SEAT).map(String::as_str), Some("hi"));
        // And digits typed into the box stay text.
        press(&mut ui, KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(ui.effects.is_empty());
        assert_eq!(ui.conversation_state.drafts.get(SEAT).map(String::as_str), Some("hi1"));
    }

    /// A call too long for the band is cut there and its allow withheld, by click and by key;
    /// Now shows it whole and offers allow.
    #[test]
    fn a_call_too_long_for_the_band_is_allowed_only_where_it_is_shown_whole() {
        let mut world = bash_world();
        world.pending_calls.insert(
            SEAT.into(),
            vec![call("Bash", serde_json::json!({"command": (1..=12).map(|n| format!("echo step {n}")).collect::<Vec<_>>().join("\n")}))],
        );
        let allow = Hit::Alert("attention/one".into(), AlertTap::Answer("allow".into()));
        let mut ui = in_conversation(world.clone());
        ui.live = true;
        ui.click(Hit::Alert(SEAT.into(), AlertTap::Toggle));
        let shown = render(&ui, 80, 40);
        assert!(shown.contains("too long to show here in full"), "{shown}");
        assert!(!shown.contains("echo step 12"), "{shown}");
        assert!(!ui.frame.borrow().hits.iter().any(|(_, hit)| *hit == allow));
        press(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut ui, KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(ui.effects.is_empty(), "the key cannot allow what is not shown");

        let ui = in_now(world);
        let shown = render(&ui, 120, 60);
        assert!(shown.contains("echo step 12"), "{shown}");
        assert!(ui.frame.borrow().hits.iter().any(|(_, hit)| *hit == allow));
    }

    /// A question is answered with choices made here, by number or Enter, and sent together.
    #[test]
    fn a_question_is_chosen_by_number_and_sent_once_complete() {
        let case = &cases()[3];
        let mut ui = in_now(world_with(case));
        ui.live = true;
        render(&ui, 100, 60);
        press(&mut ui, KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(ui.prompt_keys.as_deref(), Some("attention/one"));
        // Enter on the marked option chooses it; nothing is sent.
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut ui, KeyCode::Char('2'), KeyModifiers::NONE);
        assert!(ui.effects.is_empty());
        let shown = render(&ui, 100, 60);
        assert!(shown.contains("[✓] Tulips") && shown.contains("[✓] Asters"), "{shown}");
        // Another question call replaces it: the picks made for the first never answer it.
        let asked = ui.world.pending_calls[SEAT][0].clone();
        ui.world.pending_calls.insert(SEAT.into(), vec![PendingCall { id: "call-replaced".into(), ..asked.clone() }]);
        let shown = render(&ui, 100, 60);
        assert!(shown.contains("[ ] Tulips") && !shown.contains("[✓]"), "{shown}");
        ui.world.pending_calls.insert(SEAT.into(), vec![asked]);
        press(&mut ui, KeyCode::Char('3'), KeyModifiers::NONE);
        let [Effect::Attention { answer: Some(answer), .. }] = ui.effects.as_slice() else {
            panic!("{:?}", ui.effects);
        };
        assert_eq!(serde_json::from_str::<serde_json::Value>(answer).unwrap(), serde_json::json!({"0": [0, 1]}));
    }

    #[test]
    fn a_click_on_an_option_answers_it_and_a_gone_prompt_says_so() {
        let mut ui = in_conversation(bash_world());
        ui.live = true;
        ui.click(Hit::Alert("attention/one".into(), AlertTap::Answer("deny".into())));
        assert!(matches!(ui.effects.as_slice(), [Effect::Attention { answer: Some(answer), .. }] if answer == "deny"));
        let mut ui = in_conversation(bash_world());
        ui.live = true;
        ui.world.attention = Load::Ready(vec![]);
        ui.click(Hit::Alert("attention/one".into(), AlertTap::Answer("allow".into())));
        assert!(ui.effects.is_empty());
        assert_eq!(ui.flash.as_ref().map(|(text, _)| text.as_str()), Some(GONE));
    }

    #[test]
    fn several_unanswered_calls_offer_no_allow_and_another_harness_answers_in_its_terminal() {
        let mut world = bash_world();
        world.pending_calls.insert(
            SEAT.into(),
            vec![call("Bash", serde_json::json!({"command": "make"})), call("Write", serde_json::json!({"file_path": "notes.md"}))],
        );
        let ui = in_now(world.clone());
        let shown = render(&ui, 120, 60);
        assert!(shown.contains("2 calls are unanswered"), "{shown}");
        let labels: Vec<String> = options(&shown).into_iter().map(|(_, _, label)| label).collect();
        assert_eq!(labels, ["No"]);
        world.attention = Load::Ready(vec![prompt("attention/one", &[], "Pi asks for approval")]);
        let ui = in_now(world);
        let shown = render(&ui, 120, 60);
        assert!(shown.contains("Answer it in the seat's terminal"), "{shown}");
        assert!(options(&shown).is_empty());
    }

    /// The band keeps a card between frames only while nothing it was drawn from changed.
    #[test]
    fn a_kept_band_card_follows_its_call_its_mark_and_its_age() {
        let mut ui = in_conversation(bash_world());
        ui.click(Hit::Alert(SEAT.into(), AlertTap::Toggle));
        assert!(render(&ui, 80, 40).contains("make clean"));
        ui.world.pending_calls.insert(SEAT.into(), vec![call("Bash", serde_json::json!({"command": "make tulips"}))]);
        let shown = render(&ui, 80, 40);
        assert!(shown.contains("make tulips") && !shown.contains("make clean"), "{shown}");
        ui.prompt_marks.insert("attention/one".into(), 1);
        assert!(options(&render(&ui, 80, 40)).iter().any(|(marked, _, label)| *marked && label == "No"));
        if let Load::Ready(items) = &mut ui.world.attention {
            items[0].age = "7m".into();
        }
        assert!(render(&ui, 80, 40).contains("WAITING 7M"));
    }

    /// Now follows the conversation of the prompt it shows, where the call it asks about is.
    #[test]
    fn now_follows_the_seat_of_the_prompt_it_shows() {
        let ui = in_now(bash_world());
        assert!(ui.live_conversations().contains(&SEAT.to_owned()));
    }

    /// A prompt st stops listing stays under Recently closed, offering no answer, in Now or in place.
    #[test]
    fn a_closed_prompt_offers_no_answer() {
        let world = bash_world();
        let mut ui = in_now(world.clone());
        ui.live = true;
        assert!(options(&render(&ui, 80, 40)).len() == 2);
        let mut next = world;
        next.attention = Load::Ready(vec![]);
        ui.set_world(next);
        let index = ui.ids().iter().position(|id| id == "attention/one").unwrap();
        ui.select(index);
        let shown = render(&ui, 80, 40);
        assert!(shown.contains("closed, x clears it"), "{shown}");
        assert!(options(&shown).is_empty(), "{shown}");
        assert!(!shown.contains("to answer with the keys"), "{shown}");
        assert!(ui.alert_band(SEAT, 80, 20).is_empty());
    }

    /// The bar names Esc only while a form here has the keys, and Esc still folds a form whose
    /// answers went while it had them (st stops offering them once the prompt waits too long).
    #[test]
    fn esc_folds_the_band_it_names_even_after_the_answers_go() {
        let world = bash_world();
        let mut ui = in_conversation(world.clone());
        ui.live = true;
        ui.toggle_alerts(SEAT);
        let bar = |ui: &Ui| ui.alert_band(SEAT, 80, 30)[0].line.to_string();
        assert!(bar(&ui).contains("▾ fold") && !bar(&ui).contains("Esc"), "{}", bar(&ui));
        ui.toggle_alerts(SEAT);
        press(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(bar(&ui).contains("▾ fold · Esc"), "{}", bar(&ui));
        let mut next = world;
        if let Load::Ready(items) = &mut next.attention {
            for item in items.iter_mut() {
                if let AttentionKind::Prompt { answers, .. } = &mut item.kind {
                    answers.clear();
                }
            }
        }
        ui.set_world(next);
        assert!(options(&render(&ui, 80, 40)).is_empty());
        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.prompt_keys.is_none());
        assert_eq!(ui.alert_band(SEAT, 80, 30).len(), 1, "folded to its line");
        assert!(ui.effects.is_empty());
    }

    #[test]
    fn updates_and_messages_are_not_alerts() {
        let mut update = prompt("attention/update", &[], "");
        update.kind = AttentionKind::Update {
            from: "atlas".into(),
            body: "done".into(),
            about: String::new(),
            subjects: vec![],
        };
        let mut world = demo::world();
        world.attention = Load::Ready(vec![update]);
        let ui = Ui::new(world);
        assert!(ui.alert_band(SEAT, 80, 20).is_empty());
    }

    #[test]
    fn the_collapsed_line_fits_a_narrow_pane() {
        let ui = in_conversation(bash_world());
        for width in [12usize, 24, 40] {
            assert_eq!(ui.alert_band(SEAT, width, 10).len(), 1);
        }
    }
}
