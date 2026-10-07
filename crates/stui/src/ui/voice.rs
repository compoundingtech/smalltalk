//! Voice mode in a message input: Ctrl+R (or a click on ◉ voice) starts the speech helper; the
//! input shows a waveform and the words as they come. Enter sends them, Tab keeps them in the
//! input to edit, Esc drops them. A message whose words came from voice is tagged `dictated`.

use super::{Ui, theme};
use crate::voice::{self, Event, Listening};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};
use ratatui::text::{Line, Span};
use st3_conversation_ui::text as wrap;

/// What becomes of the words once the helper has them all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Then {
    Send,
    Edit,
}

/// One input listening.
pub struct VoiceState {
    /// The input's draft key: an agent, or a Home item.
    pub input: String,
    listening: Option<Listening>,
    pub levels: Vec<f32>,
    pub heard: String,
    pub device: String,
    /// Said once the person chose; the words arrive after.
    pub then: Option<Then>,
    /// Something to know while listening (only silence so far).
    pub note: Option<String>,
    /// When listening started, and when the person chose what to do with the words.
    pub started: Instant,
    pub chosen_at: Option<Instant>,
}

/// How long the helper may take to say it is ready, or to hand over the words once asked, before
/// voice is given up (Nathan, 2026-10-06: keys stopped working until the terminal was closed).
const VOICE_PATIENCE: Duration = Duration::from_secs(10);

impl VoiceState {
    #[cfg(test)]
    pub fn stand_in(input: &str) -> VoiceState {
        VoiceState {
            input: input.into(),
            listening: None,
            levels: Vec::new(),
            heard: String::new(),
            device: String::new(),
            then: None,
            note: None,
            started: Instant::now(),
            chosen_at: None,
        }
    }
}

impl Ui {
    /// Start listening for the focused input, or say why voice is not here.
    pub(crate) fn start_voice(&mut self) {
        // A conversation's message box for now; Home's answer boxes draw differently.
        if self.tab != 1 {
            self.flash("Voice works in a conversation's message box for now");
            return;
        }
        let Some(input) = self.draft_key() else {
            return;
        };
        let Some(helper) = voice::helper() else {
            self.flash(voice::unavailable());
            return;
        };
        match Listening::start(&helper) {
            Ok(listening) => {
                self.voice = Some(VoiceState {
                    input,
                    listening: Some(listening),
                    levels: Vec::new(),
                    heard: String::new(),
                    device: String::new(),
                    then: None,
                    note: None,
                    started: Instant::now(),
                    chosen_at: None,
                });
            }
            Err(error) => self.flash(format!("Voice could not start: {error}")),
        }
    }

    /// Whether voice is listening for this input.
    pub(crate) fn voice_for(&self, input: &str) -> Option<&VoiceState> {
        self.voice.as_ref().filter(|state| state.input == input)
    }

