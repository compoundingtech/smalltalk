//! silent messages: stored and readable, but they wake nobody.
//!
//! A message is held when it is stored with the `st3-silent` tag: its sender chose `kind: silent`.
//! Every sender and seat defaults to wake. The daemon decides this once,
//! when it accepts the message, so every later reader classifies a message from its own tags
//! and sender, without another query. A held message stays out of the seat's delivery until a
//! message that wakes it goes out; then they go out together, at the start of that turn. Until
//! the recipient reads it, a held message is unread like any other.

use crate::model::MessageView;

/// The sender chose the silent message kind.
pub const SILENT_TAG: &str = "st3-silent";
pub const BATCH_LIMIT: usize = 8;
/// Delivery-only metadata, never stored in a message claim.
pub const REMAINING_PREFIX: &str = "st3-silent-remaining:";
/// How long a held message may stay unread before `st doctor` lists it for its seat.
pub const HELD_TOO_LONG_MS: u128 = 24 * 60 * 60 * 1000;

/// Closed daemon event families reserved by conversation admission.
pub fn reserved_event_tag(tag: &str) -> bool {
    tag.starts_with("st3-work:")
        || tag.starts_with("st3-work-handoff:")
        || tag.starts_with("st3-fault:")
        || tag == crate::github_watch::WATCH_TAG
        || tag.starts_with("st3-run-report:")
        || tag == "st3-provider-capacity-retry"
        || tag.starts_with("st3-product-wait:")
}

/// A person's message (including one an adapter imports, such as from a chat bridge), anything
/// ready step, a fault, a gh watch event, and a work handoff always wake the recipient,
/// whatever it chose. Daemon run reports, retry nudges, product-wait and planning notices
/// also always wake; ordinary daemon conversations have no blanket exception.
pub fn always_wakes(from: &str, tags: &[String]) -> bool {
    from.starts_with("person/")
        || from.starts_with("external/")
        || tags.iter().any(|tag| reserved_event_tag(tag))
        || (from.starts_with("daemon/") && tags.iter().any(|tag| tag == "launch"))
}

/// Whether a stored message is held: it wakes nobody until something else wakes its recipient.
pub fn is_held(message: &MessageView) -> bool {
    message.tags.iter().any(|tag| tag == SILENT_TAG) && !always_wakes(&message.from, &message.tags)
}

