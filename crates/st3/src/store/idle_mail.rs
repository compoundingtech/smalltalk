//! Disposable message state for idle-hold reads. Claim writes maintain it; reads never
//! populate it. Startup backfill commits at most 64 message subjects at a time.
use super::*;

const FOLD: &str = r#"
INSERT OR REPLACE INTO local_idle_messages(subject,recipient,wait_tag,source_prefix,sent_at,pending,delivered)
SELECT sent.subject,COALESCE(json_extract(sent.body,'$.fields.to'),''),
       json_extract(sent.body,'$.fields.tags[1]'),
       CASE WHEN json_extract(sent.body,'$.fields.tags[0]')='github-watch' THEN 'github-watch'
            WHEN json_extract(sent.body,'$.fields.tags[0]') GLOB 'st3-run-report:*' THEN 'st3-run-report:' END,
       CAST(sent.accepted_at_unix_ms AS INTEGER),
       NOT EXISTS(SELECT 1 FROM claims terminal INDEXED BY claims_subject_kind_index
                  WHERE terminal.subject=sent.subject
                    AND terminal.kind IN ('message.delivered','message.read','message.closed')),
       EXISTS(SELECT 1 FROM claims terminal INDEXED BY claims_subject_kind_index
              WHERE terminal.subject=sent.subject AND terminal.kind IN ('message.delivered','message.read'))
