//! Transactional, constant-work row changes and bounded-work digest reads.
//!
//! Each logical row contributes a domain-separated SHA-512 value to a 512-bit modular sum.
//! Count and sum commit the multiset independently of insertion order. SHA-256 commits each
//! table's name, column schema, count and sum, then the sorted table digests commit the graph.
//! These diagnostic digests do not replace authenticated envelope hashes and signatures.
use super::*;
use rusqlite::functions::FunctionFlags;
use sha2::Sha512;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projection_digest_state (
    table_name TEXT PRIMARY KEY,
    columns_json TEXT NOT NULL,
    row_count INTEGER NOT NULL CHECK(row_count>=0),
    accumulator BLOB NOT NULL CHECK(length(accumulator)=64)
);
CREATE TABLE IF NOT EXISTS projection_digest_operation_rows (
    operation_id TEXT PRIMARY KEY, row_json TEXT NOT NULL
);
-- Repair meaning outlives receipt rows removed by a checkpoint. This local exclusion cache
-- is covered by the authenticated record.repaired claims, not by its physical row inventory.
CREATE TABLE IF NOT EXISTS projection_digest_repaired_claims (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS projection_digest_generation (
    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL
);
INSERT OR IGNORE INTO projection_digest_generation VALUES(1,0);";

pub fn row_hash(table: &str, columns: &str, row: &str) -> [u8; 64] {
    let mut hash = Sha512::new();
    hash.update(b"st3-projection-row-v1\0");
    for value in [table, columns, row] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

pub fn add(accumulator: &mut [u8; 64], hash: &[u8; 64], subtract: bool) {
    let mut carry = u16::from(subtract);
    for (sum, byte) in accumulator.iter_mut().zip(hash).rev() {
        let byte = if subtract { !byte } else { *byte };
        carry += u16::from(*sum) + u16::from(byte);
        *sum = carry as u8;
        carry >>= 8;
    }
}

/// Register before any writes on every writer connection, including checkpoint proof copies.
pub fn register(connection: &Connection) -> Result<()> {
    // REPLACE deletes its previous row; digest that delete as well as the insertion.
    connection.execute_batch("PRAGMA recursive_triggers=ON;")?;
    connection.create_scalar_function(
        "st_projection_change",
        6,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let bytes = context.get::<Vec<u8>>(0)?;
            let mut accumulator: [u8; 64] = bytes.try_into().map_err(|_| {
                rusqlite::Error::UserFunctionError("invalid projection accumulator".into())
            })?;
            let table = context.get::<String>(1)?;
            let columns = context.get::<String>(2)?;
            for (index, subtract) in [(3, true), (4, false)] {
                if let Some(row) = context.get::<Option<String>>(index)? {
                    add(
                        &mut accumulator,
                        &row_hash(&table, &columns, &row),
                        subtract,
                    );
                }
            }
            // The sixth argument fences future algorithm versions in persisted triggers.
            if context.get::<i64>(5)? != 1 {
                return Err(rusqlite::Error::UserFunctionError(
                    "unknown projection digest version".into(),
                ));
            }
            Ok(accumulator.to_vec())
        },
    )?;
    Ok(())
}

pub fn columns(connection: &Connection, table: &str, excluded: &[&str]) -> Result<Vec<String>> {
    let mut columns = connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|column| !excluded.contains(&column.as_str()))
        .collect::<Vec<_>>();
    // An additive migration appends a column, while a fresh schema may declare it
    // in the middle. Physical layout must not change the logical row encoding.
    columns.sort();
    Ok(columns)
}

