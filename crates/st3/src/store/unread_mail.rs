//! Exact age counts without visiting retained message history. Each unread message contributes
//! to eight timestamp prefixes. A cutoff reads at most 256 sibling prefixes at each level,
//! regardless of how many messages are fresh, overdue, read or closed.

use super::*;

const COUNT_SQL: &str = "WITH levels(shift) AS (VALUES(56),(48),(40),(32),(24),(16),(8),(0))
         SELECT COALESCE(SUM((
             SELECT SUM(count) FROM unread_mail_prefixes
             WHERE shift=levels.shift AND prefix>=((?1 >> (levels.shift+8)) << 8)
             AND prefix<(?1 >> levels.shift)
         )),0) FROM levels";

const SCHEMA: &str = r#"
BEGIN IMMEDIATE;
DROP INDEX IF EXISTS claims_message_sent_time_index;
CREATE TABLE IF NOT EXISTS unread_mail (
    subject TEXT PRIMARY KEY,
    sent_time INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS unread_mail_prefixes (
    shift INTEGER NOT NULL,
    prefix INTEGER NOT NULL,
    count INTEGER NOT NULL CHECK(count >= 0),
    PRIMARY KEY(shift, prefix)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS unread_mail_pending (
    subject TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS unread_mail_insert AFTER INSERT ON unread_mail
BEGIN
    INSERT INTO unread_mail_prefixes(shift, prefix, count)
    SELECT shift, NEW.sent_time >> shift, 1 FROM (
        SELECT 56 AS shift UNION ALL SELECT 48 UNION ALL SELECT 40 UNION ALL SELECT 32
        UNION ALL SELECT 24 UNION ALL SELECT 16 UNION ALL SELECT 8 UNION ALL SELECT 0
    ) WHERE true
    ON CONFLICT(shift,prefix) DO UPDATE SET count=count+1;
END;
CREATE TRIGGER IF NOT EXISTS unread_mail_delete AFTER DELETE ON unread_mail
BEGIN
    UPDATE unread_mail_prefixes SET count=count-1
    WHERE (shift,prefix) IN (
        (56,OLD.sent_time >> 56),(48,OLD.sent_time >> 48),(40,OLD.sent_time >> 40),
        (32,OLD.sent_time >> 32),(24,OLD.sent_time >> 24),(16,OLD.sent_time >> 16),
        (8,OLD.sent_time >> 8),(0,OLD.sent_time)
    );
    DELETE FROM unread_mail_prefixes WHERE count=0 AND (shift,prefix) IN (
        (56,OLD.sent_time >> 56),(48,OLD.sent_time >> 48),(40,OLD.sent_time >> 40),
        (32,OLD.sent_time >> 32),(24,OLD.sent_time >> 24),(16,OLD.sent_time >> 16),
        (8,OLD.sent_time >> 8),(0,OLD.sent_time)
    );
END;
-- Delivery writes only deduplicate a changed message ID. Prefix maintenance runs when
-- the count is read, so ordinary delivery and replication do not write eight counters.
DROP TRIGGER IF EXISTS unread_mail_claim_sent;
DROP TRIGGER IF EXISTS unread_mail_claim_terminal;
DROP TRIGGER IF EXISTS unread_mail_claim_delete;
CREATE TRIGGER IF NOT EXISTS unread_mail_claim_pending AFTER INSERT ON claims
WHEN NEW.kind IN ('message.sent','message.read','message.closed')
BEGIN
    INSERT OR IGNORE INTO unread_mail_pending(subject) VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS unread_mail_claim_pending_delete AFTER DELETE ON claims
WHEN OLD.kind IN ('message.sent','message.read','message.closed')
BEGIN
    INSERT OR IGNORE INTO unread_mail_pending(subject) VALUES(OLD.subject);
END;
COMMIT;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    let filled: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='unread_mail_prefixes')",
        [],
        |row| row.get(0),
    )?;
    if !filled {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM unread_mail;
             DELETE FROM unread_mail_prefixes;
             INSERT INTO unread_mail(subject,sent_time)
             SELECT subject,MAX(CAST(accepted_at_unix_ms AS INTEGER)) FROM claims
             WHERE kind='message.sent' AND NOT EXISTS (
                 SELECT 1 FROM claims terminal WHERE terminal.subject=claims.subject
                 AND terminal.kind IN ('message.read','message.closed')
             ) GROUP BY subject;
             DELETE FROM unread_mail_pending;
             INSERT INTO meta(key,value) VALUES('unread_mail_prefixes','1');
             COMMIT;",
        )?;
    }
    Ok(())
}

