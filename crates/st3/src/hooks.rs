//! The lifecycle hook set an st3 seat's harness runs.
//!
//! Every script execs `"$ST3_BIN" driver-hook NAME`: the st3 binary that launched the seat answers
//! the hook itself, so no seat looks an `st2` program up on PATH. The set is published beneath
//! st3's own state directory as an immutable content-addressed directory,
//! `STATE/hooks/sets/sha256-…`, and `$ST_HOOKS` names it. A changed script is a new directory, and
//! an older directory stays in place, so a running seat keeps the set it started with until it
//! restarts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

/// Each file in the set, by name. The pi and omp extensions belong to st's
/// daemon channel; historical seats retain the shared driver's original assets.
pub const FILES: [(&str, &[u8]); 6] = [
    ("claude-observe.sh", include_bytes!("../hooks/claude-observe.sh")),
    ("claude-statusline.sh", include_bytes!("../hooks/claude-statusline.sh")),
    ("pi-channel.ts", include_bytes!("../hooks/pi-channel.ts")),
    ("omp-channel.ts", include_bytes!("../hooks/omp-channel.ts")),
    ("omp-harness-control.ts", include_bytes!("../hooks/omp-harness-control.ts")),
    ("omp-harness-ask.ts", include_bytes!("../hooks/omp-harness-ask.ts")),
];

/// The file that makes a directory an st3 hook set. st2 skips an `$ST_HOOKS` that holds it.
pub const MANIFEST: &str = st_drivers::hooks::ST3_SET_MARKER;
const SETS_DIR: &str = "sets";

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn manifest_bytes() -> Vec<u8> {
    let files = FILES
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), format!("sha256:{}", sha256(bytes))))
        .collect::<BTreeMap<_, _>>();
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema": 1,
        "owner": "st3",
        "files": files,
    }))
    .expect("the hook manifest serializes");
    bytes.push(b'\n');
    bytes
}

/// The content identity of this binary's hook set.
pub fn set_id() -> String {
    format!("sha256-{}", sha256(&manifest_bytes()))
}

/// The hook root beneath an st3 state directory.
pub fn root(state_dir: &Path) -> PathBuf {
    state_dir.join("hooks")
}

/// This binary's set directory beneath `root`. Resolving it changes nothing.
pub fn set_dir(root: &Path) -> PathBuf {
    root.join(SETS_DIR).join(set_id())
}

/// Whether an older immutable set contains this hook. Running harnesses retain `ST_HOOKS`
/// across binary replacement; only their already-published scripts may use retired aliases.
pub(crate) fn older_set_contains_hook(dir: &Path, name: &str) -> bool {
    let Ok(bytes) = fs::read(dir.join(MANIFEST)) else {
        return false;
    };
    let identity = format!("sha256-{}", sha256(&bytes));
    if identity == set_id() || dir.file_name().and_then(|part| part.to_str()) != Some(&identity) {
        return false;
    }
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if manifest["schema"] != 1 || manifest["owner"] != "st3" {
        return false;
    }
    let file = format!("{name}.sh");
    let Ok(script) = fs::read(dir.join(&file)) else {
        return false;
    };
    manifest["files"][&file].as_str() == Some(&format!("sha256:{}", sha256(&script)))
}

/// Check that `dir` holds this binary's exact, executable set.
pub fn verify(dir: &Path) -> Result<()> {
    let manifest = fs::read(dir.join(MANIFEST))
        .with_context(|| format!("reading the hook-set manifest in {}", dir.display()))?;
    anyhow::ensure!(
        manifest == manifest_bytes(),
        "the hook-set manifest in {} is not this binary's",
        dir.display()
    );
    for (name, expected) in FILES {
        let path = dir.join(name);
        let actual = fs::read(&path).with_context(|| format!("reading hook {}", path.display()))?;
        anyhow::ensure!(
            actual == expected,
            "hook {} differs from this binary's",
            path.display()
        );
        #[cfg(unix)]
        if name.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt as _;
            anyhow::ensure!(
                fs::metadata(&path)?.permissions().mode() & 0o111 != 0,
                "hook {} is not executable",
                path.display()
            );
        }
    }
    Ok(())
}

