//! What `st demo play` drives in the terminal interface it runs: the key the presenter presses
//! for the next beat, and text typed into the message input at a person's pace.
//!
//! Everything else a demo shows (panes opening, messages, steps, alerts) reaches the interface
//! the way it does from real seats: `st ui open` and the graph. Only these two have no such path,
//! because they are the presenter's own hands.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// What the player types into the focused agent's message input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Typed {
    /// Text inserted at the cursor, as if typed; the player sends it a few characters at a time.
    Text(String),
    /// Enter: the message is sent to the agent, as the person.
    Send,
}

/// The two ends the interface holds while a demo plays.
pub struct Playback {
    advance: Advance,
    pressed: mpsc::Sender<()>,
    typed: mpsc::Receiver<Typed>,
    closed: Arc<AtomicBool>,
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("advance", &self.advance)
            .finish_non_exhaustive()
    }
}

/// The player's ends: it hears the advance key and sends what to type.
pub struct Player {
    pub pressed: mpsc::Receiver<()>,
    pub typed: mpsc::Sender<Typed>,
    closed: Arc<AtomicBool>,
}

impl Player {
    /// Whether the presenter has quit the interface.
    pub fn ended(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// Connect a player to the interface it drives.
pub fn link(advance: Advance) -> (Playback, Player) {
    let (pressed_tx, pressed_rx) = mpsc::channel();
    let (typed_tx, typed_rx) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    (
        Playback {
            advance,
            pressed: pressed_tx,
            typed: typed_rx,
            closed: closed.clone(),
        },
        Player {
            pressed: pressed_rx,
            typed: typed_tx,
            closed,
        },
    )
}

impl Playback {
    /// Whether this key is the presenter's; it is then the player's alone and does nothing else.
    pub fn take(&self, key: &KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press || !self.advance.matches(key) {
            return false;
        }
        let _ = self.pressed.send(());
        true
    }

    /// What the player has typed since the last frame.
    pub fn typed(&self) -> Vec<Typed> {
        self.typed.try_iter().collect()
    }
}

/// The key that moves a demo to its next beat: a function key, Page Down or a Ctrl+letter, so it
/// never also types into the message input or moves the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Default for Advance {
    fn default() -> Self {
        Self {
            code: KeyCode::F(8),
            modifiers: KeyModifiers::NONE,
        }
    }
}

impl Advance {
    /// `f1`…`f12`, `pagedown`, or `ctrl-` and one letter.
    pub fn parse(name: &str) -> Result<Self, String> {
        let lower = name.trim().to_ascii_lowercase();
        let plain = |code| Self {
            code,
            modifiers: KeyModifiers::NONE,
        };
        if lower == "pagedown" {
            return Ok(plain(KeyCode::PageDown));
        }
        if let Some(number) = lower.strip_prefix('f')
            && let Ok(number) = number.parse::<u8>()
            && (1..=12).contains(&number)
        {
            return Ok(plain(KeyCode::F(number)));
        }
        if let Some(letter) = lower.strip_prefix("ctrl-")
            && let [letter] = letter.chars().collect::<Vec<_>>()[..]
            && letter.is_ascii_lowercase()
            // Ctrl+Q quits; Ctrl+C, Ctrl+W and Ctrl+K are the interface's own.
            && !"qcwk".contains(letter)
        {
            return Ok(Self {
                code: KeyCode::Char(letter),
                modifiers: KeyModifiers::CONTROL,
            });
        }
        Err(format!(
            "the advance key `{name}` is not one of f1…f12, pagedown or ctrl- and a letter other than q, c, w or k"
        ))
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn the_advance_key_is_taken_once_per_press_and_no_other_key_is() {
        let (playback, player) = link(Advance::parse("ctrl-n").unwrap());
        assert!(playback.take(&press(KeyCode::Char('n'), KeyModifiers::CONTROL)));
        assert!(!playback.take(&press(KeyCode::Char('n'), KeyModifiers::NONE)));
        let mut release = press(KeyCode::Char('n'), KeyModifiers::CONTROL);
        release.kind = KeyEventKind::Release;
        assert!(!playback.take(&release));
        assert_eq!(player.pressed.try_iter().count(), 1);
    }

    #[test]
    fn only_keys_that_type_nothing_can_advance_a_demo() {
        assert_eq!(Advance::parse("F8").unwrap(), Advance::default());
        assert!(Advance::parse("pagedown").is_ok());
        for refused in ["space", "a", "f13", "ctrl-q", "ctrl-k", "ctrl-nn", "enter"] {
            assert!(Advance::parse(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn typed_text_reaches_the_interface_in_order() {
        let (playback, player) = link(Advance::default());
        player.typed.send(Typed::Text("Ship".into())).unwrap();
        player.typed.send(Typed::Text(" it".into())).unwrap();
        player.typed.send(Typed::Send).unwrap();
        assert_eq!(
            playback.typed(),
            [Typed::Text("Ship".into()), Typed::Text(" it".into()), Typed::Send]
        );
        assert!(playback.typed().is_empty());
        assert!(!player.ended());
        drop(playback);
        assert!(player.ended());
    }
}
