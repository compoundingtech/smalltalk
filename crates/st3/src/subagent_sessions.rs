//! A parent session's subagent transcripts (client contract `conversation-blocks.v1` §1b, q2).
//! OMP stores each subagent's transcript beside its parent's, in a directory named after the
//! parent file: `<parent>.jsonl` -> `<parent>/<task id>.jsonl`. A child session id encodes its
//! parent and task, so the link a client holds stays resolvable across daemon restarts without
//! any stored index: conversation content stays volatile and nothing here writes a file.
use crate::external_sessions::ExternalSession;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Child ids extend the external session prefix, so existing `session/external-` routing keeps
/// working, while staying distinct from every inventoried session id (a plain hash).
const CHILD_PREFIX: &str = "session/external-child-";

/// The reversible id of a subagent conversation: the parent's external id and the task id,
/// encoded together. It is stable for the same parent and task, and distinct from every other
/// session id, because no inventoried id contains the `-child-` infix.
pub(crate) fn child_session_id(parent: &ExternalSession, task_id: &str) -> String {
    format!(
        "{CHILD_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(json!([parent.id, task_id]).to_string())
    )
}

/// Split a child session id back into `(parent external id, task id)`.
pub(crate) fn child_parts(session_id: &str) -> Option<(String, String)> {
    let encoded = session_id.strip_prefix(CHILD_PREFIX)?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let pair = serde_json::from_slice::<serde_json::Value>(&decoded).ok()?;
    let parent = pair.get(0)?.as_str()?;
    let task = pair.get(1)?.as_str()?;
    if !parent.starts_with("session/external-") || !valid_task_id(task) {
        return None;
    }
    Some((parent.to_owned(), task.to_owned()))
}

/// A task id must stay one path component: no separators, no traversal, no dot-prefixed name.
fn valid_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 256
        && !task_id.starts_with('.')
        && !task_id.contains(['/', '\\', '\0'])
}

/// The child transcript the harness writes for `task_id`, when it exists as a regular file.
pub(crate) fn child_transcript(parent: &ExternalSession, task_id: &str) -> Option<PathBuf> {
    if !valid_task_id(task_id) {
        return None;
    }
    let path = parent
        .transcript
        .with_extension("")
        .join(format!("{task_id}.jsonl"));
    std::fs::metadata(&path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|_| path)
}

/// Map a child session id back to its session. The parent must still resolve through normal
/// discovery, with the same owner rules as any other native session, and the child transcript
/// must exist beside it; otherwise the id names no conversation.
pub(crate) fn resolve_child(
    home: Option<&Path>,
    session_id: &str,
) -> anyhow::Result<Option<ExternalSession>> {
    let Some((parent_id, task_id)) = child_parts(session_id) else {
        return Ok(None);
    };
    let Some(parent) = crate::external_sessions::find(home, &parent_id)? else {
        return Ok(None);
    };
    let Some(transcript) = child_transcript(&parent, &task_id) else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(&transcript)?;
    let updated = metadata.modified().ok();
    let started = metadata.created().ok().or(updated);
    let unix = |time: Option<std::time::SystemTime>| {
        time.map(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        })
        .unwrap_or_default()
    };
    Ok(Some(ExternalSession {
        id: session_id.to_owned(),
        revision: String::new(),
        driver: parent.driver,
        native_id: format!("{}/{task_id}", parent.native_id),
        transcript,
        codex_home: parent.codex_home.clone(),
        cwd: parent.cwd.clone(),
        title: Some(format!(
            "{} · {task_id}",
            parent.title.as_deref().unwrap_or(&parent.native_id)
        )),
        started_at_unix_ms: unix(started),
        updated_at_unix_ms: unix(updated),
        process: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_sessions::ExternalDriver;
    use sha2::{Digest as _, Sha256};

    fn parent(root: &Path) -> ExternalSession {
        let transcript = root.join(".omp/agent/sessions/example/native-test.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"session\",\"id\":\"native-test\",\"cwd\":\"/work/example\",\"timestamp\":\"2026-10-06T12:00:00Z\"}\n",
                "{\"type\":\"message\",\"id\":\"message-test\",\"timestamp\":\"2026-10-06T12:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"parent only\"}]}}\n",
            ),
        )
        .unwrap();
        ExternalSession {
            id: format!(
                "session/external-{}",
                &hex::encode(Sha256::digest(b"omp:native-test"))[..24]
            ),
            revision: "revision-test".into(),
            driver: ExternalDriver::Omp,
            native_id: "native-test".into(),
            transcript,
            codex_home: None,
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        }
    }

    fn child_file(parent: &ExternalSession, task_id: &str) -> PathBuf {
        let path = child_transcript_path(parent, task_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"id\":\"child\",\"cwd\":\"/work/example\",\"timestamp\":\"2026-10-06T12:01:00Z\"}\n",
                "{\"type\":\"message\",\"id\":\"child-message\",\"timestamp\":\"2026-10-06T12:01:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"child only\"}]}}\n",
            ),
        )
        .unwrap();
        path
    }

    fn child_transcript_path(parent: &ExternalSession, task_id: &str) -> PathBuf {
        parent
            .transcript
            .with_extension("")
            .join(format!("{task_id}.jsonl"))
    }

    #[test]
    fn traversal_and_hidden_task_ids_never_leave_the_parent_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(root.path());
        for task in [
            "",
            "..",
            "../escape",
            "sub/dir",
            "back\\slash",
            ".hidden",
            ".",
        ] {
            assert!(child_transcript(&parent, task).is_none(), "{task:?}");
        }
        // A directory in the child slot is not a transcript, even with a valid id.
        std::fs::create_dir_all(child_transcript_path(&parent, "directory")).unwrap();
        assert!(child_transcript(&parent, "directory").is_none());
        assert!(child_file(&parent, "task-1").is_file());
        assert_eq!(
            child_transcript(&parent, "task-1"),
            Some(child_transcript_path(&parent, "task-1"))
        );
    }

    #[test]
    fn child_ids_round_trip_through_resolution_without_stored_state() {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(root.path());
        let expected = child_file(&parent, "task-1");
        let id = child_session_id(&parent, "task-1");
        assert!(id.starts_with("session/external-child-"));
        assert_eq!(
            child_parts(&id),
            Some((parent.id.clone(), "task-1".to_owned()))
        );
        // The id holds no path separator, so it stays one route segment.
        assert!(!id.trim_start_matches("session/").contains('/'));
        let resolved = resolve_child(Some(root.path()), &id).unwrap().unwrap();
        assert_eq!(resolved.id, id);
        assert_eq!(resolved.transcript, expected);
        assert_eq!(resolved.driver, ExternalDriver::Omp);
        assert_eq!(resolved.native_id, "native-test/task-1");
        // Without the file, or for a task that never ran, the id names nothing.
        std::fs::remove_file(&expected).unwrap();
        assert!(resolve_child(Some(root.path()), &id).unwrap().is_none());
        let missing = child_session_id(&parent, "never-ran");
        assert!(
            resolve_child(Some(root.path()), &missing)
                .unwrap()
                .is_none()
        );
        // Unknown shapes are not child ids at all.
        assert!(child_parts("session/external-notachild").is_none());
        assert!(child_parts("session/external-child-not-base64!").is_none());
    }
}
