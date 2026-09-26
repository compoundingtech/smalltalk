//! Workspace pre-trust (R12) for an explicit provider runtime or operator command. The eval runner
//! and the reconciler never infer a provider from an opaque command.
//!
//! The claude config is a JSON object at `$CLAUDE_CONFIG_DIR/.claude.json` (else `$HOME/.claude.json`)
//! whose `projects` map is keyed by absolute workspace path. A workspace is trusted when its entry has
//! `"hasTrustDialogAccepted": true`; st2 also sets `"hasCompletedProjectOnboarding": true` to skip
//! onboarding friction. We merge into any existing entry and never clobber the
//! other per-project fields — and write ALL requested dirs in ONE atomic read-modify-write.
//!
//! A booted Claude process periodically flushes `.claude.json`. One explicit batch prevents sibling
//! trust writes from losing updates.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::flock::{self, FileLock};

/// The claude config file: `$CLAUDE_CONFIG_DIR/.claude.json` if set, else `$HOME/.claude.json`.
fn config_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        Ok(PathBuf::from(dir).join(".claude.json"))
    } else {
        let home =
            std::env::var_os("HOME").context("neither $CLAUDE_CONFIG_DIR nor $HOME is set")?;
        Ok(PathBuf::from(home).join(".claude.json"))
    }
}

/// Pre-trust `dirs` for BOTH harnesses — claude (`~/.claude.json`) AND codex
/// (`~/.codex/config.toml`). Writing the other harness's entry is harmless because each harness
/// reads only its own config. Returns the number of dirs written.
pub fn pretrust(dirs: &[PathBuf]) -> Result<usize> {
    let n = pretrust_at(&config_path()?, dirs)?;
    // Codex has its OWN "Do you trust this directory?" prompt (confirmed: bypass-approvals does not
    // skip it), stored separately — mark the dirs trusted there too so a codex seat never hangs.
    pretrust_codex_at(&codex_config_path()?, dirs)?;
    Ok(n)
}

/// Admit workspaces for Claude in the provider runtime's selected config. A typed Claude driver
/// calls this after task environment selection and before it starts Claude.
pub fn pretrust_claude(dirs: &[PathBuf]) -> Result<usize> {
    pretrust_at(&config_path()?, dirs)
}

/// The codex config file: `$CODEX_HOME/config.toml` if set, else `~/.codex/config.toml`.
fn codex_config_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        Ok(PathBuf::from(dir).join("config.toml"))
    } else {
        let home = std::env::var_os("HOME").context("neither $CODEX_HOME nor $HOME is set")?;
        Ok(PathBuf::from(home).join(".codex").join("config.toml"))
    }
}

/// Mark each dir trusted in codex's `config.toml`: a `[projects."<abs-dir>"]` table with
/// `trust_level = "trusted"`. Preserves the existing config by APPENDING the table only when absent
/// (a full TOML round-trip would drop the user's comments/formatting), and is idempotent — a dir
/// already `trusted` is skipped. Returns dirs newly written.
pub fn pretrust_codex_at(config: &Path, dirs: &[PathBuf]) -> Result<usize> {
    let existing = std::fs::read_to_string(config).unwrap_or_default();
    // Parse read-only just to see which dirs are already trusted (never rewrites the file).
    let parsed: toml::Value =
        toml::from_str(&existing).unwrap_or(toml::Value::Table(Default::default()));
    let already = |dir: &str| -> bool {
        parsed
            .get("projects")
            .and_then(|p| p.get(dir))
            .and_then(|e| e.get("trust_level"))
            .and_then(|t| t.as_str())
            == Some("trusted")
    };

    let mut appended = String::new();
    let mut n = 0;
    for dir in dirs {
        let key = canonical_key(dir);
        if already(&key) {
            continue;
        }
        appended.push_str(&format!(
            "\n[projects.{}]\ntrust_level = \"trusted\"\n",
            toml_key(&key)
        ));
        n += 1;
    }
    if !appended.is_empty() {
        let mut out = existing;
        out.push_str(&appended);
        write_atomic_str(config, &out)?;
    }
    Ok(n)
}

