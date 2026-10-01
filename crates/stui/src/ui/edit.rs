//! Text typed into stui's inputs, edited at a cursor with the keys a shell's line editor has:
//! the arrows, Ctrl+A/E, Ctrl+B/F, Alt+B/F, Ctrl+W/U/K/D, Backspace and Delete.
//!
//! One input has the keyboard at a time, so one cursor serves them all. It remembers which
//! input it belongs to, so an input it does not belong to types at its end. It counts from the
//! end of the text, so a draft cleared by sending puts it back at the end.

use super::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use st3_conversation_ui::text::{self, Run};
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct Cursor {
    input: RefCell<String>,
    from_end: Cell<usize>,
}

impl Cursor {
    /// The cursor's byte offset in `input`'s `text`: its end unless the cursor was moved there.
    pub fn at(&self, input: &str, text: &str) -> usize {
        if *self.input.borrow() != input {
            return text.len();
        }
        floor(text, text.len().saturating_sub(self.from_end.get()))
    }

    fn set(&self, input: &str, text: &str, at: usize) {
        if *self.input.borrow() != input {
            *self.input.borrow_mut() = input.to_owned();
        }
        self.from_end.set(text.len() - at.min(text.len()));
    }
}

/// The nearest character boundary at or before `at`.
fn floor(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn previous(text: &str, at: usize) -> usize {
    text[..at]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .next()
        .map_or(at, |character| at + character.len_utf8())
}

fn word_start(text: &str, at: usize) -> usize {
    let before = &text[..at];
    let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_alphanumeric())
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// Ctrl+W's word: back over spaces, then over everything up to a space; at a line's start,
/// the line break.
fn space_word_start(text: &str, at: usize) -> usize {
    let before = text[..at].trim_end_matches([' ', '\t']);
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    if start == at && text[..at].ends_with('\n') {
        at - 1
    } else {
        start
    }
}

fn word_end(text: &str, at: usize) -> usize {
    let after = &text[at..];
    let skipped = after.len()
        - after
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .len();
    let rest = &after[skipped..];
    at + skipped
        + rest
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(rest.len(), |(index, _)| index)
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |index| at + index)
}

/// Type `typed` at the cursor.
pub fn insert(text: &mut String, cursor: &Cursor, input: &str, typed: &str) {
    let at = cursor.at(input, text);
    text.insert_str(at, typed);
    cursor.set(input, text, at + typed.len());
}

/// Edit `input`'s `text` with `key`. Returns whether the key belonged to the text; a typing
/// key never reaches anything else. Up and Down do not belong to it.
pub fn edit(text: &mut String, cursor: &Cursor, input: &str, key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let at = cursor.at(input, text);
    let delete = |text: &mut String, from: usize, to: usize| {
        text.replace_range(from..to, "");
        cursor.set(input, text, from);
    };
    let moved = match key.code {
        KeyCode::Backspace if control || alt => {
            delete(text, word_start(text, at), at);
            None
        }
        KeyCode::Char('w') if control && !alt => {
            delete(text, space_word_start(text, at), at);
            None
        }
        KeyCode::Backspace => {
            delete(text, previous(text, at), at);
            None
        }
        KeyCode::Char('h') if control && !alt => {
            delete(text, previous(text, at), at);
            None
        }
        KeyCode::Delete => {
            delete(text, at, next(text, at));
            None
        }
        KeyCode::Char('d') if control && !alt => {
            delete(text, at, next(text, at));
            None
        }
        KeyCode::Char('u') if control && !alt => {
            delete(text, line_start(text, at), at);
            None
        }
        KeyCode::Char('k') if control && !alt => {
            delete(text, at, line_end(text, at));
            None
        }
        KeyCode::Left if control || alt => Some(word_start(text, at)),
        KeyCode::Right if control || alt => Some(word_end(text, at)),
        KeyCode::Char('b') if alt && !control => Some(word_start(text, at)),
        KeyCode::Char('f') if alt && !control => Some(word_end(text, at)),
        KeyCode::Left => Some(previous(text, at)),
        KeyCode::Char('b') if control && !alt => Some(previous(text, at)),
        KeyCode::Right => Some(next(text, at)),
        KeyCode::Char('f') if control && !alt => Some(next(text, at)),
        KeyCode::Home => Some(line_start(text, at)),
        KeyCode::Char('a') if control && !alt => Some(line_start(text, at)),
        KeyCode::End => Some(line_end(text, at)),
        KeyCode::Char('e') if control && !alt => Some(line_end(text, at)),
        // AltGr arrives as Ctrl+Alt on some terminals: that is typing too.
        KeyCode::Char(character) if control == alt => {
            insert(text, cursor, input, character.encode_utf8(&mut [0; 4]));
            None
        }
        _ => return false,
    };
    if let Some(to) = moved {
        cursor.set(input, text, to);
    }
    true
}

