//! Per-harness composer adapters behind one positional rule.
//!
//! A pane has exactly one live composer and it is the lowest one on screen; anything
//! composer-shaped above it is scrollback, typically a captured screen from the other harness.
//! Asking every harness where its composer is and then classifying the lowest makes that a single
//! comparison rather than a special case per harness pair, and it means adding a harness cannot
//! reintroduce a preference ordering.
//!
//! Active-turn and modal detection is per-harness on purpose: the shapes are harness-specific TUI
//! chrome, not a shared contract. Only genuinely cross-harness shapes stay in `composer`.

pub(super) mod claude;
pub(super) mod codex;
pub(super) mod opencode;

use super::composer::ComposerState;

/// What one maintained harness can prove after a transport attempted to submit one exact notice.
///
/// The shared delivery state machine consumes only this type. Harness-specific screen vocabulary
/// stays behind [`Harness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiptState {
    /// The expected notice text is visible in the harness's submitted-prompt or queued-message
    /// pattern while the live composer is empty or an accepted idle placeholder.
    Accepted,
    /// The exact notice remains the complete live composer and Return is currently safe.
    RetainedSafe,
    /// The exact notice remains the complete live composer, but the harness is active or blocked.
    RetainedBlocked,
    /// A maintained harness parsed its live composer and positively proved that the exact notice
    /// is neither retained as its complete contents nor visible as an accepted submission.
    NotRetained,
    /// No positive acceptance or exact retained-composer state was proven.
    Unproven,
}

/// One peeked screen in both forms a harness may need: the raw bytes, in which the Codex composer
/// markers are written as ANSI sequences, and the stripped text, in which the Claude rules are
/// found. Both describe the same screen, so a row index is comparable across harnesses.
pub(super) struct Screen<'a> {
    pub(super) raw: &'a str,
    pub(super) plain: &'a str,
}

/// Where a harness found its live composer.
pub(super) struct Located {
    /// The composer's first row in the stripped screen. The router classifies the lowest.
    pub(super) row: usize,
}

pub(super) trait Harness {
    /// How this harness is named in operator-facing diagnostics.
    fn name(&self) -> &'static str;

    /// Locate this harness's composer, if this screen has one.
    fn locate(&self, screen: &Screen<'_>) -> Option<Located>;

    /// Classify the composer `locate` found. Only called when `locate` returned `Some`, so each
    /// implementation re-derives it rather than the registry threading a harness-specific payload
    /// through; a screen is one viewport, so the extra scan is a few string searches.
    fn classify(&self, screen: &Screen<'_>, expected: &str) -> ComposerState;

    /// Classify post-submit evidence for one exact notice. Implementations may return `Accepted`
    /// only when the lowest live composer is empty or an accepted idle placeholder and the
    /// expected notice text is visible in that harness's submitted-prompt or queued-message
    /// pattern.
    fn receipt(&self, screen: &Screen<'_>, expected: &str) -> ReceiptState;
}

/// The normalized expected notice text is visible after this harness's submitted-prompt glyph or
/// after the explicit queued-message heading. Whitespace compaction admits renderer soft wraps;
/// the caller separately proves that the lowest live composer is empty or an accepted placeholder.
pub(super) fn screen_has_accepted_notice(
    screen: &Screen<'_>,
    submitted_prompt: char,
    expected: &str,
) -> bool {
    const QUEUED_MESSAGES: &str = "Messages to be submitted after next tool call";

    fn compact(input: &str) -> String {
        input
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    let screen = super::normalize_line(screen.plain);
    let expected = super::normalize_line(expected);
    let submitted = format!("{submitted_prompt} {expected}");
    let compact_screen = compact(&screen);
    let compact_expected = compact(&expected);
    let compact_submitted = format!("{submitted_prompt}{compact_expected}");

    screen.contains(&submitted)
        || compact_screen.contains(&compact_submitted)
        || screen
            .split_once(QUEUED_MESSAGES)
            .is_some_and(|(_, queued)| {
                queued.contains(&expected) || compact(queued).contains(&compact_expected)
            })
}

/// Every registered harness. Claude is last so that an exact row tie resolves to Claude, which is
/// how the positional comparison behaved before the harnesses were split apart.
pub(super) fn all() -> [&'static dyn Harness; 3] {
    [&codex::Codex, &claude::Claude, &opencode::OpenCode]
}
