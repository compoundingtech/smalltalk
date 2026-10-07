//! Transactional old/new staging for explicitly covered st3 source tables.
//!
//! This is input capture for the foundation Installer, not another registry or installer.
//! It owns no Ready flag, source cut or published output. A runtime finalizer must consume
//! it in the same source transaction; an unconsumed/gapped capture cannot certify a read.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use smallclaims::ivm::install::Mutation;

pub mod scope;

const MAX_TABLES: usize = 32;
const MAX_COLUMNS: usize = 64;
const MAX_ROWS: u64 = 256;
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_PAYLOAD: u64 = 64 * 1024;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS st3_ivm_capture_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), fingerprint TEXT NOT NULL,
 epoch INTEGER NOT NULL, gap TEXT, rows INTEGER NOT NULL DEFAULT 0,
 bytes INTEGER NOT NULL DEFAULT 0,
 guarded INTEGER NOT NULL DEFAULT 0 CHECK(guarded IN (0,1)),
 managed INTEGER NOT NULL DEFAULT 0 CHECK(managed IN (0,1))
);
CREATE TABLE IF NOT EXISTS st3_ivm_capture (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT, source_table TEXT NOT NULL,
 payload TEXT NOT NULL, bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS st3_ivm_insert_before (
 source_table TEXT PRIMARY KEY, old_key TEXT, old_row TEXT
);
CREATE TRIGGER IF NOT EXISTS st3_ivm_capture_removed AFTER DELETE ON st3_ivm_capture BEGIN
 UPDATE st3_ivm_capture_state SET rows=rows-1,bytes=bytes-OLD.bytes WHERE singleton=1;
END;
"#;

/// An explicit source schema descriptor, supplied by the st3 dependency owner.
/// All columns and the entire declared primary key must be captured. Alternate unique
/// constraints, expression/partial keys and virtual tables are not certified by this lane.
pub struct Table {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub key: &'static [&'static str],
}

#[derive(Debug)]
pub struct Status {
    pub fingerprint: String,
    pub epoch: u64,
    pub gap: Option<String>,
    pub pending_rows: u64,
    pub pending_bytes: u64,
}

#[derive(Debug)]
pub struct Captured {
    pub sequence: u64,
    pub table: String,
    /// A key change becomes a retraction followed by insertion, never losing the old owner.
    pub replacements: Vec<Mutation>,
}

fn identifier(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid source SQL identifier"
    );
    Ok(format!("\"{name}\""))
}
fn scalar(prefix: &str, name: &str) -> Result<String> {
    let column = format!("{prefix}.{}", identifier(name)?);
    // JSON cannot encode SQLite blobs. The tag retains exact bytes without coercion.
    Ok(format!(
        "CASE WHEN typeof({column})='blob' THEN json_object('$blob',hex({column})) ELSE {column} END"
    ))
}
fn key(table: &Table, prefix: &str) -> Result<String> {
    let columns = table
        .key
        .iter()
        .map(|column| scalar(prefix, column))
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("json_array({})", columns.join(",")))
}
fn row(table: &Table, prefix: &str) -> Result<String> {
    let mut fields = Vec::new();
    for column in table.columns {
        fields.push(format!("'{column}'"));
        fields.push(scalar(prefix, column)?);
    }
    Ok(format!("json_object({})", fields.join(",")))
}
fn validate(connection: &Connection, table: &Table) -> Result<()> {
    identifier(table.name)?;
    ensure!(
        !table.name.starts_with("st3_ivm_")
            && (1..=MAX_COLUMNS).contains(&table.columns.len())
            && !table.key.is_empty(),
        "invalid capture table shape"
    );
    for name in table.columns.iter().chain(table.key) {
        identifier(name)?;
    }
    let sql: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
        [table.name],
        |row| row.get(0),
    )?;
    ensure!(
        !sql.to_ascii_uppercase().contains("CREATE VIRTUAL TABLE"),
        "virtual source tables require another extractor"
    );
    let mut stmt =
        connection.prepare(&format!("PRAGMA table_info({})", identifier(table.name)?))?;
    let fields = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, usize>(5)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let declared = table
        .columns
        .iter()
        .map(|name| name.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        declared.len() == table.columns.len()
            && fields
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<std::collections::BTreeSet<_>>()
                == declared,
        "capture must include every source column exactly once"
    );
    let mut primary = fields
        .into_iter()
        .filter(|(_, position)| *position > 0)
        .collect::<Vec<_>>();
    primary.sort_by_key(|(_, position)| *position);
    ensure!(
        primary
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            == table.key,
        "capture key must be the complete primary key"
    );
    let mut stmt =
        connection.prepare(&format!("PRAGMA index_list({})", identifier(table.name)?))?;
    let indexes = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        indexes
            .iter()
            .all(|(_, unique, origin)| !unique || origin == "pk"),
        "alternate unique conflicts require explicit old-row extraction"
    );
    Ok(())
}

