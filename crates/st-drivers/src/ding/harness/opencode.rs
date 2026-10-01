//! The OpenCode full-screen composer: bordered input rows immediately above its mode footer.

use super::{Harness, Located, ReceiptState, Screen};
use crate::ding::composer::ComposerState;

pub(super) struct OpenCode;

impl Harness for OpenCode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn locate(&self, screen: &Screen<'_>) -> Option<Located> {
        locate_composer(screen.plain).map(|composer| Located {
            row: composer.start,
        })
    }

    fn classify(&self, screen: &Screen<'_>, expected: &str) -> ComposerState {
        let Some(composer) = locate_composer(screen.plain) else {
            return ComposerState::Ambiguous;
        };
        let exact = equivalent(&composer.input, expected);
        if exact {
            if composer.idle {
                ComposerState::ExactSafe
            } else {
                ComposerState::ExactBlocked
            }
        } else if composer.input.is_empty() && composer.idle {
            ComposerState::EmptySafe
        } else if composer.input.is_empty() {
            ComposerState::Ambiguous
        } else {
            ComposerState::Changed
        }
    }

    fn receipt(&self, screen: &Screen<'_>, expected: &str) -> ReceiptState {
        let Some(composer) = locate_composer(screen.plain) else {
            return ReceiptState::Unproven;
        };
        if equivalent(&composer.input, expected) {
            return if composer.idle {
                ReceiptState::RetainedSafe
            } else {
                ReceiptState::RetainedBlocked
            };
        }
        if !composer.input.is_empty() {
            return ReceiptState::NotRetained;
        }
        if composer.idle && preceding_submitted_box(&screen.plain[..composer.start_byte], expected)
        {
            ReceiptState::Accepted
        } else if composer.idle {
            ReceiptState::NotRetained
        } else {
            ReceiptState::Unproven
        }
    }
}

struct Composer {
    start: usize,
    start_byte: usize,
    input: String,
    idle: bool,
}

