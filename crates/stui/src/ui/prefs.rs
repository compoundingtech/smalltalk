//! This device's own choices, kept beside its spaces and never synced through st: how dense a
//! conversation reads (Nathan, 2026-10-02: "a per-device setting, not synced").

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    /// Simplified conversations: a tool call to a line, a run of calls to one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simple: Option<bool>,
    /// Before conversation_first: an agent's tab opened on its terminal. Read only to keep the
    /// choice of a device that turned it on, and written beside conversation_first so an older
    /// stui keeps opening agents the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_first: Option<bool>,
    /// An agent's tab opens on its conversation rather than attaching its terminal. Attaching is
    /// the default (2026-10-15); absent means this device never chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_first: Option<bool>,
    /// Use the daemon after a failed direct attachment; absence means enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_fallback: Option<bool>,
}

/// Whether this device opens agents on their conversation: its own choice, else no. Before the
/// setting existed every file carried `terminal_first`, true only when the person turned it on
/// and false otherwise, so false is no choice and both mean attaching.
pub fn conversation_first(prefs: &Prefs) -> Option<bool> {
    prefs.conversation_first
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
                conversation_first: Some(true),
                terminal_fallback: Some(false),
            },
        )
        .unwrap();
        assert_eq!(load(&path).simple, Some(true));
        assert_eq!(load(&path).terminal_first, Some(true));
        assert_eq!(load(&path).terminal_fallback, Some(false));
        assert_eq!(conversation_first(&load(&path)), Some(true));
        // A file from before the setting is no choice: the terminal attaches.
        std::fs::write(&path, br#"{"simple": false}"#).unwrap();
        assert_eq!(conversation_first(&load(&path)), None);
    }

    #[test]
    fn an_old_terminal_first_choice_still_attaches_and_its_default_is_no_choice() {
        // Before 2026-10-15 every saved file carried terminal_first; true was a choice to attach,
        // false only the default of the day. Neither turns into conversation first.
        for old in [r#"{"terminal_first": true}"#, r#"{"simple": true, "terminal_first": false}"#] {
            let prefs: Prefs = serde_json::from_str(old).unwrap();
            assert_eq!(conversation_first(&prefs), None, "{old}");
        }
        // A newer file an older stui reads keeps working there: unknown keys are ignored.
        let new = serde_json::to_string(&Prefs {
            conversation_first: Some(true),
            terminal_first: Some(false),
            ..Prefs::default()
        })
        .unwrap();
        let old: serde_json::Value = serde_json::from_str(&new).unwrap();
        assert_eq!(old["terminal_first"], false);
    }
}
