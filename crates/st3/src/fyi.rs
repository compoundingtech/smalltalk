//! FYI messages: stored and readable, but they wake nobody.
//!
//! A message is held when it is stored with the `st3-fyi` tag: its sender said `--fyi`, or its
//! recipient wakes only on questions and the message asks nothing. The daemon decides this once,
//! when it accepts the message, so every later reader classifies a message from its own tags
//! and sender, without another query. A held message stays out of the seat's delivery until a
//! message that wakes it goes out; then they go out together, at the start of that turn. Until
//! the recipient reads it, a held message is unread like any other.

use serde_json::Value;

use crate::model::MessageView;

/// The sender declared, or the recipient's setting decided, that this message wakes nobody.
pub const FYI_TAG: &str = "st3-fyi";
/// The recipient's `wake-on "questions"` setting held this message, not its sender.
pub const HELD_BY_SETTING_TAG: &str = "st3-fyi-held-by-setting";
/// The sender declared that this message asks the recipient something.
pub const QUESTION_TAG: &str = "st3-question";
/// Every message in a thread names each seat that asked a question in it, so a reply to that
/// seat is the answer it waits for. The tag is carried from parent to reply when it is sent.
pub const QUESTION_THREAD_PREFIX: &str = "st3-question-thread:";
/// How long a held message may stay unread before `st doctor` lists it for its seat.
pub const HELD_TOO_LONG_MS: u128 = 24 * 60 * 60 * 1000;

/// A seat's choice of what wakes it, declared as `wake-on "all"` (the default) or
/// `wake-on "questions"` on the agent, and set with `st agents wake-on`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WakeOn {
    #[default]
    All,
    Questions,
}

impl WakeOn {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "questions" => Some(Self::Questions),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Questions => "questions",
        }
    }
}

/// Read only the current declaration; a seat with no `wake-on` child wakes on everything.
pub fn declared_wake_on(desired: Option<&Value>) -> WakeOn {
    desired
        .and_then(|desired| desired.get("children")?.as_array())
        .and_then(|children| {
            children
                .iter()
                .find(|child| child.get("name").and_then(Value::as_str) == Some("wake-on"))
        })
        .and_then(|child| child.get("arguments")?.as_array()?.first()?.as_str())
        .and_then(WakeOn::parse)
        .unwrap_or_default()
}

/// A person's message (including one an adapter imports, such as from a chat bridge), anything
/// st itself sends (a ready step, a fault, a gh watch event, an answer from a person), and a work
/// handoff always wake the recipient, whatever it chose.
pub fn always_wakes(from: &str, tags: &[String]) -> bool {
    from.starts_with("person/")
        || from.starts_with("external/")
        || from.starts_with("daemon/")
        || tags.iter().any(|tag| {
            tag.starts_with("st3-work:")
                || tag.starts_with("st3-work-handoff:")
                || tag.starts_with("st3-fault:")
                || tag == crate::github_watch::WATCH_TAG
        })
}

/// Whether a stored message is held: it wakes nobody until something else wakes its recipient.
pub fn is_held(message: &MessageView) -> bool {
    message.tags.iter().any(|tag| tag == FYI_TAG) && !always_wakes(&message.from, &message.tags)
}

/// Whether a message asks `recipient` something: the sender declared a question, or the thread
/// holds a question `recipient` asked, so this is its answer.
pub fn asks(recipient: &str, tags: &[String]) -> bool {
    tags.iter().any(|tag| {
        tag == QUESTION_TAG
            || tag
                .strip_prefix(QUESTION_THREAD_PREFIX)
                .is_some_and(|asker| asker == recipient)
    })
}