/// Explicit installation only. Existing populated tables and changed fingerprints are
/// fenced for a separately proved bounded installation. Reopen never clears a gap.
pub fn install(
    tx: &Transaction<'_>,
    tables: &[Table],
    fingerprint: &str,
    epoch: u64,
) -> Result<()> {
    ensure!(
        (1..=MAX_TABLES).contains(&tables.len())
            && !fingerprint.is_empty()
            && epoch > 0
            && epoch <= i64::MAX as u64,
        "invalid capture identity"
    );
    let mut names = std::collections::BTreeSet::new();
    for table in tables {
        ensure!(names.insert(table.name), "duplicate source table");
        validate(tx, table)?;
    }
    tx.execute_batch(SCHEMA)?;
    // Explicit installations from the staging-only schema retain their gap and queue.
    // Adding scope metadata never attests coverage or restores a serving view.
    let fields = tx
        .prepare("PRAGMA table_info(st3_ivm_capture_state)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
    for field in ["guarded", "managed"] {
        if !fields.contains(field) {
            tx.execute_batch(&format!("ALTER TABLE st3_ivm_capture_state ADD COLUMN {field} INTEGER NOT NULL DEFAULT 0 CHECK({field} IN (0,1))"))?;
        }
    }
    let existing = tx
        .query_row(
            "SELECT fingerprint,epoch FROM st3_ivm_capture_state WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()?;
    if let Some((stored, stored_epoch)) = existing {
        if stored != fingerprint || stored_epoch != epoch {
            gap(
                tx,
                "capture identity changed; explicit replacement required",
            )?;
            return Ok(());
        }
    } else {
        tx.execute(
            "INSERT INTO st3_ivm_capture_state(singleton,fingerprint,epoch) VALUES(1,?1,?2)",
            params![fingerprint, epoch],
        )?;
        for table in tables {
            let populated: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM {} LIMIT 1)",
                    identifier(table.name)?
                ),
                [],
                |row| row.get(0),
            )?;
            if populated {
                gap(
                    tx,
                    "populated source requires explicit bounded installation",
                )?;
            }
        }
    }
    for table in tables {
        triggers(tx, table)?;
    }
    Ok(())
}