fn flush_pending(connection: &Connection) -> Result<()> {
    let pending = connection
        .prepare_cached("SELECT subject FROM unread_mail_pending")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut remove = connection.prepare_cached("DELETE FROM unread_mail WHERE subject=?1")?;
    let mut insert = connection.prepare_cached(
        "INSERT INTO unread_mail(subject,sent_time)
         SELECT ?1,MAX(CAST(accepted_at_unix_ms AS INTEGER)) FROM claims
         WHERE subject=?1 AND kind='message.sent'
         AND NOT EXISTS (
             SELECT 1 FROM claims WHERE subject=?1
             AND kind IN ('message.read','message.closed')
         ) HAVING COUNT(*)>0",
    )?;
    let mut clear =
        connection.prepare_cached("DELETE FROM unread_mail_pending WHERE subject=?1")?;
    for subject in pending {
        remove.execute([&subject])?;
        insert.execute([&subject])?;
        clear.execute([&subject])?;
    }
    Ok(())
}

/// A pinned diagnostic snapshot cannot write. Apply the queued subjects' exact delta
/// to that snapshot's cached count without changing either the snapshot or the queue.
pub(super) fn count_in_snapshot(connection: &Connection, before_unix_ms: u128) -> Result<u64> {
    let before = i64::try_from(before_unix_ms).unwrap_or(i64::MAX);
    let base: i64 = connection
        .prepare_cached(COUNT_SQL)?
        .query_row([before], |row| row.get(0))?;
    let delta: i64 = connection
        .prepare_cached(
            "SELECT COALESCE(SUM(
            CASE WHEN (SELECT MAX(CAST(accepted_at_unix_ms AS INTEGER)) FROM claims
                       WHERE subject=pending.subject AND kind='message.sent') < ?1
                 AND NOT EXISTS (SELECT 1 FROM claims WHERE subject=pending.subject
                                 AND kind IN ('message.read','message.closed'))
                 THEN 1 ELSE 0 END
            - CASE WHEN EXISTS (SELECT 1 FROM unread_mail
                                 WHERE subject=pending.subject AND sent_time < ?1)
                   THEN 1 ELSE 0 END
         ),0) FROM unread_mail_pending pending",
        )?
        .query_row([before], |row| row.get(0))?;
    Ok(u64::try_from(base + delta)?)
}