/// `text` as one row of runs per line, with the cursor drawn at byte `at` when it has one: a
/// block past a line's end, the character under it inverted otherwise.
pub fn lines(text: &str, at: Option<usize>, style: Style) -> Vec<Vec<Run>> {
    let mut start = 0;
    text.split('\n')
        .map(|line| {
            let end = start + line.len();
            let runs = match at {
                Some(at) if (start..=end).contains(&at) => {
                    let column = at - start;
                    let mut runs = vec![text::run(line[..column].to_owned(), style)];
                    match line[column..].chars().next() {
                        Some(under) => {
                            runs.push(text::run(
                                under.to_string(),
                                theme::fg(theme::CRUST).bg(theme::ACCENT),
                            ));
                            runs.push(text::run(
                                line[column + under.len_utf8()..].to_owned(),
                                style,
                            ));
                        }
                        None => runs.push(text::run("█", theme::fg(theme::ACCENT))),
                    }
                    runs
                }
                _ => vec![text::run(line.to_owned(), style)],
            };
            start = end + 1;
            runs
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }
    fn press(text: &mut String, cursor: &Cursor, code: KeyCode, modifiers: KeyModifiers) -> bool {
        edit(text, cursor, "draft", key(code, modifiers))
    }
    fn typed(text: &mut String, cursor: &Cursor, typed: &str) {
        for character in typed.chars() {
            press(text, cursor, KeyCode::Char(character), KeyModifiers::NONE);
        }
    }
    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const NONE: KeyModifiers = KeyModifiers::NONE;

    #[test]
    fn the_line_editor_keys_move_and_edit_at_the_cursor() {
        let cursor = Cursor::default();
        let mut text = String::new();
        typed(&mut text, &cursor, "hello world");
        press(&mut text, &cursor, KeyCode::Char('a'), CONTROL);
        typed(&mut text, &cursor, "> ");
        assert_eq!(text, "> hello world");
        press(&mut text, &cursor, KeyCode::Char('e'), CONTROL);
        press(&mut text, &cursor, KeyCode::Char('b'), ALT);
        assert_eq!(cursor.at("draft", &text), "> hello ".len());
        press(&mut text, &cursor, KeyCode::Char('k'), CONTROL);
        assert_eq!(text, "> hello ");
        press(&mut text, &cursor, KeyCode::Char('w'), CONTROL);
        assert_eq!(text, "> ");
        typed(&mut text, &cursor, "héllo there");
        press(&mut text, &cursor, KeyCode::Left, CONTROL);
        press(&mut text, &cursor, KeyCode::Left, NONE);
        press(&mut text, &cursor, KeyCode::Backspace, NONE);
        assert_eq!(text, "> héll there");
        press(&mut text, &cursor, KeyCode::Char('d'), CONTROL);
        assert_eq!(text, "> héllthere");
        press(&mut text, &cursor, KeyCode::Char('u'), CONTROL);
        assert_eq!(text, "there");
        press(&mut text, &cursor, KeyCode::Char('f'), ALT);
        assert_eq!(cursor.at("draft", &text), text.len());
        // Up and Down are not the text's.
        assert!(!press(&mut text, &cursor, KeyCode::Up, NONE));
    }

    #[test]
    fn line_keys_stay_on_the_cursors_line_and_another_input_types_at_its_end() {
        let cursor = Cursor::default();
        let mut text = "first\nsecond".to_owned();
        press(&mut text, &cursor, KeyCode::Char('a'), CONTROL);
        assert_eq!(cursor.at("draft", &text), "first\n".len());
        insert(&mut text, &cursor, "draft", "the ");
        assert_eq!(text, "first\nthe second");
        press(&mut text, &cursor, KeyCode::Char('u'), CONTROL);
        assert_eq!(text, "first\nsecond");
        // A cleared draft has its cursor at its end.
        let mut cleared = String::new();
        assert_eq!(cursor.at("draft", &cleared), 0);
        typed(&mut cleared, &cursor, "new");
        assert_eq!(cleared, "new");
        let mut other = "elsewhere".to_owned();
        assert_eq!(cursor.at("find", &other), other.len());
        edit(&mut other, &cursor, "find", key(KeyCode::Char('!'), NONE));
        assert_eq!(other, "elsewhere!");
    }

    #[test]
    fn the_cursor_draws_as_a_block_at_a_lines_end_or_over_its_character() {
        let plain = |runs: &Vec<Run>| runs.iter().map(|run| run.text.as_str()).collect::<String>();
        let shown = lines("ab\ncd", Some(1), Style::default());
        assert_eq!(shown.iter().map(plain).collect::<Vec<_>>(), ["ab", "cd"]);
        assert_eq!(shown[0][1].text, "b");
        assert_eq!(shown[0][1].style.bg, Some(theme::ACCENT));
        let shown = lines("ab\ncd", Some(5), Style::default());
        assert_eq!(plain(&shown[1]), "cd█");
        assert_eq!(lines("ab", None, Style::default())[0].len(), 1);
    }
}
