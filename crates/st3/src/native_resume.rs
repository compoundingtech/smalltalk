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

/// An explicitly empty resume selector means fresh context, not ordinary continuation.
/// This uses the same launch-only selector as strict resume; no third selection channel exists.
pub fn fresh() -> bool {
    std::env::var_os(crate::suspension::RESUME_ENV).is_some_and(|value| value.is_empty())
}

/// Remove authored native selectors for an explicit restart, preserving literal arguments.
pub fn strip_selection(harness: &str, mut argv: Vec<String>) -> Vec<String> {
        let mut program = true;
        let mut literal = false;
        let mut selection_value = false;
        let mut preserve_value = false;
        let mut image_values = false;
        argv.retain(|arg| {
            if program {
                program = false;
                return true;
            }
            if literal {
                return true;
            }
            if preserve_value {
                preserve_value = false;
                return true;
            }
            if image_values {
                if !arg.starts_with('-') {
                    return true;
                }
                image_values = false;
            }
            if selection_value {
                selection_value = false;
                if !arg.starts_with('-') {
                    return false;
                }
            }
            if arg == "--" {
                literal = true;
                return true;
            }
            let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            // Codex's image option is variadic. Its launch parser requires a `--` prompt
            // boundary for automatic resume, so image paths must remain opaque up to it.
            if harness == "codex" && matches!(name, "--image" | "-i") {
                image_values = true;
                return true;
            }
            // Keep values opaque, even when prompt text or a model name happens to spell a
            // session selector. Provider-specific short flags have different meanings.
            let takes_unrelated_value = matches!(
                name,
                "--model"
                    | "--system-prompt"
                    | "--append-system-prompt"
                    | "--system-prompt-file"
                    | "--append-system-prompt-file"
                    | "--mcp-config"
                    | "--settings"
                    | "--permission-mode"
                    | "--session-dir"
                    | "--provider"
                    | "--thinking"
                    | "--extension"
                    | "-e"
                    | "--config"
                    | "--profile"
                    | "--cd"
                    | "--sandbox"
                    | "--ask-for-approval"
                    | "--enable"
                    | "--disable"
                    | "--add-dir"
                    | "--remote"
                    | "--remote-auth"
                    | "--image"
                    | "-i"
                    | "-m"
                    | "--agent"
                    | "--prompt"
                    | "--port"
                    | "--hostname"
            ) || (harness == "codex"
                && matches!(
                    name,
                    "-c" | "-p"
                        | "-C"
                        | "-s"
                        | "-a"
                        | "--local-provider"
                        | "--remote-auth-token-env"
                ));
            if takes_unrelated_value {
                preserve_value = !arg.contains('=');
                return true;
            }
            let selection = match harness {
                "claude" => match name {
                    "-c" | "--continue" | "--fork-session" => Some(false),
                    "-r" | "--resume" | "--from-pr" | "--session-id" | "--teleport" => Some(true),
                    _ => None,
                },
                "omp" => match name {
                    "-c" | "--continue" | "--no-session" | "--fork" | "--from-claude"
                    | "--from-codex" => Some(false),
                    "-r" | "--resume" | "--session" | "--session-id" => Some(true),
                    _ => None,
                },
                "pi" => match name {
                    "-c" | "--continue" | "--no-session" | "--fork" => Some(false),
                    "-r" | "--resume" | "--session" | "--session-id" => Some(true),
                    _ => None,
                },
                "opencode" => match name {
                    "-c" | "--continue" | "--fork" => Some(false),
                    "-s" | "--session" => Some(true),
                    _ => None,
                },
                "codex" => match name {
                    "--last" | "--all" => Some(false),
                    "resume" | "fork" => Some(true),
                    _ => None,
                },
                _ => None,
            };
            if let Some(takes_value) = selection {
                selection_value = takes_value && !arg.contains('=');
                false
            } else {
                true
            }
        });
    argv
}