fn enqueue(table: &Table, old_key: &str, old: &str, new_key: &str, new: &str) -> String {
    let payload = format!(
        "json_object('old_key',json({old_key}),'old',json({old}),'new_key',json({new_key}),'new',json({new}))"
    );
    format!(
        r#"
 UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,'source mutation outside managed transaction')
 WHERE singleton=1 AND guarded=1 AND managed=0;
 UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,'source capture quota exceeded')
 WHERE singleton=1 AND (rows>={MAX_ROWS} OR length(CAST({payload} AS BLOB))>{MAX_PAYLOAD}
 OR bytes+length(CAST({payload} AS BLOB))>{MAX_BYTES});
 INSERT INTO st3_ivm_capture(source_table,payload,bytes)
 SELECT '{name}',{payload},length(CAST({payload} AS BLOB))
 WHERE (SELECT gap FROM st3_ivm_capture_state WHERE singleton=1) IS NULL
 ON CONFLICT DO NOTHING;
 UPDATE st3_ivm_capture_state SET rows=rows+1,bytes=bytes+length(CAST({payload} AS BLOB))
 WHERE singleton=1 AND gap IS NULL;
 "#,
        name = table.name
    )
}
fn triggers(tx: &Transaction<'_>, table: &Table) -> Result<()> {
    let name = identifier(table.name)?;
    let old_key = key(table, "OLD")?;
    let old = row(table, "OLD")?;
    let new_key = key(table, "NEW")?;
    let new = row(table, "NEW")?;
    let null_key = table
        .key
        .iter()
        .map(|column| format!("NEW.{} IS NULL", identifier(column).unwrap()))
        .collect::<Vec<_>>()
        .join(" OR ");
    let matching = table
        .key
        .iter()
        .map(|column| {
            format!(
                "before.{} IS NEW.{}",
                identifier(column).unwrap(),
                identifier(column).unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let before_key = key(table, "before")?;
    let before = row(table, "before")?;
    let inserted = enqueue(
        table,
        "(SELECT old_key FROM st3_ivm_insert_before WHERE source_table='SOURCE_TABLE')",
        "(SELECT old_row FROM st3_ivm_insert_before WHERE source_table='SOURCE_TABLE')",
        &new_key,
        &new,
    )
    .replace("SOURCE_TABLE", table.name);
    let updated = enqueue(table, &old_key, &old, &new_key, &new);
    let deleted = enqueue(table, &old_key, &old, "NULL", "NULL");
    // BEFORE captures the conflicting primary-key row for OR REPLACE even when SQLite's
    // recursive DELETE triggers are disabled. AFTER excludes failed/ignored INSERT attempts.
    tx.execute_batch(&format!(r#"
 CREATE TRIGGER IF NOT EXISTS st3_ivm_{table_name}_before_insert BEFORE INSERT ON {name} BEGIN
  UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,'null primary source key') WHERE singleton=1 AND ({null_key});
  UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,'source capture payload exceeded') WHERE singleton=1
   AND COALESCE((SELECT length(CAST({before} AS BLOB)) FROM {name} before WHERE {matching}),0)>{MAX_PAYLOAD};
  INSERT INTO st3_ivm_insert_before(source_table,old_key,old_row)
  VALUES('{table_name}',(SELECT {before_key} FROM {name} before WHERE {matching} AND (SELECT gap FROM st3_ivm_capture_state WHERE singleton=1) IS NULL),
   (SELECT {before} FROM {name} before WHERE {matching} AND (SELECT gap FROM st3_ivm_capture_state WHERE singleton=1) IS NULL))
  ON CONFLICT(source_table) DO UPDATE SET old_key=excluded.old_key,old_row=excluded.old_row;
 END;
 CREATE TRIGGER IF NOT EXISTS st3_ivm_{table_name}_insert AFTER INSERT ON {name} BEGIN
 {inserted}
 DELETE FROM st3_ivm_insert_before WHERE source_table='{table_name}';
 END;
 CREATE TRIGGER IF NOT EXISTS st3_ivm_{table_name}_update AFTER UPDATE ON {name} BEGIN
  UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,'null primary source key') WHERE singleton=1 AND ({null_key});
 {updated}
 END;
 CREATE TRIGGER IF NOT EXISTS st3_ivm_{table_name}_delete AFTER DELETE ON {name} BEGIN
 {deleted}
 DELETE FROM st3_ivm_insert_before WHERE source_table='{table_name}' AND old_key={old_key};
 END;
 "#,table_name=table.name))?;
    Ok(())
}

pub fn gap(tx: &Transaction<'_>, reason: &str) -> Result<()> {
    let bounded = reason.chars().take(256).collect::<String>();
    tx.execute(
        "UPDATE st3_ivm_capture_state SET gap=COALESCE(gap,?1) WHERE singleton=1",
        [bounded],
    )?;
    Ok(())
}
pub fn status(connection: &Connection) -> Result<Status> {
    Ok(connection.query_row(
        "SELECT fingerprint,epoch,gap,rows,bytes FROM st3_ivm_capture_state WHERE singleton=1",
        [],
        |row| {
            Ok(Status {
                fingerprint: row.get(0)?,
                epoch: row.get(1)?,
                gap: row.get(2)?,
                pending_rows: row.get(3)?,
                pending_bytes: row.get(4)?,
            })
        },
    )?)
}
pub fn clean(connection: &Connection) -> Result<bool> {
    let status = status(connection)?;
    Ok(status.gap.is_none() && status.pending_rows == 0)
}

/// Bounded, indexed capture page, read only within the source transaction's finalizer.
/// Installer::record and affected-key dispatch must succeed before ack; row selection must
/// independently reject an unconsumed or gapped capture. This page is never a public feed.
pub fn page(connection: &Connection, limit: usize) -> Result<Vec<Captured>> {
    ensure!(
        (1..=128).contains(&limit),
        "capture page limit must be1..=128"
    );
    ensure!(
        status(connection)?.gap.is_none(),
        "source capture coverage unavailable"
    );
    let mut stmt = connection.prepare_cached(
        "SELECT sequence,source_table,payload FROM st3_ivm_capture ORDER BY sequence LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(sequence, table, payload)| {
            let value: Value = serde_json::from_str(&payload)?;
            let old = value.get("old").filter(|value| !value.is_null()).cloned();
            let new = value.get("new").filter(|value| !value.is_null()).cloned();
            let old_key = old
                .as_ref()
                .map(|_| serde_json::to_string(&serde_json::json!([table, value["old_key"]])))
                .transpose()?;
            let new_key = new
                .as_ref()
                .map(|_| serde_json::to_string(&serde_json::json!([table, value["new_key"]])))
                .transpose()?;
            let replacements = if old_key == new_key {
                vec![Mutation {
                    key: old_key.or(new_key).context("mutation without source key")?,
                    old,
                    new,
                }]
            } else {
                old_key
                    .into_iter()
                    .map(|key| Mutation {
                        key,
                        old: old.clone(),
                        new: None,
                    })
                    .chain(new_key.into_iter().map(|key| Mutation {
                        key,
                        old: None,
                        new: new.clone(),
                    }))
                    .collect()
            };
            Ok(Captured {
                sequence,
                table,
                replacements,
            })
        })
        .collect()
}

/// Call only after the whole returned page was recorded/dispatched in this same transaction.
/// No gap restoration or source/Views publication is implied by acknowledging staged rows.
pub fn ack(tx: &Transaction<'_>, page: &[Captured]) -> Result<()> {
    ensure!(
        status(tx)?.gap.is_none(),
        "gapped capture cannot be acknowledged"
    );
    if let Some(last) = page.last() {
        let count: usize = tx.query_row(
            "SELECT count(*) FROM st3_ivm_capture WHERE sequence<=?1",
            [last.sequence],
            |row| row.get(0),
        )?;
        ensure!(
            count == page.len(),
            "capture acknowledgment must cover the complete oldest page"
        );
        tx.execute(
            "DELETE FROM st3_ivm_capture WHERE sequence<=?1",
            [last.sequence],
        )?;
    }
    tx.execute("DELETE FROM st3_ivm_insert_before", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use smallclaims::ivm::install::Installer;
    use std::sync::Arc;

    const TABLES: &[Table] = &[Table {
        name: "fixture_source",
        columns: &["id", "owner", "sample", "payload"],
        key: &["id"],
    }];
    fn fixture() -> Arc<crate::store::Store> {
        let store = Arc::new(crate::store::Store::open_memory("alder").unwrap());
        store.connection.batched(|tx| {
            tx.execute_batch("CREATE TABLE fixture_source(id TEXT PRIMARY KEY, owner TEXT NOT NULL, sample INTEGER, payload BLOB)")?;
            install(tx,TABLES,"fixture.capture.v1",1)?;
            Ok::<_,anyhow::Error>(())
        }).unwrap().unwrap();
        store
    }
    #[test]
    fn source_mutations_and_installer_record_share_outer_commit_and_rollback() {
        let store = fixture();
        let installer = Installer::new(vec![]).unwrap();
        store
            .connection
            .batched(|tx| {
                installer.create_schema(tx)?;
                installer.register_source(tx, "fixture.source", "fixture.source.v1", 1)?;
                tx.execute(
                    "INSERT INTO fixture_source VALUES('row/a','person/avery',7,x'00ff')",
                    [],
                )?;
                let page = page(tx, 128)?;
                assert_eq!(page.len(), 1);
                assert!(page[0].replacements[0].old.is_none());
                assert_eq!(
                    page[0].replacements[0].new.as_ref().unwrap()["payload"],
                    json!({"$blob":"00FF"})
                );
                assert!(!clean(tx)?);
                for captured in &page {
                    for mutation in &captured.replacements {
                        installer.record(tx, "fixture.source", mutation)?;
                    }
                }
                ack(tx, &page)?;
                assert!(clean(tx)?);
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        let failed: Result<()> = store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE fixture_source SET owner='person/intruder',sample=8 WHERE id='row/a'",
                    [],
                )?;
                let captured = page(tx, 1)?;
                assert_eq!(
                    captured[0].replacements[0].old.as_ref().unwrap()["owner"],
                    "person/avery"
                );
                for mutation in &captured[0].replacements {
                    installer.record(tx, "fixture.source", mutation)?;
                }
                ack(tx, &captured)?;
                anyhow::bail!("fixture rollback")
            })
            .unwrap();
        assert!(failed.is_err());
        let conn = store.readers.get();
        assert!(clean(&conn).unwrap());
        assert_eq!(
            installer
                .position(&conn, "fixture.source")
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT owner FROM fixture_source WHERE id='row/a'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "person/avery"
        );
    }
    #[test]
    fn ignored_insert_replace_and_key_reassignment_keep_actual_old_new_rows() {
        let store = fixture();
        {
            let writer = store.connection.write();
            writer
                .execute_batch("PRAGMA recursive_triggers=OFF")
                .unwrap();
        }
        store.connection.batched(|tx| {
            tx.execute("INSERT INTO fixture_source VALUES('row/a','person/avery',1,NULL)",[])?;
            ack(tx,&page(tx,128)?)?;
            tx.execute("INSERT OR IGNORE INTO fixture_source VALUES('row/a','person/intruder',2,NULL)",[])?;
            assert!(page(tx,128)?.is_empty());
            tx.execute("INSERT OR REPLACE INTO fixture_source VALUES('row/a','person/intruder',3,NULL)",[])?;
            let captured=page(tx,128)?;assert_eq!(captured.len(),1);
            assert_eq!(captured[0].replacements[0].old.as_ref().unwrap()["owner"],"person/avery");
            assert_eq!(captured[0].replacements[0].new.as_ref().unwrap()["owner"],"person/intruder");
            ack(tx,&captured)?;
            tx.execute("UPDATE fixture_source SET id='row/b',owner='person/avery' WHERE id='row/a'",[])?;
            let captured=page(tx,128)?;assert_eq!(captured[0].replacements.len(),2);
            assert!(captured[0].replacements[0].new.is_none());assert!(captured[0].replacements[1].old.is_none());
            assert_ne!(captured[0].replacements[0].key,captured[0].replacements[1].key);
            ack(tx,&captured)?;tx.execute("DELETE FROM fixture_source WHERE id='row/b'",[])?;
            let captured=page(tx,128)?;assert!(captured[0].replacements[0].new.is_none());
            Ok::<_,anyhow::Error>(())
        }).unwrap().unwrap();
    }
    #[test]
    fn recursive_replace_records_delete_then_insert_without_duplicate_old_row() {
        let store = fixture();
        let mut writer = store.connection.write();
        writer
            .execute_batch("PRAGMA recursive_triggers=ON")
            .unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(
            "INSERT INTO fixture_source VALUES('row/a','person/avery',1,NULL)",
            [],
        )
        .unwrap();
        ack(&tx, &page(&tx, 128).unwrap()).unwrap();
        tx.execute(
            "INSERT OR REPLACE INTO fixture_source VALUES('row/a','person/intruder',3,NULL)",
            [],
        )
        .unwrap();
        let captured = page(&tx, 128).unwrap();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].replacements[0].old.is_some());
        assert!(captured[0].replacements[0].new.is_none());
        assert!(captured[1].replacements[0].old.is_none());
        assert!(captured[1].replacements[0].new.is_some());
        tx.commit().unwrap();
    }
    #[test]
    fn quota_gap_preserves_source_admission_and_cannot_be_cleared_by_ack() {
        let store = fixture();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO fixture_source VALUES('row/first','person/avery',NULL,NULL)",
                    [],
                )?;
                let first = page(tx, 1)?;
                for n in 1..=MAX_ROWS {
                    tx.execute(
                        "INSERT INTO fixture_source VALUES(?1,'person/avery',NULL,NULL)",
                        [format!("row/{n}")],
                    )?;
                }
                assert!(ack(tx, &first).is_err());
                let state = status(tx)?;
                assert_eq!(state.pending_rows, MAX_ROWS);
                assert!(state.gap.is_some());
                assert!(page(tx, 128).is_err());
                assert!(!clean(tx)?);
                assert_eq!(
                    tx.query_row("SELECT count(*) FROM fixture_source", [], |row| row
                        .get::<_, u64>(0))?,
                    MAX_ROWS + 1
                );
                install(tx, TABLES, "fixture.capture.v1", 1)?;
                assert!(status(tx)?.gap.is_some());
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
    }
    #[test]
    fn populated_install_schema_omission_and_alternate_unique_keys_stay_unqualified() {
        let store = Arc::new(crate::store::Store::open_memory("alder").unwrap());
        store.connection.batched(|tx| {
            tx.execute_batch("CREATE TABLE fixture_source(id TEXT PRIMARY KEY, owner TEXT NOT NULL, sample INTEGER, payload BLOB); INSERT INTO fixture_source VALUES('row/a','person/avery',1,NULL)")?;
            install(tx,TABLES,"fixture.capture.v1",1)?;assert!(!clean(tx)?);
            install(tx,TABLES,"fixture.capture.v2",2)?;assert_eq!(status(tx)?.epoch,1);
            assert_eq!(status(tx)?.fingerprint,"fixture.capture.v1");
            let omitted=Table {name:"fixture_source",columns:&["id","owner"],key:&["id"]};
            assert!(install(tx,&[omitted],"omitted",1).is_err());
            tx.execute_batch("CREATE UNIQUE INDEX fixture_owner_unique ON fixture_source(owner)")?;
            assert!(install(tx,TABLES,"alias",1).is_err());Ok::<_,anyhow::Error>(())
        }).unwrap().unwrap();
    }
}
