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
       NOT EXISTS(SELECT 1 FROM claims terminal
                  WHERE terminal.subject=sent.subject
                    AND terminal.kind IN ('message.delivered','message.read','message.closed')),
       EXISTS(SELECT 1 FROM claims terminal
              WHERE terminal.subject=sent.subject AND terminal.kind IN ('message.delivered','message.read'))
FROM claims sent WHERE sent.store_index=(
    SELECT store_index FROM claims
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
DROP INDEX IF EXISTS local_idle_messages_wake;
CREATE INDEX IF NOT EXISTS local_idle_messages_wake_v1
ON local_idle_messages(recipient,wait_tag,source_prefix,sent_at DESC)
WHERE delivered=1 AND source_prefix IS NOT NULL;
DROP TRIGGER IF EXISTS local_idle_messages_insert;
DROP TRIGGER IF EXISTS local_idle_messages_delete;
DROP TRIGGER IF EXISTS local_idle_messages_update;
"#,
    )?;
    for (name, event, reference) in [
        ("sent_v1", "AFTER INSERT", "NEW"),
        ("delete_v1", "AFTER DELETE", "OLD"),
    ] {
        let subject = format!("{reference}.subject");
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS local_idle_messages_{name} {event} ON claims
             WHEN {reference}.kind IN ({kinds})
             BEGIN DELETE FROM local_idle_messages WHERE subject={subject}; {} END;",
            FOLD.replace("SUBJECT", &subject),
            kinds = if name == "sent_v1" {
                "'message.sent'"
            } else {
                "'message.sent','message.delivered','message.read','message.closed'"
            },
        ))?;
    }
    // Receipts are monotone. If replication brings one before its send, the sent fold finds it.
    // Updating only state avoids reading and parsing a potentially large sent body per receipt.
    connection.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS local_idle_messages_receipt_v1 AFTER INSERT ON claims
         WHEN NEW.kind IN ('message.delivered','message.read','message.closed')
         BEGIN UPDATE local_idle_messages SET pending=0,
           delivered=CASE WHEN NEW.kind IN ('message.delivered','message.read') THEN 1 ELSE delivered END
           WHERE subject=NEW.subject AND (pending<>0 OR
             (delivered<>1 AND NEW.kind IN ('message.delivered','message.read'))); END;",
    )?;
    // Claim bytes are immutable in normal admission; migration/heal can update them.
    connection.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS local_idle_messages_update_v1
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
        let started = std::time::Instant::now();
        let mut last_processed = &subjects[0];
        let result = (|| -> Result<()> {
            for subject in &subjects {
                remove.execute([subject])?;
                fold.execute([subject])?;
                last_processed = subject;
                if started.elapsed() >= std::time::Duration::from_millis(20) {
                    break;
                }
            }
            connection.execute(
                "INSERT INTO meta(key,value) VALUES('local_idle_messages_backfill_after',?1)
                                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [last_processed],
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
        after = last_processed.clone();
    }
    // Both opens can finish the same idempotent backfill. Completion must be atomic and
    // tolerate the other opener's marker; a crash cannot clear the cursor without the marker.
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         DELETE FROM meta WHERE key='local_idle_messages_backfill_after';
         INSERT OR IGNORE INTO meta(key,value) VALUES('local_idle_messages_v1','1');
         COMMIT;",
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

    fn seed_messages(store: &Store, count: usize) {
        store.connection.write().execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES('idle-history','node',1,'fixture','1')", []).unwrap();
        store.seed_idle_history("agent/node.worker", 1, 0..count, 0..0);
    }

    #[test]
    fn idle_mail_backfill_resumes_after_a_committed_64_subject_batch() {
        let store = Store::open_memory("node").unwrap();
        seed_messages(&store, 130);
        let connection = store.connection.write();
        let boundary: String = connection
            .query_row(
                "SELECT subject FROM message_index ORDER BY subject LIMIT 1 OFFSET 64",
                [],
                |row| row.get(0),
            )
            .unwrap();
        connection.execute_batch("DELETE FROM local_idle_messages; DELETE FROM meta WHERE key='local_idle_messages_v1';").unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TEMP TRIGGER interrupt_backfill BEFORE INSERT ON local_idle_messages
             WHEN NEW.subject='{boundary}' BEGIN SELECT RAISE(ABORT,'test interruption'); END;"
            ))
            .unwrap();
        assert!(create_schema(&connection).is_err());
        let count = || {
            connection
                .query_row("SELECT COUNT(*) FROM local_idle_messages", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        let committed = count();
        assert!(committed > 0 && committed <= 64);
        let cursor: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key='local_idle_messages_backfill_after'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cursor < boundary);
        connection
            .execute_batch(
                "DROP TRIGGER interrupt_backfill;
            CREATE TEMP TABLE resumed_subjects(subject TEXT);
            CREATE TEMP TRIGGER record_resume AFTER INSERT ON local_idle_messages
            BEGIN INSERT INTO resumed_subjects VALUES(NEW.subject); END;",
            )
            .unwrap();
        create_schema(&connection).unwrap();
        assert_eq!(count(), 130);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM resumed_subjects", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            130 - committed
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='local_idle_messages_backfill_after'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        create_schema(&connection).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM resumed_subjects", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            130 - committed
        );
    }

    #[test]
    fn idle_mail_two_openers_can_complete_the_same_backfill() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let store = Store::open(&path, "node").unwrap();
        seed_messages(&store, 130);
        store.connection.write().execute_batch("DELETE FROM local_idle_messages; DELETE FROM meta WHERE key='local_idle_messages_v1';").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let connection = Connection::open(path).unwrap();
                    connection
                        .busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    barrier.wait();
                    create_schema(&connection).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let connection = store.connection.write();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM local_idle_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            130
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='local_idle_messages_v1'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='local_idle_messages_backfill_after'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn idle_mail_trigger_writes_survive_removing_planner_indexes() {
        let store = Store::open_memory("node").unwrap();
        seed_messages(&store, 0);
        let connection = store.connection.write();
        // A dropped planner index must not make every claim write fail at trigger preparation.
        connection.execute_batch("DROP INDEX claims_subject_kind_index; DROP INDEX claims_subject_kind_accepted_index;").unwrap();
        connection.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES('send-without-index','idle-history','message/without-index','message.sent','node',?1,'[]','2')", [json!({"fields":{"to":"agent/node.worker"}}).to_string()]).unwrap();
        connection.execute_batch("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES('receipt-without-index','idle-history','message/without-index','message.delivered','node','{\"fields\":{}}','[]','3');").unwrap();
        assert_eq!(connection.query_row("SELECT pending,delivered FROM local_idle_messages WHERE subject='message/without-index'", [], |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?))).unwrap(), (0,1));
    }

    #[test]
    fn idle_mail_receipt_rollback_and_message_trim_preserve_state() {
        let store = Store::open_memory("node").unwrap();
        seed_messages(&store, 130);
        let connection = store.connection.write();
        connection
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM claims WHERE kind='message.delivered';")
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT SUM(pending),SUM(delivered) FROM local_idle_messages",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                )
                .unwrap(),
            (130, 0)
        );
        connection.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT SUM(pending),SUM(delivered) FROM local_idle_messages",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                )
                .unwrap(),
            (0, 130)
        );
        connection
            .execute_batch("DELETE FROM claims WHERE kind IN ('message.sent','message.delivered');")
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM local_idle_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    /// Measures real SQLite claim triggers (including digest maintenance) on a private store
    /// copy supplied by the operator. Graph admission/projection is outside this insert control.
    #[test]
    #[ignore = "requires ST_IDLE_INSERT_BENCH_STORE, a private writable benchmark-store copy"]
    fn idle_mail_insert_benchmark() {
        let path = std::env::var("ST_IDLE_INSERT_BENCH_STORE").unwrap();
        let store = Store::open(std::path::Path::new(&path), "idle-bench").unwrap();
        let connection = store.connection.write();
        let claims: i64 = connection
            .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
            .unwrap();
        connection.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES('idle-insert-bench','idle-bench',1,'fixture','1')", []).unwrap();
        let mut results = Vec::new();
        for bytes in [8192, 16384] {
            for mode in ["baseline", "full_fold_receipts", "state_update_receipts"] {
                create_schema(&connection).unwrap();
                if mode == "baseline" {
                    for name in ["sent", "receipt", "delete", "update"] {
                        connection
                            .execute_batch(&format!("DROP TRIGGER local_idle_messages_{name}_v1"))
                            .unwrap();
                    }
                } else if mode == "full_fold_receipts" {
                    connection
                        .execute_batch(&format!(
                            "DROP TRIGGER local_idle_messages_receipt_v1;
                         CREATE TRIGGER local_idle_messages_receipt_v1 AFTER INSERT ON claims
                         WHEN NEW.kind IN ('message.delivered','message.read','message.closed')
                         BEGIN DELETE FROM local_idle_messages WHERE subject=NEW.subject; {} END;",
                            FOLD.replace("SUBJECT", "NEW.subject")
                        ))
                        .unwrap();
                }
                let body = json!({"fields":{"to":"agent/idle-bench.worker","tags":["github-watch","bench-watch"],"content":"x".repeat(bytes)}}).to_string();
                let rows: Vec<_> = (0..1000)
                    .map(|n| {
                        (
                            format!("idle-bench-{n}-sent"),
                            format!("message/idle-bench-{n}"),
                            "message.sent",
                            body.clone(),
                        )
                    })
                    .collect();
                let receipts: Vec<_> = (0..1000)
                    .flat_map(|n| {
                        ["message.delivered", "message.read", "message.closed"]
                            .into_iter()
                            .map(move |kind| {
                                (
                                    format!("idle-bench-{n}-{kind}"),
                                    format!("message/idle-bench-{n}"),
                                    kind,
                                    "{\"fields\":{}}".to_owned(),
                                )
                            })
                    })
                    .collect();
                for round in 0..3 {
                    let insert_phase = |rows: &[(String, String, &str, String)]| {
                        let mut insert = connection.prepare_cached("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES(?1,'idle-insert-bench',?2,?3,'idle-bench',?4,'[]','1')").unwrap();
                        let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
                        let start = std::time::Instant::now();
                        let mut max_writer_ms = 0.0_f64;
                        for chunk in rows.chunks(64) {
                            let batch_start = std::time::Instant::now();
                            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
                            for (id, subject, kind, body) in chunk {
                                insert.execute(params![id, subject, kind, body]).unwrap();
                            }
                            connection.execute_batch("COMMIT").unwrap();
                            max_writer_ms =
                                max_writer_ms.max(batch_start.elapsed().as_secs_f64() * 1000.0);
                        }
                        (
                            start.elapsed().as_secs_f64() * 1000.0,
                            max_writer_ms,
                            scope.finish(),
                        )
                    };
                    let (sent_ms, sent_max, sent_work) = insert_phase(&rows);
                    let (receipt_ms, receipt_max, receipt_work) = insert_phase(&receipts);
                    results.push(json!({"mode":mode,"body_content_bytes":bytes,"round":round,"sent_claims":1000,"receipt_claims":3000,"sent_ms":sent_ms,"receipt_ms":receipt_ms,"max_writer_batch_ms":sent_max.max(receipt_max),"sent_vm_steps":sent_work.vm_steps,"receipt_vm_steps":receipt_work.vm_steps,"sent_fullscan_steps":sent_work.fullscan_steps,"receipt_fullscan_steps":receipt_work.fullscan_steps}));
                    connection.execute_batch("DELETE FROM claims WHERE batch_id='idle-insert-bench'; DELETE FROM local_idle_messages WHERE subject LIKE 'message/idle-bench-%';").unwrap();
                }
                // Restore the optimized receipt trigger before testing the next mode.
                connection
                    .execute_batch("DROP TRIGGER IF EXISTS local_idle_messages_receipt_v1;")
                    .unwrap();
            }
        }
        eprintln!(
            "idle insert benchmark: {}",
            json!({"initial_claims":claims,"batch_claims":64,"results":results})
        );
    }

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
