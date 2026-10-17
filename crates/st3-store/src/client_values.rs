//! How client resources spell times, step states and attention cards. The API and the store's
//! published views both build client rows, so these sit below both.
use sha2::{Digest as _, Sha256};

pub fn client_timestamp(unix_ms: u128) -> String {
    let unix_ms = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Translate internal step states once for every client projection.
pub fn client_work_state(status: &str) -> &str {
    match status {
        "pending" => "waiting",
        "working" => "claimed",
        other => other,
    }
}

pub fn client_attention_actions(
    kind: &str,
    review_mode: Option<&str>,
) -> Vec<&'static str> {
    if kind.starts_with("custom.") {
        return vec!["custom.reply"];
    }
    match kind {
        "human-gate" if review_mode == Some("feedback") => {
            vec!["review.approve", "review.request-changes"]
        }
        "human-gate" => vec!["review.approve", "review.reject"],
        "launch-approval" => vec!["launch.approve", "launch.cancel"],
        "revision-approval" => {
            vec!["mission.approve-revision", "mission.cancel-revision"]
        }
        "unread-message" => vec!["message.read"],
        "person-step" => vec!["work.done"],
        "fault" | "agent-request" => Vec::new(),
        _ => Vec::new(),
    }
}

/// An attention card's ID: its source, recipient and waiting episode. A source asked again
/// (a human gate's new request, a person step's new episode) gets a new card.
pub fn client_attention_id(
    subject: &str,
    person: &str,
    episode: &str,
) -> anyhow::Result<String> {
    let identity = serde_json::to_vec(&(subject, person, episode))?;
    Ok(format!(
        "attention/{}",
        &hex::encode(Sha256::digest(identity))[..32]
    ))
}
