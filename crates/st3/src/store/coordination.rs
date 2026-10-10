//! Count-only coordination reads. Writers populate metadata; readers never scan claim bodies.
use super::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS coordination_sends (
    subject TEXT PRIMARY KEY,
    sent_ms INTEGER NOT NULL,
    agent_to_agent INTEGER NOT NULL,
    fyi INTEGER NOT NULL,
    to_person INTEGER NOT NULL,
    held INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS coordination_sends_time
ON coordination_sends(sent_ms, agent_to_agent, fyi, to_person);
CREATE TABLE IF NOT EXISTS local_coordination_backfill (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    cursor INTEGER NOT NULL,
    ceiling INTEGER NOT NULL,
    complete INTEGER NOT NULL,
    progress_ms INTEGER NOT NULL DEFAULT 0
);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    let columns = |table: &str| -> Result<Vec<String>> {
        Ok(connection
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    };
    if !columns("coordination_sends")?
        .iter()
        .any(|name| name == "held")
    {
        connection.execute_batch(
            "ALTER TABLE coordination_sends ADD COLUMN held INTEGER NOT NULL DEFAULT 0;
             UPDATE local_coordination_backfill SET cursor=0,ceiling=0,complete=0;",
        )?;
    }
    if !columns("local_coordination_backfill")?
        .iter()
        .any(|name| name == "progress_ms")
    {
        connection.execute_batch("ALTER TABLE local_coordination_backfill ADD COLUMN progress_ms INTEGER NOT NULL DEFAULT 0;")?;
    }
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS coordination_sends_held ON coordination_sends(held,sent_ms);
         CREATE TRIGGER IF NOT EXISTS coordination_offer AFTER INSERT ON claims
         WHEN NEW.kind IN ('message.staged','message.delivered','message.read','message.closed')
         BEGIN
             UPDATE coordination_sends SET held=0 WHERE subject=NEW.subject AND held!=0;
         END;",
    )?;
    Ok(())
}

