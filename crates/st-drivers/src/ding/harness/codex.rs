//! The Codex composer: a `›` prompt written directly as an ANSI sequence, with the model/cwd
//! status line below it.

use super::{Harness, Located, ReceiptState, Screen, screen_has_accepted_notice};
use crate::ding::composer::{
    ComposerState, logical_soft_wrap_candidates, looks_like_choice_menu, strip_ansi,
};

pub(super) struct Codex;

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn locate(&self, screen: &Screen<'_>) -> Option<Located> {
        // The markers are ANSI, so this locator works in raw byte offsets while the router compares
        // stripped rows. Every marker starts at an `\x1b[` boundary, so stripping the prefix is
        // faithful and its newline count is that composer's row.
        located_bottom_codex_composer(screen.raw).map(|(start, _)| Located {
            row: strip_ansi(&screen.raw[..start]).matches('\n').count(),
        })
    }

    fn classify(&self, screen: &Screen<'_>, expected: &str) -> ComposerState {
        classify_codex_composer(screen.raw, screen.plain, expected)
    }

    fn receipt(&self, screen: &Screen<'_>, expected: &str) -> ReceiptState {
        let blocked = interaction_blocked(screen.plain);
        let Some((start, composer)) = located_bottom_codex_composer(screen.raw) else {
            return ReceiptState::Unproven;
        };
        let idle_footer = codex_idle_footer(&screen.raw[start..]);
        match composer {
            CodexComposer::Empty if screen_has_accepted_notice(screen, '›', expected) => {
                ReceiptState::Accepted
            }
            CodexComposer::Empty => ReceiptState::NotRetained,
            CodexComposer::Typed(input) => {
                // Codex may wrap after a hyphen and leave a short preceding row. The two-cell
                // continuation indent and non-empty content prove each wrap boundary.
                let candidates = logical_soft_wrap_candidates(&input, 1);
                let Some(candidates) = candidates.proven() else {
                    return ReceiptState::Unproven;
                };
                let exact = candidates.iter().any(|input| input == expected);
                if exact && !blocked && idle_footer {
                    ReceiptState::RetainedSafe
                } else if exact {
                    ReceiptState::RetainedBlocked
                } else {
                    ReceiptState::NotRetained
                }
            }
        }
    }
}

const CODEX_EMPTY_COMPOSERS: [&str; 3] = [
    "\x1b[1;22m›\x1b[1C\x1b[22;2m",
    "\x1b[1m›\x1b[1C\x1b[22;2m",
    "\x1b[1m›\x1b[22m \x1b[2m",
];

const CODEX_TYPED_COMPOSERS: [&str; 4] = [
    "\x1b[1;22m›\x1b[1C\x1b[0m",
    "\x1b[1;2m› \x1b[0m",
    "\x1b[1m›\x1b[1C\x1b[0m",
    "\x1b[1m›\x1b[22m ",
];

enum CodexComposer {
    Empty,
    Typed(String),
}

fn classify_codex_composer(screen: &str, plain: &str, expected: &str) -> ComposerState {
    let blocked = interaction_blocked(plain);
    let Some((start, composer)) = located_bottom_codex_composer(screen) else {
        return ComposerState::Ambiguous;
    };
    let idle_footer = codex_idle_footer(&screen[start..]);
    match composer {
        CodexComposer::Empty if !blocked && idle_footer => ComposerState::EmptySafe,
        CodexComposer::Empty => ComposerState::Ambiguous,
        CodexComposer::Typed(input) => {
            // Codex may wrap after a hyphen and leave a short preceding row. The two-cell
            // continuation indent and non-empty content prove each wrap boundary.
            let candidates = logical_soft_wrap_candidates(&input, 1);
            let Some(candidates) = candidates.proven() else {
                return ComposerState::Ambiguous;
            };
            let exact = candidates.iter().any(|input| input == expected);
            if exact && !blocked && idle_footer {
                ComposerState::ExactSafe
            } else if exact {
                ComposerState::ExactBlocked
            } else {
                ComposerState::Changed
            }
        }
    }
}

