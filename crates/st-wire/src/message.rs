//! The rows `message ls --json` and `message read --json` emit.

use serde::{Deserialize, Serialize};

/// One message as the CLI's CLI reports it.
///
/// The same type serializes in the CLI and deserializes in every reader, which is the point: a reader
/// cannot disagree with the producer about which fields exist or which of them may be absent.
///
/// # What is optional, and why it is not the same as empty
///
/// `from`, `subject`, `in_reply_to` and `priority` are `Option` because the message file's
/// frontmatter genuinely may not carry them — a message sent without `--subject` has no subject,
/// and the CLI emits `null`. That is distinct from a message whose subject is the empty string, which
/// is a subject that happens to say nothing. The distinction is preserved here rather than
/// flattened, so a reader can choose how to present each; collapsing them into `""` would discard
/// the difference before the reader ever saw it.
///
/// `filename` and `ts` are not optional. Both are derived by the CLI from the message's filename —
/// `ts` from its unix-ms prefix, falling back to `0` — so neither can be absent from a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRow {
    /// `<unix-ms>-<rand6>.md` — the message's identity within its box.
    pub filename: String,
    /// Send time in unix ms, from the filename prefix.
    pub ts: u64,
    /// The claimed sender, absent when the frontmatter carries none.
    pub from: Option<String>,
    pub subject: Option<String>,
    /// The filename of the message this replies to.
    #[serde(rename = "inReplyTo")]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub priority: Option<String>,
    /// The caller's optional operation identity for exact retry semantics.
    #[serde(
        rename = "idempotencyKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotency_key: Option<String>,
    /// The declared stream that produced this inbox event, when this is an event record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// The producer-owned stable identity of this event.
    #[serde(rename = "eventId", default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// The optional stream-local supersession key.
    #[serde(rename = "eventKey", default, skip_serializing_if = "Option::is_none")]
    pub event_key: Option<String>,
    /// The markdown body.
    ///
    /// Absent — the key omitted entirely, not `null` — from a `ls --json` row unless
    /// `--include-body` was passed, because a list of bodies is a different and much larger
    /// request than a list of messages. `read --json` always carries it. A reader must therefore
    /// treat "no body" as "not asked for", never as "the message is empty".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// How much sender history the CLI can prove is represented by [`SentMessages`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all = "kebab-case")]
pub enum SentCoverage {
    /// This sender has no index marker, so an empty row set says nothing about earlier sends.
    Unavailable,
    /// Every successful send at or after `since` is represented.
    Since { since: u64 },
    /// One or more durable send intents have not reached a terminal sender-owned state.
    Partial { since: u64, pending: usize },
}

