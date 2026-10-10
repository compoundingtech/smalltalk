//! Count-only coordination reads. Writers populate metadata; readers never scan claim bodies.
use super::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS coordination_sends (
    subject TEXT PRIMARY KEY,
    sent_ms INTEGER NOT NULL,
    agent_to_agent INTEGER NOT NULL,
    fyi INTEGER NOT NULL,
    to_person INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS coordination_sends_time
ON coordination_sends(sent_ms, agent_to_agent, fyi, to_person);
CREATE TABLE IF NOT EXISTS local_coordination_backfill (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    cursor INTEGER NOT NULL,
    ceiling INTEGER NOT NULL,
    complete INTEGER NOT NULL
);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
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
    let agent = from.starts_with("agent/");
    let agent_to_agent = agent && to.starts_with("agent/");
    let to_person = agent && to.starts_with("person/");
    let tags = fields["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let fyi = agent_to_agent
        && tags.iter().any(|tag| tag == crate::fyi::FYI_TAG)
        && !crate::fyi::always_wakes(from, &tags);
    if !agent_to_agent && !to_person {
        transaction.execute("DELETE FROM coordination_sends WHERE subject=?1", [subject])?;
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO coordination_sends VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(subject) DO UPDATE SET sent_ms=excluded.sent_ms,
             agent_to_agent=excluded.agent_to_agent, fyi=excluded.fyi, to_person=excluded.to_person
         WHERE sent_ms!=excluded.sent_ms OR agent_to_agent!=excluded.agent_to_agent
             OR fyi!=excluded.fyi OR to_person!=excluded.to_person",
        params![subject, at, agent_to_agent, fyi, to_person],
    )?;
    Ok(())
}

/// At most eight historical subjects per writer batch. No startup scan or read-side build.
/// Live pending sends are maintained by the existing send projection, including replication.
pub(super) fn backfill(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO local_coordination_backfill
         SELECT 1,0,COALESCE(MAX(store_index),0),0 FROM claims WHERE kind='message.sent'",
        [],
    )?;
    let (cursor, ceiling, complete): (u64, u64, bool) = transaction.query_row(
        "SELECT cursor,ceiling,complete FROM local_coordination_backfill WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if complete {
        return Ok(());
    }
    let batch = transaction.prepare_cached(
        "SELECT store_index,subject FROM claims INDEXED BY claims_kind_index
         WHERE kind='message.sent' AND store_index>?1 AND store_index<=?2 ORDER BY store_index LIMIT 8",
    )?.query_map(params![cursor, ceiling], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (_, subject) in &batch {
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
    }
    transaction.execute(
        "UPDATE local_coordination_backfill SET cursor=?1,complete=?2 WHERE singleton=1",
        params![
            batch.last().map_or(ceiling, |(index, _)| *index),
            batch.len() < 8
        ],
    )?;
    Ok(())
}

impl Store {
    /// One covering range read; counts distinct message subjects, including already read mail.
    /// FYI is the held subset of agent-to-agent sends. Person-directed sends are separate.
    pub fn coordination_counts(&self, since: u64, until: u64) -> Result<Value> {
        let connection = self.readers.get();
        let (agents, fyi, people, complete): (u64, u64, u64, bool) = connection.query_row(
            "SELECT COALESCE(SUM(agent_to_agent),0),COALESCE(SUM(fyi),0),COALESCE(SUM(to_person),0),
                COALESCE((SELECT complete FROM local_coordination_backfill WHERE singleton=1),0)
             FROM coordination_sends INDEXED BY coordination_sends_time WHERE sent_ms>=?1 AND sent_ms<?2",
            params![since, until], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(
            json!({"since_ms":since,"until_ms":until,"agent_to_agent":agents,"fyi":fyi,"to_person":people,"complete":complete}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_backfill_is_bounded_and_reads_leave_partial_state_untouched() {
        let store = Store::open_memory("node").unwrap();
        for n in 0..19 {
            store.append_claim(&ClaimInput {
                subject: format!("message/history-{n}"), kind: "message.sent".into(),
                actor: Some("agent/example/writer".into()),
                fields: BTreeMap::from([
                    ("from".into(),json!("agent/example/writer")),
                    ("to".into(),json!("agent/example/reader")),
                    ("tags".into(),json!([crate::fyi::FYI_TAG])),
                ]), evidence: vec![], expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
        store.connection.batched(|tx| {
            tx.execute("DELETE FROM coordination_sends", [])?;
            tx.execute("UPDATE local_coordination_backfill SET cursor=0,ceiling=?1,complete=0", [current_index(tx)?])?;
            backfill(tx)
        }).unwrap().unwrap();
        let partial = store.coordination_counts(0,u64::MAX/2).unwrap();
        assert_eq!(partial["agent_to_agent"],8);
        assert_eq!(partial["complete"],false);
        assert_eq!(store.coordination_counts(0,u64::MAX/2).unwrap(),partial);
        store.connection.batched(backfill).unwrap().unwrap();
        assert_eq!(store.coordination_counts(0,u64::MAX/2).unwrap()["agent_to_agent"],16);
        store.connection.batched(backfill).unwrap().unwrap();
        let complete = store.coordination_counts(0,u64::MAX/2).unwrap();
        assert_eq!(complete["agent_to_agent"],19);
        assert_eq!(complete["fyi"],19);
        assert_eq!(complete["complete"],true);
    }

    #[test]
    fn distinct_counts_survive_receipts_and_cover_the_time_range_without_read_writes() {
        let store = Store::open_memory("node").unwrap();
        for (subject, to, tags) in [
            ("message/ordinary", "agent/example/reader", vec![]),
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
        for _ in 0..2 {
            store
                .append_claim(&ClaimInput {
                    subject: "message/fyi".into(),
                    kind: "message.read".into(),
                    actor: Some("agent/example/reader".into()),
                    fields: BTreeMap::from([("status".into(), json!("read"))]),
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
