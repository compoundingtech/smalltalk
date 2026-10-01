//! The Claude Code composer: a `❯` row between two full-width rules, with the permission-mode
//! footer below it.

use super::{Harness, Located, ReceiptState, Screen, screen_has_accepted_notice};
use crate::ding::composer::{
    ComposerState, SoftWrapCandidates, logical_soft_wrap_candidates, looks_like_choice_menu,
};

pub(super) struct Claude;

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn locate(&self, screen: &Screen<'_>) -> Option<Located> {
        located_bottom_claude_composer(screen.plain).map(|(row, _, _)| Located { row })
    }

    fn classify(&self, screen: &Screen<'_>, expected: &str) -> ComposerState {
        let Some((_, logical_inputs, footer)) = located_bottom_claude_composer(screen.plain) else {
            return ComposerState::Ambiguous;
        };
        classify_claude_composer(screen.plain, (logical_inputs, footer), expected)
    }

    fn receipt(&self, screen: &Screen<'_>, expected: &str) -> ReceiptState {
        let Some((_, logical_inputs, footer)) = located_bottom_claude_composer(screen.plain) else {
            return ReceiptState::Unproven;
        };
        let Some(logical_inputs) = logical_inputs.proven() else {
            return ReceiptState::Unproven;
        };
        let exact = logical_inputs.iter().any(|input| input == expected);
        let placeholder = logical_inputs.len() == 1
            && (logical_inputs[0].is_empty() || is_claude_idle_placeholder(&logical_inputs[0]));
        let idle_footer = footer.contains("⏵⏵") && footer.contains("permissions on");
        let blocked = interaction_blocked(screen.plain);
        if exact && idle_footer && !blocked {
            ReceiptState::RetainedSafe
        } else if exact {
            ReceiptState::RetainedBlocked
        } else if placeholder && screen_has_accepted_notice(screen, '❯', expected) {
            ReceiptState::Accepted
        } else {
            ReceiptState::NotRetained
        }
    }
}

fn classify_claude_composer(
    plain: &str,
    (logical_inputs, footer): (SoftWrapCandidates, String),
    expected: &str,
) -> ComposerState {
    let Some(logical_inputs) = logical_inputs.proven() else {
        return ComposerState::Ambiguous;
    };
    let exact = logical_inputs.iter().any(|input| input == expected);
    // An empty composer is a stronger positive-empty proof than the placeholder below, because no
    // human draft can be empty. Claude only shows the rotating placeholder on an unused pane.
    let placeholder = logical_inputs.len() == 1
        && (logical_inputs[0].is_empty() || is_claude_idle_placeholder(&logical_inputs[0]));
    // The keybinding hint is runtime-composed and may be absent or remapped. The mode marker and
    // permission state are the stable safety signals; active/modal/changed states remain fail-closed.
    let idle_footer = footer.contains("⏵⏵") && footer.contains("permissions on");
    let blocked = interaction_blocked(plain);
    if exact {
        if idle_footer && !blocked {
            ComposerState::ExactSafe
        } else {
            ComposerState::ExactBlocked
        }
    } else if placeholder && idle_footer && !blocked {
        ComposerState::EmptySafe
    } else {
        ComposerState::Changed
    }
}

/// Claude 2.1.220 rotates repository-aware examples (file names and verbs vary) without styling
/// them differently from typed input. The exact `Try "<single-line example>"` grammar plus the
/// maintained idle footer is therefore the narrowest available positive heuristic.
fn is_claude_idle_placeholder(input: &str) -> bool {
    input
        .strip_prefix("Try \"")
        .and_then(|example| example.strip_suffix('"'))
        .is_some_and(|example| {
            !example.is_empty()
                && example.chars().count() <= 72
                && !example.chars().any(char::is_control)
                && !example.contains(['\r', '\n'])
        })
}

/// Extract all logical strings consistent with Claude's renderer-proven soft wraps. At a full-width
/// row Claude may either wrap at a discarded space or split a token, so each proven boundary has
/// exactly two candidates: join with one space or with none. The bounded DING length keeps this set
/// small; any unfamiliar multiline shape fails closed.
fn located_bottom_claude_composer(plain: &str) -> Option<(usize, SoftWrapCandidates, String)> {
    let lines: Vec<&str> = plain.lines().collect();
    let separators: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            (trimmed.chars().count() >= 40 && trimmed.chars().all(|ch| ch == '─')).then_some(index)
        })
        .collect();
    let (&bottom, before_bottom) = separators.split_last()?;
    let &top = before_bottom.last()?;
    if bottom <= top + 1 {
        return None;
    }

    let rows = &lines[top + 1..bottom];
    // Strip the prompt before trimming: the separator Claude emits is U+00A0, which is itself
    // trailing whitespace, so trimming first erases the prompt of an empty composer.
    let first_row = rows.first()?;
    let first = first_row
        .strip_prefix("❯\u{00a0}")
        .or_else(|| first_row.strip_prefix("❯ "))?
        .trim_end();
    let input = std::iter::once(first)
        .chain(rows[1..].iter().map(|row| row.trim_end()))
        .collect::<Vec<_>>()
        .join("\n");
    // Claude word-wraps before the next word when it does not fit, so the preceding row may be
    // short. The two-cell continuation indent and non-empty content prove each wrap boundary.
    let candidates = logical_soft_wrap_candidates(&input, 1);
    let footer = lines[bottom + 1..].join("\n");
    // `top + 1` is the composer's first row, which is what the router compares against the Codex
    // composer's row to find the lower — and therefore live — of the two.
    Some((top + 1, candidates, footer))
}

/// Claude 2.1.220 renders an in-flight turn as a status line and ships no interrupt hint with it.
/// Sampled across real panes, an active turn looks like `✻ Frolicking… (3m 35s · ↓ 6.9k tokens)`,
/// `✽ Schlepping…`, or `· Metamorphosing…`, while a finished turn looks like `✻ Brewed for 5s` or
/// `✻ Cogitated for 11s · 1 shell still running`.
///
/// Two things follow. The leading glyph animates (`·` `✶` `✻` `✽` all observed), so it cannot be
/// matched literally. And the elapsed timer is absent from some active frames, so requiring it
/// would fail open on exactly the frames that matter. What separates the two states is the
/// ellipsis directly after a single gerund: a finished turn says `for <n>s` instead.
///
/// Matching the finished shape too would be catastrophic rather than merely conservative — every
/// idle pane shows it immediately after a turn, so DING would never deliver again.
fn claude_turn_in_flight(plain: &str) -> bool {
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

/// States in which Return must not be sent. Several of these strings are Claude-specific chrome
/// that the pre-split shared predicate also applied to Codex panes; they are duplicated rather
/// than narrowed here so that splitting the harnesses apart changes no behaviour.
fn interaction_blocked(plain: &str) -> bool {
    claude_turn_in_flight(plain)
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