/// The native session this launch continues when it can, with where the harness kept it: every
/// relaunch of a seat other than a resume. A refusal here starts a new session instead.
pub fn continued() -> Option<(String, Option<PathBuf>)> {
    if std::env::var_os(crate::suspension::RESUME_ENV).is_some() {
        return None;
    }
    let session = std::env::var(crate::suspension::CONTINUE_ENV)
        .ok()
        .filter(|id| !id.trim().is_empty())?;
    let path = std::env::var_os(crate::suspension::CONTINUE_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    Some((session, path))
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

/// Session selectors shared by native resume and its request-time preflight.
fn selection_flags(driver: &str) -> &'static [&'static str] {
    match driver {
        "claude" => &[
            "-c",
            "--continue",
            "-r",
            "--resume",
            "--session-id",
            "--fork-session",
            "--from-pr",
            "--teleport",
        ],
        "pi" | "omp" => &[
            "-c",
            "--continue",
            "-r",
            "--resume",
            "--session",
            "--session-id",
            "--fork",
            "--no-session",
        ],
        "opencode" => &["-c", "--continue", "-s", "--session", "--fork"],
        "codex" => &["resume", "fork"],
        _ => &[],
    }
}

fn codex_selection(argv: &[String]) -> Option<String> {
    argv.iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .find(|argument| matches!(argument.as_str(), "resume" | "fork"))
        .cloned()
}

/// Check only the declared argv, without reading a native transcript or requiring rollout
/// support. This uses the same selectors that strict native resume refuses.
pub fn authored_session_selection(member: &crate::model::MemberSpec) -> Option<String> {
    let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
        return None;
    };
    let provider = argv
        .iter()
        .position(|argument| argument == "--")
        .map_or(argv.as_slice(), |index| &argv[index + 1..]);
    let driver = member.driver.as_deref()?;
    if driver == "codex" {
        codex_selection(provider)
    } else {
        authors_selection(provider, selection_flags(driver))
    }
}

/// The normalized authored selector, independent of unrelated launch arguments.
pub fn selection_scope(member: &crate::model::MemberSpec) -> Option<String> {
    let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
        return None;
    };
    let provider = argv.iter().position(|arg| arg == "--")
        .map_or(argv.as_slice(), |index| &argv[index + 1..]);
    selector_scope(member.driver.as_deref()?, provider)
}

pub fn selector_scope(driver: &str, argv: &[String]) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    let flags = selection_flags(driver);
    let mut selected = Vec::new();
    let mut args = argv.iter().skip(1).take_while(|arg| arg.as_str() != "--").peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = arg.split_once('=').map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
        if flags.contains(&flag) {
            let canonical = match flag {
                "-r" => "--resume",
                "-s" => "--session",
                "-c" if driver != "codex" => "--continue",
                _ => flag,
            };
            let takes_value = matches!(canonical, "--resume" | "--session" | "--session-id" | "--from-pr" | "--teleport" | "resume" | "fork");
            let value = inline.or_else(|| takes_value.then(|| args.peek().filter(|value| !value.starts_with('-')).map(|value| value.as_str())).flatten());
            selected.push((canonical.to_owned(), value.map(str::to_owned)));
            if inline.is_none() && value.is_some() {
                args.next();
            }
        }
    }
    (!selected.is_empty()).then(|| hex::encode(Sha256::digest(serde_json::to_vec(&selected).expect("selector strings serialize"))))
}

#[cfg(test)]
#[test]
fn authored_selector_scope_ignores_unrelated_launch_edits() {
    let argv = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
    let original = selector_scope("omp", &argv(&["omp", "--resume", "/transcript", "--model", "one"]));
    assert!(original.is_some());
    assert_eq!(original, selector_scope("omp", &argv(&["omp", "--model", "two", "--resume=/transcript"])));
    assert_ne!(original, selector_scope("omp", &argv(&["omp", "--resume", "/other"])));
    assert!(selector_scope("omp", &argv(&["omp", "--", "--resume", "/transcript"])).is_none());
    assert_eq!(selector_scope("omp", &argv(&["omp", "--continue", "one"])), selector_scope("omp", &argv(&["omp", "-c", "two"])));
    assert!(selector_scope("codex", &argv(&["codex", "fork", "thread"])).is_some());
}

