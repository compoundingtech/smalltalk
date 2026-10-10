//! What a send costs this person, from this stui: Enter to the pending copy drawn, to st's
//! acknowledgement, to the delivered entry shown in the same conversation. One JSON line per
//! send in `$XDG_STATE_HOME/st3/stui/submit-timing.jsonl`, on the machine that ran stui. Only
//! milliseconds, the build and ids: never the message, never a conversation, never a person.
//! The file is this device's own and is never sent anywhere.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;

/// The file grows to this size, then its oldest half is dropped.
const LIMIT: u64 = 256 * 1024;

/// How one send ended, as far as this stui saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// st took it and the delivered entry showed in the conversation.
    Delivered,
    /// st refused it or never answered.
    Failed,
    /// The person cleared it before it finished.
    Cleared,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::Delivered => "delivered",
            Outcome::Failed => "failed",
            Outcome::Cleared => "cleared",
        }
    }
}

/// The marks of one send, taken as they happen.
#[derive(Clone, Debug)]
pub(super) struct Marks {
    entered: Instant,
    epoch_ms: u64,
    pub(super) drawn_ms: Option<f64>,
    pub(super) acked_ms: Option<f64>,
    /// The line for this send was written: it is written once.
    pub(super) logged: bool,
}

impl Marks {
    pub(super) fn begin() -> Self {
        Self {
            entered: Instant::now(),
            epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            drawn_ms: None,
            acked_ms: None,
            logged: false,
        }
    }

    fn since(&self) -> f64 {
        self.entered.elapsed().as_secs_f64() * 1000.0
    }

    /// The pending copy has been drawn (first time only).
    pub(super) fn drawn(&mut self) {
        self.drawn_ms.get_or_insert_with(|| self.entered.elapsed().as_secs_f64() * 1000.0);
    }

    /// st answered the request (first time only).
    pub(super) fn acked(&mut self) {
        let now = self.since();
        self.acked_ms.get_or_insert(now);
    }

    /// The line for a finished send. `delivered_ms` is only for a delivered one.
    pub(super) fn line(&self, outcome: Outcome, message_id: Option<&str>, build: &str) -> String {
        let delivered = (outcome == Outcome::Delivered).then(|| self.since());
        serde_json::json!({
            "type": "submit",
            "epoch_ms": self.epoch_ms,
            "build": build,
            "outcome": outcome.word(),
            "to_pending_draw_ms": self.drawn_ms,
            "to_ack_ms": self.acked_ms,
            "to_delivered_ms": delivered,
            "message_id": message_id,
        })
        .to_string()
    }
}

/// `$XDG_STATE_HOME/st3/stui/submit-timing.jsonl` (or under `~/.local/state`).
pub(super) fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    base.is_absolute()
        .then(|| base.join("st3").join("stui").join("submit-timing.jsonl"))
}

/// Add one line, keeping the file bounded. Failing to write is never the person's problem.
pub(super) fn append(path: &std::path::Path, line: &str) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::metadata(path).is_ok_and(|meta| meta.len() > LIMIT) {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let keep = text
                .split_inclusive('\n')
                .skip(text.matches('\n').count() / 2)
                .collect::<String>();
            std::fs::write(path, keep)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{line}")
    })();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivered_send_records_milliseconds_and_ids_and_nothing_else() {
        let mut marks = Marks::begin();
        marks.drawn();
        marks.acked();
        let line: serde_json::Value =
            serde_json::from_str(&marks.line(Outcome::Delivered, Some("message/abc"), "0.1.0+test")).unwrap();
        assert_eq!(line["type"], "submit");
        assert_eq!(line["outcome"], "delivered");
        assert_eq!(line["message_id"], "message/abc");
        for field in ["to_pending_draw_ms", "to_ack_ms", "to_delivered_ms"] {
            assert!(line[field].as_f64().unwrap() >= 0.0, "{field}");
        }
        assert!(line["to_pending_draw_ms"].as_f64() <= line["to_ack_ms"].as_f64());
        assert!(line["to_ack_ms"].as_f64() <= line["to_delivered_ms"].as_f64());
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
        assert!(keys.iter().all(|key| !matches!(*key, "text" | "body" | "content" | "agent" | "person")), "{keys:?}");
    }

    #[test]
    fn a_failed_or_cleared_send_has_no_delivered_time_and_marks_are_taken_once() {
        let mut marks = Marks::begin();
        marks.drawn();
        let first = marks.drawn_ms;
        marks.drawn();
        assert_eq!(marks.drawn_ms, first, "the first draw counts");
        let line: serde_json::Value =
            serde_json::from_str(&marks.line(Outcome::Failed, None, "b")).unwrap();
        assert!(line["to_delivered_ms"].is_null() && line["to_ack_ms"].is_null());
        assert_eq!(line["outcome"], "failed");
    }

    #[test]
    fn the_file_stays_bounded_by_dropping_its_oldest_half() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("st3").join("submit-timing.jsonl");
        let line = format!("{{\"type\":\"submit\",\"pad\":\"{}\"}}", "x".repeat(1000));
        for _ in 0..400 {
            append(&file, &line);
        }
        let size = std::fs::metadata(&file).unwrap().len();
        assert!(size <= LIMIT + 2048, "{size}");
        assert!(std::fs::read_to_string(&file).unwrap().lines().count() > 100);
    }
}
