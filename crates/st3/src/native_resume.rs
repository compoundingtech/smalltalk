//! A driver's half of resuming a suspended seat: relaunch the harness on exactly the native
//! session the seat suspended on, or refuse with a typed reason.
//!
//! The daemon names the session in [`crate::suspension::RESUME_ENV`]. Each harness has its own
//! selector, and each refuses rather than letting the harness pick another session: a missing
//! transcript, or a selector the declaration already authored, ends the launch before the harness
//! starts. The driver then reports the session the harness actually bound, and the daemon compares
//! the two.

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};

/// Why a driver cannot relaunch the named session. `code` is stable; `reason` is for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub reason: String,
}

impl Refusal {
    pub fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// The native session this launch must resume, if the daemon named one.
pub fn requested() -> Option<String> {
    std::env::var(crate::suspension::RESUME_ENV)
        .ok()
        .filter(|id| !id.trim().is_empty())
}

fn valid_id(id: &str) -> Result<(), Refusal> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('-') || id.contains("..") {
        return Err(Refusal::new(
            "invalid-session-id",
            format!("{id:?} is not a native session ID"),
        ));
    }
    Ok(())
}

/// Whether the authored argv, before any `--`, already picks a session.
fn authors_selection(argv: &[String], flags: &[&str]) -> Option<String> {
    argv.iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .find(|argument| {
            flags
                .iter()
                .any(|flag| argument.as_str() == *flag || argument.starts_with(&format!("{flag}=")))
        })
        .cloned()
}

fn refuse_authored(argv: &[String], flags: &[&str]) -> Result<(), Refusal> {
    match authors_selection(argv, flags) {
        Some(flag) => Err(Refusal::new(
            "authored-session-selection",
            format!("the seat declares its own session selection ({flag}), so st cannot resume"),
        )),
        None => Ok(()),
    }
}

fn insert_after_program(mut argv: Vec<String>, arguments: &[&str]) -> Vec<String> {
    argv.splice(1..1, arguments.iter().map(|item| (*item).to_owned()));
    argv
}

/// Claude's config directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

/// Claude keeps a project's transcripts in a directory named after its path, with every
/// character other than an ASCII letter or digit replaced by `-`.
pub fn claude_transcript(home: &Path, workspace: &Path, id: &str) -> PathBuf {
    let project: String = workspace
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    home.join("projects")
        .join(project)
        .join(format!("{id}.jsonl"))
}

/// `claude --resume ID` for the workspace's own transcript of `id` under Claude's config
/// directory `home` ([`claude_home`]).
pub fn claude_argv(
    argv: Vec<String>,
    id: &str,
    workspace: &Path,
    home: Option<&Path>,
) -> Result<Vec<String>, Refusal> {
    valid_id(id)?;
    refuse_authored(
        &argv,
        &[
            "-c",
            "--continue",
            "-r",
            "--resume",
            "--session-id",
            "--fork-session",
            "--from-pr",
            "--teleport",
        ],
    )?;
    let home =
        home.ok_or_else(|| Refusal::new("transcript-missing", "Claude has no config directory"))?;
    let transcript = claude_transcript(home, workspace, id);
    if !transcript.is_file() {
        return Err(Refusal::new(
            "transcript-missing",
            format!(
                "Claude session {id} has no transcript at {}",
                transcript.display()
            ),
        ));
    }
    Ok(insert_after_program(argv, &["--resume", id]))
}

/// The session a pi-family transcript names in its header, which is authoritative over its file
/// name. A title line may precede the header.
fn pi_family_header_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(2) {
        let value: serde_json::Value = serde_json::from_str(&line.ok()?).ok()?;
        if value["type"] == "session" {
            return value["id"].as_str().map(str::to_owned);
        }
    }
    None
}

/// The seat's own transcript of pi-family session `id`: `<time>_<id>.jsonl` in its private
/// session directory, whose header names the same session.
pub fn pi_family_transcript(agent_dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("_{id}.jsonl");
    fs::read_dir(agent_dir.join("provider-sessions"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&suffix))
        })
        .find(|path| pi_family_header_id(path).as_deref() == Some(id))
}

/// pi resumes by transcript path, omp by session ID. pi silently starts a new session at a
/// path that does not exist, so both check the transcript first.
pub fn pi_family_argv(
    driver: &str,
    argv: Vec<String>,
    agent_dir: &Path,
    id: &str,
) -> Result<Vec<String>, Refusal> {
    valid_id(id)?;
    refuse_authored(
        &argv,
        &[
            "-c",
            "--continue",
            "-r",
            "--resume",
            "--session",
            "--session-id",
            "--fork",
            "--no-session",
        ],
    )?;
    let transcript = pi_family_transcript(agent_dir, id).ok_or_else(|| {
        Refusal::new(
            "transcript-missing",
            format!(
                "{driver} session {id} has no transcript in {}",
                agent_dir.join("provider-sessions").display()
            ),
        )
    })?;
    Ok(match driver {
        "omp" => insert_after_program(argv, &["--resume", id]),
        _ => insert_after_program(argv, &["--session", &transcript.to_string_lossy()]),
    })
}

/// OpenCode's data directory: `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode`.
pub fn opencode_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|data| data.join("opencode"))
}