/// Quote a path as a TOML basic string (escape `\` and `"`), for a `[projects."<dir>"]` header.
fn toml_key(dir: &str) -> String {
    format!("\"{}\"", dir.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The core, taking the config path explicitly so it is testable without touching the real config or
/// the process environment. Idempotent: re-trusting an already-trusted dir is a no-op merge.
pub fn pretrust_at(config: &Path, dirs: &[PathBuf]) -> Result<usize> {
    let _lock = ConfigLock::acquire(config)?;
    // Read the existing config, or start from an empty object if it is absent/blank.
    let mut root: Value = match std::fs::read_to_string(config) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", config.display()))?
        }
        _ => json!({}),
    };
    let obj = root
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", config.display()))?;
    let projects = obj
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`projects` is not a JSON object")?;

    for dir in dirs {
        let key = canonical_key(dir);
        let entry = projects
            .entry(key)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("a `projects` entry is not a JSON object")?;
        // Merge — set only the trust flags and preserve every other field Claude wrote.
        entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
        entry.insert("hasCompletedProjectOnboarding".into(), Value::Bool(true));
    }

    write_atomic(config, &root)?;
    Ok(dirs.len())
}

/// The writer lock for one foreign harness config. Read only by `Drop`: the guard's job is to
/// outlive the read-merge-publish cycle, and closing the descriptor is what releases it.
struct ConfigLock {
    _held: FileLock,
}

impl ConfigLock {
    fn acquire(config: &Path) -> Result<Self> {
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut path = config.as_os_str().to_owned();
        path.push(".st2trust.lock");
        let path = PathBuf::from(path);
        let file = flock::open(&path, flock::Open::Create)
            .with_context(|| format!("opening {}", path.display()))?;
        let held = FileLock::hold_blocking(file, flock::Mode::Exclusive)
            .map_err(|error| anyhow::anyhow!("locking {}: {error}", path.display()))?;
        Ok(Self { _held: held })
    }
}

/// The absolute path claude keys a project by: the canonical (symlink-resolved) path when the dir
/// exists — matching `getcwd` for a booted agent whose cwd is this workspace — else the dir made
/// absolute against the current directory, else the path verbatim.
fn canonical_key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .or_else(|_| std::env::current_dir().map(|c| c.join(dir)))
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Write `value` to `config` atomically, so a crashed write never corrupts the real config.
fn write_atomic(config: &Path, value: &Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(value).context("serializing claude config")?;
    write_atomic_str(config, &rendered)
}

