//! Durable mailbox deltas. Notifications are hints; the indexed claim range is the cursor.
use super::*;

const MESSAGE_CHANGES: &str = "SELECT DISTINCT changed.subject FROM claims changed NOT INDEXED
                 WHERE changed.store_index>?1 AND changed.subject GLOB 'message/*'
                   AND (changed.subject IN (SELECT value FROM json_each(?4))
                     OR EXISTS(SELECT 1 FROM claims sent INDEXED BY claims_subject_kind_index
                        WHERE sent.subject=changed.subject AND sent.kind='message.sent'
                          AND json_extract(sent.body,'$.fields.to') IN (?2,?3))
                     OR EXISTS(SELECT 1 FROM desired,json_each(desired.body,'$.children') child
                        WHERE desired.subject=changed.subject AND desired.kind='message'
                          AND json_extract(child.value,'$.name')='to'
                          AND json_extract(child.value,'$.arguments[0]') IN (?2,?3)))";

/// A live stream's canonical order keys, by message subject.
type MailboxOrder = BTreeMap<String, canonical::ClaimKey>;

pub(crate) struct MailboxChanges {
    pub mark: MailboxWatermark,
    pub seat: bool,
    pub resync: bool,
    pub messages: Vec<String>,
}

impl Store {
    /// Called inside a pinned read, so the cursor and every point read describe one commit.
    /// The rowid range excludes a lifetime subject scan (including for DISTINCT).
    /// Seek new claims, then test their message's recipient using subject indexes. In
    /// particular, a staged/read/closed claim need not repeat the original recipient.
    pub(crate) fn mailbox_changes(
        &self,
        fence: &crate::mailbox::Fence,
        previous: &MailboxWatermark,
        messages: &[MessageView],
    ) -> Result<MailboxChanges> {
        let mark = self.mailbox_watermark(fence)?;
        if mark == *previous {
            return Ok(MailboxChanges {
                mark,
                seat: false,
                resync: false,
                messages: Vec::new(),
            });
        }
        let connection = self.readers.get();
        let (mut seat, mut resync): (bool, bool) = connection
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND store_index>?2)
                 OR EXISTS(SELECT 1 FROM local_observations WHERE subject=?1 AND id>?3),
                    EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND store_index>?2
                      AND (kind='intent.desired' OR json_extract(body,'$.fields.action')='rollout'))
                 OR EXISTS(SELECT 1 FROM claims INDEXED BY claims_kind_index
                      WHERE kind IN ('owned-set.revised','record.repaired') AND store_index>?2)",
            )?
            .query_row(
                params![fence.subject, previous.index, previous.local],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        // Configuration publication or replication repair may change message projections
        // without a new claim on each affected subject. Explicit resync, never an idle fold.
        resync |= mark.desired != previous.desired;
        seat |= resync;
        let recipient = normalize_message_party(&fence.subject);
        let bare = recipient
            .strip_prefix("agent/")
            .filter(|suffix| !suffix.contains('/'))
            .unwrap_or(&recipient);
        let messages = if fence.component == "delivery" && mark.index != previous.index {
            connection
                .prepare_cached(MESSAGE_CHANGES)?
                .query_map(
                    params![
                        previous.index,
                        recipient,
                        bare,
                        serde_json::to_string(
                            &messages
                                .iter()
                                .map(|message| &message.subject)
                                .collect::<Vec<_>>()
                        )?
                    ],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<Vec<String>>>()?
        } else {
            Vec::new()
        };
        // Binding replacement and incarnation validity are independent of message content.
        // No history fold on idle rechecks; a changed seat or owner still checks the full fence.
        if seat || mark.owner != previous.owner {
            self.check_mailbox(fence)?;
        }
        Ok(MailboxChanges {
            seat,
            resync: resync || mark.index < previous.index || mark.local < previous.local,
            mark,
            messages,
        })
    }

    /// `sort_messages_canonically`, reading keys only for subjects missing from `keys`, all in
    /// one statement. A key is the subject's first send or declaration, so it changes only
    /// with a claim on that subject: callers remove each changed subject before ordering.
    /// Keeps only the current mailbox's keys.
    pub(crate) fn order_mailbox_messages(
        &self,
        messages: &mut [MessageView],
        keys: &mut MailboxOrder,
    ) -> Result<()> {
        let mut current = MailboxOrder::new();
        let mut missing = Vec::new();
        for message in messages.iter() {
            match keys.remove(&message.subject) {
                Some(key) => {
                    current.insert(message.subject.clone(), key);
                }
                None => missing.push(&message.subject),
            }
        }
        if !missing.is_empty() {
            static QUERY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
                let position = canonical::position_sql("claims");
                format!("SELECT claims.subject, claims.accepted_at_unix_ms, batches.origin,
                    batches.replica_sequence, claims.batch_id, {position}, claims.id
                    FROM claims JOIN batches ON batches.id=claims.batch_id
                    WHERE claims.subject IN (SELECT value FROM json_each(?1))
                      AND claims.kind IN ('message.sent','intent.desired')")
            });
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(&QUERY)?;
            let mut rows = statement.query([serde_json::to_string(&missing)?])?;
            // ClaimKey order is the canonical order, so a subject's least key is its first claim.
            while let Some(row) = rows.next()? {
                let subject: String = row.get(0)?;
                let time: String = row.get(1)?;
                let key = (time.parse()?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?);
                let entry = current.entry(subject).or_insert_with(|| key.clone());
                if key < *entry {
                    *entry = key;
                }
            }
        }
        if let Some(message) = messages.iter().find(|message| !current.contains_key(&message.subject)) {
            anyhow::bail!("mailbox message {} has no send or declaration claim", message.subject);
        }
        messages.sort_by(|left, right| current[&left.subject].cmp(&current[&right.subject]));
        *keys = current;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::Fence;

    #[test]
    fn delta_seeks_new_claims_and_point_checks_recipients() {
        let store = Store::open_memory("node").unwrap();
        let connection = store.readers.get();
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {MESSAGE_CHANGES}"))
            .unwrap()
            .query_map(params![0, "agent/fixture", "fixture", "[]"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("; ");
        assert!(
            plan.contains("SEARCH changed USING INTEGER PRIMARY KEY (rowid>?)"),
            "{plan}"
        );
        assert!(
            plan.contains(
                "SEARCH sent USING INDEX claims_subject_kind_index (subject=? AND kind=?)"
            ),
            "{plan}"
        );
        assert!(!plan.contains("SCAN changed"), "{plan}");
    }

    #[test]
    fn a_pinned_mailbox_cursor_cannot_skip_a_concurrent_commit() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("graph.db"), "node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let previous = store
            .read_snapshot(|pinned| {
                let before = store.mailbox_watermark(&fence)?;
                assert_eq!(before.index, pinned);
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            store.append_claim(&ClaimInput {
                                subject: "message/concurrent".into(),
                                kind: "message.sent".into(),
                                actor: Some("person/fixture".into()),
                                fields: BTreeMap::from([
                                    ("status".into(), json!("sent")),
                                    ("from".into(), json!("person/fixture")),
                                    ("to".into(), json!(fence.subject)),
                                    ("content".into(), json!("Committed beyond the pin.")),
                                ]),
                                evidence: vec![],
                                expected_subject: None,
                                idempotency_key: None,
                            })
                        })
                        .join()
                        .unwrap()
                })?;
                assert!(store.index()? > pinned);
                let changes = store.mailbox_changes(&fence, &before, &[])?;
                assert_eq!(
                    changes.mark.index, pinned,
                    "the cursor must describe the read, not the process atomic"
                );
                assert!(changes.messages.is_empty());
                Ok(changes.mark)
            })
            .unwrap();
        let changes = store
            .read_snapshot(|_| store.mailbox_changes(&fence, &previous, &[]))
            .unwrap();
        assert_eq!(changes.messages, ["message/concurrent"]);
        assert!(changes.mark.index > previous.index);
    }
}