/// The tags st stores on an unsigned message it accepts. `parent` is the message it replies to,
/// and `wake_on` is read only when it can change the answer.
pub fn stored_tags(
    from: &str,
    to: &str,
    requested: &[String],
    parent: Option<&MessageView>,
    wake_on: impl FnOnce() -> WakeOn,
) -> Vec<String> {
    let mut tags = Vec::with_capacity(requested.len() + 2);
    let push = |tag: String, tags: &mut Vec<String>| {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    };
    for tag in requested {
        // A sender declares a question or an FYI; the thread and the setting tags are st's.
        if tag.starts_with(QUESTION_THREAD_PREFIX) || tag == HELD_BY_SETTING_TAG {
            continue;
        }
        push(tag.clone(), &mut tags);
    }
    if let Some(parent) = parent {
        for tag in &parent.tags {
            if tag.starts_with(QUESTION_THREAD_PREFIX) {
                push(tag.clone(), &mut tags);
            }
        }
        if parent.tags.iter().any(|tag| tag == QUESTION_TAG) {
            push(format!("{QUESTION_THREAD_PREFIX}{}", parent.from), &mut tags);
        }
    }
    if tags.iter().any(|tag| tag == QUESTION_TAG) {
        push(format!("{QUESTION_THREAD_PREFIX}{from}"), &mut tags);
    }
    if always_wakes(from, &tags) {
        // A message that always wakes is never stored as FYI.
        tags.retain(|tag| tag != FYI_TAG);
    } else if !tags.iter().any(|tag| tag == FYI_TAG)
        && !asks(to, &tags)
        && wake_on() == WakeOn::Questions
    {
        push(FYI_TAG.into(), &mut tags);
        push(HELD_BY_SETTING_TAG.into(), &mut tags);
    }
    tags
}

/// The mail a seat's delivery offers now. A held message that has not been offered stays back
/// until a message that wakes the seat is offered, then goes with it; one already offered stays.
pub fn release(messages: &mut Vec<MessageView>) {
    let waking = messages
        .iter()
        .any(|message| message.status == "sent" && !is_held(message));
    if !waking {
        messages.retain(|message| message.status != "sent" || !is_held(message));
    }
}

