//! Calls Claude's auto mode classifier denied, which only the seat's person can allow.
//!
//! Auto mode refuses a call it judges unsafe without asking anyone: Claude records the call's
//! result as `Permission for this action was denied by the Claude Code auto mode classifier.
//! Reason: [...]` and the model is told not to work around it. The seat then waits on a person
//! nobody told. The `Stop` hook reads each new denial of the turn that just ended from the
//! session's transcript; st raises it as an alert for the seat's person, who may allow it once or
//! allow that kind of call for the seat. A `PreToolUse` hook decision of `allow` lets the next
//! matching call through. Only a call auto mode denied waits on a person: a seat in another mode,
//! or a call a harness rule refused, raises nothing.
//!
//! The ledger beside the seat's state record holds what the `PreToolUse` hook needs to match a
//! call: its tool, a digest of its input, and its kind (for a shell command, its first two words,
//! such as `git checkout`). The input itself stays in the conversation.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::harness_state;

const LEDGER_FILE: &str = "claude-denials.json";
const LOCK_FILE: &str = "claude-denials.lock";
/// The permission mode Claude Code records while auto mode's classifier judges each call.
const AUTO_MODE: &str = "auto";
/// The words Claude Code 2.1.296 opens a classifier denial's tool result with.
const DENIED: &str = "Permission for this action was denied by the Claude Code auto mode classifier.";
/// The most denials the ledger keeps: an old one nobody answered gives way to newer ones.
const KEPT: usize = 16;

/// A call the classifier denied, as the session's transcript records it.
#[derive(Debug, Clone, PartialEq)]
pub struct Denial {
    /// The tool use's id, which the seat's conversation shows the call under.
    pub call_id: String,
    pub tool: String,
    pub input: Value,
    /// The classifier's reason, such as `Irreversible Local Destruction`.
    pub reason: String,
}

/// One denial the hook reported, and what the person answered once it is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub call_id: String,
    pub tool: String,
    pub kind: String,
    pub digest: String,
    /// The st claim that raised it.
    pub claim: Option<String>,
    /// `approve-once`, `approve-kind` or `dismiss`, once read from st.
    pub answer: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// Where the transcript was last read to, per session.
    cursor: Option<(String, u64)>,
    /// The session's permission mode where the transcript was last read to.
    #[serde(default)]
    mode: Option<String>,
    entries: Vec<Entry>,
}

fn ledger_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(LEDGER_FILE)
}