/// Refuse authored session selectors before a rollout can stop the incumbent.
pub fn rollout_support(member: &crate::model::MemberSpec) -> Result<(), Refusal> {
    if !member.terminal {
        return Err(Refusal::new(
            "unsupported-rollout",
            "native rollout requires its PTY session",
        ));
    }
    let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
        return Err(Refusal::new(
            "unsupported-rollout",
            "rollout needs a typed native harness launch",
        ));
    };
    let provider = argv
        .iter()
        .position(|argument| argument == "--")
        .map_or(argv.as_slice(), |index| &argv[index + 1..]);
    match member.driver.as_deref() {
        Some("claude") => refuse_authored(provider, selection_flags("claude")),
        Some("pi" | "omp") => refuse_authored(provider, selection_flags("pi")),
        Some("opencode") => {
            refuse_authored(provider, selection_flags("opencode"))
        }
        Some("codex") => codex_check(provider, "rollout-preflight"),
        _ => Err(Refusal::new(
            "unsupported-rollout",
            "rollout needs a supported native harness",
        )),
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

/// Bring Claude's transcript of `id` from `from`, where the seat's earlier workspace kept it,
/// into this workspace's project directory, so a seat whose workspace changed continues its
/// conversation there. A transcript already in place is left alone.
pub fn claude_carry_transcript(
    id: &str,
    workspace: &Path,
    home: Option<&Path>,
    from: Option<&Path>,
) -> std::io::Result<bool> {
    let (Some(home), Some(from)) = (home, from) else {
        return Ok(false);
    };
    if valid_id(id).is_err()
        || from.file_name().and_then(|name| name.to_str()) != Some(&format!("{id}.jsonl"))
        || !from.is_file()
    {
        return Ok(false);
    }
    let transcript = claude_transcript(home, workspace, id);
    if transcript.exists() {
        return Ok(false);
    }
    if let Some(parent) = transcript.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, &transcript)?;
    Ok(true)
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
    refuse_authored(&argv, selection_flags("claude"))?;
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
/// session directory `sessions`, whose header names the same session.
pub fn pi_family_transcript(sessions: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("_{id}.jsonl");
    fs::read_dir(sessions)
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

/// Make an authored pi/omp resume visible to the managed transcript inventory, without copying
/// a file the provider will keep appending to. `Ok(false)` means no link was needed.
#[cfg(unix)]
pub fn pi_family_link_transcript(argv: &[String], sessions: &Path) -> Result<bool, Refusal> {
    use std::os::unix::fs::{MetadataExt as _, symlink};

    let mut arguments = argv
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--");
    let mut selected = None;
    while let Some(argument) = arguments.next() {
        if argument == "--resume" {
            selected = arguments.next().map(String::as_str);
            break;
        }
        if let Some(path) = argument.strip_prefix("--resume=") {
            selected = Some(path);
            break;
        }
    }
    let Some(selected) = selected else {
        return Ok(false);
    };
    let transcript = Path::new(selected);
    if !transcript.is_absolute() {
        return Err(Refusal::new(
            "resume-path-relative",
            "authored resume is not an absolute path",
        ));
    }
    let source = fs::metadata(transcript)
        .map_err(|error| Refusal::new("transcript-missing", error.to_string()))?;
    if !source.is_file() {
        return Err(Refusal::new(
            "transcript-missing",
            "authored resume is not a file",
        ));
    }
    let filename = transcript.file_name().and_then(|name| name.to_str());
    let id = filename
        .and_then(|name| name.strip_suffix(".jsonl"))
        .and_then(|name| name.rsplit_once('_'))
        .map(|(_, id)| id)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or_else(|| Refusal::new("transcript-name-mismatch", "expected <time>_<uuid>.jsonl"))?;
    if pi_family_header_id(transcript).as_deref() != Some(id) {
        return Err(Refusal::new(
            "transcript-header-mismatch",
            "filename and session header disagree",
        ));
    }
    let parent = fs::canonicalize(transcript.parent().expect("absolute file has a parent"))
        .map_err(|error| Refusal::new("transcript-unreadable", error.to_string()))?;
    // Serialize starters on the containing directory, without adding inventory entries.
    use std::os::fd::AsRawFd as _;
    let container = sessions.parent().expect("managed directory has a parent");
    let lock = fs::File::open(container)
        .map_err(|error| Refusal::new("managed-directory-unreadable", error.to_string()))?;
    // SAFETY: lock owns a valid descriptor for the duration of the operation.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Refusal::new(
            "managed-directory-unreadable",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let mut migrate = false;
    let artifact_name = transcript.file_stem().expect("validated transcript filename");
    let artifacts = parent.join(artifact_name);
    match fs::symlink_metadata(sessions) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::canonicalize(sessions).ok().as_ref() != Some(&parent) {
                return Err(Refusal::new(
                    "managed-directory-foreign-link",
                    "managed directory links elsewhere",
                ));
            }
            migrate = true;
        }
        Ok(metadata) if metadata.is_dir() => {
            if fs::read_dir(sessions)
                .map_err(|error| Refusal::new("managed-directory-unreadable", error.to_string()))?
                .filter_map(Result::ok)
                .any(|entry| {
                    fs::metadata(entry.path()).is_ok_and(|target| {
                        target.dev() == source.dev() && target.ino() == source.ino()
                    })
                })
            {
                fs::create_dir_all(&artifacts)
                    .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
                let companion = sessions.join(artifact_name);
                return match fs::symlink_metadata(&companion) {
                    Ok(_) => Ok(false),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        symlink(&artifacts, &companion)
                            .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
                        Ok(true)
                    }
                    Err(error) => Err(Refusal::new("managed-directory-unreadable", error.to_string())),
                };
            }
            // remove_dir is the empty-directory check too: never recursively remove contents.
            fs::remove_dir(sessions)
                .map_err(|error| Refusal::new("managed-directory-not-empty", error.to_string()))?;
        }
        Ok(_) => {
            return Err(Refusal::new(
                "managed-directory-not-directory",
                "managed path is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Refusal::new(
                "managed-directory-unreadable",
                error.to_string(),
            ));
        }
    }
    let prepared = tempfile::Builder::new()
        .prefix(".transcript-inventory-")
        .tempdir_in(container)
        .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    let filename = transcript.file_name().expect("validated transcript filename");
    symlink(parent.join(filename), prepared.path().join(filename))
        .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    // Recursive artifact creation through a dangling symlink fails: prepare its target first.
    // create_dir_all uses the harness's default directory mode, subject to the process umask.
    fs::create_dir_all(&artifacts)
        .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    symlink(&artifacts, prepared.path().join(artifact_name))
        .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    if migrate {
        exchange_transcript_inventory(prepared.path(), sessions)
            .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
        // After exchange, this path is the old symlink, not the user's directory.
        fs::remove_file(prepared.path())
            .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    } else {
        fs::rename(prepared.path(), sessions)
            .map_err(|error| Refusal::new("managed-directory-link-failed", error.to_string()))?;
    }
    Ok(true)
}