pub(super) fn count_before(transaction: &Transaction<'_>, before_unix_ms: u128) -> Result<u64> {
    // The caller holds one writer transaction: pending changes and their exact age count
    // become visible together, and a concurrent message cannot disappear between the two.
    flush_pending(transaction)?;
    let before = i64::try_from(before_unix_ms).unwrap_or(i64::MAX);
    let mut statement = transaction.prepare_cached(COUNT_SQL)?;
    Ok(statement.query_row([before], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_counts_follow_time_boundaries_terminal_history_and_backfill() {
        let store = Store::open_memory("node").unwrap();
        let writer = std::cell::Cell::new(0);
        let append = |subject: &str, kind: &str, time: u128| {
            store.set_write_clock_at(time).unwrap();
            // Populate the claim log like replication, which can arrive out of lifecycle
            // order; the normal message command correctly refuses these local transitions.
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            // Independent writers let accepted times arrive out of order without violating
            // one writer's monotonic clock (including a newer local batch).
            let origin = format!("replica-{}", writer.get());
            writer.set(writer.get() + 1);
            append_claim_record_tx(
                &transaction,
                &origin,
                subject,
                kind,
                Some("person/operator"),
                &json!({"fields":{"status":kind.strip_prefix("message.").unwrap()}}),
                &[],
                None,
            )
            .unwrap();
            transaction.commit().unwrap();
        };
        let times = [1, 255, 256, 257, 65_535, 65_536, 1_700_000_000_000];
        for (index, time) in times.iter().enumerate() {
            append(&format!("message/prefix-{index}"), "message.sent", *time);
        }
        {
            let connection = store.connection.write();
            // Delivery must not maintain eight prefix counters per sent claim. The first
            // exact count folds these queued IDs; later counts reuse the completed index.
            let prefixes: u64 = connection
                .query_row("SELECT COUNT(*) FROM unread_mail_prefixes", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let pending: u64 = connection
                .query_row("SELECT COUNT(*) FROM unread_mail_pending", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(prefixes, 0);
            assert_eq!(pending, times.len() as u64);
        }
        let check = |expected: &[u128]| {
            // Check before any ordinary count flushes the queue: both newly unread messages
            // and pending terminal transitions must be exact inside a read-only snapshot.
            assert_eq!(
                store
                    .read_snapshot(|_| store.unread_mail_count_before(u128::MAX))
                    .unwrap(),
                expected.len() as u64
            );
            for cutoff in times
                .into_iter()
                .flat_map(|time| [time - 1, time, time + 1])
                .chain([0, i64::MAX as u128, u128::MAX])
            {
                let count_in_snapshot = store
                    .read_snapshot(|_| store.unread_mail_count_before(cutoff))
                    .unwrap();
                assert_eq!(
                    count_in_snapshot,
                    expected.iter().filter(|time| **time < cutoff).count() as u64,
                    "pinned cutoff {cutoff}"
                );
                assert_eq!(
                    store.unread_mail_count_before(cutoff).unwrap(),
                    expected.iter().filter(|time| **time < cutoff).count() as u64,
                    "cutoff {cutoff}"
                );
            }
        };
        check(&times);
        append("message/prefix-0", "message.delivered", 500);
        check(&times);
        // A new canonical sent time moves its count; an older replica cannot move it back.
        append("message/prefix-0", "message.sent", 256);
        append("message/prefix-0", "message.sent", 2);
        let mut expected = times.to_vec();
        expected[0] = 256;
        check(&expected);
        append("message/prefix-1", "message.read", 600);
        append("message/prefix-2", "message.closed", 600);
        append("message/prefix-1", "message.sent", 700);
        expected.remove(2);
        expected.remove(1);
        check(&expected);
        // An out-of-order terminal claim also protects a later arriving sent claim.
        append("message/terminal-first", "message.read", 800);
        append("message/terminal-first", "message.sent", 1);
        check(&expected);
        {
            let connection = store.connection.write();
            connection
                .execute_batch(
                    "DELETE FROM unread_mail;
                     DELETE FROM meta WHERE key='unread_mail_prefixes';",
                )
                .unwrap();
            create_schema(&connection).unwrap();
        }
        check(&expected);
        // Removing a terminal claim restores the remaining sent time during checkpoint trim.
        store
            .connection
            .write()
            .execute(
                "DELETE FROM claims WHERE subject='message/prefix-2' AND kind='message.closed'",
                [],
            )
            .unwrap();
        expected.push(256);
        check(&expected);
    }

    #[test]
    fn age_count_work_is_bounded_for_fresh_and_aged_mail() {
        let store = Store::open_memory("node").unwrap();
        let mut connection = store.connection.write();
        let mut insert = connection
            .prepare("INSERT INTO unread_mail(subject,sent_time) VALUES(?1,?2)")
            .unwrap();
        let cutoff = 1_700_000_000_000_i64;
        // Spread timestamps across the boundary and every low-byte prefix. The count must
        // stay bounded even when almost all messages fall into the cutoff's one hour.
        for index in 0..10_000_i64 {
            insert
                .execute(params![
                    format!("message/scale-{index}"),
                    cutoff + index - 5_000
                ])
                .unwrap();
        }
        drop(insert);
        for (before, expected) in [
            (cutoff - 10_000, 0),
            (cutoff, 5_000),
            (cutoff + 10_000, 10_000),
        ] {
            let transaction = connection.transaction().unwrap();
            assert_eq!(
                count_before(&transaction, before as u128).unwrap(),
                expected
            );
            transaction.commit().unwrap();
        }
        let mut statement = connection.prepare_cached(COUNT_SQL).unwrap();
        let count: u64 = statement.query_row([cutoff], |row| row.get(0)).unwrap();
        assert_eq!(count, 5_000);
        assert!(statement.get_status(rusqlite::StatementStatus::VmStep) < 4_000);
        assert!(statement.get_status(rusqlite::StatementStatus::FullscanStep) <= 7);
    }
}