/// Stage-and-rename `contents` over `config`. The staging name carries the pid so concurrent
/// pretrusts do not collide.
///
/// Deliberately NOT the shared `fsatomic` primitive, and the reason is the same one that keeps
/// this module out of it: these are files st2 does not own — `~/.claude.json` and
/// `~/.codex/config.toml` — so the staged file must inherit the umask a harness's own config
/// carries rather than st2's `0600`, and the staging path must sit beside the config under a name
/// its owner will recognize.
fn write_atomic_str(config: &Path, contents: &str) -> Result<()> {
    let mut tmp = config.as_os_str().to_owned();
    tmp.push(format!(".st2trust.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, config)
        .with_context(|| format!("renaming {} into {}", tmp.display(), config.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretrust_creates_a_missing_config_and_trusts_the_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".claude.json");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let n = pretrust_at(&cfg, std::slice::from_ref(&ws)).unwrap();
        assert_eq!(n, 1);

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let key = std::fs::canonicalize(&ws)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], json!(true));
        assert_eq!(
            v["projects"][&key]["hasCompletedProjectOnboarding"],
            json!(true)
        );
    }

    #[test]
    fn pretrust_merges_into_an_existing_entry_without_clobbering_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".claude.json");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let key = std::fs::canonicalize(&ws)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // Seed a config with a top-level key AND an existing project entry carrying extra fields.
        let seed = json!({
            "oauthAccount": { "keep": "me" },
            "projects": {
                &key: { "lastCost": 1.23, "hasTrustDialogAccepted": false },
                "/other/dir": { "hasTrustDialogAccepted": true }
            }
        });
        std::fs::write(&cfg, serde_json::to_string(&seed).unwrap()).unwrap();

        pretrust_at(&cfg, std::slice::from_ref(&ws)).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // The trust flag flipped true; the sibling field and the other top-level/project keys survive.
        assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], json!(true));
        assert_eq!(
            v["projects"][&key]["hasCompletedProjectOnboarding"],
            json!(true)
        );
        assert_eq!(
            v["projects"][&key]["lastCost"],
            json!(1.23),
            "existing field clobbered"
        );
        assert_eq!(
            v["oauthAccount"]["keep"],
            json!("me"),
            "top-level key clobbered"
        );
        assert_eq!(
            v["projects"]["/other/dir"]["hasTrustDialogAccepted"],
            json!(true),
            "sibling clobbered"
        );
    }

    #[test]
    fn pretrust_codex_appends_trust_preserving_config_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let key = std::fs::canonicalize(&ws)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // Existing codex config with a COMMENT and an unrelated project entry — must survive.
        std::fs::write(&cfg, "# my codex config\nmodel = \"gpt-5\"\n\n[projects.\"/other\"]\ntrust_level = \"trusted\"\n").unwrap();

        assert_eq!(
            pretrust_codex_at(&cfg, std::slice::from_ref(&ws)).unwrap(),
            1
        );
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# my codex config"), "comment clobbered");
        assert!(
            text.contains("model = \"gpt-5\""),
            "top-level setting clobbered"
        );
        assert!(
            text.contains("[projects.\"/other\"]"),
            "existing project clobbered"
        );
        // The new dir is trusted, and the whole file still parses as TOML.
        let v: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(v["projects"][&key]["trust_level"].as_str(), Some("trusted"));

        // Idempotent: re-trusting writes nothing (already trusted) and doesn't duplicate the table.
        assert_eq!(
            pretrust_codex_at(&cfg, std::slice::from_ref(&ws)).unwrap(),
            0
        );
        let again = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(
            again
                .matches(&format!("[projects.{}]", toml_key(&key)))
                .count(),
            1,
            "duplicated table"
        );
    }

    #[test]
    fn pretrust_is_batch_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".claude.json");
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        // Batch: both dirs in one write.
        assert_eq!(pretrust_at(&cfg, &[a.clone(), b.clone()]).unwrap(), 2);
        // Idempotent: re-trusting is a clean no-op merge (still trusted, no error).
        pretrust_at(&cfg, std::slice::from_ref(&a)).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        for d in [&a, &b] {
            let key = std::fs::canonicalize(d)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], json!(true));
        }
    }

    /// The transport hardened this lock file with `O_NOFOLLOW`; nothing pinned it before.
    #[test]
    fn a_symlinked_trust_lock_is_refused_instead_of_locking_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join(".claude.json");
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(".claude.json.st2trust.lock"))
            .unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let error = pretrust_at(&config, std::slice::from_ref(&workspace)).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::ELOOP),
            "a symlinked trust lock must be refused, got {error:#}"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
        assert!(!config.exists(), "a refused lock must publish nothing");
    }

    #[test]
    fn the_trust_lock_is_created_private_and_excludes_a_second_holder() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join(".claude.json");
        let lock_path = tmp.path().join(".claude.json.st2trust.lock");
        let held = ConfigLock::acquire(&config).unwrap();
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // `flock` locks the open file description, so a second open in this process contends.
        let probe = || {
            let file = flock::open(&lock_path, flock::Open::Existing).unwrap();
            FileLock::hold(file, flock::Mode::Exclusive, flock::Wait::Now)
                .unwrap()
                .is_some()
        };
        assert!(
            !probe(),
            "a live trust-lock holder must exclude a second writer"
        );
        drop(held);
        assert!(probe(), "dropping the guard must release the trust lock");
    }
}