pub fn row_sql(columns: &[String], prefix: &str) -> String {
    let values = columns
        .iter()
        .map(|column| {
            // SQLite JSON does not accept blobs; their complete bytes are shared data.
            if matches!(column.as_str(), "bytes" | "binding_key") {
                format!("hex({prefix}{column})")
            } else {
                // JSON functions attach a transient subtype to TEXT. NEW values can retain
                // it in triggers, while a later table scan sees plain stored TEXT. Strip it
                // without changing the persisted scalar type or the logical row encoding.
                let value = format!("{prefix}{column}");
                format!("CASE WHEN typeof({value})='text' THEN {value}||'' ELSE {value} END")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("json_array({values})")
}

pub fn change_sql(table: &str, old: &str, new: &str, delta: i32) -> String {
    format!(
        "UPDATE projection_digest_state SET row_count=row_count+({delta}),
        accumulator=st_projection_change(accumulator,table_name,columns_json,{old},{new},1)
        WHERE table_name='{table}';
        UPDATE projection_digest_generation SET value=value+1 WHERE id=1;"
    )
}

/// Initialize once per registry/schema version. Subsequent opens inspect table schemas only.
pub fn initialize(connection: &Connection, tables: &[(&str, &[&str])]) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    let registry = tables
        .iter()
        .map(|(table, excluded)| Ok((*table, columns(connection, table, excluded)?)))
        .collect::<Result<Vec<_>>>()?;
    // Rebuild v6 triggers and cached sums that could hash transient JSON subtypes.
    let signature = serde_json::to_string(&(7, &registry))?;
    let previous: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key='projection_digest_registry'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let state_rows: usize =
        connection.query_row("SELECT COUNT(*) FROM projection_digest_state", [], |row| {
            row.get(0)
        })?;
    let trigger_rows: usize=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'projection_digest_%'",[],|row|row.get(0))?;
    if previous.as_deref() == Some(&signature)
        && state_rows == tables.len() + 1
        && trigger_rows == 3 * (tables.len() - 1) + 8 + 8 + 4
    {
        return Ok(());
    }
    let transaction = connection.unchecked_transaction()?;
    let triggers = transaction.prepare(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'projection_digest_%'"
    )?.query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for trigger in triggers {
        transaction.execute_batch(&format!("DROP TRIGGER {trigger}"))?;
    }
    transaction.execute(
        "INSERT OR IGNORE INTO projection_digest_repaired_claims
        SELECT claim_id FROM replica_records WHERE state='repaired' AND claim_id IS NOT NULL",
        [],
    )?;
    transaction.execute("DELETE FROM projection_digest_state", [])?;
    transaction.execute("DELETE FROM projection_digest_operation_rows", [])?;
    for (table, columns) in &registry {
        let encoded = serde_json::to_string(columns)?;
        let row = row_sql(columns, "");
        let query = if *table == "operations" {
            operation_rows()
        } else {
            format!("SELECT {row} FROM {table}")
        };
        seed(&transaction, table, &encoded, &query)?;
        if *table == "operations" {
            operation_triggers(&transaction, columns)?;
            continue;
        }
        let old = row_sql(columns, "OLD.");
        let new = row_sql(columns, "NEW.");
        let changed = columns
            .iter()
            .map(|column| format!("OLD.{column} IS NOT NEW.{column}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        transaction.execute_batch(&format!(
            "
            CREATE TRIGGER projection_digest_{table}_insert AFTER INSERT ON {table}
            BEGIN {} END;
            CREATE TRIGGER projection_digest_{table}_update AFTER UPDATE ON {table}
            WHEN {changed} BEGIN {} END;
            CREATE TRIGGER projection_digest_{table}_delete AFTER DELETE ON {table}
            BEGIN {} END;",
            change_sql(table, "NULL", &new, 1),
            change_sql(table, &old, &new, 0),
            change_sql(table, &old, "NULL", -1)
        ))?;
    }
    // One immutable claim identity covers its authenticated body, including the source facts
    // of on-demand views. A retained claim and its checkpoint tombstone are one logical row.
    seed(
        &transaction,
        "claim_sources",
        "[\"id\",\"accepted_at_unix_ms\"]",
        source_query(),
    )?;
    for (table, other) in [
        ("claims", "checkpoint_claims"),
        ("checkpoint_claims", "claims"),
    ] {
        transaction.execute_batch(&format!(
            "
            CREATE TRIGGER projection_digest_{table}_insert AFTER INSERT ON {table}
            WHEN NOT EXISTS(SELECT 1 FROM {other} WHERE id=NEW.id)
              AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=NEW.id)
            BEGIN {} END;
            CREATE TRIGGER projection_digest_{table}_delete AFTER DELETE ON {table}
            WHEN NOT EXISTS(SELECT 1 FROM {other} WHERE id=OLD.id)
              AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=OLD.id)
            BEGIN {} END;",
            change_sql(
                "claim_sources",
                "NULL",
                "json_array(NEW.id,CAST(NEW.accepted_at_unix_ms AS TEXT))",
                1
            ),
            change_sql(
                "claim_sources",
                "json_array(OLD.id,CAST(OLD.accepted_at_unix_ms AS TEXT))",
                "NULL",
                -1
            )
        ))?;
    }
    for table in ["claims", "checkpoint_claims"] {
        let visible = if table == "claims" {
            "1"
        } else {
            "NOT EXISTS(SELECT 1 FROM claims WHERE id=NEW.id)"
        };
        transaction.execute_batch(&format!("CREATE TRIGGER projection_digest_{table}_time_update
            AFTER UPDATE OF accepted_at_unix_ms ON {table}
            WHEN {visible} AND OLD.accepted_at_unix_ms IS NOT NEW.accepted_at_unix_ms
              AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=NEW.id) BEGIN {} END;
            CREATE TRIGGER projection_digest_{table}_identity_update BEFORE UPDATE OF id ON {table}
            WHEN OLD.id IS NOT NEW.id BEGIN SELECT RAISE(ABORT,'claim source identity is immutable'); END;",
            change_sql("claim_sources","json_array(OLD.id,CAST(OLD.accepted_at_unix_ms AS TEXT))",
                "json_array(NEW.id,CAST(NEW.accepted_at_unix_ms AS TEXT))",0)))?;
    }
    repair_source_triggers(&transaction)?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('projection_digest_registry',?1)",
        [&signature],
    )?;
    transaction.execute(
        "UPDATE projection_digest_generation SET value=value+1 WHERE id=1",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

/// The hot operations table names a retained claim because of its foreign key. Once every
/// claim of an operation is trimmed, its complete logical row is instead in tombstones.
pub fn operation_fallback(operation: &str) -> String {
    format!("(SELECT json_array(
        (SELECT MIN(c.id) FROM checkpoint_claims c WHERE c.operation_id={operation}
          AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=c.id)
          AND c.request_digest=(SELECT MIN(m.request_digest) FROM checkpoint_claims m WHERE m.operation_id={operation}
            AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=m.id))),
        operation_id,MIN(request_digest),
        CASE WHEN COUNT(DISTINCT request_digest)>1 THEN 'conflict' ELSE 'active' END)
        FROM checkpoint_claims WHERE operation_id={operation}
          AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=checkpoint_claims.id)
        GROUP BY operation_id)")
}