pub(super) fn initialize(transaction: &Transaction<'_>) -> Result<()> {
    let maximum: u64 = transaction.query_row(
        "SELECT COALESCE((SELECT store_index FROM claims INDEXED BY claims_kind_index
         WHERE kind='message.sent' ORDER BY store_index DESC LIMIT 1),0)",
        [],
        |row| row.get(0),
    )?;
    let current: Option<(u64, bool)> = transaction
        .query_row(
            "SELECT ceiling,complete FROM local_coordination_backfill WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let now = u64::try_from(now_ms()).unwrap_or(i64::MAX as u64);
    match current {
        None => {
            transaction.execute(
                "INSERT INTO local_coordination_backfill VALUES(1,0,?1,0,?2)",
                params![maximum, now],
            )?;
        }
        Some((ceiling, complete)) if ceiling < maximum => {
            // Old binaries do not populate this cache. Recheck the uncovered tail on
            // every reopen, without rebuilding or dropping existing count metadata.
            transaction.execute("UPDATE local_coordination_backfill SET cursor=CASE WHEN ?4 THEN ?1 ELSE cursor END,ceiling=?2,complete=0,progress_ms=?3 WHERE singleton=1", params![ceiling,maximum,now,complete])?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn sync(
    transaction: &Transaction<'_>,
    subject: &str,
    sent: Option<(u64, &Value)>,
) -> Result<()> {
    let Some((at, fields)) = sent else {
        transaction.execute("DELETE FROM coordination_sends WHERE subject=?1", [subject])?;
        return Ok(());
    };
    let from = fields["from"].as_str().unwrap_or_default();
    let to = fields["to"].as_str().unwrap_or_default();
    let agent = from.starts_with("agent/") && !agent_messages::synthetic(fields);
    let agent_to_agent = agent && to.starts_with("agent/");
    let to_person = agent && to.starts_with("person/");
    let tags = fields["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let tagged_held =
        tags.iter().any(|tag| tag == crate::fyi::FYI_TAG) && !crate::fyi::always_wakes(from, &tags);
    let fyi = agent_to_agent && tagged_held;
    let held = tagged_held
        && transaction.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM claims WHERE subject=?1
         AND kind IN ('message.staged','message.delivered','message.read','message.closed'))",
            [subject],
            |row| row.get::<_, bool>(0),
        )?;
    if !agent_to_agent && !to_person && !held {
        transaction.execute("DELETE FROM coordination_sends WHERE subject=?1", [subject])?;
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO coordination_sends VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(subject) DO UPDATE SET sent_ms=excluded.sent_ms,
             agent_to_agent=excluded.agent_to_agent, fyi=excluded.fyi, to_person=excluded.to_person,
             held=excluded.held
         WHERE sent_ms!=excluded.sent_ms OR agent_to_agent!=excluded.agent_to_agent
             OR fyi!=excluded.fyi OR to_person!=excluded.to_person OR held!=excluded.held",
        params![subject, at, agent_to_agent, fyi, to_person, held],
    )?;
    Ok(())
}

/// At most eight historical subjects per writer batch. No startup scan or read-side build.
/// Live pending sends are maintained by the existing send projection, including replication.
pub(super) fn backfill(transaction: &Transaction<'_>) -> Result<bool> {
    let started = std::time::Instant::now();
    let (cursor, ceiling, complete): (u64, u64, bool) = transaction.query_row(
        "SELECT cursor,ceiling,complete FROM local_coordination_backfill WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if complete {
        return Ok(true);
    }
    let batch = transaction.prepare_cached(
        "SELECT store_index,subject FROM claims INDEXED BY claims_kind_index
         WHERE kind='message.sent' AND store_index>?1 AND store_index<=?2 ORDER BY store_index LIMIT 8",
    )?.query_map(params![cursor, ceiling], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut processed: usize = 0;
    for (_, subject) in &batch {
        // Leave room for the rest of the FIFO writer batch under its 100 ms hold limit.
        if processed > 0 && started.elapsed() >= std::time::Duration::from_millis(10) {
            break;
        }
        let sent: Option<(u64, String)> = transaction
            .query_row(
                "SELECT CAST(accepted_at_unix_ms AS INTEGER),body FROM claims
             WHERE subject=?1 AND kind='message.sent'
             ORDER BY CAST(accepted_at_unix_ms AS INTEGER),id LIMIT 1",
                [subject],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let body = sent
            .as_ref()
            .map(|(_, body)| serde_json::from_str::<Value>(body))
            .transpose()?;
        sync(
            transaction,
            subject,
            sent.as_ref()
                .zip(body.as_ref())
                .map(|((at, _), body)| (*at, &body["fields"])),
        )?;
        processed += 1;
    }
    let complete = processed == batch.len() && batch.len() < 8;
    transaction.execute(
        "UPDATE local_coordination_backfill SET cursor=?1,complete=?2,progress_ms=?3 WHERE singleton=1",
        params![
            batch
                .get(processed.wrapping_sub(1))
                .map_or(ceiling, |(index, _)| *index),
            complete,
            u64::try_from(now_ms()).unwrap_or(i64::MAX as u64)
        ],
    )?;
    Ok(complete)
}

impl Store {
    /// One bounded bootstrap job on the existing FIFO writer. The daemon awaits this job
    /// and pauses between jobs, so several per-claim projections cannot multiply its budget.
    pub fn advance_coordination_counts(&self) -> Result<bool> {
        let mut connection = self.connection.write_background();
        let transaction = connection.transaction()?;
        let complete = backfill(&transaction)?;
        transaction.commit()?;
        Ok(complete)
    }

    pub fn coordination_backfill_status(&self) -> Result<(u64, u64, bool, u64)> {
        Ok(self.readers.get().query_row(
            "SELECT cursor,ceiling,complete,progress_ms FROM local_coordination_backfill WHERE singleton=1",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
    }

    /// One covering range read; counts distinct message subjects, including already read mail.
    /// FYI is the held subset of agent-to-agent sends. Person-directed sends are separate.
    pub fn coordination_counts(&self, since: u64, until: u64) -> Result<Value> {
        let connection = self.readers.get();
        let (agents, fyi, people, complete, cursor, ceiling, progress): (u64, u64, u64, bool, u64, u64, u64) = connection.query_row(
            "SELECT COALESCE(SUM(agent_to_agent),0),COALESCE(SUM(fyi),0),COALESCE(SUM(to_person),0),
                COALESCE((SELECT complete FROM local_coordination_backfill WHERE singleton=1),0),
                COALESCE((SELECT cursor FROM local_coordination_backfill WHERE singleton=1),0),
                COALESCE((SELECT ceiling FROM local_coordination_backfill WHERE singleton=1),0),
                COALESCE((SELECT progress_ms FROM local_coordination_backfill WHERE singleton=1),0)
             FROM coordination_sends INDEXED BY coordination_sends_time WHERE sent_ms>=?1 AND sent_ms<?2",
            params![i64::try_from(since).unwrap_or(i64::MAX), i64::try_from(until).unwrap_or(i64::MAX)], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )?;
        Ok(
            json!({"since_ms":since,"until_ms":until,"agent_to_agent":agents,"fyi":fyi,"to_person":people,"complete":complete,"cursor":cursor,"ceiling":ceiling,"progress_ms":progress}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_historical_body_keeps_bootstrap_incomplete_without_blocking_open() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let store = Store::open(&path, "node").unwrap();
        let claim = store
            .append_claim(&ClaimInput {
                subject: "message/corrupt-history".into(),
                kind: "message.sent".into(),
                actor: Some("agent/example/writer".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("agent/example/writer")),
                    ("to".into(), json!("agent/example/reader")),
                    ("content".into(), json!("history")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // SQLite accepts this JSON; serde's recursion guard rejects it.
        let body = format!(
            "{},\"deep\":{}0{}}}",
            serde_json::to_string(&claim.body)
                .unwrap()
                .strip_suffix('}')
                .unwrap(),
            "[".repeat(140),
            "]".repeat(140)
        );
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE claims SET body=?1 WHERE id=?2",
                    params![body, claim.id],
                )?;
                // The old allowance projection is already populated. The source edit
                // above dirties it through an UPDATE trigger; remove that fixture-only
                // rebuild request so startup reaches the new historical bootstrap.
                tx.execute("DELETE FROM local_agent_message_pending", [])?;
                tx.execute("DELETE FROM coordination_sends", [])?;
                tx.execute(
                    "UPDATE local_coordination_backfill SET cursor=0,ceiling=?1,complete=0",
                    [current_index(tx)?],
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        drop(store);
        let reopened = Store::open(&path, "node")
            .expect("a failed count bootstrap must not block daemon startup");
        assert_eq!(
            reopened.coordination_counts(0, u64::MAX / 2).unwrap()["complete"],
            false
        );
        assert_eq!(reopened.coordination_backfill_status().unwrap().0, 0);
        assert!(reopened.advance_coordination_counts().is_err());
    }

    #[test]
    fn remaining_markers_from_raw_claims_never_enter_message_views() {
        let store = Store::open_memory("node").unwrap();
        let marker = format!("{}{}", crate::fyi::REMAINING_PREFIX, usize::MAX);
        store
            .append_claim(&ClaimInput {
                subject: "message/raw-marker".into(),
                kind: "message.sent".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/example")),
                    ("to".into(), json!("agent/example/reader")),
                    ("content".into(), json!("raw")),
                    ("tags".into(), json!([marker, "dictated"])),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(
            store.message("message/raw-marker").unwrap().unwrap().tags,
            ["dictated"]
        );
        assert_eq!(
            store
                .messages_for_delivery_through("agent/example/reader", store.index().unwrap())
                .unwrap()[0]
                .tags,
            ["dictated"]
        );
        assert_eq!(
            store
                .latest_claim("message/raw-marker", Some("message.sent"))
                .unwrap()
                .unwrap()
                .body["fields"]["tags"][0],
            marker
        );
    }

    #[test]
    fn reconnect_hydrates_eight_held_subjects_and_doctor_reads_at_most_128() {
        let store = Store::open_memory("node").unwrap();
        for n in 0..140 {
            store
                .append_claim(&ClaimInput {
                    subject: format!("message/held-{n}"),
                    kind: "message.sent".into(),
                    actor: Some("agent/example/writer".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("agent/example/writer")),
                        ("to".into(), json!("agent/example/reader")),
                        ("content".into(), json!("held")),
                        ("tags".into(), json!([crate::fyi::FYI_TAG])),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let delivery = store
            .messages_for_delivery_through("agent/example/reader", store.index().unwrap())
            .unwrap();
        assert_eq!(delivery.len(), crate::fyi::BATCH_LIMIT);
        assert_eq!(delivery[0].subject, "message/held-132");
        assert!(
            delivery[0]
                .tags
                .contains(&format!("{}132", crate::fyi::REMAINING_PREFIX))
        );
        assert_eq!(
            store
                .messages(Some("agent/example/reader"), false)
                .unwrap()
                .len(),
            140
        );
        assert_eq!(store.held_mail_before(u128::MAX).unwrap()[0].1, 128);
        assert_eq!(store.held_mail_count_before(u128::MAX).unwrap(), 140);
        store
            .append_claim(&ClaimInput {
                subject: delivery[0].subject.clone(),
                kind: "message.staged".into(),
                actor: Some("agent/example/reader".into()),
                fields: BTreeMap::from([("status".into(), json!("staged"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(store.held_mail_count_before(u128::MAX).unwrap(), 139);
        // Simulate an old binary's missing cache and a previously complete marker.
        store
            .connection
            .batched(|tx| {
                tx.execute("DELETE FROM coordination_sends", [])?;
                tx.execute(
                    "UPDATE local_coordination_backfill SET cursor=0,ceiling=0,complete=1",
                    [],
                )?;
                initialize(tx)?;
                Ok::<(), anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            store.coordination_counts(0, u64::MAX / 2).unwrap()["complete"],
            false
        );
        while !store.advance_coordination_counts().unwrap() {}
        assert_eq!(
            store.coordination_counts(0, u64::MAX / 2).unwrap()["agent_to_agent"],
            140
        );
        assert_eq!(store.held_mail_count_before(u128::MAX).unwrap(), 139);
    }

    #[test]
    fn historical_backfill_is_bounded_and_reads_leave_partial_state_untouched() {
        let store = Store::open_memory("node").unwrap();
        for n in 0..19 {
            store
                .append_claim(&ClaimInput {
                    subject: format!("message/history-{n}"),
                    kind: "message.sent".into(),
                    actor: Some("agent/example/writer".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("agent/example/writer")),
                        ("to".into(), json!("agent/example/reader")),
                        ("tags".into(), json!([crate::fyi::FYI_TAG])),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let digest = graph_digest(&store.readers.get()).unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute("DELETE FROM coordination_sends", [])?;
                tx.execute(
                    "UPDATE local_coordination_backfill SET cursor=0,ceiling=?1,complete=0",
                    [current_index(tx)?],
                )?;
                // Simulate opening an older store before the allowance cache existed.
                // Its existing rebuild must not populate all new count metadata at once.
                tx.execute("DELETE FROM meta WHERE key='agent_message_days_v1'", [])?;
                agent_messages::open(tx)
            })
            .unwrap()
            .unwrap();
        let partial = store.coordination_counts(0, u64::MAX / 2).unwrap();
        assert!(partial["agent_to_agent"].as_u64().unwrap() <= 8);
        assert_eq!(partial["complete"], false);
        assert_eq!(store.coordination_counts(0, u64::MAX / 2).unwrap(), partial);
        // Many projections can share one outer transaction. None may spend another
        // historical budget: only the single daemon bootstrap job advances the cursor.
        store
            .connection
            .batched(|tx| {
                for _ in 0..16 {
                    agent_messages::flush(tx)?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(store.coordination_counts(0, u64::MAX / 2).unwrap(), partial);
        let mut complete = partial;
        for _ in 0..20 {
            if complete["complete"] == true {
                break;
            }
            let previous = complete["agent_to_agent"].as_u64().unwrap();
            store.advance_coordination_counts().unwrap();
            complete = store.coordination_counts(0, u64::MAX / 2).unwrap();
            let next = complete["agent_to_agent"].as_u64().unwrap();
            assert!(next >= previous && next <= previous + 8);
        }
        assert_eq!(complete["agent_to_agent"], 19);
        assert_eq!(complete["fyi"], 19);
        assert_eq!(complete["complete"], true);
        assert_eq!(graph_digest(&store.readers.get()).unwrap(), digest);
    }

    #[test]
    fn distinct_counts_survive_receipts_and_cover_the_time_range_without_read_writes() {
        let store = Store::open_memory("node").unwrap();
        for (subject, to, tags) in [
            ("message/ordinary", "agent/example/reader", vec![]),
            (
                "message/probe",
                "agent/example/delivery-probe/reader",
                vec![crate::fyi::FYI_TAG],
            ),
            (
                "message/soak",
                "agent/example/reader",
                vec!["soak", crate::fyi::FYI_TAG],
            ),
            (
                "message/fyi",
                "agent/example/reader",
                vec![crate::fyi::FYI_TAG],
            ),
            (
                "message/person",
                "person/example",
                vec![crate::fyi::FYI_TAG],
            ),
            (
                "message/handoff",
                "agent/example/reader",
                vec![
                    crate::fyi::FYI_TAG,
                    "st3-work-handoff:step-run/example/task",
                ],
            ),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.sent".into(),
                    actor: Some("agent/example/writer".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("agent/example/writer")),
                        ("to".into(), json!(to)),
                        ("content".into(), json!("metadata count fixture")),
                        ("tags".into(), json!(tags)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(subject.into()),
                })
                .unwrap();
        }
        for status in ["staged", "delivered", "read", "read", "closed"] {
            store
                .append_claim(&ClaimInput {
                    subject: "message/fyi".into(),
                    kind: format!("message.{status}"),
                    actor: Some("agent/example/reader".into()),
                    fields: BTreeMap::from([("status".into(), json!(status))]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let before = store.index().unwrap();
        let counts = store.coordination_counts(0, u64::MAX / 2).unwrap();
        assert_eq!(counts["agent_to_agent"], 3);
        assert_eq!(counts["fyi"], 1);
        assert_eq!(counts["to_person"], 1);
        assert_eq!(counts["complete"], true);
        assert_eq!(
            store.coordination_counts(0, 1).unwrap()["agent_to_agent"],
            0
        );
        assert_eq!(store.index().unwrap(), before);
        let plan:String=store.readers.get().query_row(
            "EXPLAIN QUERY PLAN SELECT SUM(fyi) FROM coordination_sends INDEXED BY coordination_sends_time WHERE sent_ms>=1 AND sent_ms<2", [], |row| row.get(3)).unwrap();
        assert!(plan.contains("COVERING INDEX"), "{plan}");
    }
}
