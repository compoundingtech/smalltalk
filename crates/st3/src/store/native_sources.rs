//! Host-local source indexes, not an authorization projection. Membership is
//! read directly from authority; no aggregate work is added to source writes.
use super::*;

const CURSOR_SECRET_KEY: &str = "native_cursor_secret";

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let secret: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        [CURSOR_SECRET_KEY],
        |row| row.get(0),
    )?;
    if !secret {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        transaction.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)",
            params![CURSOR_SECRET_KEY, hex::encode(bytes)],
        )?;
    }
    // The unmerged predecessor caches hold no authority. Retain the secret,
    // discard both aggregate versions, and install indexes in this transaction.
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS native_source_claims_insert;
         DROP TRIGGER IF EXISTS native_source_claims_delete;
         DROP TRIGGER IF EXISTS native_source_local_observations_insert;
         DROP TRIGGER IF EXISTS native_source_local_observations_delete;
         DROP TRIGGER IF EXISTS native_source_repair_insert;
         DROP TRIGGER IF EXISTS native_source_repair_delete;
         DROP TRIGGER IF EXISTS native_source_v2_claims_seal;
         DROP TRIGGER IF EXISTS native_source_v2_claims_insert;
         DROP TRIGGER IF EXISTS native_source_v2_claims_historical;
         DROP TRIGGER IF EXISTS native_source_v2_claims_delete;
         DROP TRIGGER IF EXISTS native_source_v2_local_observations_seal;
         DROP TRIGGER IF EXISTS native_source_v2_local_observations_insert;
         DROP TRIGGER IF EXISTS native_source_v2_local_observations_historical;
         DROP TRIGGER IF EXISTS native_source_v2_local_observations_delete;
         DROP TRIGGER IF EXISTS native_source_v2_repair_insert;
         DROP TRIGGER IF EXISTS native_source_v2_repair_delete;
         DROP TABLE IF EXISTS native_source_ranges;
         DROP TABLE IF EXISTS native_source_ranges_v2;
         DELETE FROM meta WHERE key IN ('native_source_ranges_v1','native_source_ranges_v2');
         CREATE INDEX IF NOT EXISTS native_claims_cover
         ON claims(subject,store_index,id,kind,actor);
         CREATE INDEX IF NOT EXISTS native_local_cover
         ON local_observations(subject,id,kind,actor,after_store_index);
         CREATE INDEX IF NOT EXISTS local_observations_graph_position
         ON local_observations(after_store_index,id);",
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Membership {
    pub count: u64,
    pub first: Option<u64>,
    pub last: Option<u64>,
}

// Local observations are appended at a nondecreasing graph index under the
// writer lock. Intersect both original fences by an indexed graph-position seek.
pub(super) fn local_cutoff(connection: &Connection, fence: &NativeSourceFence) -> Result<u64> {
    let graph_cutoff: u64 = connection.query_row(
        "SELECT COALESCE((SELECT id FROM local_observations
         WHERE after_store_index<=?1 ORDER BY after_store_index DESC,id DESC LIMIT 1),0)",
        [fence.graph_index],
        |row| row.get(0),
    )?;
    Ok(graph_cutoff.min(fence.local_position))
}

/// Per-store HMAC key; a missing key is an internal error, not a public fallback.
pub(super) fn cursor_secret(connection: &Connection) -> Result<Vec<u8>> {
    let secret: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key=?1",
            [CURSOR_SECRET_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let secret = secret.ok_or_else(|| anyhow::anyhow!("native cursor secret is missing"))?;
    Ok(hex::decode(secret)?)
}

pub(super) struct SourceSelection<'a> {
    pub subject: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub lower: &'a str,
    pub end: &'a str,
    pub recorded_actor: Option<&'a str>,
}