/// Publish this binary's set beneath `root` unless it is already there, and return its directory.
/// A set is written to a temporary directory and renamed into place, so a reader never sees a
/// partial set. A damaged set is moved aside and published again.
pub fn ensure_installed(root: &Path) -> Result<PathBuf> {
    let dir = set_dir(root);
    if verify(&dir).is_ok() {
        return Ok(dir);
    }
    let sets = root.join(SETS_DIR);
    fs::create_dir_all(&sets).with_context(|| format!("creating {}", sets.display()))?;
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    if dir.exists() {
        let aside = sets.join(format!(".damaged-{}-{unique}", set_id()));
        fs::rename(&dir, &aside)
            .with_context(|| format!("moving the damaged hook set {} aside", dir.display()))?;
    }
    let staging = sets.join(format!(".staging-{unique}"));
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let written = (|| -> Result<()> {
        for (name, bytes) in FILES {
            let path = staging.join(name);
            fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
        }
        fs::write(staging.join(MANIFEST), manifest_bytes())?;
        match fs::rename(&staging, &dir) {
            Ok(()) => Ok(()),
            // Another process published the same set first.
            Err(_) if verify(&dir).is_ok() => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("publishing the hook set {}", dir.display()))
            }
        }
    })();
    let _ = fs::remove_dir_all(&staging);
    written?;
    verify(&dir)?;
    Ok(dir)
}

/// The Claude settings every st3 Claude seat carries: each lifecycle event goes to
/// `claude-observe.sh` and the status line to `claude-statusline.sh`, both in `$ST_HOOKS`.
///
/// No SessionStart context is injected: an st3 seat takes no startup turn, and the native channel
/// delivers its messages. These hooks only externalize Claude's real turn and context state.
pub fn claude_settings_registration() -> serde_json::Value {
    fn observe(event: &str) -> serde_json::Value {
        serde_json::json!([{ "hooks": [{
            "type": "command",
            // Quoted so a state directory with whitespace still names one executable.
            "command": format!("\"$ST_HOOKS/claude-observe.sh\" {event}"),
        }] }])
    }
    serde_json::json!({
        "$schema": "https://json.schemastore.org/claude-code-settings.json",
        "hooks": {
            "SessionStart": observe("SessionStart"),
            "PreCompact": observe("PreCompact"),
            "PostCompact": observe("PostCompact"),
            "StopFailure": observe("StopFailure"),
            "UserPromptSubmit": observe("UserPromptSubmit"),
            "Stop": observe("Stop"),
            "PermissionRequest": observe("PermissionRequest"),
            "PreToolUse": observe("PreToolUse"),
            "PostToolUse": observe("PostToolUse"),
            // Subagents the seat runs, and the session end that ends them.
            "SubagentStart": observe("SubagentStart"),
            "SubagentStop": observe("SubagentStop"),
            "SessionEnd": observe("SessionEnd"),
        },
        // Claude's status line is the only channel that carries a context window. The slot is
        // single-valued, so the tee chains to the operator's own renderer.
        "statusLine": {
            "type": "command",
            "command": "\"$ST_HOOKS/claude-statusline.sh\"",
            "padding": 0,
            "refreshInterval": 5,
        }
    })
}

/// A Claude seat's private driver directory beneath the daemon's `drivers` directory: where its
/// hooks write the harness records and the `claude-native-session` binding.
pub fn claude_agent_dir(drivers_dir: &Path, subject: &str, host: &str) -> PathBuf {
    let native = drivers_dir
        .join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24])
        .join("observations");
    if native.exists() {
        native
    } else {
        legacy_claude_agent_dir(drivers_dir, subject, host)
    }
}

/// Existing providers keep these directories through binary replacement until ordinary restart.
pub fn legacy_claude_agent_dir(drivers_dir: &Path, subject: &str, host: &str) -> PathBuf {
    let identity = subject.strip_prefix("agent/").unwrap_or(subject);
    drivers_dir
        .join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24])
        .join("catalog")
        .join("agents")
        .join(host)
        .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16])
}

/// The file the SessionStart hook writes to bind a Claude seat's wrapper session to Claude's own
/// session, which is how st finds the seat's transcript.
pub const CLAUDE_BINDING_FILE: &str = "claude-native-session";

