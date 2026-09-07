//! Durable working-state context for lossless restarts.
//!
//! An agent's lossless-restart memory of "what it was mid-doing", so state survives a cold restart
//! (it ties to the boot hooks: on SessionStart an agent can rehydrate from here). Per agent, under
//! `<agent_dir>/resources/context/`:
//!   - `now.md` — the current working state (overwritten by `write`).
//!   - `decisions/` — an append-only log, one `<unix-ms>-<rand6>.md` file per entry (same grammar as
//!     the inbox, so lex order = chronological). Each file is a single bullet line:
//!     `- <ISO-8601> <decision>. why: <why>.`
//!
//! Every read tolerates a missing folder/file — absent → empty text. Writes are atomic (tmp+rename),
//! so a concurrent reader never sees a partial file and two appends never race (distinct filenames).
//! The stable file shape lets an agent recover after a process or host restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;

use crate::message;

const NOW_LOCK: &str = ".now.lock";

/// `<agent_dir>/resources/context`.
pub fn context_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("resources").join("context")
}

fn now_file(dir: &Path) -> PathBuf {
    dir.join("now.md")
}
fn decisions_dir(dir: &Path) -> PathBuf {
    dir.join("decisions")
}

/// Which view `read` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `now.md` — the current working state (default).
    Now,
    /// The concatenated decision log.
    Decisions,
    /// `now.md` then the decision log, heading-separated.
    Full,
}

/// Read a context view. Missing → empty string (never an error — a fresh agent has no context yet).
pub fn read(context_dir: &Path, view: View) -> String {
    match view {
        View::Now => read_if_present(&now_file(context_dir)),
        View::Decisions => read_decisions(context_dir),
        View::Full => {
            let now = read_if_present(&now_file(context_dir));
            let dec = read_decisions(context_dir);
            let mut parts: Vec<String> = Vec::new();
            if !now.is_empty() {
                parts.push(now.trim_end().to_string());
            }
            if !dec.is_empty() {
                parts.push("# decisions/".to_string());
                parts.push(dec.trim_end().to_string());
            }
            let joined = parts.join("\n");
            if joined.is_empty() {
                joined
            } else {
                format!("{joined}\n")
            }
        }
    }
}

/// Read `now.md` only when its mtime is younger than `max_age`. Missing, unreadable, or stale state
/// returns empty text so lifecycle hooks can safely omit it.
pub fn read_now_fresh(context_dir: &Path, max_age: Duration) -> String {
    let path = now_file(context_dir);
    let fresh = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < max_age);
    if fresh {
        read_if_present(&path)
    } else {
        String::new()
    }
}

/// Overwrite `now.md` with `content` (atomic). Creates the context dir.
///
/// Every writer takes the same lock used by [`write_now_if_blank`], so a conditional recovery
/// write cannot pass its predicate and then overwrite authored state that landed in between.
pub fn write_now(context_dir: &Path, content: &str) -> anyhow::Result<()> {
    let _lock = lock_now(context_dir)?;
    write_atomic(&now_file(context_dir), content)
}

/// Atomically replace an absent or successfully decoded whitespace-only `now.md`.
///
/// Read errors other than `NotFound` are preserved. Treating an unreadable file as empty would
/// overwrite state precisely when its contents cannot be proven blank.
pub fn write_now_if_blank(context_dir: &Path, content: &str) -> anyhow::Result<bool> {
    let _lock = lock_now(context_dir)?;
    let path = now_file(context_dir);
    match fs::read_to_string(&path) {
        Ok(current) if !current.trim().is_empty() => Ok(false),
        Ok(_) => {
            write_atomic(&path, content)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_atomic(&path, content)?;
            Ok(true)
        }
        Err(error) => Err(error).context("read current now.md before conditional write"),
    }
}

fn lock_now(context_dir: &Path) -> anyhow::Result<fs::File> {
    crate::harness_state::lock_exclusive(&context_dir.join(NOW_LOCK))
        .context("acquire now.md writer lock")
}