pub fn operation_rows() -> String {
    format!(
        "SELECT json_array(canonical_claim_id,id,request_digest,state) AS row_json FROM operations
        UNION ALL SELECT {} FROM
        (SELECT DISTINCT operation_id FROM checkpoint_claims WHERE operation_id IS NOT NULL
          AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=checkpoint_claims.id)) dropped
        WHERE NOT EXISTS(SELECT 1 FROM operations WHERE id=dropped.operation_id)",
        operation_fallback("dropped.operation_id")
    )
}

pub fn refresh_operation(operation: &str) -> String {
    let previous = format!(
        "(SELECT row_json FROM projection_digest_operation_rows WHERE operation_id={operation})"
    );
    let current = format!(
        "COALESCE((SELECT json_array(canonical_claim_id,id,request_digest,state) FROM operations WHERE id={operation}),{})",
        operation_fallback(operation)
    );
    format!("UPDATE projection_digest_state SET
        row_count=row_count+({current} IS NOT NULL)-({previous} IS NOT NULL),
        accumulator=st_projection_change(accumulator,table_name,columns_json,{previous},{current},1)
        WHERE table_name='operations' AND {previous} IS NOT {current};
        UPDATE projection_digest_generation SET value=value+1 WHERE id=1 AND changes()>0;
        INSERT INTO projection_digest_operation_rows(operation_id,row_json)
            SELECT {operation},{current} WHERE {current} IS NOT NULL
            ON CONFLICT(operation_id) DO UPDATE SET row_json=excluded.row_json
            WHERE row_json IS NOT excluded.row_json;
        DELETE FROM projection_digest_operation_rows WHERE operation_id={operation} AND {current} IS NULL;")
}

pub fn operation_triggers(connection: &Connection, columns: &[String]) -> Result<()> {
    connection.execute(
        &format!(
            "INSERT INTO projection_digest_operation_rows
        SELECT json_extract(row_json,'$[1]'),row_json FROM ({}) AS rows",
            operation_rows()
        ),
        [],
    )?;
    let changed = columns
        .iter()
        .map(|column| format!("OLD.{column} IS NOT NEW.{column}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    for (table, key) in [("operations", "id"), ("checkpoint_claims", "operation_id")] {
        for (event, prefixes) in [
            ("INSERT", vec!["NEW"]),
            ("DELETE", vec!["OLD"]),
            ("UPDATE", vec!["OLD", "NEW"]),
        ] {
            for (index, prefix) in prefixes.iter().enumerate() {
                let operation = format!("{prefix}.{key}");
                let distinct = if index == 1 {
                    format!(" AND NEW.{key} IS NOT OLD.{key}")
                } else {
                    String::new()
                };
                let when = if table == "operations" {
                    if event == "UPDATE" {
                        format!("({changed}){distinct}")
                    } else {
                        "1".to_owned()
                    }
                } else {
                    format!(
                        "{operation} IS NOT NULL AND NOT EXISTS(SELECT 1 FROM operations WHERE id={operation}){distinct}"
                    )
                };
                connection.execute_batch(&format!(
                    "CREATE TRIGGER projection_digest_operation_{table}_{event}_{prefix}
                    AFTER {event} ON {table} WHEN {when} BEGIN {} END;",
                    refresh_operation(&operation)
                ))?;
            }
        }
    }
    Ok(())
}

pub fn source_query() -> &'static str {
    "SELECT json_array(id,accepted_at_unix_ms) FROM (
        SELECT id,accepted_at_unix_ms FROM claims UNION ALL
        SELECT id,CAST(accepted_at_unix_ms AS TEXT) FROM checkpoint_claims
        WHERE NOT EXISTS(SELECT 1 FROM claims WHERE claims.id=checkpoint_claims.id)) sources
        WHERE NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=sources.id)"
}

