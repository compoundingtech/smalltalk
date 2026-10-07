//! Event eligibility is local admission state; payloads belong only to claims.
//! A temporary legacy payload table is drained in small transactions after upgrade.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction};

pub const MIGRATION_CHUNK: usize = 64;

const VIEW: &str = "CREATE VIEW events AS
    SELECT p.store_index,c.kind,p.subject,c.body
    FROM event_positions p CROSS JOIN claims c ON c.store_index=p.store_index";

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='events'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let transaction = connection.unchecked_transaction()?;
    if kind.as_deref() == Some("table") {
        // No copy or scan here. Existing positions, including holes, keep their identity.
        transaction.execute_batch("ALTER TABLE events RENAME TO local_event_payloads;")?;
        // Old checkpoints did not record retired store positions. MIN(events) also cannot
        // distinguish excluded-prefix gaps from lost history or expose a retired middle row.
        // Require one explicit resync at the upgrade frontier instead of inventing continuity.
        raise_resume_floor_tx(&transaction, super::current_index(&transaction)?)?;
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_positions (
             store_index INTEGER PRIMARY KEY,
             subject TEXT NOT NULL
         );",
    )?;
    let legacy: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_event_payloads')",
        [],
        |row| row.get(0),
    )?;
    if kind.as_deref() == Some("table") || kind.is_none() {
        transaction.execute_batch(VIEW)?;
        if legacy {
            transaction.execute_batch(
                "DROP VIEW events;
                 CREATE VIEW events AS
                 SELECT p.store_index,c.kind,p.subject,c.body
                 FROM event_positions p CROSS JOIN claims c ON c.store_index=p.store_index
                 UNION ALL
                 SELECT p.store_index,c.kind,p.subject,c.body
                 FROM local_event_payloads p CROSS JOIN claims c ON c.store_index=p.store_index
                 WHERE NOT EXISTS(SELECT 1 FROM event_positions n WHERE n.store_index=p.store_index);"
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Drain one legacy chunk, retaining the exact original eligibility. Inserts during migration
/// write only the position index, and a crash leaves both reads and progress transactionally valid.
pub fn migrate_chunk_tx(transaction: &Transaction<'_>) -> Result<usize> {
    let legacy: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_event_payloads')",
        [],
        |row| row.get(0),
    )?;
    if !legacy {
        return Ok(0);
    }
    let positions = transaction
        .prepare_cached(
            "SELECT store_index FROM local_event_payloads ORDER BY store_index LIMIT ?1",
        )?
        .query_map([MIGRATION_CHUNK], |row| row.get::<_, u64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for position in &positions {
        transaction.execute(
            "INSERT OR IGNORE INTO event_positions(store_index,subject)
             SELECT store_index,subject FROM claims WHERE store_index=?1",
            [position],
        )?;
        let present: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE store_index=?1)",
            [position],
            |row| row.get(0),
        )?;
        if !present {
            raise_resume_floor_tx(transaction, *position)?;
        }
        transaction.execute(
            "DELETE FROM local_event_payloads WHERE store_index=?1",
            [position],
        )?;
    }
    if positions.len() < MIGRATION_CHUNK {
        transaction.execute_batch("DROP VIEW events; DROP TABLE local_event_payloads;")?;
        transaction.execute_batch(VIEW)?;
    }
    Ok(positions.len())
}

pub fn raise_resume_floor_tx(transaction: &Transaction<'_>, index: u64) -> Result<()> {
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES('event_resume_floor',?1)
         ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),?1) AS TEXT)",
        [index],
    )?;
    Ok(())
}

/// A checkpoint may remove a middle position while older durable events remain. Remember the
/// highest removed position, rather than inferring replay continuity from the first remaining row.
pub fn remove_claim_tx(transaction: &Transaction<'_>, claim: &str) -> Result<()> {
    let position: Option<u64> = transaction
        .query_row(
            "SELECT store_index FROM claims WHERE id=?1",
            [claim],
            |row| row.get(0),
        )
        .optional()?;
    let Some(position) = position else {
        return Ok(());
    };
    let indexed: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM event_positions WHERE store_index=?1)",
        [position],
        |row| row.get(0),
    )?;
    let legacy: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_event_payloads')",
        [],
        |row| row.get(0),
    )?;
    let pending = if legacy {
        transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_event_payloads WHERE store_index=?1)",
            [position],
            |row| row.get(0),
        )?
    } else {
        false
    };
    if !indexed && !pending {
        return Ok(());
    }
    raise_resume_floor_tx(transaction, position)?;
    transaction.execute(
        "DELETE FROM event_positions WHERE store_index=?1",
        [position],
    )?;
    if pending {
        transaction.execute(
            "DELETE FROM local_event_payloads WHERE store_index=?1",
            [position],
        )?;
    }
    Ok(())
}