pub fn append_decision_to_dir(dir: &Path, decision: &str, why: &str) -> anyhow::Result<String> {
    let decision = decision.trim();
    let why = why.trim();
    if decision.is_empty() {
        anyhow::bail!("--decision is required and cannot be empty");
    }
    if why.is_empty() {
        anyhow::bail!("--why is required and cannot be empty");
    }
    if decision.contains('\n') || why.contains('\n') {
        anyhow::bail!(
            "context append: --decision and --why must be single lines (the log is a scannable list)"
        );
    }
    fs::create_dir_all(dir)?;
    let line = format!(
        "- {} {}. why: {}.\n",
        iso_utc_now(),
        trim_trailing_period(decision),
        trim_trailing_period(why)
    );
    let filename = message::new_filename();
    write_atomic(&dir.join(&filename), &line)?;
    Ok(filename)
}

/// Concatenate the decision entries (lex-sorted = chronological), each already a bullet line.
fn read_decisions(context_dir: &Path) -> String {
    let dir = decisions_dir(context_dir);
    let mut files: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
            .collect(),
        Err(_) => return String::new(),
    };
    files.sort();
    let lines: Vec<String> = files
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn read_if_present(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn trim_trailing_period(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

/// Atomic write: tmp sibling + rename.
fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".ctx.tmp-{}-{}",
        std::process::id(),
        message::now_ms()
    ));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Current time as an ISO-8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`), dependency-free (civil-from-days).
fn iso_utc_now() -> String {
    let secs = (message::now_ms() / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_context_reads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = context_dir(tmp.path());
        assert_eq!(read(&dir, View::Now), "");
        assert_eq!(read(&dir, View::Decisions), "");
        assert_eq!(read(&dir, View::Full), "");
    }

    #[test]
    fn write_then_read_now_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = context_dir(tmp.path());
        write_now(&dir, "mid-refactor: step 3 of 5\n").unwrap();
        assert_eq!(read(&dir, View::Now), "mid-refactor: step 3 of 5\n");
        assert_eq!(
            read_now_fresh(&dir, Duration::from_secs(60)),
            "mid-refactor: step 3 of 5\n"
        );
        assert_eq!(read_now_fresh(&dir, Duration::ZERO), "");
    }

    #[test]
    fn conditional_write_replaces_only_decoded_blank_state_and_preserves_read_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = context_dir(tmp.path());

        assert!(write_now_if_blank(&dir, "stub\n").unwrap());
        assert_eq!(read(&dir, View::Now), "stub\n");

        write_now(&dir, "authored\n").unwrap();
        assert!(!write_now_if_blank(&dir, "replacement\n").unwrap());
        assert_eq!(read(&dir, View::Now), "authored\n");

        std::fs::remove_file(now_file(&dir)).unwrap();
        std::fs::write(now_file(&dir), [0xff]).unwrap();
        let error = write_now_if_blank(&dir, "replacement\n")
            .expect_err("an undecodable now.md must not be classified as blank");
        assert!(
            error
                .to_string()
                .contains("read current now.md before conditional write"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(now_file(&dir)).unwrap(),
            [0xff],
            "undecodable state stays byte-for-byte intact"
        );

    }

    #[test]
    fn append_decisions_are_ordered_bullets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = context_dir(tmp.path());
        append_decision_to_dir(
            &decisions_dir(&dir),
            "use hook-enforced perms",
            "never prompts an autonomous pty",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        append_decision_to_dir(&decisions_dir(&dir), "defer shims", "scope enforcement is follow-on").unwrap();

        let dec = read(&dir, View::Decisions);
        let lines: Vec<&str> = dec.lines().collect();
        assert_eq!(lines.len(), 2);
        // Format + chronological order.
        assert!(
            lines[0].contains("use hook-enforced perms. why: never prompts an autonomous pty.")
        );
        assert!(lines[1].contains("defer shims. why: scope enforcement is follow-on."));
        assert!(lines[0].starts_with("- 2")); // ISO timestamp

        // Full = now.md + decisions heading.
        write_now(&dir, "current focus\n").unwrap();
        let full = read(&dir, View::Full);
        assert!(full.starts_with("current focus\n# decisions/"));
        assert!(full.contains("defer shims"));
    }

    #[test]
    fn append_rejects_empty_or_multiline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = context_dir(tmp.path());
        assert!(append_decision_to_dir(&decisions_dir(&dir), "", "why").is_err());
        assert!(append_decision_to_dir(&decisions_dir(&dir), "d", "").is_err());
        assert!(append_decision_to_dir(&decisions_dir(&dir), "line1\nline2", "why").is_err());
    }
}
