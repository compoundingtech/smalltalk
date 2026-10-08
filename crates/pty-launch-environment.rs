//! Placement controls owned by the launcher runtime, never by a seat overlay.
//! Shared as source by the two launch backends so this list cannot drift.

use std::path::Path;
use std::process::Command;

/// The pinned PTY registry resolver reads only these PTY_* placement variables.
/// HOME is a fallback only when neither is set; we always set the canonical root.
pub(crate) const PTY_PLACEMENT_ENV: [&str; 2] = ["PTY_ROOT", "PTY_SESSION_DIR"];

/// Apply after the seat overlay, including removals of inherited legacy controls.
pub(crate) fn configure_pty_placement(command: &mut Command, root: &Path) {
    for key in PTY_PLACEMENT_ENV {
        command.env_remove(key);
    }
    command.env("PTY_ROOT", root);
}