fn codex_idle_footer(screen_from_composer: &str) -> bool {
    let plain = strip_ansi(screen_from_composer);
    let lines = plain
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut candidates = lines
        .iter()
        .copied()
        .filter(|line| line.starts_with("gpt-"));
    let Some(footer) = candidates.next() else {
        return false;
    };
    if candidates.next().is_some() || lines.last() != Some(&footer) {
        return false;
    }
    let mut fields = footer.split(" · ");
    fields.next().is_some_and(|model| model.starts_with("gpt-"))
        && fields.any(|field| {
            field.starts_with('/') || field.starts_with("~/") || codex_context_field(field)
        })
}

fn codex_context_field(field: &str) -> bool {
    let Some(context) = field.strip_prefix("Context ") else {
        return false;
    };
    let mut parts = context.split(' ');
    let (Some(percent), Some(direction), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    matches!(direction, "left" | "used")
        && percent
            .strip_suffix('%')
            .and_then(|value| value.parse::<u8>().ok())
            .is_some_and(|value| value <= 100)
}

fn located_bottom_codex_composer(screen: &str) -> Option<(usize, CodexComposer)> {
    let empty = CODEX_EMPTY_COMPOSERS
        .iter()
        .filter_map(|marker| {
            screen
                .rfind(marker)
                .map(|start| (start, marker.len(), true))
        })
        .max_by_key(|(start, _, _)| *start);
    let typed = CODEX_TYPED_COMPOSERS
        .iter()
        .filter_map(|marker| {
            screen
                .rfind(marker)
                .map(|start| (start, marker.len(), false))
        })
        .max_by_key(|(start, _, _)| *start);
    let (start, marker_len, empty) = match (empty, typed) {
        (Some(empty), Some(typed)) if empty.0 >= typed.0 => empty,
        (_, Some(typed)) => typed,
        (Some(empty), None) => empty,
        (None, None) => return None,
    };
    if empty {
        return Some((start, CodexComposer::Empty));
    }
    let tail = &screen[start + marker_len..];
    let input = tail
        .split_once("\r\n \x1b[")
        .or_else(|| tail.split_once("\n \x1b["))
        .or_else(|| tail.split_once("\r\n\r\n"))
        .or_else(|| tail.split_once("\n\n"))
        .map(|(input, _)| input)
        .unwrap_or(tail);
    Some((
        start,
        CodexComposer::Typed(normalize_codex_composer_input(input)),
    ))
}

/// Strip ANSI from one bottom-composer input while preserving rows for soft-wrap reconstruction.
fn normalize_codex_composer_input(input: &str) -> String {
    strip_ansi(input)
}

/// States in which Return must not be sent. Several of these strings are Claude-specific chrome
/// that the pre-split shared predicate also applied to Codex panes; they are duplicated rather
/// than narrowed here so that splitting the harnesses apart changes no behaviour.
/// The progress-line shape inherited from the pre-split shared predicate. It is another harness's
/// rendering, kept here only so the split changed no behaviour, and duplicated rather than called
/// across module lines so that narrowing it is a local edit to this file.
fn inherited_progress_line(plain: &str) -> bool {
    plain.lines().any(|line| {
        let Some((head, _)) = line.trim().split_once('…') else {
            return false;
        };
        let mut tokens = head.split_whitespace();
        let (Some(glyph), Some(word), None) = (tokens.next(), tokens.next(), tokens.next()) else {
            return false;
        };
        glyph.chars().count() == 1
            && !glyph.chars().any(char::is_alphanumeric)
            && !word.is_empty()
            && word.chars().all(char::is_alphabetic)
    })
}

fn interaction_blocked(plain: &str) -> bool {
    inherited_progress_line(plain)
        || plain.contains("Working (")
        || plain.contains("esc to interrupt")
        || plain.contains("Esc to interrupt")
        || plain.contains("ctrl+c to interrupt")
        || plain.contains("Messages to be submitted after next tool call")
        || plain.contains("press esc to interrupt and send")
        || plain.contains("Our systems are thinking a bit more")
        || plain.contains("Retry with a faster model")
        || plain.contains("Create a plan?")
        || looks_like_choice_menu(plain)
}