/// One sender-owned message row. The sender is the selected index owner; `to` is the directional
/// field that varies between rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessageRow {
    pub filename: String,
    pub ts: u64,
    pub to: String,
    pub subject: Option<String>,
    #[serde(rename = "inReplyTo")]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub priority: Option<String>,
    #[serde(
        rename = "idempotencyKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// The stable `message sent --json` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessages {
    pub coverage: SentCoverage,
    pub messages: Vec<SentMessageRow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> MessageRow {
        MessageRow {
            filename: "1785000000000-abcdef.md".to_string(),
            ts: 1785000000000,
            from: Some("agent.example".to_string()),
            subject: Some("a subject".to_string()),
            in_reply_to: None,
            tags: Vec::new(),
            priority: None,
            idempotency_key: None,
            stream: None,
            event_id: None,
            event_key: None,
            body: None,
        }
    }

    /// The bug this crate exists to prevent: a reader that cannot accept what the CLI emits. A message
    /// sent without `--subject` is ordinary, and `null` must round-trip to `None` rather than fail.
    #[test]
    fn a_null_subject_or_sender_round_trips_as_absent() {
        let absent = MessageRow {
            from: None,
            subject: None,
            ..row()
        };
        let json = serde_json::to_string(&absent).expect("serialize");
        assert!(json.contains(r#""subject":null"#), "{json}");
        assert!(json.contains(r#""from":null"#), "{json}");
        assert_eq!(
            serde_json::from_str::<MessageRow>(&json).expect("deserialize"),
            absent
        );
    }

    /// Absent and empty are different claims and must not collapse into each other.
    #[test]
    fn an_empty_subject_is_not_the_same_as_no_subject() {
        let empty = MessageRow {
            subject: Some(String::new()),
            ..row()
        };
        let none = MessageRow {
            subject: None,
            ..row()
        };
        assert_ne!(empty, none);
        for value in [empty, none] {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(
                serde_json::from_str::<MessageRow>(&json).expect("deserialize"),
                value
            );
        }
    }

    /// `body` is omitted rather than nulled when it was not requested, and a reader must not read
    /// that absence as an empty message.
    #[test]
    fn an_unrequested_body_is_omitted_from_the_wire_entirely() {
        let json = serde_json::to_string(&row()).expect("serialize");
        assert!(!json.contains("body"), "{json}");
        assert_eq!(
            serde_json::from_str::<MessageRow>(&json)
                .expect("deserialize")
                .body,
            None
        );

        let with_body = MessageRow {
            body: Some(String::new()),
            ..row()
        };
        let json = serde_json::to_string(&with_body).expect("serialize");
        assert!(json.contains(r#""body":"""#), "{json}");
    }

    #[test]
    fn event_identity_round_trips_without_changing_ordinary_message_rows() {
        let ordinary = serde_json::to_value(row()).unwrap();
        assert!(ordinary.get("stream").is_none());
        assert!(ordinary.get("eventId").is_none());
        assert!(ordinary.get("eventKey").is_none());

        let event = MessageRow {
            stream: Some("gh-ci".to_string()),
            event_id: Some("run-812".to_string()),
            event_key: Some("main".to_string()),
            ..row()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["stream"], "gh-ci");
        assert_eq!(json["eventId"], "run-812");
        assert_eq!(json["eventKey"], "main");
        assert_eq!(serde_json::from_value::<MessageRow>(json).unwrap(), event);
    }

    /// An ABSENT optional key and an explicit `null` both mean "not carried", and both must parse.
    ///
    /// This is the distinction the defect turned on: `#[serde(default)]` covers only the absent
    /// case, so a hand-written reader that used it accepted a missing `subject` and rejected the
    /// `null` the producer actually emits. `Option` covers both, and this test is what stops anyone
    /// "simplifying" these fields back to a defaulted `String`.
    #[test]
    fn an_absent_optional_key_parses_the_same_as_an_explicit_null() {
        let absent = r#"{"filename":"1785000000000-abcdef.md","ts":1785000000000,"tags":[]}"#;
        let nulled = r#"{"filename":"1785000000000-abcdef.md","ts":1785000000000,"tags":[],
            "from":null,"subject":null,"inReplyTo":null,"priority":null}"#;
        let absent = serde_json::from_str::<MessageRow>(absent).expect("absent keys parse");
        let nulled = serde_json::from_str::<MessageRow>(nulled).expect("null keys parse");
        assert_eq!(absent, nulled);
        assert_eq!(absent.subject, None);
        assert_eq!(absent.from, None);
    }

    /// A reader pinned to this crate may be older than the `the CLI` binary it invokes, so a field it
    /// has never heard of must be ignored rather than fatal.
    #[test]
    fn a_field_this_reader_does_not_know_is_ignored_not_rejected() {
        let json = r#"{"filename":"1785000000000-abcdef.md","ts":1785000000000,"from":null,
            "subject":null,"inReplyTo":null,"tags":[],"priority":null,"somethingAddedLater":42}"#;
        let parsed = serde_json::from_str::<MessageRow>(json).expect("unknown fields are ignored");
        assert_eq!(parsed.filename, "1785000000000-abcdef.md");
    }

    #[test]
    fn sent_rows_carry_to_and_coverage_never_collapses_unavailable_into_empty() {
        let unavailable = SentMessages {
            coverage: SentCoverage::Unavailable,
            messages: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&unavailable).unwrap(),
            serde_json::json!({
                "coverage": { "_tag": "unavailable" },
                "messages": [],
            })
        );

        let indexed = SentMessages {
            coverage: SentCoverage::Since {
                since: 1_785_000_000_000,
            },
            messages: vec![SentMessageRow {
                filename: "1785000000000-abcdef.md".to_string(),
                ts: 1_785_000_000_000,
                to: "h.recipient".to_string(),
                subject: None,
                in_reply_to: None,
                tags: Vec::new(),
                priority: None,
                idempotency_key: None,
                body: None,
            }],
        };
        let json = serde_json::to_value(indexed).unwrap();
        assert_eq!(json["messages"][0]["to"], "h.recipient");
        assert!(json["messages"][0].get("from").is_none());
        assert!(json["messages"][0].get("body").is_none());
    }
}