fn locate_composer(plain: &str) -> Option<Composer> {
    let lines = plain.split_inclusive('\n').collect::<Vec<_>>();
    let closure = lines.iter().rposition(|line| {
        let line = line.trim();
        line.strip_prefix('╹').is_some_and(|rule| {
            rule.chars().filter(|ch| *ch == '▀').count() >= 40
                && rule.chars().all(|ch| ch == '▀' || ch.is_whitespace())
        })
    })?;
    let footer = (0..closure).rev().find(|index| {
        let line = lines[*index].trim();
        line.starts_with('┃') && line.contains(" · ")
    })?;
    let start = (0..=footer)
        .rev()
        .take_while(|index| lines[*index].trim_start().starts_with('┃'))
        .last()?;
    let input = lines[start..footer]
        .iter()
        .map(|line| line.trim_start().strip_prefix('┃').unwrap_or(line).trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let start_byte = lines[..start].iter().map(|line| line.len()).sum();
    let tail = lines[footer..].concat();
    Some(Composer {
        start,
        start_byte,
        input,
        // Return is only safe when the pinned OpenCode idle shortcut chrome is
        // positively present. Unknown modal/active states fail closed even if
        // they happen not to render one of the known interrupt hints.
        idle: tail.contains("ctrl+p commands"),
    })
}

fn equivalent(observed: &str, expected: &str) -> bool {
    if observed.contains('\n') || expected.contains('\n') {
        return observed == expected;
    }
    let normalized = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized(observed) == normalized(expected)
}

fn preceding_submitted_box(before_composer: &str, expected: &str) -> bool {
    let mut lines = before_composer.lines().rev();
    let first = lines.find(|line| !line.trim().is_empty());
    let Some(first) = first else {
        return false;
    };
    let Some(content) = first.trim_start().strip_prefix('┃') else {
        return false;
    };
    let mut box_lines = vec![content.trim()];
    for line in lines {
        let Some(content) = line.trim_start().strip_prefix('┃') else {
            break;
        };
        box_lines.push(content.trim());
    }
    box_lines.reverse();
    equivalent(&box_lines.join("\n"), expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: &str = "[PING] id=abc from=dev3.cos subject=liveness";

    fn screen(composer: &str, history: &str, status: &str) -> String {
        format!(
            "{history}\n  ┃ {composer}\n  ┃\n  ┃\n  ┃ Build auto · MiMo V2.5 Free OpenCode Zen\n  ╹{}\n   /work  39K {status}\n",
            "▀".repeat(60)
        )
    }

    #[test]
    fn recognizes_idle_empty_and_exact_composers() {
        let empty = screen("", "answer", "ctrl+p commands");
        let exact = screen(EXPECTED, "answer", "ctrl+p commands");
        assert_eq!(
            OpenCode.classify(
                &Screen {
                    raw: &empty,
                    plain: &empty
                },
                EXPECTED
            ),
            ComposerState::EmptySafe
        );
        assert_eq!(
            OpenCode.classify(
                &Screen {
                    raw: &exact,
                    plain: &exact
                },
                EXPECTED
            ),
            ComposerState::ExactSafe
        );
    }

    #[test]
    fn active_turn_blocks_submission() {
        let active = screen(EXPECTED, "answer", "esc interrupt");
        assert_eq!(
            OpenCode.classify(
                &Screen {
                    raw: &active,
                    plain: &active
                },
                EXPECTED
            ),
            ComposerState::ExactBlocked
        );
    }

    #[test]
    fn unfamiliar_or_modal_chrome_never_enables_return() {
        for status in ["select an option", "unknown footer"] {
            let exact = screen(EXPECTED, "answer", status);
            let empty = screen("", "answer", status);
            assert_eq!(
                OpenCode.classify(
                    &Screen {
                        raw: &exact,
                        plain: &exact
                    },
                    EXPECTED
                ),
                ComposerState::ExactBlocked
            );
            assert_eq!(
                OpenCode.classify(
                    &Screen {
                        raw: &empty,
                        plain: &empty
                    },
                    EXPECTED
                ),
                ComposerState::Ambiguous
            );
        }
    }

    #[test]
    fn human_modified_composer_is_never_exact() {
        for draft in [
            format!("human prefix {EXPECTED}"),
            format!("{EXPECTED} human suffix"),
            EXPECTED.replace(" from=", "from="),
        ] {
            let modified = screen(&draft, "answer", "ctrl+p commands");
            assert_eq!(
                OpenCode.classify(
                    &Screen {
                        raw: &modified,
                        plain: &modified
                    },
                    EXPECTED
                ),
                ComposerState::Changed
            );
        }
    }

    #[test]
    fn provider_name_does_not_identify_the_opencode_composer() {
        let anthropic = screen(EXPECTED, "answer", "ctrl+p commands")
            .replace("MiMo V2.5 Free OpenCode Zen", "Claude Opus 4.1 Anthropic");
        assert_eq!(
            OpenCode.classify(
                &Screen {
                    raw: &anthropic,
                    plain: &anthropic
                },
                EXPECTED
            ),
            ComposerState::ExactSafe
        );
    }

    #[test]
    fn accepted_receipt_requires_history_and_an_empty_idle_composer() {
        let accepted = screen("", &format!("  ┃ {EXPECTED}\n\n"), "ctrl+p commands");
        assert_eq!(
            OpenCode.receipt(
                &Screen {
                    raw: &accepted,
                    plain: &accepted
                },
                EXPECTED
            ),
            ReceiptState::Accepted
        );
    }

    #[test]
    fn ordinary_transcript_text_is_not_an_acceptance_receipt() {
        let transcript = screen(
            "",
            &format!("assistant repeated {EXPECTED}\n\n"),
            "ctrl+p commands",
        );
        assert_eq!(
            OpenCode.receipt(
                &Screen {
                    raw: &transcript,
                    plain: &transcript
                },
                EXPECTED
            ),
            ReceiptState::NotRetained
        );
    }

    #[test]
    fn hard_line_break_in_composer_is_not_exact() {
        let multiline = screen(
            &EXPECTED.replace(" from=", "\n  ┃ from="),
            "answer",
            "ctrl+p commands",
        );
        assert_eq!(
            OpenCode.classify(
                &Screen {
                    raw: &multiline,
                    plain: &multiline
                },
                EXPECTED
            ),
            ComposerState::Changed
        );
    }

    #[test]
    fn old_matching_box_separated_by_output_is_not_a_receipt() {
        let old = screen(
            "",
            &format!("  ┃ {EXPECTED}\n\nassistant output\n"),
            "ctrl+p commands",
        );
        assert_eq!(
            OpenCode.receipt(
                &Screen {
                    raw: &old,
                    plain: &old
                },
                EXPECTED
            ),
            ReceiptState::NotRetained
        );
    }

    #[test]
    fn modified_submitted_box_is_not_an_acceptance_receipt() {
        for submitted in [
            format!("human prefix {EXPECTED}"),
            format!("{EXPECTED} human suffix"),
        ] {
            let modified = screen("", &format!("  ┃ {submitted}\n\n"), "ctrl+p commands");
            assert_eq!(
                OpenCode.receipt(
                    &Screen {
                        raw: &modified,
                        plain: &modified
                    },
                    EXPECTED
                ),
                ReceiptState::NotRetained
            );
        }
    }
}