pub fn bounds(connection: &Connection) -> Result<(u64, u64)> {
    let floor: u64 = connection
        .query_row(
            "SELECT value FROM meta WHERE key='event_resume_floor'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(0);
    // Excluded claims and sparse indexes are ordinary gaps, not discarded history.
    // Only an explicit retirement raises the resync floor.
    let newest = super::current_index(connection)?;
    Ok((floor.saturating_add(1), newest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn legacy(count: u64) -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE,
                 kind TEXT,subject TEXT,body TEXT);
             CREATE TABLE events(store_index INTEGER PRIMARY KEY,kind TEXT,subject TEXT,body TEXT);"
        ).unwrap();
        for index in 1..=count {
            connection.execute(
                "INSERT INTO claims(store_index,id,kind,subject,body) VALUES(?1,?2,'custom.test.recorded',?3,?4)",
                params![index*3,format!("claim-{index}"),format!("custom/test/{}",index%2),format!("{{\"index\":{index}}}")],
            ).unwrap();
            connection.execute("INSERT INTO events SELECT store_index,kind,subject,body FROM claims WHERE store_index=?1",[index*3]).unwrap();
        }
        connection
    }

    fn rows(connection: &Connection) -> Vec<(u64, String, String, String)> {
        connection
            .prepare("SELECT store_index,kind,subject,body FROM events ORDER BY store_index")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn event_migration_keeps_sparse_membership_across_chunks_rollback_and_reopen() {
        let mut connection = legacy(130);
        let expected = rows(&connection);
        initialize(&connection).unwrap();
        assert_eq!(
            bounds(&connection).unwrap(),
            (391, 390),
            "legacy upgrade explicitly invalidates unprovable historical cursors"
        );
        assert_eq!(rows(&connection), expected);
        {
            let transaction = connection.transaction().unwrap();
            assert_eq!(migrate_chunk_tx(&transaction).unwrap(), MIGRATION_CHUNK);
            // Interruption rolls the entire chunk back, including removed legacy payloads.
        }
        assert_eq!(rows(&connection), expected);
        for count in [64, 64, 2, 0] {
            let transaction = connection.transaction().unwrap();
            assert_eq!(migrate_chunk_tx(&transaction).unwrap(), count);
            transaction.commit().unwrap();
            initialize(&connection).unwrap();
            assert_eq!(rows(&connection), expected);
        }
        let legacy: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_event_payloads')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !legacy,
            "the payload copy is removed when migration finishes"
        );
        let columns = connection
            .prepare("PRAGMA table_info(event_positions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(columns, ["store_index", "subject"]);
    }

    #[test]
    fn event_migration_does_not_invent_excluded_claims_and_merges_new_arrivals() {
        let mut connection = legacy(2);
        connection.execute("INSERT INTO claims(store_index,id,kind,subject,body) VALUES(4,'glass','glass.upserted','glass/example','{}')",[]).unwrap();
        connection.execute("INSERT INTO claims(store_index,id,kind,subject,body) VALUES(5,'duplicate','custom.test.recorded','custom/test/duplicate','{}')",[]).unwrap();
        initialize(&connection).unwrap();
        connection.execute("INSERT INTO claims(store_index,id,kind,subject,body) VALUES(9,'new','custom.test.recorded','custom/test/new','{}')",[]).unwrap();
        connection
            .execute(
                "INSERT INTO event_positions VALUES(9,'custom/test/new')",
                [],
            )
            .unwrap();
        // The old and new index can overlap transiently; the view emits the position once.
        connection
            .execute("INSERT INTO event_positions VALUES(3,'custom/test/1')", [])
            .unwrap();
        assert_eq!(
            rows(&connection)
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [3, 6, 9]
        );
        let transaction = connection.transaction().unwrap();
        assert_eq!(migrate_chunk_tx(&transaction).unwrap(), 2);
        transaction.commit().unwrap();
        assert_eq!(
            rows(&connection)
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [3, 6, 9]
        );
    }

    #[test]
    fn event_checkpoint_hole_and_all_removed_keep_an_explicit_resume_floor() {
        let mut connection = legacy(3);
        connection.execute_batch("DROP TABLE events").unwrap();
        initialize(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO event_positions SELECT store_index,subject FROM claims",
                [],
            )
            .unwrap();
        assert_eq!(
            bounds(&connection).unwrap(),
            (1, 9),
            "ordinary sparse gaps on a new membership store remain valid"
        );
        let transaction = connection.transaction().unwrap();
        remove_claim_tx(&transaction, "claim-2").unwrap();
        transaction
            .execute("DELETE FROM claims WHERE id='claim-2'", [])
            .unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            bounds(&connection).unwrap(),
            (7, 9),
            "an older remaining row does not conceal a middle gap"
        );
        for claim in ["claim-1", "claim-3"] {
            let transaction = connection.transaction().unwrap();
            remove_claim_tx(&transaction, claim).unwrap();
            transaction
                .execute("DELETE FROM claims WHERE id=?1", [claim])
                .unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(bounds(&connection).unwrap(), (10, 9));
        initialize(&connection).unwrap();
        assert_eq!(
            bounds(&connection).unwrap(),
            (10, 9),
            "restart cannot silently reset a stale cursor"
        );
    }
    #[test]
    fn event_global_page_work_is_independent_of_unrelated_history() {
        use rusqlite::StatementStatus;
        let mut costs = Vec::new();
        for population in [1_000u64, 10_000, 100_000] {
            let mut connection = legacy(0);
            connection.execute_batch("DROP TABLE events").unwrap();
            initialize(&connection).unwrap();
            let transaction = connection.transaction().unwrap();
            transaction.execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
                 INSERT INTO claims(store_index,id,kind,subject,body)
                 SELECT x,printf('claim-%d',x),'custom.test.recorded',
                    CASE WHEN x>?1-3 THEN 'custom/test/target' ELSE 'custom/test/other' END,'{}' FROM n",
                [population],
            ).unwrap();
            transaction
                .execute(
                    "INSERT INTO event_positions SELECT store_index,subject FROM claims",
                    [],
                )
                .unwrap();
            // Replay the old table's subject query as a negative control on the same rows.
            transaction.execute_batch("CREATE TABLE legacy_cost_events(store_index INTEGER PRIMARY KEY,kind TEXT,subject TEXT,body TEXT); INSERT INTO legacy_cost_events SELECT store_index,kind,subject,body FROM claims").unwrap();
            let mut page = transaction
                .prepare(
                    "SELECT store_index,kind,subject,body FROM events
                 WHERE store_index>?1 ORDER BY store_index LIMIT ?2",
                )
                .unwrap();
            let rows = page
                .query_map(params![0, 4], |row| row.get::<_, u64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(rows, [1, 2, 3, 4]);
            let bounded = page.get_status(StatementStatus::VmStep);
            assert_eq!(page.get_status(StatementStatus::FullscanStep), 0);
            drop(page);
            let mut old = transaction
                .prepare(
                    "SELECT store_index,kind,subject,body FROM legacy_cost_events
                 WHERE subject=?1 AND store_index>?2 ORDER BY store_index LIMIT ?3",
                )
                .unwrap();
            assert_eq!(
                old.query_map(params!["custom/test/target", 0, 4], |row| row
                    .get::<_, u64>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap(),
                [population - 2, population - 1, population]
            );
            let legacy = old.get_status(StatementStatus::VmStep);
            // A rowid range is reported as a seek even when its subject predicate inspects
            // the entire suffix. VM work, not the full-scan counter, exposes that old cost.
            assert!(legacy > population as i32);
            costs.push((bounded, legacy));
        }
        assert!(
            costs.iter().all(|(bounded, _)| *bounded == costs[0].0),
            "{costs:?}"
        );
        assert!(
            costs[2].1 > costs[0].1 * 50,
            "the old query must expose history growth: {costs:?}"
        );
    }
    #[test]
    fn event_upgrade_of_pretrimmed_checkpointed_store_requires_explicit_resync() {
        let mut connection = legacy(5);
        connection
            .execute_batch(
                "CREATE TABLE checkpoint_claims(claim_id TEXT PRIMARY KEY);
             INSERT INTO checkpoint_claims VALUES('claim-2');
             DELETE FROM events WHERE store_index<=6;
             DELETE FROM claims WHERE store_index=6;",
            )
            .unwrap();
        let expected = rows(&connection);
        initialize(&connection).unwrap();
        assert_eq!(bounds(&connection).unwrap(), (16, 15));
        let transaction = connection.transaction().unwrap();
        assert_eq!(migrate_chunk_tx(&transaction).unwrap(), 3);
        transaction.commit().unwrap();
        initialize(&connection).unwrap();
        assert_eq!(rows(&connection), expected);
        assert_eq!(
            bounds(&connection).unwrap(),
            (16, 15),
            "migration completion and reopen never reset the upgrade floor"
        );
    }
}
