//! This device's own choices, kept beside its spaces and never synced through st: how dense a
//! conversation reads (Nathan, 2026-10-02: "a per-device setting, not synced").

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    /// Simplified conversations: a tool call to a line, a run of calls to one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simple: Option<bool>,
    /// An agent's tab opens on its terminal rather than its conversation (Nathan, 2026-10-07:
    /// a setting, and the conversation stays the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_first: Option<bool>,
}

/// `$XDG_STATE_HOME/st3/stui/prefs.json` (or under `~/.local/state`).
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    base.is_absolute()
        .then(|| base.join("st3").join("stui").join("prefs.json"))
}

pub fn load(path: &Path) -> Prefs {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, prefs: &Prefs) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, serde_json::to_vec_pretty(prefs)?)?;
    std::fs::rename(partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_survives_a_restart_and_a_missing_file_is_no_choice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("st3").join("stui").join("prefs.json");
        assert_eq!(load(&path), Prefs::default());
        save(
            &path,
            &Prefs {
                simple: Some(true),
                terminal_first: Some(true),
            },
        )
        .unwrap();
        assert_eq!(load(&path).simple, Some(true));
        assert_eq!(load(&path).terminal_first, Some(true));
        // A file from before the setting reads as the default: the conversation.
        std::fs::write(&path, br#"{"simple": false}"#).unwrap();
        assert_eq!(load(&path).terminal_first, None);
    }
}