pub fn repair_source_triggers(connection: &Connection) -> Result<()> {
    for event in ["INSERT", "UPDATE OF state,claim_id"] {
        let suffix = if event == "INSERT" {
            "insert"
        } else {
            "update"
        };
        connection.execute_batch(&format!("CREATE TRIGGER projection_digest_repair_record_{suffix}
            AFTER {event} ON replica_records WHEN NEW.state='repaired' AND NEW.claim_id IS NOT NULL
            BEGIN INSERT OR IGNORE INTO projection_digest_repaired_claims VALUES(NEW.claim_id); END;"))?;
    }
    for (event, prefix, subtract) in [("INSERT", "NEW", true), ("DELETE", "OLD", false)] {
        let row=format!("(SELECT json_array(id,CAST(accepted_at_unix_ms AS TEXT)) FROM claims WHERE id={prefix}.id
            UNION ALL SELECT json_array(id,CAST(accepted_at_unix_ms AS TEXT)) FROM checkpoint_claims
              WHERE id={prefix}.id AND NOT EXISTS(SELECT 1 FROM claims WHERE id={prefix}.id) LIMIT 1)");
        let operation=format!("COALESCE((SELECT json_extract(body,'$._operation.id') FROM claims WHERE id={prefix}.id),
            (SELECT operation_id FROM checkpoint_claims WHERE id={prefix}.id))");
        connection.execute_batch(&format!("CREATE TRIGGER projection_digest_repaired_claims_{event}
            AFTER {event} ON projection_digest_repaired_claims WHEN {row} IS NOT NULL BEGIN {} {} END;",
            if subtract {change_sql("claim_sources",&row,"NULL",-1)} else {change_sql("claim_sources","NULL",&row,1)},
            refresh_operation(&operation)))?;
    }
    Ok(())
}

pub fn scan(
    connection: &Connection,
    table: &str,
    columns: &str,
    query: &str,
) -> Result<(u64, [u8; 64])> {
    let mut accumulator = [0; 64];
    let mut count = 0;
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        add(
            &mut accumulator,
            &row_hash(table, columns, &row.get::<_, String>(0)?),
            false,
        );
        count += 1;
    }
    Ok((count, accumulator))
}

pub fn seed(connection: &Connection, table: &str, columns: &str, query: &str) -> Result<()> {
    let (count, accumulator) = scan(connection, table, columns, query)?;
    connection.execute(
        "INSERT INTO projection_digest_state VALUES(?1,?2,?3,?4)",
        params![table, columns, count, accumulator.as_slice()],
    )?;
    Ok(())
}

pub fn table_digest(table: &str, columns: &str, count: u64, accumulator: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"st3-projection-table-v1\0");
    for value in [table.as_bytes(), columns.as_bytes()] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    hash.update(count.to_be_bytes());
    hash.update(accumulator);
    hex::encode(hash.finalize())
}