fn load(agent_dir: &Path) -> Ledger {
    fs::read(ledger_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(agent_dir: &Path, ledger: &Ledger) -> Result<()> {
    harness_state::write_json_atomic(&ledger_path(agent_dir), ledger, agent_dir, ".claude-denials")
}

/// The kind a call belongs to for an approval of its kind: a shell command's first two words,
/// any other tool's name.
pub fn kind(tool: &str, input: &Value) -> String {
    match input.get("command").and_then(Value::as_str) {
        Some(command) if matches!(tool, "Bash" | "PowerShell") => {
            let words = command.split_whitespace().take(2).collect::<Vec<_>>();
            if words.is_empty() { tool.to_owned() } else { words.join(" ") }
        }
        _ => tool.to_owned(),
    }
}

/// A digest of a call's tool and input: an approval of one call allows only that exact call.
pub fn digest(tool: &str, input: &Value) -> String {
    let digest = Sha256::digest(format!("{tool}\n{input}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The classifier's reason in a denial's tool result, or `None` when it is no classifier denial.
fn denial_reason(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    let rest = text.strip_prefix(DENIED)?;
    let reason = rest
        .trim_start()
        .strip_prefix("Reason: [")
        .and_then(|reason| reason.split_once(']'))
        .map(|(reason, _)| reason.trim().to_owned())
        .unwrap_or_default();
    Some(reason)
}

/// The denials of the turn that just ended that `transcript` records since the ledger last read
/// it, which the ledger now holds. Reading resumes where it stopped, so each denial is reported
/// once. A denial counts only while the session's permission mode is auto, and only in the newest
/// turn read: an earlier turn's denial, such as one a first read finds in a long session's
/// history, was reported at that turn's end or waits on nobody now.
pub fn new_denials(agent_dir: &Path, session_id: &str, transcript: &Path) -> Result<Vec<Denial>> {
    let _lock = harness_state::lock_exclusive(&agent_dir.join(LOCK_FILE))?;
    let mut ledger = load(agent_dir);
    let mut file = fs::File::open(transcript)?;
    let length = file.metadata()?.len();
    let start = ledger
        .cursor
        .as_ref()
        .filter(|(session, offset)| session == session_id && *offset <= length)
        .map_or(0, |(_, offset)| *offset);
    let mut mode = if start == 0 { None } else { ledger.mode.clone() };
    // The prompt whose turn the lines read belong to.
    let mut turn: Option<String> = None;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut offset = start;
    let mut calls = BTreeMap::<String, (String, Value)>::new();
    let mut denied = Vec::new();
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // A growing transcript may end in a partial line: read it on the next Stop.
            if !line.ends_with('\n') {
                break;
            }
            offset = offset.saturating_add(bytes as u64);
            continue;
        };
        offset = offset.saturating_add(bytes as u64);
        if let Some(recorded) = value.get("permissionMode").and_then(Value::as_str) {
            mode = Some(recorded.to_owned());
        }
        // A subagent's calls are its own sidechain: its denials reach the person as its own.
        let parts = value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if value["type"] == "user" {
            // Each of a turn's lines names its prompt; a transcript that names none starts a turn
            // with a line that answers no call.
            let prompt = value.get("promptId").and_then(Value::as_str);
            let new_turn = match (prompt, &turn) {
                (Some(prompt), Some(turn)) => prompt != turn,
                (Some(_), None) => false,
                (None, _) => !parts.iter().any(|part| part["type"] == "tool_result"),
            };
            if new_turn {
                denied.clear();
            }
            if let Some(prompt) = prompt {
                turn = Some(prompt.to_owned());
            }
        }
        for part in parts {
            match part["type"].as_str() {
                Some("tool_use") => {
                    if let (Some(id), Some(name)) = (part["id"].as_str(), part["name"].as_str()) {
                        calls.insert(id.to_owned(), (name.to_owned(), part["input"].clone()));
                    }
                }
                Some("tool_result") => {
                    let Some(id) = part["tool_use_id"].as_str() else {
                        continue;
                    };
                    if mode.as_deref() != Some(AUTO_MODE) {
                        continue;
                    }
                    let Some(reason) = denial_reason(&part["content"]) else {
                        continue;
                    };
                    let (tool, input) = calls
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| ("tool".to_owned(), Value::Null));
                    denied.push(Denial {
                        call_id: id.to_owned(),
                        tool,
                        input,
                        reason,
                    });
                }
                _ => {}
            }
        }
    }
    ledger.cursor = Some((session_id.to_owned(), offset));
    ledger.mode = mode;
    let mut reported = Vec::new();
    for denial in denied {
        if ledger.entries.iter().any(|entry| entry.call_id == denial.call_id) {
            continue;
        }
        ledger.entries.push(Entry {
            call_id: denial.call_id.clone(),
            tool: denial.tool.clone(),
            kind: kind(&denial.tool, &denial.input),
            digest: digest(&denial.tool, &denial.input),
            claim: None,
            answer: None,
        });
        reported.push(denial);
    }
    let excess = ledger.entries.len().saturating_sub(KEPT);
    ledger.entries.drain(..excess);
    save(agent_dir, &ledger)?;
    Ok(reported)
}

/// Remember the st claim that raised the denial of `call_id`.
pub fn raised(agent_dir: &Path, call_id: &str, claim: &str) -> Result<()> {
    let _lock = harness_state::lock_exclusive(&agent_dir.join(LOCK_FILE))?;
    let mut ledger = load(agent_dir);
    if let Some(entry) = ledger.entries.iter_mut().find(|entry| entry.call_id == call_id) {
        entry.claim = Some(claim.to_owned());
        save(agent_dir, &ledger)?;
    }
    Ok(())
}

/// Whether the person allowed the call about to run (`tool` with `input`): an approval of that
/// exact denied call, used once, or of its kind, kept for the seat. `answer` reads a denial's
/// answer from st, at most once per denial: an answer is final. A dismissed denial is forgotten.
pub fn approved(
    agent_dir: &Path,
    tool: &str,
    input: &Value,
    mut answer: impl FnMut(&str) -> Option<String>,
) -> Result<bool> {
    let ledger_path = ledger_path(agent_dir);
    if !ledger_path.exists() {
        return Ok(false);
    }
    let _lock = harness_state::lock_exclusive(&agent_dir.join(LOCK_FILE))?;
    let mut ledger = load(agent_dir);
    let (call_digest, call_kind) = (digest(tool, input), kind(tool, input));
    let mut allowed = false;
    let mut changed = false;
    ledger.entries.retain_mut(|entry| {
        if entry.tool != tool || allowed {
            return true;
        }
        if entry.answer.is_none()
            && (entry.digest == call_digest || entry.kind == call_kind)
            && let Some(claim) = entry.claim.as_deref()
            && let Some(read) = answer(claim)
        {
            entry.answer = Some(read);
            changed = true;
        }
        match entry.answer.as_deref() {
            Some("approve-once") if entry.digest == call_digest => {
                allowed = true;
                changed = true;
                false
            }
            Some("approve-kind") if entry.kind == call_kind => {
                allowed = true;
                true
            }
            Some("dismiss") => {
                changed = true;
                false
            }
            _ => true,
        }
    });
    if changed {
        save(agent_dir, &ledger)?;
    }
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A session transcript as Claude Code 2.1.296 writes it in auto mode: the assistant's tool
    /// use, then the classifier's denial as that call's result (measured 2026-10-13).
    fn transcript(dir: &Path, lines: &[Value]) -> PathBuf {
        let mut auto = vec![mode("auto")];
        auto.extend_from_slice(lines);
        write_transcript(dir, &auto)
    }

    fn write_transcript(dir: &Path, lines: &[Value]) -> PathBuf {
        let path = dir.join("session-1.jsonl");
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn tool_use(id: &str, command: &str) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]}})
    }

    fn denied(id: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": id, "is_error": true,
            "content": "Permission for this action was denied by the Claude Code auto mode classifier. Reason: [Irreversible Local Destruction]. If you have other tasks that don't depend on this action, continue working on those."}]}})
    }

    /// The line Claude Code writes as each turn begins, naming the session's permission mode.
    fn mode(mode: &str) -> Value {
        json!({"type": "permission-mode", "permissionMode": mode, "sessionId": "session-1"})
    }

    /// A person's prompt, which begins turn `prompt`.
    fn prompt(prompt: &str, mode: &str) -> Value {
        json!({"type": "user", "promptId": prompt, "permissionMode": mode,
            "message": {"role": "user", "content": "Carry on with the invented task."}})
    }

    /// `line` as one of turn `prompt`'s lines.
    fn in_turn(mut line: Value, prompt: &str) -> Value {
        line["promptId"] = json!(prompt);
        line
    }

    fn ran(id: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": id, "content": "Updated 1 path from the index"}]}})
    }

    #[test]
    fn each_denial_in_the_transcript_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(
            dir.path(),
            &[tool_use("toolu_1", "git status"), ran("toolu_1"), tool_use("toolu_2", "git checkout --theirs a.txt"), denied("toolu_2")],
        );
        let reported = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(
            reported,
            vec![Denial {
                call_id: "toolu_2".into(),
                tool: "Bash".into(),
                input: json!({"command": "git checkout --theirs a.txt"}),
                reason: "Irreversible Local Destruction".into(),
            }]
        );
        assert!(new_denials(dir.path(), "session-1", &path).unwrap().is_empty(), "once");
        // A later denial in the same session is new.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{}\n{}\n", tool_use("toolu_3", "git reset --hard"), denied("toolu_3")));
        fs::write(&path, text).unwrap();
        let later = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].call_id, "toolu_3");
    }

    /// A seat in bypass mode is never asked: auto mode's classifier does not run there, and a call
    /// a harness rule refused, such as Claude Code's block on a foreground sleep, is no denial.
    /// A first read of a long session raises nothing from its history either: an auto-mode turn
    /// that ended long ago waits on nobody.
    #[test]
    fn a_bypass_seat_and_an_old_turn_raise_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": "toolu_2", "is_error": true,
            "content": "Blocked: sleep 25 followed by: gh pr checks 1. To wait for a condition, use Monitor with an until-loop."}]}});
        let path = write_transcript(
            dir.path(),
            &[
                mode("auto"),
                prompt("prompt-1", "auto"),
                tool_use("toolu_1", "git checkout --theirs a.txt"),
                in_turn(denied("toolu_1"), "prompt-1"),
                mode("bypassPermissions"),
                prompt("prompt-2", "bypassPermissions"),
                tool_use("toolu_2", "sleep 25 && gh pr checks 1"),
                in_turn(sleep, "prompt-2"),
            ],
        );
        assert!(new_denials(dir.path(), "session-1", &path).unwrap().is_empty());
        // Even a classifier's words in a bypass turn are no denial that waits on a person.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{}\n{}\n", tool_use("toolu_3", "git reset --hard"), in_turn(denied("toolu_3"), "prompt-2")));
        fs::write(&path, &text).unwrap();
        assert!(new_denials(dir.path(), "session-1", &path).unwrap().is_empty());
        // Back in auto mode, the turn that just ended raises its own denial, and only that.
        text.push_str(&format!(
            "{}\n{}\n{}\n{}\n",
            mode("auto"),
            prompt("prompt-3", "auto"),
            tool_use("toolu_4", "git clean -fdx"),
            in_turn(denied("toolu_4"), "prompt-3")
        ));
        fs::write(&path, &text).unwrap();
        let reported = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(reported.iter().map(|denial| denial.call_id.as_str()).collect::<Vec<_>>(), ["toolu_4"]);
    }

    /// A first read of a session in auto mode reports only the newest turn's denials.
    #[test]
    fn a_first_read_reports_only_the_turn_that_just_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            &[
                mode("auto"),
                prompt("prompt-1", "auto"),
                tool_use("toolu_1", "git checkout --theirs a.txt"),
                in_turn(denied("toolu_1"), "prompt-1"),
                mode("auto"),
                prompt("prompt-2", "auto"),
                tool_use("toolu_2", "git reset --hard"),
                in_turn(denied("toolu_2"), "prompt-2"),
            ],
        );
        let reported = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(reported.iter().map(|denial| denial.call_id.as_str()).collect::<Vec<_>>(), ["toolu_2"]);
    }

    /// The mode that counts is the one in force at the denial: a turn that leaves auto mode keeps
    /// the denial it had, and one that enters it raises only what follows. A ledger an earlier
    /// version wrote holds no mode, so it raises nothing until the transcript names one, which it
    /// does as each turn begins: it fails closed for the rest of that turn.
    #[test]
    fn the_mode_at_the_denial_counts_and_an_unknown_mode_raises_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            &[
                mode("auto"),
                prompt("prompt-1", "auto"),
                tool_use("toolu_1", "git checkout --theirs a.txt"),
                in_turn(denied("toolu_1"), "prompt-1"),
                mode("bypassPermissions"),
                tool_use("toolu_2", "git reset --hard"),
                in_turn(denied("toolu_2"), "prompt-1"),
            ],
        );
        let reported = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(reported.iter().map(|denial| denial.call_id.as_str()).collect::<Vec<_>>(), ["toolu_1"]);
        // A second read at the next turn's end raises nothing it raised before.
        assert!(new_denials(dir.path(), "session-1", &path).unwrap().is_empty());

        // An earlier version's ledger: a cursor, and no mode.
        let length = fs::metadata(&path).unwrap().len();
        fs::write(
            dir.path().join(LEDGER_FILE),
            json!({"cursor": ["session-1", length], "entries": []}).to_string(),
        )
        .unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{}\n{}\n", tool_use("toolu_3", "git clean -fdx"), in_turn(denied("toolu_3"), "prompt-1")));
        fs::write(&path, &text).unwrap();
        assert!(new_denials(dir.path(), "session-1", &path).unwrap().is_empty(), "fails closed");
        text.push_str(&format!(
            "{}\n{}\n{}\n{}\n",
            mode("auto"),
            prompt("prompt-2", "auto"),
            tool_use("toolu_4", "git push --force"),
            in_turn(denied("toolu_4"), "prompt-2")
        ));
        fs::write(&path, &text).unwrap();
        let reported = new_denials(dir.path(), "session-1", &path).unwrap();
        assert_eq!(reported.iter().map(|denial| denial.call_id.as_str()).collect::<Vec<_>>(), ["toolu_4"]);
    }

    #[test]
    fn an_approval_lets_the_matching_call_through_once_or_for_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(
            dir.path(),
            &[tool_use("toolu_2", "git checkout --theirs a.txt"), denied("toolu_2"),
              tool_use("toolu_3", "git reset --hard HEAD~1"), denied("toolu_3")],
        );
        new_denials(dir.path(), "session-1", &path).unwrap();
        raised(dir.path(), "toolu_2", "claim-2").unwrap();
        raised(dir.path(), "toolu_3", "claim-3").unwrap();
        let answers = BTreeMap::from([("claim-2", "approve-once"), ("claim-3", "approve-kind")]);
        let mut asked = Vec::new();
        let mut answer = |claim: &str| {
            asked.push(claim.to_owned());
            answers.get(claim).map(|answer| (*answer).to_owned())
        };
        let checkout = json!({"command": "git checkout --theirs a.txt"});
        // Not yet answered calls of another tool are never asked about.
        assert!(!approved(dir.path(), "Write", &json!({"file_path": "a.txt"}), &mut answer).unwrap());
        // The exact call, once.
        assert!(approved(dir.path(), "Bash", &checkout, &mut answer).unwrap());
        assert!(!approved(dir.path(), "Bash", &checkout, &mut answer).unwrap(), "once");
        // Another `git checkout` is not that call.
        assert!(!approved(dir.path(), "Bash", &json!({"command": "git checkout --ours a.txt"}), &mut answer).unwrap());
        // Any `git reset`, for this seat.
        for command in ["git reset --hard HEAD~1", "git reset --hard origin/main"] {
            assert!(approved(dir.path(), "Bash", &json!({"command": command}), &mut answer).unwrap(), "{command}");
        }
        // Each answer is read from st once.
        assert_eq!(asked.iter().filter(|claim| *claim == "claim-3").count(), 1);
    }

    #[test]
    fn a_dismissed_denial_is_forgotten_and_an_ordinary_result_is_no_denial() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!approved(dir.path(), "Bash", &json!({"command": "ls"}), |_| unreachable!()).unwrap());
        let path = transcript(dir.path(), &[tool_use("toolu_1", "git checkout --theirs a.txt"), ran("toolu_1"),
            tool_use("toolu_2", "git checkout --theirs b.txt"), denied("toolu_2")]);
        assert_eq!(new_denials(dir.path(), "session-1", &path).unwrap().len(), 1);
        raised(dir.path(), "toolu_2", "claim-2").unwrap();
        let checkout = json!({"command": "git checkout --theirs b.txt"});
        assert!(!approved(dir.path(), "Bash", &checkout, |_| Some("dismiss".into())).unwrap());
        assert!(!approved(dir.path(), "Bash", &checkout, |_| unreachable!()).unwrap(), "forgotten");
        assert_eq!(kind("Bash", &json!({"command": "  git   checkout --theirs"})), "git checkout");
        assert_eq!(kind("Write", &json!({"file_path": "a"})), "Write");
    }
}