    /// Keys while listening: Enter sends, Tab keeps the words to edit, Esc drops them. Every
    /// other key waits, so typing never mixes into what is being heard.
    pub(crate) fn voice_key(&mut self, key: KeyEvent) -> bool {
        let Some(state) = self.voice.as_mut() else {
            return false;
        };
        let then = match key.code {
            KeyCode::Esc => {
                self.cancel_voice();
                return true;
            }
            // A chord is never typing: Ctrl+Q quits and Ctrl+C is not swallowed while a helper
            // that never answers is waited for. Voice is dropped and the key goes on its way.
            _ if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_voice();
                return false;
            }
            KeyCode::Enter => Then::Send,
            KeyCode::Tab => Then::Edit,
            _ => return true,
        };
        if state.then.is_none() {
            state.then = Some(then);
            state.chosen_at = Some(Instant::now());
            if let Some(listening) = state.listening.as_mut() {
                listening.finish();
            }
        }
        true
    }

    pub(crate) fn cancel_voice(&mut self) {
        if let Some(listening) = self
            .voice
            .take()
            .and_then(|mut state| state.listening.take())
        {
            listening.cancel();
        }
    }

    /// Take in what the helper said since the last pass, and give up on one that says nothing.
    pub(crate) fn step_voice(&mut self) {
        if let Some(state) = &self.voice {
            let waiting_since = match (state.chosen_at, state.device.is_empty()) {
                (Some(chosen), _) => Some(chosen),
                (None, true) => Some(state.started),
                (None, false) => None,
            };
            if waiting_since.is_some_and(|since| since.elapsed() >= VOICE_PATIENCE) {
                self.cancel_voice();
                self.flash("Voice did not answer, so it was stopped; your keys work again");
                return;
            }
            // Voice belongs to the input it started in: leaving that conversation ends it, so
            // it cannot keep holding the keys from a tab where it is not shown.
            if self.draft_key().as_deref() != Some(state.input.as_str()) {
                self.cancel_voice();
                self.flash("Voice stopped: you left that conversation");
                return;
            }
        }
        let mut events = Vec::new();
        if let Some(listening) = self
            .voice
            .as_ref()
            .and_then(|state| state.listening.as_ref())
        {
            events.extend(listening.events.try_iter());
        }
        for event in events {
            self.voice_event(event);
        }
    }

    pub(crate) fn voice_event(&mut self, event: Event) {
        let Some(state) = self.voice.as_mut() else {
            return;
        };
        match event {
            Event::Ready { device } => state.device = device,
            Event::Level(level) => {
                state.levels.push(level);
                let excess = state.levels.len().saturating_sub(120);
                state.levels.drain(..excess);
                if level > 0.0 {
                    state.note = None;
                }
            }
            Event::Text { text, .. } => state.heard = text,
            Event::Silent { device } => {
                state.note = Some(format!("only silence from {device} so far"));
            }
            Event::Done { text } => self.finish_voice(text),
            Event::Error(message) => {
                self.voice = None;
                self.flash(format!("Voice stopped: {message}"));
            }
            // Gone without its words: keep what was heard when the person had chosen.
            Event::Exited if state.then.is_some() => {
                let heard = state.heard.clone();
                self.finish_voice(heard);
            }
            Event::Exited => {
                self.voice = None;
                self.flash("Voice stopped");
            }
        }
    }

    /// The words are in: into the input after its draft, then sent or left to edit.
    fn finish_voice(&mut self, words: String) {
        let Some(state) = self.voice.take() else {
            return;
        };
        let words = words.trim();
        if words.is_empty() {
            self.flash("Heard nothing");
            return;
        }
        let draft = self
            .conversation_state
            .drafts
            .entry(state.input.clone())
            .or_default();
        if draft.trim().is_empty() {
            *draft = words.to_owned();
        } else {
            let kept = draft.trim_end().len();
            draft.truncate(kept);
            draft.push(' ');
            draft.push_str(words);
        }
        self.dictated.insert(state.input.clone());
        self.editing = true;
        if state.then == Some(Then::Send) && self.draft_key().as_deref() == Some(&state.input) {
            self.submit();
        }
    }

    /// The input's lines while listening: the waveform, the words so far, and the keys.
    pub(crate) fn voice_lines(&self, state: &VoiceState, width: usize) -> Vec<Line<'static>> {
        let status = match state.then {
            Some(Then::Send) => "● sending what was heard…".to_owned(),
            Some(Then::Edit) => "● finishing…".to_owned(),
            None if state.device.is_empty() => "● starting…".to_owned(),
            None => format!("● listening · {}", state.device),
        };
        let wave_width = width.saturating_sub(wrap::width(&status) + 4).min(40);
        let mut lines = vec![Line::from(vec![
            Span::styled(status, theme::strong(theme::RED)),
            Span::raw("  "),
            Span::styled(
                voice::waveform(&state.levels, wave_width),
                theme::fg(theme::LAVENDER),
            ),
        ])];
        if let Some(note) = &state.note {
            lines.push(Line::from(Span::styled(
                format!("  {note}"),
                theme::fg(theme::WAITING),
            )));
        }
        let heard = if state.heard.is_empty() {
            vec![wrap::run("Say something…", theme::dim())]
        } else {
            vec![wrap::run(&state.heard, theme::text())]
        };
        let mut words = wrap::wrap(
            &heard,
            width.saturating_sub(1),
            &[wrap::run("› ", theme::strong(theme::ACCENT))],
            &[wrap::run("  ", theme::dim())],
            None,
        );
        let excess = words.len().saturating_sub(6);
        words.drain(..excess);
        lines.extend(words);
        lines.push(Line::from(Span::styled(
            "  enter send · tab edit first · esc drop",
            theme::dim(),
        )));
        lines
    }
}

impl Drop for VoiceState {
    fn drop(&mut self) {
        if let Some(listening) = self.listening.take() {
            listening.cancel();
        }
    }
}

/// Whether an effect is a message whose words came from voice.
#[cfg(test)]
pub fn dictated(effect: &super::Effect) -> bool {
    matches!(effect, super::Effect::Send { tags, .. } if tags.iter().any(|tag| tag == "dictated"))
}