pub fn generation(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        "SELECT value FROM projection_digest_generation WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}

pub fn tables(connection: &Connection) -> Result<BTreeMap<String, String>> {
    let mut statement = connection.prepare(
        "SELECT table_name,columns_json,row_count,accumulator FROM projection_digest_state ORDER BY table_name")?;
    Ok(statement
        .query_map([], |row| {
            let table = row.get::<_, String>(0)?;
            let digest = table_digest(
                &table,
                &row.get::<_, String>(1)?,
                row.get(2)?,
                &row.get::<_, Vec<u8>>(3)?,
            );
            Ok((table, digest))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn root(tables: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"st3-projection-graph-v1\0");
    for (table, digest) in tables {
        hash.update((table.len() as u64).to_be_bytes());
        hash.update(table.as_bytes());
        hash.update(digest.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub fn differing(
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
) -> Vec<String> {
    local
        .keys()
        .chain(remote.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|table| local.get(*table) != remote.get(*table))
        .cloned()
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
pub fn oracle(
    connection: &Connection,
    tables: &[(&str, &[&str])],
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (table, excluded) in tables {
        let columns = columns(connection, table, excluded)?;
        let encoded = serde_json::to_string(&columns)?;
        let (count, accumulator) = scan(
            connection,
            table,
            &encoded,
            &if *table == "operations" {
                operation_rows()
            } else {
                format!("SELECT {} FROM {table}", row_sql(&columns, ""))
            },
        )?;
        result.insert(
            (*table).to_owned(),
            table_digest(table, &encoded, count, &accumulator),
        );
    }
    let (count, accumulator) = scan(
        connection,
        "claim_sources",
        "[\"id\",\"accepted_at_unix_ms\"]",
        source_query(),
    )?;
    result.insert(
        "claim_sources".into(),
        table_digest(
            "claim_sources",
            "[\"id\",\"accepted_at_unix_ms\"]",
            count,
            &accumulator,
        ),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_rows_match_stored_scalars_for_json_producing_writes() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE fixture(text_value TEXT, integer_value INTEGER, real_value REAL, null_value TEXT, bytes BLOB);
             CREATE TABLE captured(row_json TEXT);",
        ).unwrap();
        let columns = [
            "text_value",
            "integer_value",
            "real_value",
            "null_value",
            "bytes",
        ]
        .map(str::to_owned);
        for (event, prefix) in [("INSERT", "NEW."), ("UPDATE", "NEW."), ("DELETE", "OLD.")] {
            connection
                .execute_batch(&format!(
                    "CREATE TRIGGER capture_{event} AFTER {event} ON fixture BEGIN
                   INSERT INTO captured VALUES({}); END;",
                    row_sql(&columns, prefix),
                ))
                .unwrap();
        }
        for json in ["{}", "[1,2]", "true", "null", "7", "2.5", "\"signal\""] {
            connection
                .execute(
                    "INSERT INTO fixture VALUES(json(?1),7,2.5,NULL,x'0102')",
                    [json],
                )
                .unwrap();
            let stored: String = connection
                .query_row(
                    &format!("SELECT {} FROM fixture", row_sql(&columns, "")),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&stored).unwrap(),
                serde_json::json!([json, 7, 2.5, null, "0102"]),
            );
            let captured = || {
                connection
                    .query_row(
                        "SELECT row_json FROM captured ORDER BY rowid DESC LIMIT 1",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap()
            };
            assert_eq!(captured(), stored, "INSERT {json}");
            connection
                .execute("UPDATE fixture SET text_value=json(?1)", [json])
                .unwrap();
            assert_eq!(captured(), stored, "UPDATE {json}");
            connection.execute("DELETE FROM fixture", []).unwrap();
            assert_eq!(captured(), stored, "DELETE {json}");
        }
    }
}