pub(super) fn membership(
    connection: &Connection,
    source: u8,
    upper: u64,
    selection: &SourceSelection<'_>,
) -> Result<Membership> {
    // These four shapes stay static and cached; a concrete subject must use an
    // equality seek rather than an optional-predicate family scan.
    macro_rules! membership_sql {
        ($table:literal, $position:literal, $index:literal, $subject:literal, $admitted:literal) => {
            concat!(
                "SELECT COUNT(*),MIN(record.", $position, "),MAX(record.", $position, ")
                 FROM ", $table, " record INDEXED BY ", $index, "
                 WHERE record.", $position, "<=?1 AND ", $subject, "
                   AND (?5 IS NULL OR record.kind=?5)
                   AND (?6 IS NULL OR record.actor=?6)
                   AND (?5 IS NULL OR ?6 IS NULL) ", $admitted
            )
        };
    }
    let query = match (source == 0, selection.subject.is_some()) {
        (true, true) => membership_sql!(
            "claims", "store_index", "native_claims_cover", "record.subject=?2",
            "AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
                            WHERE repaired.id=record.id)"
        ),
        (true, false) => membership_sql!(
            "claims", "store_index", "native_claims_cover", "record.subject>=?3 AND record.subject<?4",
            "AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
                            WHERE repaired.id=record.id)"
        ),
        (false, true) => membership_sql!(
            "local_observations", "id", "native_local_cover", "record.subject=?2", ""
        ),
        (false, false) => membership_sql!(
            "local_observations", "id", "native_local_cover", "record.subject>=?3 AND record.subject<?4", ""
        ),
    };
    Ok(connection.prepare_cached(query)?.query_row(
        params![upper, selection.subject, selection.lower, selection.end,
                selection.kind, selection.recorded_actor],
        |row| Ok(Membership { count: row.get(0)?, first: row.get(1)?, last: row.get(2)? }),
    )?)
}