/// Whether held mail is waiting for the recipient's next turn rather than for its delivery.
pub fn waits_for_turn(message: &MessageView) -> bool {
    message.status == "sent" && is_held(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(subject: &str, from: &str, status: &str, tags: &[&str]) -> MessageView {
        MessageView {
            subject: subject.into(),
            from: from.into(),
            to: "agent/example/reader".into(),
            content: "body".into(),
            status: status.into(),
            title: None,
            in_reply_to: None,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            attachments: Vec::new(),
            created_index: 0,
        }
    }

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|tag| (*tag).to_owned()).collect()
    }

    #[test]
    fn an_fyi_stays_back_until_a_waking_message_goes_out() {
        let fyi = message("message/fyi", "agent/example/writer", "sent", &[FYI_TAG]);
        let mut alone = vec![fyi.clone()];
        release(&mut alone);
        assert!(alone.is_empty(), "an FYI alone wakes nobody");

        let wake = message("message/wake", "agent/example/writer", "sent", &[]);
        let mut batch = vec![fyi.clone(), wake.clone()];
        release(&mut batch);
        assert_eq!(batch.len(), 2, "held mail goes out with the next waking message");

        // Once offered, the FYI stays in the delivery set; a new FYI waits for the next wake.
        let offered_fyi = message("message/fyi", "agent/example/writer", "staged", &[FYI_TAG]);
        let offered_wake = message("message/wake", "agent/example/writer", "staged", &[]);
        let later = message("message/later", "agent/example/writer", "sent", &[FYI_TAG]);
        let mut after = vec![offered_fyi, offered_wake, later];
        release(&mut after);
        assert_eq!(
            after.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(),
            ["message/fyi", "message/wake"]
        );
    }

    #[test]
    fn always_waking_kinds_are_never_held() {
        for (from, tag) in [
            ("person/operator", FYI_TAG),
            ("external/discord/someone", FYI_TAG),
            ("daemon/runtime", "st3-work:step-run/a/b@1@1@x"),
            ("daemon/runtime", "st3-fault:episode"),
            ("daemon/example", crate::github_watch::WATCH_TAG),
            ("agent/example/writer", "st3-work-handoff:step-run/a/b"),
        ] {
            let stored = stored_tags(
                from,
                "agent/example/reader",
                &tags(&[FYI_TAG, tag]),
                None,
                || WakeOn::Questions,
            );
            assert!(!stored.contains(&FYI_TAG.to_owned()), "{from} {tag}");
            let mut delivered = vec![message("message/x", from, "sent", &[FYI_TAG, tag])];
            release(&mut delivered);
            assert_eq!(delivered.len(), 1, "{from} {tag} wakes");
        }
    }

    #[test]
    fn the_questions_setting_holds_everything_that_asks_nothing() {
        let questions = || WakeOn::Questions;
        let status = stored_tags("agent/example/writer", "agent/example/reader", &[], None, questions);
        assert_eq!(status, tags(&[FYI_TAG, HELD_BY_SETTING_TAG]));

        let question = stored_tags(
            "agent/example/writer",
            "agent/example/reader",
            &tags(&[QUESTION_TAG]),
            None,
            questions,
        );
        assert!(!question.contains(&FYI_TAG.to_owned()));

        // The default setting delivers everything not marked FYI.
        let all = stored_tags("agent/example/writer", "agent/example/reader", &[], None, || WakeOn::All);
        assert!(all.is_empty());
        // The setting is read only when it matters.
        let declared = stored_tags(
            "agent/example/writer",
            "agent/example/reader",
            &tags(&[FYI_TAG]),
            None,
            || unreachable!("a declared FYI needs no setting"),
        );
        assert_eq!(declared, tags(&[FYI_TAG]));
    }

    #[test]
    fn a_thread_the_recipient_started_with_a_question_wakes_it() {
        let asker = "agent/example/reader";
        let answerer = "agent/example/writer";
        let questions = || WakeOn::Questions;
        // The reader asks; its question carries the thread tag naming it.
        let root_tags = stored_tags(asker, answerer, &tags(&[QUESTION_TAG]), None, questions);
        let mut root = message("message/root", asker, "read", &[]);
        root.to = answerer.into();
        root.tags = root_tags;
        // The answer wakes the reader, though it asks nothing itself.
        let answer_tags = stored_tags(answerer, asker, &[], Some(&root), questions);
        assert!(!answer_tags.contains(&FYI_TAG.to_owned()));
        // The reader's follow-up is not a question to the writer, which also wakes on questions.
        let mut answer = message("message/answer", answerer, "read", &[]);
        answer.tags = answer_tags;
        let follow_up = stored_tags(asker, answerer, &[], Some(&answer), questions);
        assert!(follow_up.contains(&FYI_TAG.to_owned()));
        // A later reply to the reader is still in the thread it started with a question.
        let mut follow = message("message/follow", asker, "read", &[]);
        follow.tags = follow_up;
        let again = stored_tags(answerer, asker, &[], Some(&follow), questions);
        assert!(!again.contains(&FYI_TAG.to_owned()));
        // A sender cannot claim a thread tag; st derives it.
        let forged = stored_tags(
            answerer,
            asker,
            &tags(&[&format!("{QUESTION_THREAD_PREFIX}{asker}")]),
            None,
            questions,
        );
        assert!(forged.contains(&FYI_TAG.to_owned()));
    }

    #[test]
    fn the_setting_is_read_from_the_declaration() {
        let declared = |value: &str| {
            serde_json::json!({"children": [{"name": "wake-on", "arguments": [value]}]})
        };
        assert_eq!(declared_wake_on(Some(&declared("questions"))), WakeOn::Questions);
        assert_eq!(declared_wake_on(Some(&declared("all"))), WakeOn::All);
        assert_eq!(declared_wake_on(Some(&serde_json::json!({"children": []}))), WakeOn::All);
        assert_eq!(declared_wake_on(None), WakeOn::All);
    }
}