FROM claims sent WHERE sent.store_index=(
    SELECT store_index FROM claims INDEXED BY claims_subject_kind_accepted_index
    WHERE subject=SUBJECT AND kind='message.sent'
    ORDER BY length(accepted_at_unix_ms) DESC,accepted_at_unix_ms DESC LIMIT 1);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_idle_messages (
    subject TEXT PRIMARY KEY,
    recipient TEXT NOT NULL,
    wait_tag TEXT,
    source_prefix TEXT,
    sent_at INTEGER NOT NULL,
    pending INTEGER NOT NULL,
    delivered INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_idle_messages_pending
ON local_idle_messages(recipient) WHERE pending=1;
CREATE INDEX IF NOT EXISTS local_idle_messages_wake
ON local_idle_messages(recipient,wait_tag,source_prefix,sent_at DESC) WHERE delivered=1;
"#,
    )?;
    for (name, event, reference) in [
        ("insert", "AFTER INSERT", "NEW"),
        ("delete", "AFTER DELETE", "OLD"),
    ] {
        let subject = format!("{reference}.subject");
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS local_idle_messages_{name} {event} ON claims
             WHEN {reference}.kind IN ('message.sent','message.delivered','message.read','message.closed')
             BEGIN DELETE FROM local_idle_messages WHERE subject={subject}; {} END;",
            FOLD.replace("SUBJECT", &subject),
        ))?;
    }
    // Claim bytes are immutable in normal admission; migration/heal can update them.
    connection.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS local_idle_messages_update
         AFTER UPDATE OF subject,kind,body,accepted_at_unix_ms ON claims
         WHEN OLD.kind IN ('message.sent','message.delivered','message.read','message.closed')
           OR NEW.kind IN ('message.sent','message.delivered','message.read','message.closed')
         BEGIN DELETE FROM local_idle_messages WHERE subject IN (OLD.subject,NEW.subject);
         {} {} END;",
        FOLD.replace("SUBJECT", "OLD.subject"),
        FOLD.replace("SUBJECT", "NEW.subject"),
    ))?;
    let filled: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='local_idle_messages_v1')",
        [],
        |row| row.get(0),
    )?;
    if filled {
        return Ok(());
    }
    let mut after: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='local_idle_messages_backfill_after'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let fold_sql = FOLD.replace("SUBJECT", "?1");
    let mut fold = connection.prepare_cached(&fold_sql)?;
    let mut remove =
        connection.prepare_cached("DELETE FROM local_idle_messages WHERE subject=?1")?;
    loop {
        let subjects = connection
            .prepare_cached(
                "SELECT subject FROM message_index WHERE subject>?1 ORDER BY subject LIMIT 64",
            )?
            .query_map([&after], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if subjects.is_empty() {
            break;
        }
        // Idempotent batches also recover an interrupted first startup without a read-side build.
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for subject in &subjects {
                remove.execute([subject])?;
                fold.execute([subject])?;
            }
            connection.execute(
                "INSERT INTO meta(key,value) VALUES('local_idle_messages_backfill_after',?1)
                                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [subjects.last().unwrap()],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => connection.execute_batch("COMMIT")?,
            Err(error) => {
                connection.execute_batch("ROLLBACK")?;
                return Err(error);
            }
        }
        after = subjects.last().unwrap().clone();
    }
    connection.execute(
        "DELETE FROM meta WHERE key='local_idle_messages_backfill_after'",
        [],
    )?;
    connection.execute(
        "INSERT INTO meta(key,value) VALUES('local_idle_messages_v1','1')",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
impl Store {
    /// Synthetic retained history goes through the real SQLite claim triggers. It deliberately
    /// skips graph admission and projection unrelated to these reads, keeping scale tests cheap.
    pub(crate) fn seed_idle_history(
        &self,
        agent: &str,
        at: u128,
        messages: std::ops::Range<usize>,
        observations: std::ops::Range<usize>,
    ) {
        let mut connection = self.connection.write();
        let transaction = connection.transaction().unwrap();
        let batch: String = transaction
            .query_row("SELECT id FROM batches LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let mut insert = transaction.prepare_cached(
            "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
             VALUES(?1,?2,?3,?4,'fixture',NULL,?5,'[]',?6)",
        ).unwrap();
        for index in messages {
            let subject = format!("message/retained-{index}");
            for (suffix, kind, body) in [
                (
                    "s",
                    "message.sent",
                    json!({"fields":{"from":"agent/node.helper","to":agent,"tags":["github-watch",format!("subscription/watch/acme/garden/{}/{}",12+index%5,agent.strip_prefix("agent/").unwrap())],"title":"History","content":"Delivered history","status":"sent"}}),
                ),
                (
                    "d",
                    "message.delivered",
                    json!({"fields":{"status":"delivered"}}),
                ),
            ] {
                insert
                    .execute(params![
                        format!("fixture-mail-{index}-{suffix}"),
                        batch,
                        subject,
                        kind,
                        body.to_string(),
                        at.to_string()
                    ])
                    .unwrap();
            }
        }
        for index in observations {
            insert
                .execute(params![
                    format!("fixture-pr-{index}"),
                    batch,
                    "resource/github/acme/garden/pull-request/12",
                    "resource.observed",
                    json!({"fields":{"facts":{"state":"open","merge_queue":{"state":"queued"}}}})
                        .to_string(),
                    at.to_string()
                ])
                .unwrap();
        }
        drop(insert);
        transaction.commit().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_mail_tracks_receipts_before_sends_updates_deletes_and_backfill() {
        let store = Store::open_memory("node").unwrap();
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        // Replication can bring the receipt first. Use a real admitted batch as its container.
        transaction.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES('idle-test','node',1,'fixture','1')", []).unwrap();
        let insert = |id: &str, kind: &str, body: Value| {
            transaction.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES(?1,'idle-test','message/idle-test',?2,'node',?3,'[]','1')", params![id,kind,body.to_string()]).unwrap();
        };
        insert("receipt", "message.delivered", json!({"fields":{}}));
        insert(
            "send",
            "message.sent",
            json!({"fields":{"to":"agent/node.worker","tags":["github-watch","subscription/watch/acme/garden/12/node.worker"]}}),
        );
        let state = || {
            transaction
                .query_row(
                    "SELECT pending,delivered FROM local_idle_messages",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap()
        };
        assert_eq!(state(), (0, 1));
        transaction.execute("UPDATE claims SET body=json_set(body,'$.fields.to','agent/node.helper') WHERE id='send'", []).unwrap();
        assert_eq!(
            transaction
                .query_row("SELECT recipient FROM local_idle_messages", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "agent/node.helper"
        );
        transaction
            .execute("DELETE FROM claims WHERE id='receipt'", [])
            .unwrap();
        assert_eq!(state(), (1, 0));
        transaction.commit().unwrap();
        connection.execute_batch("DELETE FROM local_idle_messages; DELETE FROM meta WHERE key='local_idle_messages_v1';").unwrap();
        create_schema(&connection).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT pending FROM local_idle_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        connection
            .execute("DELETE FROM claims WHERE id='send'", [])
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM local_idle_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