/// Normalize the accepted delivery tags once, without reading a recipient declaration.
pub fn stored_tags(from: &str, requested: &[String]) -> Vec<String> {
    let mut tags = Vec::with_capacity(requested.len());
    for tag in requested {
        if !tag.starts_with(REMAINING_PREFIX) && !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    if always_wakes(from, &tags) {
        tags.retain(|tag| tag != SILENT_TAG);
    }
    tags
}

/// The mail a seat's delivery offers now. A held message that has not been offered stays back
/// until a message that wakes the seat is offered, then goes with it; one already offered stays.
pub fn release(messages: &mut Vec<MessageView>) {
    let waking = messages.iter().any(|m| m.status == "sent" && !is_held(m));
    let mut held = messages
        .iter()
        .filter(|m| waits_for_turn(m))
        .map(|m| (m.created_index, m.subject.clone()))
        .collect::<Vec<_>>();
    held.sort();
    let omitted = held.len().saturating_sub(BATCH_LIMIT);
    let selected = held
        .into_iter()
        .rev()
        .take(BATCH_LIMIT)
        .map(|(_, subject)| subject)
        .collect::<std::collections::BTreeSet<_>>();
    messages.retain(|m| !waits_for_turn(m) || (waking && selected.contains(&m.subject)));
    if waking {
        let previous = messages
            .iter()
            .flat_map(|m| &m.tags)
            .filter_map(|tag| tag.strip_prefix(REMAINING_PREFIX)?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        for m in messages.iter_mut() {
            m.tags.retain(|tag| !tag.starts_with(REMAINING_PREFIX));
        }
        if let Some(m) = messages
            .iter_mut()
            .find(|m| m.status == "sent" && !is_held(m))
        {
            let remaining = previous.saturating_add(omitted);
            if remaining > 0 {
                m.tags.push(format!("{REMAINING_PREFIX}{remaining}"));
            }
        }
    }
}

/// Carry a bounded held batch across legacy polling pages; the wake may be on a later page.
pub fn release_page(messages: &mut Vec<MessageView>, pending: &mut Vec<MessageView>) {
    pending.extend(messages.iter().filter(|m| waits_for_turn(m)).cloned());
    messages.retain(|m| !waits_for_turn(m));
    if messages.iter().any(|m| m.status == "sent" && !is_held(m)) {
        pending.append(messages);
        std::mem::swap(messages, pending);
        release(messages);
    } else {
        let previous = pending
            .iter()
            .flat_map(|m| &m.tags)
            .filter_map(|tag| tag.strip_prefix(REMAINING_PREFIX)?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        pending.sort_by_key(|m| (m.created_index, m.subject.clone()));
        let omitted = pending.len().saturating_sub(BATCH_LIMIT);
        pending.drain(..omitted);
        for m in pending.iter_mut() {
            m.tags.retain(|tag| !tag.starts_with(REMAINING_PREFIX));
        }
        let remaining = previous.saturating_add(omitted);
        if remaining > 0
            && let Some(m) = pending.first_mut()
        {
            m.tags.push(format!("{REMAINING_PREFIX}{remaining}"));
        }
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
    fn silent_mail_stays_back_until_a_waking_message_goes_out() {
        let silent = message(
            "message/silent",
            "agent/example/writer",
            "sent",
            &[SILENT_TAG],
        );
        let mut alone = vec![silent.clone()];
        release(&mut alone);
        assert!(alone.is_empty(), "silent mail alone wakes nobody");

        let wake = message("message/wake", "agent/example/writer", "sent", &[]);
        let mut batch = vec![silent.clone(), wake.clone()];
        release(&mut batch);
        assert_eq!(
            batch.len(),
            2,
            "held mail goes out with the next waking message"
        );

        // Once offered, the silent stays in the delivery set; a new silent waits for the next wake.
        let offered_silent = message(
            "message/silent",
            "agent/example/writer",
            "staged",
            &[SILENT_TAG],
        );
        let offered_wake = message("message/wake", "agent/example/writer", "staged", &[]);
        let later = message(
            "message/later",
            "agent/example/writer",
            "sent",
            &[SILENT_TAG],
        );
        let mut after = vec![offered_silent, offered_wake, later];
        release(&mut after);
        assert_eq!(
            after.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(),
            ["message/silent", "message/wake"]
        );
    }

    #[test]
    fn a_large_held_batch_offers_only_the_newest_eight_and_keeps_the_rest_durable() {
        let mut held = (0..700)
            .map(|i| {
                let mut m = message(
                    &format!("message/{i}"),
                    "agent/example/writer",
                    "sent",
                    &[SILENT_TAG],
                );
                m.created_index = i;
                m
            })
            .collect::<Vec<_>>();
        held.push(message("message/wake", "person/operator", "sent", &[]));
        release(&mut held);
        assert_eq!(held.len(), BATCH_LIMIT + 1);
        assert_eq!(held[0].subject, "message/692");
        assert!(
            held.last()
                .unwrap()
                .tags
                .contains(&format!("{REMAINING_PREFIX}692"))
        );
    }

    #[test]
    fn polling_keeps_a_bounded_batch_across_pages_until_the_wake() {
        let mut pending = Vec::new();
        for page in 0..2 {
            let mut items = (0..200)
                .map(|i| {
                    let mut m = message(
                        &format!("message/{}", page * 200 + i),
                        "agent/example/writer",
                        "sent",
                        &[SILENT_TAG],
                    );
                    m.created_index = page * 200 + i;
                    m
                })
                .collect();
            release_page(&mut items, &mut pending);
            assert!(items.is_empty());
            assert_eq!(pending.len(), 8);
        }
        let mut wake = vec![message("message/wake", "person/operator", "sent", &[])];
        release_page(&mut wake, &mut pending);
        assert_eq!(wake.len(), 9);
        assert!(pending.is_empty());
        assert!(
            wake.last()
                .unwrap()
                .tags
                .contains(&format!("{REMAINING_PREFIX}392"))
        );
    }

    #[test]
    fn remaining_notices_saturate_and_agent_launch_stays_held() {
        let marker = format!("{REMAINING_PREFIX}{}", usize::MAX);
        let mut held = (0..10)
            .map(|n| {
                let mut item = message(
                    &format!("message/{n}"),
                    "agent/example/writer",
                    "sent",
                    &[SILENT_TAG],
                );
                item.created_index = n;
                item
            })
            .collect::<Vec<_>>();
        held[0].tags.push(marker.clone());
        let mut pending = Vec::new();
        release_page(&mut held, &mut pending);
        assert_eq!(pending.len(), BATCH_LIMIT);
        assert!(pending[0].tags.contains(&marker));
        let mut wake = vec![message("message/wake", "person/operator", "sent", &[])];
        release_page(&mut wake, &mut pending);
        assert!(wake.last().unwrap().tags.contains(&marker));
        assert!(!reserved_event_tag("launch"));
        assert!(is_held(&message(
            "message/launch",
            "agent/example/writer",
            "sent",
            &[SILENT_TAG, "launch"]
        )));
    }

    #[test]
    fn always_waking_kinds_are_never_held() {
        for (from, tag) in [
            ("person/operator", SILENT_TAG),
            ("external/discord/someone", SILENT_TAG),
            ("daemon/runtime", "st3-work:step-run/a/b@1@1@x"),
            ("daemon/runtime", "st3-fault:episode"),
            ("daemon/example", crate::github_watch::WATCH_TAG),
            ("daemon/runtime", "st3-run-report:failed"),
            ("daemon/runtime", "st3-provider-capacity-retry"),
            ("daemon/runtime", "st3-product-wait:step-run/example/work"),
            ("daemon/runtime", "launch"),
            ("agent/example/writer", "st3-work-handoff:step-run/a/b"),
        ] {
            let stored = stored_tags(from, &tags(&[SILENT_TAG, tag]));
            assert!(!stored.contains(&SILENT_TAG.to_owned()), "{from} {tag}");
            let mut delivered = vec![message("message/x", from, "sent", &[SILENT_TAG, tag])];
            release(&mut delivered);
            assert_eq!(delivered.len(), 1, "{from} {tag} wakes");
        }
        for from in ["agent/example/writer", "daemon/example"] {
            let unrelated = tags(&[SILENT_TAG, "unrelated-event"]);
            assert!(!always_wakes(from, &unrelated));
            assert!(is_held(&message(
                "message/unrelated",
                from,
                "sent",
                &[SILENT_TAG]
            )));
        }
    }

    #[test]
    fn wake_is_default_and_silent_is_explicit() {
        assert!(stored_tags("agent/example/writer", &[]).is_empty());
        assert_eq!(
            stored_tags("agent/example/writer", &tags(&[SILENT_TAG])),
            tags(&[SILENT_TAG])
        );
        assert!(stored_tags("person/operator", &tags(&[SILENT_TAG])).is_empty());
    }
}
