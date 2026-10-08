//! Explicit, bounded same-transaction availability fencing for an adapter's gap singleton.
use anyhow::{Context, Result, ensure};
use rusqlite::Transaction;
use sha2::{Digest, Sha256};

fn identifier(name: &str) -> Result<String> {
    let mut bytes = name.bytes();
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.to_ascii_lowercase().starts_with("sqlite_"),
        "invalid source gap identifier"
    );
    Ok(format!("\"{name}\""))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn fence_sql(names: &[&str], reason: &str) -> String {
    let list = names
        .iter()
        .map(|name| literal(name))
        .collect::<Vec<_>>()
        .join(",");
    let rows = names
        .iter()
        .map(|name| format!("({},substr(CAST({reason} AS TEXT),1,1024))", literal(name)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "UPDATE ivm_views SET ready=0 WHERE name IN ({list}) AND ready<>0;
         INSERT INTO ivm_view_errors(view,error) VALUES {rows}
           ON CONFLICT(view) DO UPDATE SET error=excluded.error
           WHERE error IS NOT excluded.error;"
    )
}

pub(super) fn install<'a>(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    names: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let table_sql = identifier(table)?;
    let column_sql = identifier(column)?;
    let names = names.take(257).collect::<Vec<_>>();
    ensure!(
        !names.is_empty() && names.len() <= 256,
        "source gap registry bound exceeded"
    );
    ensure!(
        names.iter().map(|s| s.len()).sum::<usize>() <= 64 * 1024,
        "source gap registry byte bound exceeded"
    );
    let ordinary: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='main' AND name=?1 AND type='table')",
        [table], |r| r.get(0),
    )?;
    ensure!(ordinary, "source gap requires an ordinary main table");
    let columns = tx
        .prepare(&format!("PRAGMA main.table_info({table_sql})"))?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?
        .take(129)
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(columns.len() <= 128, "source gap column bound exceeded");
    ensure!(
        columns.iter().any(|(name, _)| name == column),
        "source gap column missing"
    );
    let keys = columns
        .iter()
        .filter(|(_, pk)| *pk != 0)
        .collect::<Vec<_>>();
    ensure!(
        keys.len() == 1,
        "source gap requires one explicit primary-key column"
    );
    let key_sql = identifier(&keys[0].0)?;
    let gaps = tx.prepare(&format!("SELECT CASE WHEN {column_sql} IS NULL THEN NULL ELSE substr(CAST({column_sql} AS TEXT),1,1024) END FROM main.{table_sql} LIMIT 2"))?
        .query_map([], |r| r.get::<_,Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(gaps.len() == 1, "source gap requires exactly one state row");
    // Fail before DDL when the owner has not installed the IVM schema. A gap trigger does not
    // initialize views, certify source extraction, or repair an incompatible projection.
    tx.prepare("SELECT ready FROM ivm_views LIMIT 0")?;
    tx.prepare("SELECT error FROM ivm_view_errors LIMIT 0")?;
    let prefix = format!(
        "ivm_source_gap_{}",
        hex::encode(Sha256::digest(format!("{table}\0{column}")))
    );
    let new_gap = format!("NEW.{column_sql}");
    let insert_reason = format!(
        "CASE WHEN {new_gap} IS NOT NULL THEN {new_gap} ELSE 'source gap state has multiple rows' END"
    );
    let update_reason = format!(
        "CASE WHEN OLD.{key_sql} IS NOT NEW.{key_sql} THEN 'source gap state identity changed' ELSE {new_gap} END"
    );
    tx.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS \"{prefix}_insert\";
         DROP TRIGGER IF EXISTS \"{prefix}_update\";
         DROP TRIGGER IF EXISTS \"{prefix}_delete\";
         CREATE TRIGGER \"{prefix}_insert\" AFTER INSERT ON main.{table_sql}
         WHEN {new_gap} IS NOT NULL OR EXISTS(SELECT 1 FROM main.{table_sql} LIMIT 1 OFFSET 1)
         BEGIN {} END;
         CREATE TRIGGER \"{prefix}_update\" AFTER UPDATE ON main.{table_sql}
         WHEN ({new_gap} IS NOT NULL AND {new_gap} IS NOT OLD.{column_sql})
           OR OLD.{key_sql} IS NOT NEW.{key_sql}
         BEGIN {} END;
         CREATE TRIGGER \"{prefix}_delete\" AFTER DELETE ON main.{table_sql}
         BEGIN {} END;",
        fence_sql(&names, &insert_reason),
        fence_sql(&names, &update_reason),
        fence_sql(&names, "'source gap state removed'"),
    ))?;
    if let Some(reason) = gaps.into_iter().next().context("missing source gap row")? {
        // Named strings come from the finite Views registry and values remain SQL-bound here.
        for name in names {
            super::fence_error(tx, name, &anyhow::anyhow!(reason.clone()))?;
        }
    }
    Ok(())
}