/// Family cursors need only a count, not retained extrema. Subtract eligible
/// repaired claims from the covering count instead of probing the repair table
/// once per source row. The unique claim/repair ids make the subtraction exact;
/// absent claims and repairs outside this selector contribute nothing.
pub(super) fn family_count(
    connection: &Connection,
    source: u8,
    upper: u64,
    lower: &str,
    end: &str,
    recorded_actor: Option<&str>,
) -> Result<u64> {
    let query = if source == 0 {
        "SELECT
           (SELECT COUNT(*) FROM claims record INDEXED BY native_claims_cover
            WHERE record.subject>=?1 AND record.subject<?2 AND record.store_index<=?3
              AND (?4 IS NULL OR record.actor=?4)) -
           (SELECT COUNT(*) FROM projection_digest_repaired_claims repaired
            CROSS JOIN claims record
            WHERE record.id=repaired.id AND record.subject>=?1 AND record.subject<?2
              AND record.store_index<=?3 AND (?4 IS NULL OR record.actor=?4))"
    } else {
        "SELECT COUNT(*) FROM local_observations record INDEXED BY native_local_cover
         WHERE record.subject>=?1 AND record.subject<?2 AND record.id<=?3
           AND (?4 IS NULL OR record.actor=?4)"
    };
    Ok(connection.prepare_cached(query)?.query_row(
        params![lower, end, upper, recorded_actor], |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT UNIQUE NOT NULL,subject TEXT NOT NULL,kind TEXT NOT NULL,actor TEXT);
             CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);
             CREATE TABLE local_observations(id INTEGER PRIMARY KEY AUTOINCREMENT,
                 after_store_index INTEGER NOT NULL,subject TEXT NOT NULL,kind TEXT NOT NULL,actor TEXT);",
        ).unwrap();
        connection
    }

    fn initialize(connection: &mut Connection) {
        let transaction = connection.transaction().unwrap();
        open(&transaction).unwrap();
        transaction.commit().unwrap();
    }

    fn insert(connection: &Connection, position: u64) {
        let subject = if position.is_multiple_of(2) { "resource/selected/a" } else { "resource/selected/z" };
        let kind = if position.is_multiple_of(3) { "resource.observed" } else { "other.kind" };
        let actor = (!position.is_multiple_of(5)).then_some(if position.is_multiple_of(2) { "person/a" } else { "person/other" });
        connection.execute(
            "INSERT INTO claims VALUES(?1,?2,?3,?4,?5)",
            params![position, format!("claim-{position}"), subject, kind, actor],
        ).unwrap();
        connection.execute(
            "INSERT INTO local_observations VALUES(?1,?1,?2,?3,?4)",
            params![position, subject, kind, actor],
        ).unwrap();
    }

    fn assert_membership(connection: &Connection) {
        for source in 0..=1 {
            let (table, position, admitted) = if source == 0 {
                ("claims", "store_index", "AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims r WHERE r.id=record.id)")
            } else {
                ("local_observations", "id", "")
            };
            for upper in [0, 1, 14, 15, 16, 17, 254, 255, 256, 269, 499, (1_u64 << 40) - 1, 1_u64 << 40, (1_u64 << 40) + 1, i64::MAX as u64] {
                for subject in [None, Some("resource/selected/a"), Some("resource/selected/z"), Some("resource/missing")] {
                    for (kind, recorded_actor) in [(None, None), (Some("resource.observed"), None), (Some("missing.kind"), None), (None, Some("person/a")), (None, Some("person/other")), (None, Some("person/intruder"))] {
                        let expected = connection.query_row(
                            &format!(
                                "SELECT COUNT(*),MIN(record.{position}),MAX(record.{position})
                                 FROM {table} record WHERE record.{position}<=?1
                                   AND record.subject>='resource/' AND record.subject<'resource0'
                                   AND (?2 IS NULL OR record.subject=?2)
                                   AND (?3 IS NULL OR record.kind=?3)
                                   AND (?4 IS NULL OR record.actor=?4) {admitted}"
                            ),
                            params![upper, subject, kind, recorded_actor],
                            |row| Ok(Membership { count: row.get(0)?, first: row.get(1)?, last: row.get(2)? }),
                        ).unwrap();
                        assert_eq!(
                            membership(connection, source, upper, &SourceSelection {
                                subject, kind, recorded_actor,
                                lower: if subject.is_some() { "" } else { "resource/" },
                                end: if subject.is_some() { "" } else { "resource0" },
                            }).unwrap(),
                            expected,
                            "source={source} upper={upper} subject={subject:?} kind={kind:?} actor={recorded_actor:?}"
                        );
                        if subject.is_none() && kind.is_none() {
                            assert_eq!(
                                family_count(connection, source, upper, "resource/", "resource0", recorded_actor).unwrap(),
                                expected.count
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn indexes_preserve_sparse_fences_prune_repair_and_readmission() {
        let mut connection = source_connection();
        initialize(&mut connection);
        for position in (1..=270).chain([1_u64 << 40, (1_u64 << 40) + 1, i64::MAX as u64]) {
            let before = connection.total_changes();
            insert(&connection, position);
            assert_eq!(connection.total_changes() - before, 2, "no aggregate row writes");
        }
        assert_membership(&connection);
        for position in [1, 15, 16, 255, 269, i64::MAX as u64] {
            connection.execute("DELETE FROM claims WHERE store_index=?1", [position]).unwrap();
            connection.execute("DELETE FROM local_observations WHERE id=?1", [position]).unwrap();
        }
        assert_membership(&connection);
        for position in [17, 256, (1_u64 << 40) + 1] {
            connection.execute(
                "INSERT INTO projection_digest_repaired_claims VALUES(?1)",
                [format!("claim-{position}")],
            ).unwrap();
        }
        connection.execute("INSERT INTO projection_digest_repaired_claims VALUES('absent')", []).unwrap();
        assert_membership(&connection);
        connection.execute("DELETE FROM projection_digest_repaired_claims", []).unwrap();
        assert_membership(&connection);
        connection.execute("DELETE FROM local_observations", []).unwrap();
        assert_membership(&connection);
    }

    #[test]
    fn cache_cutover_preserves_authority_and_secret_without_installing_triggers() {
        for predecessor in [None, Some(1), Some(2)] {
            let mut connection = source_connection();
            for position in 1..=270 { insert(&connection, position); }
            connection.execute(
                "INSERT INTO meta(key,value) VALUES('native_cursor_secret',?1)",
                ["ab".repeat(32)],
            ).unwrap();
            if let Some(version) = predecessor {
                let (table, trigger, key) = if version == 1 {
                    ("native_source_ranges", "native_source_claims_insert", "native_source_ranges_v1")
                } else {
                    ("native_source_ranges_v2", "native_source_v2_claims_insert", "native_source_ranges_v2")
                };
                connection.execute_batch(&format!(
                    "CREATE TABLE {table}(count INTEGER);
                     INSERT INTO {table} VALUES(270);
                     CREATE TRIGGER {trigger} AFTER INSERT ON claims
                     BEGIN UPDATE {table} SET count=count+1; END;"
                )).unwrap();
                connection.execute("INSERT INTO meta VALUES(?1,'1')", [key]).unwrap();
            }
            initialize(&mut connection);
            assert_eq!(cursor_secret(&connection).unwrap(), vec![0xab; 32]);
            assert_membership(&connection);
            let obsolete: u64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name GLOB 'native_source*'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(obsolete, 0);
            initialize(&mut connection);
            assert_eq!(cursor_secret(&connection).unwrap(), vec![0xab; 32]);
            assert_membership(&connection);
        }
    }
}
