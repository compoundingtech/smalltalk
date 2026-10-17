//! Runtime-owned placement and value-free failure classification for PTY launchers.
//! Shared as source by both launch backends so their boundary policy cannot drift.

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

/// Closed diagnostic categories; never carry captured launcher text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PtyLaunchFailureKind {
    LauncherIncompatible,
    PlacementConflict,
    SessionInUse,
    SpawnFailed,
    Unknown,
}

impl PtyLaunchFailureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LauncherIncompatible => "launcher-incompatible",
            Self::PlacementConflict => "placement-conflict",
            Self::SessionInUse => "session-in-use",
            Self::SpawnFailed => "spawn-failed",
            Self::Unknown => "unknown",
        }
    }
}

/// Match only known diagnostic signatures. Unrecognized output stays `unknown`.
/// Byte matching avoids copying or decoding potentially value-bearing output.
pub(crate) fn classify_pty_launch_failure(stderr: &[u8], stdout: &[u8]) -> PtyLaunchFailureKind {
    let contains = |needle: &[u8]| {
        [stderr, stdout].into_iter().any(|text| {
            text.windows(needle.len()).any(|window| window == needle)
        })
    };
    if contains(b"Invalid env format:")
        && contains(b"Use --env KEY=VALUE")
        && !contains(b"or --env KEY")
    {
        PtyLaunchFailureKind::LauncherIncompatible
    } else if contains(b"both PTY_ROOT and PTY_SESSION_DIR are set")
        || contains(b"pty: PTY_ROOT is too long")
    {
        PtyLaunchFailureKind::PlacementConflict
    } else if contains(b"already in use") {
        PtyLaunchFailureKind::SessionInUse
    } else if contains(b"failed to start daemon")
        || contains(b"Daemon process exited immediately")
        || contains(b"Timeout waiting for session")
        || contains(b"Timed out waiting for daemon publication")
    {
        PtyLaunchFailureKind::SpawnFailed
    } else {
        PtyLaunchFailureKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kinds_are_closed_and_unknown_output_stays_unknown() {
        for (diagnostic, expected) in [
            (
                b"Invalid env format: \"NAME\". Use --env KEY=VALUE".as_slice(),
                PtyLaunchFailureKind::LauncherIncompatible,
            ),
            (b"both PTY_ROOT and PTY_SESSION_DIR are set", PtyLaunchFailureKind::PlacementConflict),
            (b"pty: PTY_ROOT is too long", PtyLaunchFailureKind::PlacementConflict),
            (b"Session id \"NAME\" is already in use.", PtyLaunchFailureKind::SessionInUse),
            (b"failed to start daemon: synthetic detail", PtyLaunchFailureKind::SpawnFailed),
            (b"Invalid env format: \"NAME\". Use --env KEY=VALUE or --env KEY", PtyLaunchFailureKind::Unknown),
            (b"\xffsynthetic-unrecognized-output", PtyLaunchFailureKind::Unknown),
        ] {
            assert_eq!(classify_pty_launch_failure(diagnostic, b""), expected);
            assert_eq!(classify_pty_launch_failure(b"", diagnostic), expected);
        }
    }
}
