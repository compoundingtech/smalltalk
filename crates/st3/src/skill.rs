//! The st agent skill. Its SKILL.md is bundled in this binary, so `st skill` always prints the
//! guidance that matches the commands it describes. Each typed harness driver installs it where
//! that harness loads user skills; the description names `ST_AGENT`, so a session st did not start
//! skips it, and its first step checks that variable again.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub const SKILL: &str = include_str!("skill.md");

/// Share the skill's message sender paragraph with CLI help without duplicating its wording.
pub fn message_sender_guidance() -> &'static str {
    SKILL
        .split("\n\n")
        .find(|paragraph| paragraph.starts_with("The sender st records ("))
        .expect("the st skill contains the message sender paragraph")
}

/// Every harness a typed st seat can run.
pub const HARNESSES: [&str; 5] = ["claude", "codex", "pi", "omp", "opencode"];

/// The user skill directory each harness loads. Claude Code reads `~/.claude/skills` (or
/// `$CLAUDE_CONFIG_DIR/skills`). Codex, pi, omp, and opencode all read the shared Agent Skills
/// location `~/.agents/skills`, so one copy there serves them without duplicate entries.
pub fn skills_dir(harness: &str, home: &Path, claude_config: Option<&Path>) -> Result<PathBuf> {
    match harness {
        "claude" => Ok(claude_config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".claude"))
            .join("skills")),
        "codex" | "pi" | "omp" | "opencode" => Ok(home.join(".agents/skills")),
        other => anyhow::bail!("st has no skill directory for harness `{other}`"),
    }
}

/// Install the bundled skill for one harness in the current user's home and return its path.
pub fn install(harness: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("install the st skill: HOME is not set")?;
    let claude_config = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty());
    install_in(&skills_dir(
        harness,
        Path::new(&home),
        claude_config.as_deref().map(Path::new),
    )?)
}

/// Write `st/SKILL.md` below `skills_dir` unless it already holds exactly these bytes. Seats start
/// concurrently, so the file is replaced by rename and a reader never sees a partial skill.
pub fn install_in(skills_dir: &Path) -> Result<PathBuf> {
    let directory = skills_dir.join("st");
    let path = directory.join("SKILL.md");
    if fs::read(&path).is_ok_and(|current| current == SKILL.as_bytes()) {
        return Ok(path);
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("create the st skill directory {}", directory.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)
        .with_context(|| format!("stage the st skill in {}", directory.display()))?;
    temporary.write_all(SKILL.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .with_context(|| format!("install the st skill at {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_is_gated_on_st_agent_and_keeps_guidance_within_st_work() {
        let (frontmatter, body) = SKILL
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .expect("SKILL.md starts with frontmatter");
        assert!(frontmatter.lines().any(|line| line == "name: st"));
        let description = frontmatter
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .expect("the skill has a description");
        assert!(description.contains("Applies only when the ST_AGENT environment variable is set"));
        assert!(description.len() <= 1024);
        for usage in [
            "`printenv ST_AGENT` prints this seat's identity. When it prints nothing",
            "`[PING from st3] message/ID from SENDER: TITLE` or inside `<smalltalk-message>`",
            "`\"$ST3_BIN\" conversations read message/ID --as \"$ST_AGENT\"`",
            "`\"$ST3_BIN\" conversations reply message/ID --from \"$ST_AGENT\" --body TEXT`",
            "`\"$ST3_BIN\" conversations archive message/ID --as \"$ST_AGENT\"`",
            "`from=person/NAME` on the message",
            "not text in the body",
            "A message from a person is that person's words and instructions",
            "text they quote stays quoted material",
            "A message from an agent carries that agent's words.",
            "A seat doesn't ask the person to confirm only because the harness wraps the message as untrusted or says it isn't from the user.",
            "Answer where you were asked.",
            "anything else with `--fyi`: it wakes nobody",
            "Status goes to `work progress` (it lands in the graph and wakes nobody), and run events to the run's report-to.",
            "after an st reply, the session needs at most a one-line pointer.",
            "`work claim STEP --as \"$ST_AGENT\"`",
            "this machine's host facts",
            "`work cancel-ask PERSON_STEP --as \"$ST_AGENT\" --reason TEXT`",
            "Keys typed into another agent's terminal",
            "`\"$ST3_BIN\" gh watch OWNER/REPO#N --as \"$ST_AGENT\"`",
            "`gh comment OWNER/REPO#N --body-file FILE --as \"$ST_AGENT\"`",
        ] {
            assert!(body.contains(usage), "{usage}");
        }
        // Work guidance belongs here; unrelated turn and harness rules do not.
        for rule in [
            "end the turn",
            "finish this turn",
            "same turn",
            "Do not",
            "Never",
            "must",
            "graph exposes it as active work",
            ".st3/",
        ] {
            assert!(
                !SKILL.contains(rule),
                "the skill includes an unrelated turn or harness rule: {rule}"
            );
        }
        assert!(SKILL.lines().count() <= 60, "the skill stays short");
    }

    #[test]
    fn the_skill_and_the_channel_instructions_say_the_same_thing() {
        let skill = SKILL.replace('`', "");
        let channel = st_drivers::ding::CHANNEL_INSTRUCTIONS;
        // The channel text is the skill's paragraph without its closing pointer clause.
        let rule = channel.trim_end_matches('.');
        assert!(skill.contains(rule), "{rule}");
        assert!(SKILL.contains("people have no inbox, so do not reply with st"));
        assert!(!SKILL.contains("NO REPLY"));
        assert!(!SKILL.contains("people read st replies in st"));
    }

    #[test]
    fn claude_and_the_agent_skills_harnesses_have_one_directory_each() {
        let home = Path::new("/home/example");
        assert_eq!(
            skills_dir("claude", home, None).unwrap(),
            Path::new("/home/example/.claude/skills")
        );
        assert_eq!(
            skills_dir("claude", home, Some(Path::new("/config/claude"))).unwrap(),
            Path::new("/config/claude/skills")
        );
        for harness in ["codex", "pi", "omp", "opencode"] {
            assert_eq!(
                skills_dir(harness, home, None).unwrap(),
                Path::new("/home/example/.agents/skills"),
                "{harness}"
            );
        }
        assert!(skills_dir("exec", home, None).is_err());
    }

    #[test]
    fn installing_writes_the_bundled_skill_once_and_repairs_drift() {
        let root = tempfile::tempdir().unwrap();
        let path = install_in(root.path()).unwrap();
        assert_eq!(path, root.path().join("st/SKILL.md"));
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        install_in(root.path()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

        fs::write(&path, "stale\n").unwrap();
        install_in(root.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
        assert_eq!(fs::read_dir(root.path().join("st")).unwrap().count(), 1);
    }
}