/// Swap the prepared directory and legacy symlink without an absent-path window.
#[cfg(unix)]
fn exchange_transcript_inventory(prepared: &Path, sessions: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let prepared = CString::new(prepared.as_os_str().as_bytes())?;
    let sessions = CString::new(sessions.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    // SAFETY: both C strings are live, NUL-terminated paths; the syscall retains no pointers.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            prepared.as_ptr(),
            libc::AT_FDCWD,
            sessions.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both C strings are live, NUL-terminated paths; the syscall retains no pointers.
    let result = unsafe {
        libc::renamex_np(prepared.as_ptr(), sessions.as_ptr(), libc::RENAME_SWAP)
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = {
        let _ = (prepared, sessions);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic transcript inventory migration is unsupported on this platform",
        ));
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// pi resumes by transcript path, omp by session ID. pi silently starts a new session at a
/// path that does not exist, so both check the transcript first.
pub fn pi_family_argv(
    driver: &str,
    argv: Vec<String>,
    sessions: &Path,
    id: &str,
    path: Option<&Path>,
) -> Result<Vec<String>, Refusal> {
    valid_id(id)?;
    refuse_authored(&argv, selection_flags("pi"))?;
    let transcript = if let Some(path) = path {
        if !path.is_file() || pi_family_header_id(path).as_deref() != Some(id) {
            return Err(Refusal::new("transcript-missing", format!("{driver} captured transcript does not name session {id}: {}", path.display())));
        }
        path.to_path_buf()
    } else {
        pi_family_transcript(sessions, id).ok_or_else(|| {
            Refusal::new("transcript-missing", format!("{driver} session {id} has no transcript in {}", sessions.display()))
        })?
    };
    Ok(match driver {
        "omp" if path.is_some() => insert_after_program(argv, &["--resume", &transcript.to_string_lossy()]),
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
    refuse_authored(&argv, selection_flags("opencode"))?;
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
    if codex_selection(argv).is_some() {
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
    fn suspend_preflight_names_selectors_in_typed_launches() {
        for (driver, args, expected) in [
            ("claude", "\"--continue\"", Some("--continue")),
            (
                "claude",
                "\"--resume=native-copper\"",
                Some("--resume=native-copper"),
            ),
            (
                "pi",
                "\"--session\" \"/missing-transcript\"",
                Some("--session"),
            ),
            ("omp", "\"-c\"", Some("-c")),
            ("omp", "\"--no-session\"", Some("--no-session")),
            ("opencode", "\"-s\" \"native-copper\"", Some("-s")),
            ("codex", "\"resume\" \"native-copper\"", Some("resume")),
            ("codex", "\"fork\" \"native-copper\"", Some("fork")),
            ("omp", "\"--model\" \"copper\"", None),
            ("omp", "\"--\" \"--resume\"", None),
            ("codex", "\"--\" \"resume\"", None),
        ] {
            let source = format!(
                "version 2\nagent \"example\" {{ host \"amber\"; workspace \"/workspace\"; harness {driver:?} {{ args {args}; }} }}"
            );
            let member = crate::parse_intent(&source, "amber")
                .unwrap()
                .subjects
                .into_values()
                .find_map(|desired| desired.member)
                .unwrap();
            assert_eq!(
                authored_session_selection(&member).as_deref(),
                expected,
                "{driver}: {args}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pi_family_links_authored_transcripts_without_copying_or_replacing_inventory() {
        use std::io::Write as _;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        fs::create_dir(&legacy).unwrap();
        let id = "5f9a6e16-5e30-4bce-b327-9a8241321bd6";
        let transcript = legacy.join(format!("2026-10-04_{id}.jsonl"));
        fs::write(
            &transcript,
            format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n"),
        )
        .unwrap();
        let arguments = argv(&["omp", "--resume", transcript.to_str().unwrap()]);
        let original_arguments = arguments.clone();
        let managed = root.path().join("provider-sessions");
        // Absent, then already linked. Appending remains visible through both paths.
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        assert!(!fs::symlink_metadata(&managed).unwrap().file_type().is_symlink());
        fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap()
            .write_all(b"{\"turn\":2}\n")
            .unwrap();
        assert_eq!(
            fs::read(managed.join(transcript.file_name().unwrap())).unwrap(),
            fs::read(&transcript).unwrap()
        );
        assert_eq!(arguments, original_arguments);
        fs::remove_dir_all(&managed).unwrap();
        // Empty directory and the equals form.
        fs::create_dir(&managed).unwrap();
        assert!(
            pi_family_link_transcript(
                &argv(&["pi", &format!("--resume={}", transcript.display())]),
                &managed
            )
            .unwrap()
        );
        fs::remove_dir_all(&managed).unwrap();
        // An existing inventory with the same inode is left as a directory.
        fs::create_dir(&managed).unwrap();
        fs::hard_link(&transcript, managed.join(transcript.file_name().unwrap())).unwrap();
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        let alternate = managed.join(format!("earlier_{id}.jsonl"));
        fs::hard_link(&transcript, &alternate).unwrap();
        fs::remove_file(managed.join(transcript.file_name().unwrap())).unwrap();
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        assert!(
            !fs::symlink_metadata(&managed)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&alternate).unwrap();
        fs::remove_file(managed.join(transcript.file_stem().unwrap())).unwrap();
        fs::write(managed.join("other.jsonl"), "preserve me").unwrap();
        assert_eq!(
            pi_family_link_transcript(&arguments, &managed)
                .unwrap_err()
                .code,
            "managed-directory-not-empty"
        );
        assert_eq!(
            fs::read_to_string(managed.join("other.jsonl")).unwrap(),
            "preserve me"
        );
        fs::remove_file(managed.join("other.jsonl")).unwrap();
        fs::remove_dir(&managed).unwrap();
        // Foreign and dangling symlinks are never replaced.
        for target in [root.path().to_path_buf(), root.path().join("missing")] {
            symlink(&target, &managed).unwrap();
            assert_eq!(
                pi_family_link_transcript(&arguments, &managed)
                    .unwrap_err()
                    .code,
                "managed-directory-foreign-link"
            );
            assert_eq!(fs::read_link(&managed).unwrap(), target);
            fs::remove_file(&managed).unwrap();
        }
        // Invalid authored paths never prepare an inventory.
        assert_eq!(
            pi_family_link_transcript(&argv(&["omp", "--resume", "relative.jsonl"]), &managed)
                .unwrap_err()
                .code,
            "resume-path-relative"
        );
        let missing = legacy.join("missing.jsonl");
        assert_eq!(
            pi_family_link_transcript(
                &argv(&["omp", "--resume", missing.to_str().unwrap()]),
                &managed
            )
            .unwrap_err()
            .code,
            "transcript-missing"
        );
        let bad_name = legacy.join("not-a-uuid.jsonl");
        fs::write(&bad_name, "{}\n").unwrap();
        assert_eq!(
            pi_family_link_transcript(
                &argv(&["omp", "--resume", bad_name.to_str().unwrap()]),
                &managed
            )
            .unwrap_err()
            .code,
            "transcript-name-mismatch"
        );
        fs::write(&transcript, "{\"type\":\"session\",\"id\":\"other\"}\n").unwrap();
        assert_eq!(
            pi_family_link_transcript(&arguments, &managed)
                .unwrap_err()
                .code,
            "transcript-header-mismatch"
        );
        assert!(!managed.exists());
        assert!(!pi_family_link_transcript(&argv(&["omp"]), &managed).unwrap());
        assert!(
            !pi_family_link_transcript(&argv(&["omp", "--", "--resume", "relative"]), &managed)
                .unwrap()
        );
        // A title can precede the matching header; multi-session directories remain intact.
        fs::write(
            &transcript,
            format!("{{\"type\":\"title\"}}\n{{\"type\":\"session\",\"id\":\"{id}\"}}\n"),
        )
        .unwrap();
        fs::write(legacy.join("another.jsonl"), "another session").unwrap();
        fs::write(&managed, "not a directory").unwrap();
        assert_eq!(
            pi_family_link_transcript(&arguments, &managed)
                .unwrap_err()
                .code,
            "managed-directory-not-directory"
        );
        assert_eq!(fs::read_to_string(&managed).unwrap(), "not a directory");
        fs::remove_file(&managed).unwrap();
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        assert_eq!(
            pi_family_transcript(&managed, id).unwrap(),
            managed.join(transcript.file_name().unwrap())
        );
        assert!(!managed.join("another.jsonl").exists());
        assert_eq!(fs::read_to_string(legacy.join("another.jsonl")).unwrap(), "another session");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_pi_family_links_never_overwrite_each_other() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        fs::create_dir(&legacy).unwrap();
        let id = "5f9a6e16-5e30-4bce-b327-9a8241321bd6";
        let transcript = legacy.join(format!("time_{id}.jsonl"));
        fs::write(
            &transcript,
            format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n"),
        )
        .unwrap();
        let arguments = argv(&["omp", "--resume", transcript.to_str().unwrap()]);
        let managed = root.path().join("provider-sessions");
        fs::create_dir(&managed).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let starts: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        pi_family_link_transcript(&arguments, &managed)
                    })
                })
                .collect();
            for start in starts {
                if let Err(skip) = start.join().unwrap() {
                    assert!(matches!(
                        skip.code,
                        "managed-directory-not-empty"
                            | "managed-directory-link-failed"
                            | "managed-directory-unreadable"
                    ));
                }
            }
        });
        assert!(!fs::symlink_metadata(&managed).unwrap().file_type().is_symlink());
        assert_eq!(pi_family_header_id(&transcript).as_deref(), Some(id));
    }

    #[cfg(unix)]
    #[test]
    fn authored_transcripts_have_disjoint_inventories() {
        assert_private_authored_inventories(false);
    }

    #[cfg(unix)]
    #[test]
    fn authored_transcripts_migrate_legacy_shared_links() {
        assert_private_authored_inventories(true);
    }

    #[cfg(unix)]
    #[test]
    fn authored_transcript_first_artifact_write_through_inventory_and_repair_succeed() {
        let root = tempfile::tempdir().unwrap();
        let id = "5f9a6e16-5e30-4bce-b327-9a8241321bd6";
        let transcript = root.path().join(format!("time_{id}.jsonl"));
        fs::write(&transcript, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
        let arguments = argv(&["omp", "--resume", transcript.to_str().unwrap()]);
        let managed = root.path().join("managed");
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        let companion = managed.join(transcript.file_stem().unwrap());
        assert_eq!(fs::read_link(&companion).unwrap(), transcript.with_extension(""));
        assert!(companion.is_dir());
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        // ArtifactManager recursively creates this directory before its first write.
        fs::create_dir_all(&companion).unwrap();
        fs::write(companion.join("artifact"), "lazy artifact").unwrap();
        assert_eq!(fs::read_to_string(transcript.with_extension("").join("artifact")).unwrap(), "lazy artifact");
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        assert_eq!(fs::read_to_string(companion.join("artifact")).unwrap(), "lazy artifact");
        // Repair an inventory created by the previous version without replacing it.
        fs::remove_file(&companion).unwrap();
        fs::write(managed.join("preserve"), "keep").unwrap();
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        assert_eq!(fs::read_to_string(companion.join("artifact")).unwrap(), "lazy artifact");
        assert_eq!(fs::read_to_string(managed.join("preserve")).unwrap(), "keep");
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        // Repair a dangling companion from the earlier implementation too.
        fs::remove_file(companion.join("artifact")).unwrap();
        fs::remove_dir(transcript.with_extension("")).unwrap();
        assert!(!companion.exists());
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        fs::create_dir_all(&companion).unwrap();
        fs::write(companion.join("artifact"), "repaired target").unwrap();
        assert_eq!(fs::read_to_string(transcript.with_extension("").join("artifact")).unwrap(), "repaired target");
    }

    #[cfg(unix)]
    fn assert_private_authored_inventories(migrate: bool) {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let ids = [
            "5f9a6e16-5e30-4bce-b327-9a8241321bd6",
            "a5e213bc-c28e-42ba-a548-17aaf21ec32f",
        ];
        let transcripts: Vec<_> = ids.iter().enumerate().map(|(index, id)| {
            let path = shared.join(format!("2026-10-0{}_{id}.jsonl", index + 1));
            fs::write(&path, format!("{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"2026-10-0{}T00:00:00Z\",\"cwd\":\"/example\"}}\n", index + 1)).unwrap();
            fs::create_dir(path.with_extension("")).unwrap();
            fs::write(path.with_extension("").join("artifact"), id).unwrap();
            path
        }).collect();
        let inventories: Vec<_> = (0..2).map(|index| {
            let managed = root.path().join(format!("inventory-{migrate}-{index}"));
            if migrate { symlink(&shared, &managed).unwrap(); }
            let arguments = argv(&["omp", "--resume", transcripts[index].to_str().unwrap()]);
            assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
            assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
            assert!(!fs::symlink_metadata(&managed).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_dir(&managed).unwrap().count(), 2);
            assert!(pi_family_transcript(&managed, ids[index]).is_some());
            assert!(pi_family_transcript(&managed, ids[1 - index]).is_none());
            let latest = crate::external_sessions::find_managed_omp_transcript(&managed, 0)
                .unwrap().unwrap();
            assert_eq!(latest.native_id, ids[index]);
            assert_eq!(
                fs::read_to_string(managed.join(transcripts[index].file_stem().unwrap()).join("artifact")).unwrap(),
                ids[index],
            );
            managed
        }).collect();
        assert_ne!(fs::canonicalize(&inventories[0]).unwrap(), fs::canonicalize(&inventories[1]).unwrap());
        assert_eq!(fs::read_dir(&shared).unwrap().count(), 4);
        for (index, transcript) in transcripts.iter().enumerate() {
            assert_eq!(pi_family_header_id(transcript).as_deref(), Some(ids[index]));
            assert_eq!(fs::read_to_string(transcript.with_extension("").join("artifact")).unwrap(), ids[index]);
        }
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
        let explicit = strip_selection("claude", argv(&["claude", "--resume", "authored", "--fork-session", "--model", "x"]));
        assert_eq!(claude_argv(explicit.clone(), "abc", workspace, home).unwrap(),
            argv(&["claude", "--resume", "abc", "--model", "x"]));
        assert_eq!(claude_argv(explicit, "missing", workspace, home).unwrap_err().code, "transcript-missing");
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
    fn a_seat_whose_workspace_changed_carries_its_claude_transcript_along() {
        let root = tempfile::tempdir().unwrap();
        let home = Some(root.path());
        let before = Path::new("/work/first");
        let after = Path::new("/work/second");
        let earlier = claude_transcript(root.path(), before, "abc");
        fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        fs::write(&earlier, "{\"turn\":1}\n").unwrap();
        assert_eq!(
            claude_argv(argv(&["claude"]), "abc", after, home)
                .unwrap_err()
                .code,
            "transcript-missing"
        );
        // Only the session's own transcript is carried, and never over one already there.
        let other = earlier.with_file_name("other.jsonl");
        fs::write(&other, "{}\n").unwrap();
        assert!(!claude_carry_transcript("abc", after, home, Some(&other)).unwrap());
        assert!(!claude_carry_transcript("abc", after, home, None).unwrap());
        assert!(claude_carry_transcript("abc", after, home, Some(&earlier)).unwrap());
        assert_eq!(
            claude_argv(argv(&["claude"]), "abc", after, home).unwrap(),
            argv(&["claude", "--resume", "abc"])
        );
        let carried = claude_transcript(root.path(), after, "abc");
        fs::write(&carried, "{\"turn\":2}\n").unwrap();
        assert!(!claude_carry_transcript("abc", after, home, Some(&earlier)).unwrap());
        assert_eq!(fs::read_to_string(&carried).unwrap(), "{\"turn\":2}\n");
    }

    #[test]
    fn pi_family_resume_needs_a_transcript_whose_header_names_the_session() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("provider-sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("2026-10-02_one.jsonl"),
            "{\"type\":\"session\",\"id\":\"other\"}\n",
        )
        .unwrap();
        assert_eq!(
            pi_family_argv("pi", argv(&["pi"]), &sessions, "one", None)
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
            pi_family_argv("pi", argv(&["pi", "-e", "x"]), &sessions, "two", None).unwrap(),
            argv(&["pi", "--session", &path.to_string_lossy(), "-e", "x"])
        );
        assert_eq!(
            pi_family_argv("omp", argv(&["omp"]), &sessions, "two", None).unwrap(),
            argv(&["omp", "--resume", "two"])
        );
        assert_eq!(
            pi_family_argv("omp", argv(&["omp", "--no-session"]), &sessions, "two", None)
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

    #[test]
    fn explicit_selection_strips_authored_selectors_before_strict_resume() {
        for (harness, selection) in [
            (
                "claude",
                argv(&["--resume=old", "--fork-session", "--session-id", "other"]),
            ),
            ("omp", argv(&["-r", "old", "--continue", "--from-codex"])),
            ("pi", argv(&["--session", "old", "-r", "--continue"])),
            ("codex", argv(&["resume", "old", "--last"])),
            ("opencode", argv(&["-s", "old", "--continue", "--fork"])),
        ] {
            let mut input = argv(&[harness, "--model", "keep"]);
            input.extend(selection);
            input.extend(argv(&["--", "--resume", "literal prompt"]));
            assert_eq!(
                strip_selection(harness, input),
                argv(&[
                    harness,
                    "--model",
                    "keep",
                    "--",
                    "--resume",
                    "literal prompt"
                ])
            );
        }
    }
    #[test]
    fn explicit_selection_keeps_unrelated_option_values_opaque() {
        for (harness, input) in [
            (
                "claude",
                argv(&[
                    "claude",
                    "--system-prompt",
                    "--resume",
                    "--resume",
                    "old",
                    "--mcp-config",
                    "keep",
                ]),
            ),
            (
                "codex",
                argv(&[
                    "codex",
                    "--model",
                    "resume",
                    "-c",
                    "fork",
                    "resume",
                    "old",
                    "--sandbox",
                    "keep",
                ]),
            ),
            (
                "pi",
                argv(&[
                    "pi",
                    "--session-dir",
                    "/keep",
                    "--session",
                    "old",
                    "--model",
                    "keep",
                ]),
            ),
            (
                "omp",
                argv(&["omp", "--from-codex", "unrelated prompt", "--model", "keep"]),
            ),
        ] {
            let expected = match harness {
                "claude" => argv(&[
                    "claude",
                    "--system-prompt",
                    "--resume",
                    "--mcp-config",
                    "keep",
                ]),
                "codex" => argv(&[
                    "codex",
                    "--model",
                    "resume",
                    "-c",
                    "fork",
                    "--sandbox",
                    "keep",
                ]),
                "omp" => argv(&["omp", "unrelated prompt", "--model", "keep"]),
                _ => argv(&["pi", "--session-dir", "/keep", "--model", "keep"]),
            };
            assert_eq!(strip_selection(harness, input), expected);
        }
    }

    #[test]
    fn explicit_selection_keeps_codex_provider_auth_and_variadic_images_opaque() {
        let input = argv(&[
            "codex",
            "--local-provider",
            "resume",
            "--remote-auth-token-env",
            "fork",
            "--image",
            "resume",
            "fork",
            "--",
            "literal prompt",
        ]);
        assert_eq!(
            strip_selection("codex", input.clone()),
            input,
        );
    }

    #[test]
    fn captured_pi_path_is_authoritative_and_must_name_the_requested_session() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("captured.jsonl");
        fs::write(&path, "{\"type\":\"session\",\"id\":\"captured\"}\n").unwrap();
        let sessions = root.path().join("not-the-captured-directory");
        assert_eq!(pi_family_argv("pi", argv(&["pi"]), &sessions, "captured", Some(&path)).unwrap(),
            argv(&["pi", "--session", &path.to_string_lossy()]));
        assert_eq!(pi_family_argv("omp", argv(&["omp"]), &sessions, "captured", Some(&path)).unwrap(),
            argv(&["omp", "--resume", &path.to_string_lossy()]));
        assert_eq!(pi_family_argv("pi", argv(&["pi"]), &sessions, "foreign", Some(&path)).unwrap_err().code, "transcript-missing");
        fs::remove_file(&path).unwrap();
        assert_eq!(pi_family_argv("pi", argv(&["pi"]), &sessions, "captured", Some(&path)).unwrap_err().code, "transcript-missing");
    }

}
