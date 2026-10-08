//! Recently delivered conversation rows, bounded like the current page. Only exact
//! repeats are silent: revisions, removals within a row, and new identities still arrive.
use serde_json::Value;
use std::collections::VecDeque;

pub(super) struct Delivered {
    rows: VecDeque<(String, Value, usize)>,
    bytes: usize,
    byte_limit: usize,
}

impl Delivered {
    pub fn new(byte_limit: usize) -> Self {
        Self {
            rows: VecDeque::new(),
            bytes: 0,
            byte_limit,
        }
    }

    pub fn changes(&self, items: &[Value]) -> Vec<Value> {
        items
            .iter()
            .filter(|item| {
                let Some(id) = item["id"].as_str() else {
                    return true;
                };
                !self
                    .rows
                    .iter()
                    .any(|(known, value, _)| known == id && value == *item)
            })
            .cloned()
            .collect()
    }

    /// Call only after the frame has entered the socket outbox. Replacement pages create
    /// a new Delivered; dropped followers cannot carry evidence onto another connection.
    pub fn remember(&mut self, items: &[Value]) {
        for item in items {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            if let Some(position) = self.rows.iter().position(|(known, _, _)| known == id) {
                let (_, _, bytes) = self.rows.remove(position).unwrap();
                self.bytes -= bytes;
            }
            let Ok(encoded) = serde_json::to_vec(item) else {
                continue;
            };
            let bytes = encoded.len().saturating_add(id.len());
            // Oversized rows remain deliverable, but cannot occupy an unbounded cache.
            if bytes > self.byte_limit {
                continue;
            }
            while self.rows.len() >= 200 || bytes > self.byte_limit.saturating_sub(self.bytes) {
                let (_, _, removed) = self.rows.pop_front().unwrap();
                self.bytes -= removed;
            }
            self.bytes += bytes;
            self.rows.push_back((id.to_owned(), item.clone(), bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conversation_rows_keep_new_and_revised_entries_and_silence_only_delivered_copies() {
        let a = json!({"id":"entry/a", "revision":1,"body":"draft","optional":null});
        let mut delivered = Delivered::new(4096);
        assert_eq!(delivered.changes(std::slice::from_ref(&a)), vec![a.clone()]);
        delivered.remember(std::slice::from_ref(&a));
        let b = json!({"id":"entry/b","revision":1,"body":"new"});
        let mut revised = a.clone();
        revised["revision"] = json!(2);
        revised.as_object_mut().unwrap().remove("optional");
        assert_eq!(
            delivered.changes(&[a.clone(), revised.clone(), b.clone()]),
            vec![revised.clone(), b.clone()]
        );
        // Merely preparing a frame must not acknowledge it.
        assert_eq!(
            delivered.changes(std::slice::from_ref(&revised)),
            vec![revised.clone()]
        );
        delivered.remember(&[revised.clone(), b]);
        assert!(delivered.changes(std::slice::from_ref(&revised)).is_empty());
        assert_eq!(
            Delivered::new(4096).changes(std::slice::from_ref(&revised)),
            vec![revised]
        );
    }

    #[test]
    fn conversation_rows_bound_history_and_bytes_without_dropping_uncached_rows() {
        let mut delivered = Delivered::new(4096);
        for n in 0..201 {
            delivered.remember(&[json!({"id":n.to_string()})]);
        }
        assert_eq!(delivered.rows.len(), 200);
        assert!(delivered.bytes <= 4096);
        assert_eq!(delivered.changes(&[json!({"id":"0"})]).len(), 1);
        let big = json!({"id":"large","body":"x".repeat(5000)});
        delivered.remember(std::slice::from_ref(&big));
        assert_eq!(delivered.changes(std::slice::from_ref(&big)), vec![big]);
        let opaque = json!({"body":"no identity"});
        delivered.remember(std::slice::from_ref(&opaque));
        assert_eq!(
            delivered.changes(std::slice::from_ref(&opaque)),
            vec![opaque]
        );
    }

    #[tokio::test]
    async fn conversation_follower_delivers_real_native_revision_without_resending_held_entries() {
        use super::super::*;
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_named(root.path(), "conversation-row-control");
        let agent = "agent/conversation-row-control";
        let incarnation = "conversation-row-runtime:i1";
        let append = |kind: &str, fields: Value| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: agent.into(),
                    kind: kind.into(),
                    actor: Some(agent.into()),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            signal_visible_change(&state);
        };
        append(
            "runtime.observed",
            json!({"status":"running","runtime_id":"conversation-row-runtime","incarnation_id":incarnation,"terminal":false}),
        );
        let timeline = |operation: &str, entry: &str, sequence: u64, revision: u64, text: &str| json!({"operation":operation,"entry_id":entry,"sequence":sequence,"revision":revision,"driver":"codex","incarnation_id":incarnation,"role":"assistant","entry_type":"content","final":false,"body":{"media_type":"text/plain","text":text}});
        append(
            "harness.timeline",
            timeline("append", "timeline-entry/held", 1, 1, "held"),
        );
        append(
            "harness.timeline",
            timeline("append", "timeline-entry/revised", 2, 1, "draft"),
        );
        let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let follower = tokio::spawn(follow_conversation(
            state.clone(),
            ClientSession::local(Some("person/example")).unwrap(),
            "talk".into(),
            managed_session_id(agent, incarnation),
            None,
            outbox,
        ));
        let (_, initial) = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial["kind"], "conversation", "{initial}");
        assert_eq!(initial["replace"], true);
        assert!(
            initial["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == "timeline-entry/held"),
            "{initial}"
        );
        append(
            "harness.timeline",
            timeline("replace", "timeline-entry/revised", 2, 2, "revised"),
        );
        let (_, changed) = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed["replace"], false, "{changed}");
        let items = changed["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|row| row["id"] == "timeline-entry/revised"),
            "{changed}"
        );
        assert!(
            !items.iter().any(|row| row["id"] == "timeline-entry/held"),
            "{changed}"
        );
        follower.abort();
        let _ = follower.await;
    }
}