/// The native session the binding in `agent_dir` names for wrapper session `incarnation`, if any.
pub fn claude_binding(agent_dir: &Path, incarnation: &str) -> Option<String> {
    let bytes = fs::read(agent_dir.join(CLAUDE_BINDING_FILE)).ok()?;
    let binding: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    (binding["incarnation"].as_str() == Some(incarnation))
        .then(|| binding["native_session_id"].as_str().map(str::to_owned))
        .flatten()
        .filter(|id| !id.is_empty())
}

/// Whether `text` runs an `st2` program or names a path in st2's state directory. st3 seats must
/// never depend on either, because st2 is being removed from every machine.
pub fn mentions_st2_surface(text: &str) -> bool {
    let command = text
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ';' | '|' | '&' | '`'))
        .any(|token| token == "st2" || token.ends_with("/st2"));
    command || text.contains("/st2/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_is_idempotent_and_a_changed_set_is_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let first = ensure_installed(root.path()).unwrap();
        assert_eq!(first, set_dir(root.path()));
        assert!(first.starts_with(root.path().join("sets")));
        assert!(
            first
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("sha256-")
        );
        verify(&first).unwrap();
        assert_eq!(ensure_installed(root.path()).unwrap(), first);

        // An older set, as a running seat's `$ST_HOOKS` names it, is never touched.
        let older = root.path().join("sets/sha256-older");
        fs::create_dir_all(&older).unwrap();
        fs::write(older.join("claude-observe.sh"), "old").unwrap();
        ensure_installed(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(older.join("claude-observe.sh")).unwrap(),
            "old"
        );

        // A damaged set is republished rather than trusted.
        fs::write(first.join("claude-observe.sh"), "damaged").unwrap();
        assert!(verify(&first).is_err());
        assert_eq!(ensure_installed(root.path()).unwrap(), first);
        verify(&first).unwrap();
    }

    #[test]
    fn st2_does_not_mistake_an_st3_set_for_its_hook_root() {
        let root = tempfile::tempdir().unwrap();
        let set = ensure_installed(root.path()).unwrap();
        assert!(set.join(st_drivers::hooks::ST3_SET_MARKER).is_file());
        assert!(!set.join("manifest.json").exists());
    }

    #[test]
    fn no_hook_reaches_st2() {
        for (name, bytes) in FILES {
            if !name.ends_with(".sh") {
                continue;
            }
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(!mentions_st2_surface(text), "{name} reaches st2:\n{text}");
            assert!(
                text.contains("exec \"$ST3_BIN\" driver-hook "),
                "{name} does not hand its event to st3"
            );
            assert!(!text.contains("command -v"), "{name} looks a program up");
            assert!(!text.contains("boot ritual"), "{name} ships a boot ritual");
        }
        let settings = claude_settings_registration().to_string();
        assert!(!mentions_st2_surface(&settings));
        assert!(settings.contains("claude-observe.sh"));
        assert!(!settings.contains("claude-session-start.sh"));
    }

    #[test]
    fn the_st2_surface_check_finds_programs_and_state_paths() {
        assert!(mentions_st2_surface("exec st2 --catalog x driver"));
        assert!(mentions_st2_surface("command -v st2 >/dev/null"));
        assert!(mentions_st2_surface("\"/home/example/.local/bin/st2\" status"));
        assert!(mentions_st2_surface(
            "/home/example/.local/state/st2/hooks/sets/x"
        ));
        assert!(!mentions_st2_surface("ST_CLAUDE_SESSION=abc"));
        assert!(!mentions_st2_surface(
            "\"$ST3_BIN\" driver-hook claude-observe"
        ));
        assert!(!mentions_st2_surface(
            "/home/example/.local/state/st3/hooks/sets/x"
        ));
    }

    #[test]
    fn a_binding_counts_only_for_its_own_wrapper_session() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(claude_binding(dir.path(), "wrapper-1"), None);
        fs::write(
            dir.path().join(CLAUDE_BINDING_FILE),
            r#"{"incarnation":"wrapper-1","native_session_id":"native-1"}"#,
        )
        .unwrap();
        assert_eq!(
            claude_binding(dir.path(), "wrapper-1").as_deref(),
            Some("native-1")
        );
        assert_eq!(claude_binding(dir.path(), "wrapper-2"), None);
    }
}