/// `opencode --session ID`. Where OpenCode keeps its sessions in its database, the session must
/// be there; the driver also binds it only once the server confirms it.
pub fn opencode_argv(
    argv: Vec<String>,
    id: &str,
    data_dir: Option<&Path>,
) -> Result<Vec<String>, Refusal> {
    valid_id(id)?;
    refuse_authored(&argv, &["-c", "--continue", "-s", "--session", "--fork"])?;
    if let Some(database) = data_dir
        .map(|dir| dir.join("opencode.db"))
        .filter(|path| path.is_file())
    {
        let found = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|connection| {
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )
        })
        .map_err(|error| {
            Refusal::new(
                "transcript-unreadable",
                format!(
                    "OpenCode sessions in {} are unreadable: {error}",
                    database.display()
                ),
            )
        })?;
        if !found {
            return Err(Refusal::new(
                "transcript-missing",
                format!("OpenCode session {id} is not in {}", database.display()),
            ));
        }
    }
    Ok(insert_after_program(argv, &["--session", id]))
}

/// The session an OpenCode seat's driver bound, from its record in the agent directory.
pub fn opencode_bound_session(agent_dir: &Path) -> Option<String> {
    let bytes = fs::read(agent_dir.join(st_drivers::opencode_session::NATIVE_SESSION_FILE)).ok()?;
    let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    record["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Codex resumes a thread from its rollout, and its app-server refuses a thread it has no rollout
/// for; the driver reports that refusal. A rollout walk here could miss a thread among many
/// rollouts, so it is not a precondition.
pub fn codex_check(argv: &[String], id: &str) -> Result<(), Refusal> {
    valid_id(id)?;
    if argv
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| matches!(argument.as_str(), "resume" | "fork"))
    {
        return Err(Refusal::new(
            "authored-session-selection",
            "the seat declares its own Codex thread selection, so st cannot resume",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn claude_resumes_only_its_workspace_transcript() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Path::new("/work/seat.one");
        let home = Some(root.path());
        let missing = claude_argv(argv(&["claude", "--model", "x"]), "abc", workspace, home);
        assert_eq!(missing.unwrap_err().code, "transcript-missing");
        let transcript = claude_transcript(root.path(), workspace, "abc");
        assert!(transcript.ends_with("projects/-work-seat-one/abc.jsonl"));
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "{}\n").unwrap();
        assert_eq!(
            claude_argv(argv(&["claude", "--model", "x"]), "abc", workspace, home).unwrap(),
            argv(&["claude", "--resume", "abc", "--model", "x"])
        );
        assert_eq!(
            claude_argv(argv(&["claude", "--continue"]), "abc", workspace, home)
                .unwrap_err()
                .code,
            "authored-session-selection"
        );
        assert_eq!(
            claude_argv(argv(&["claude"]), "../x", workspace, home)
                .unwrap_err()
                .code,
            "invalid-session-id"
        );
    }

    #[test]
    fn pi_family_resume_needs_a_transcript_whose_header_names_the_session() {
        let agent = tempfile::tempdir().unwrap();
        let sessions = agent.path().join("provider-sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("2026-10-02_one.jsonl"),
            "{\"type\":\"session\",\"id\":\"other\"}\n",
        )
        .unwrap();
        assert_eq!(
            pi_family_argv("pi", argv(&["pi"]), agent.path(), "one")
                .unwrap_err()
                .code,
            "transcript-missing"
        );
        let path = sessions.join("2026-10-02_two.jsonl");
        fs::write(
            &path,
            "{\"type\":\"title\"}\n{\"type\":\"session\",\"id\":\"two\"}\n",
        )
        .unwrap();
        assert_eq!(
            pi_family_argv("pi", argv(&["pi", "-e", "x"]), agent.path(), "two").unwrap(),
            argv(&["pi", "--session", &path.to_string_lossy(), "-e", "x"])
        );
        assert_eq!(
            pi_family_argv("omp", argv(&["omp"]), agent.path(), "two").unwrap(),
            argv(&["omp", "--resume", "two"])
        );
        assert_eq!(
            pi_family_argv("omp", argv(&["omp", "--no-session"]), agent.path(), "two")
                .unwrap_err()
                .code,
            "authored-session-selection"
        );
    }

    #[test]
    fn opencode_and_codex_refuse_an_authored_selection() {
        assert_eq!(
            opencode_argv(argv(&["opencode", "--model", "m"]), "ses_1", None).unwrap(),
            argv(&["opencode", "--session", "ses_1", "--model", "m"])
        );
        assert_eq!(
            opencode_argv(argv(&["opencode", "-s", "ses_2"]), "ses_1", None)
                .unwrap_err()
                .code,
            "authored-session-selection"
        );
        let data = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(data.path().join("opencode.db")).unwrap();
        connection
            .execute_batch("CREATE TABLE session (id TEXT); INSERT INTO session VALUES ('ses_1');")
            .unwrap();
        assert!(opencode_argv(argv(&["opencode"]), "ses_1", Some(data.path())).is_ok());
        assert_eq!(
            opencode_argv(argv(&["opencode"]), "ses_9", Some(data.path()))
                .unwrap_err()
                .code,
            "transcript-missing"
        );
        assert_eq!(
            codex_check(&argv(&["codex", "resume", "t"]), "t")
                .unwrap_err()
                .code,
            "authored-session-selection"
        );
    }
}
