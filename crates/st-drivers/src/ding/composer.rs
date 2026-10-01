//! What a peeked screen proves about one exact notice, and the routing that decides which
//! composer on the screen is the live one.

use super::harness::{self, ReceiptState, Screen};

/// What the current bottom composer proves about one exact normalized notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComposerState {
    /// A maintained harness is positively idle and contains only its known placeholder.
    EmptySafe,
    /// The exact notice is the complete composer and the harness is positively idle.
    ExactSafe,
    /// The exact notice is present, but a modal, active turn, or non-idle footer blocks Return.
    ExactBlocked,
    /// A maintained composer contains different text (including a human draft).
    Changed,
    /// No maintained, unambiguous composer state was proven.
    Ambiguous,
}

/// Logical inputs reconstructed from renderer-proven boundaries, or an unsupported shape.
pub(super) enum SoftWrapCandidates {
    Proven(Vec<String>),
    Unsupported,
}

impl SoftWrapCandidates {
    pub(super) fn proven(&self) -> Option<&[String]> {
        match self {
            Self::Proven(candidates) => Some(candidates),
            Self::Unsupported => None,
        }
    }
}

/// Enumerate the two logical strings possible at each renderer-shaped soft-wrap row: the TUI either
/// discarded one inter-word space or split a token. Current 80-column Codex/Claude composers wrap
/// long DING rows and indent continuations by exactly two cells. The caller supplies the minimum
/// width that proves a wrap boundary for its renderer. Short or unfamiliar multiline input is
/// unsupported rather than positive mismatch evidence.
pub(super) fn logical_soft_wrap_candidates(
    input: &str,
    minimum_previous_content_chars: usize,
) -> SoftWrapCandidates {
    let rows: Vec<&str> = input.lines().collect();
    let Some(first) = rows.first() else {
        return SoftWrapCandidates::Proven(vec![String::new()]);
    };
    if rows.len() == 1 {
        return SoftWrapCandidates::Proven(vec![(*first).to_string()]);
    }
    let mut candidates = vec![(*first).to_string()];
    let mut previous = *first;
    for (index, row) in rows[1..].iter().enumerate() {
        let required_previous_width = minimum_previous_content_chars + usize::from(index > 0) * 2;
        if previous.chars().count() < required_previous_width
            || !row.starts_with("  ")
            || row.trim().is_empty()
        {
            return SoftWrapCandidates::Unsupported;
        }
        let continuation = row.strip_prefix("  ").expect("prefix checked").trim_end();
        let mut next = Vec::with_capacity(candidates.len().saturating_mul(2).min(32));
        for candidate in candidates {
            if next.len() >= 32 {
                return SoftWrapCandidates::Unsupported;
            }
            next.push(format!("{candidate}{continuation}"));
            next.push(format!("{candidate} {continuation}"));
        }
        candidates = next;
        previous = row;
    }
    SoftWrapCandidates::Proven(candidates)
}

pub(super) fn looks_like_choice_menu(plain: &str) -> bool {
    fn starts_with_numbered_option(line: &str) -> bool {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && line.as_bytes().get(digits) == Some(&b'.')
    }

    let mut selected = false;
    let mut numbered = false;
    for line in plain.lines().map(str::trim_start) {
        selected |= ["› ", "❯ ", "> "].iter().any(|marker| {
            line.strip_prefix(marker)
                .is_some_and(starts_with_numbered_option)
        });
        numbered |= starts_with_numbered_option(line);
    }
    selected && numbered
}

/// Strip the ANSI control sequences emitted by `pty peek` while preserving rendered text. Bounded
/// cursor-forward sequences represent visible spaces in current Codex and Claude panes.
pub(super) fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            let ch = input[index..].chars().next().expect("valid UTF-8 boundary");
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        index += 1;
        if index >= bytes.len() {
            break;
        }
        match bytes[index] {
            b'[' => {
                index += 1;
                let params_start = index;
                let mut final_byte = None;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        final_byte = Some(byte);
                        break;
                    }
                }
                if final_byte == Some(b'C') {
                    let params = &bytes[params_start..index.saturating_sub(1)];
                    let width = if params.is_empty() {
                        Some(1)
                    } else if params.iter().all(u8::is_ascii_digit) {
                        std::str::from_utf8(params)
                            .ok()
                            .and_then(|value| value.parse::<usize>().ok())
                            .map(|value| value.max(1))
                    } else {
                        None
                    };
                    if let Some(width) = width.filter(|width| *width <= 512) {
                        for _ in 0..width {
                            out.push(' ');
                        }
                    }
                }
            }
            b']' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => {
                while index < bytes.len() && (0x20..=0x2f).contains(&bytes[index]) {
                    index += 1;
                }
                if index < bytes.len() && (0x30..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
            }
        }
    }
    out
}

/// Locate every maintained composer and classify the LOWEST one on screen.
///
/// A pane has exactly one live composer and it sits at the bottom of the viewport, so anything
/// composer-shaped above it is scrollback — typically a pasted or logged screen from the other
/// harness. Preferring one harness unconditionally lets that transcript decide, which is a wrong
/// *positive*: the paste and the Return go to the pane's real composer whichever text was read, so
/// it can type into, or submit, a human's live draft. Scrollback is above the live composer by
/// construction, so picking the lowest needs no per-pair special case.
pub(super) fn classify_composer(screen: &str, expected: &str) -> ComposerState {
    classify_located_composer(screen, expected).0
}

/// The same routing, additionally naming which maintained harness owned the composer that was
/// classified. `None` means no maintained composer was locatable at all — which is the difference
/// between "a human is drafting in a harness we understand" and "we do not recognise this pane",
/// and the two want different operator responses.
pub(super) fn classify_located_composer(
    screen: &str,
    expected: &str,
) -> (ComposerState, Option<&'static str>) {
    let plain = strip_ansi(screen);
    let screen = Screen {
        raw: screen,
        plain: &plain,
    };
    harness::all()
        .into_iter()
        .filter_map(|harness| {
            harness
                .locate(&screen)
                .map(|located| (located.row, harness))
        })
        .max_by_key(|(row, _)| *row)
        .map(|(_, harness)| (harness.classify(&screen, expected), Some(harness.name())))
        // No maintained composer is locatable, so nothing is proven either way.
        .unwrap_or((ComposerState::Ambiguous, None))
}

/// Route post-submit receipt classification through the lowest maintained live composer, using
/// the same positional rule as pre-submit classification.
pub(super) fn classify_receipt(screen: &str, expected: &str) -> ReceiptState {
    let plain = strip_ansi(screen);
    let screen = Screen {
        raw: screen,
        plain: &plain,
    };
    harness::all()
        .into_iter()
        .filter_map(|harness| {
            harness
                .locate(&screen)
                .map(|located| (located.row, harness))
        })
        .max_by_key(|(row, _)| *row)
        .map(|(_, harness)| harness.receipt(&screen, expected))
        .unwrap_or(ReceiptState::Unproven)
}
