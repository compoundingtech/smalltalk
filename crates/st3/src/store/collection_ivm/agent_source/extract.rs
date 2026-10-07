//! Primary-key pages of complete physical inputs, captured with an Installer source position.
//! Canonical/authority closure is maintained by the namespaced operator, never a later lookup
//! of OLD input. Finishing extraction does not attest drained kernels or publish a source cut.
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smallclaims::ivm::install::{Installer, Mutation, ScanPage};

use super::{SOURCE, TABLES, capture_fingerprint};

const MAX_ROWS: usize = 128;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_PAYLOAD: usize = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    table: usize,
    after: Option<Vec<Value>>,
}

pub fn scan_page(
    connection: &Connection,
    installer: &Installer,
    job: &str,
    rows: usize,
    bytes: usize,
) -> Result<ScanPage> {
    ensure!(
        super::super::scope::readable(connection)?,
        "source capture is pending or fenced"
    );
    let position = installer.position(connection, SOURCE)?;
    let state = super::super::status(connection)?;
    ensure!(
        state.fingerprint == capture_fingerprint()
            && position.fingerprint == state.fingerprint
            && position.epoch == state.epoch,
        "source extraction binding changed"
    );
    let progress = installer.progress(connection, job)?;
    let source: Option<String> = connection
        .query_row(
            "SELECT source FROM ivm_install_jobs WHERE id=?1",
            [job],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        progress.phase == "scan" && source.as_deref() == Some(SOURCE),
        "source extraction job mismatch"
    );
    let (next_cursor, output, finished) = extract(connection, &progress.cursor, rows, bytes)?;
    Ok(ScanPage {
        job: job.into(),
        expected_cursor: progress.cursor,
        next_cursor,
        position,
        rows: output,
        finished,
    })
}

fn bind(value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::String(value) => SqlValue::Text(value.clone()),
        Value::Number(value) if value.is_i64() => SqlValue::Integer(value.as_i64().unwrap()),
        Value::Number(value) => {
            SqlValue::Real(value.as_f64().context("invalid source key number")?)
        }
        Value::Object(value) if value.len() == 1 && value.contains_key("$blob") => SqlValue::Blob(
            hex::decode(value["$blob"].as_str().context("invalid blob source key")?)?,
        ),
        _ => anyhow::bail!("unsupported or null physical source key"),
    })
}

fn key_value(value: ValueRef<'_>) -> Result<Value> {
    Ok(match value {
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => {
            ensure!(value.is_finite(), "nonfinite source key");
            json!(value)
        }
        ValueRef::Text(value) => {
            ensure!(value.len() <= MAX_PAYLOAD, "oversized source key");
            Value::String(std::str::from_utf8(value)?.into())
        }
        ValueRef::Blob(value) => {
            ensure!(value.len() <= MAX_PAYLOAD / 2, "oversized blob source key");
            json!({"$blob":hex::encode_upper(value)})
        }
        ValueRef::Null => anyhow::bail!("null physical source key"),
    })
}

fn extract(
    connection: &Connection,
    cursor: &[u8],
    rows: usize,
    bytes: usize,
) -> Result<(Vec<u8>, Vec<Mutation>, bool)> {
    ensure!(
        (1..=MAX_ROWS).contains(&rows) && (1..=MAX_BYTES).contains(&bytes),
        "invalid source page bounds"
    );
    ensure!(cursor.len() <= MAX_PAYLOAD, "oversized source cursor");
    let mut cursor: Cursor = if cursor.is_empty() {
        Cursor::default()
    } else {
        serde_json::from_slice(cursor)?
    };
    ensure!(
        cursor.table <= TABLES.len(),
        "source cursor table outside manifest"
    );
    let mut output = Vec::with_capacity(rows);
    let mut used = 0;
    while cursor.table < TABLES.len() && output.len() < rows {
        let table = &TABLES[cursor.table];
        let key_columns = table
            .key
            .iter()
            .map(|name| super::super::identifier(name))
            .collect::<Result<Vec<_>>>()?;
        let mut bindings = vec![];
        let predicate = if let Some(after) = &cursor.after {
            ensure!(
                after.len() == key_columns.len(),
                "physical source cursor key mismatch"
            );
            bindings = after.iter().map(bind).collect::<Result<Vec<_>>>()?;
            format!(
                "WHERE ({})>({})",
                key_columns.join(","),
                vec!["?"; key_columns.len()].join(",")
            )
        } else {
            String::new()
        };
        // Inspect raw cell lengths before encoding/allocating complete SQL row JSON.
        let raw_size = table
            .columns
            .iter()
            .map(|name| {
                super::super::identifier(name)
                    .map(|column| format!("COALESCE(length(CAST({column} AS BLOB)),0)"))
            })
            .collect::<Result<Vec<_>>>()?
            .join("+");
        let sql = format!(
            "SELECT {raw_size},{} FROM {} {predicate} ORDER BY {} LIMIT ?",
            key_columns.join(","),
            super::super::identifier(table.name)?,
            key_columns.join(",")
        );
        let remaining = rows - output.len();
        bindings.push(SqlValue::Integer(remaining as i64));
        let mut statement = connection.prepare(&sql)?;
        let mut candidates = statement.query(params_from_iter(bindings))?;
        let mut count = 0;
        while let Some(candidate) = candidates.next()? {
            let size: u64 = candidate.get(0)?;
            ensure!(
                size <= MAX_PAYLOAD as u64,
                "source row exceeds extraction payload limit"
            );
            let key = (0..key_columns.len())
                .map(|i| key_value(candidate.get_ref(i + 1)?))
                .collect::<Result<Vec<_>>>()?;
            let parameters = key.iter().map(bind).collect::<Result<Vec<_>>>()?;
            let equality = key_columns
                .iter()
                .map(|column| format!("source.{column}=?"))
                .collect::<Vec<_>>()
                .join(" AND ");
            let row_sql = format!(
                "SELECT {} FROM {} AS source WHERE {equality}",
                super::super::row(table, "source")?,
                super::super::identifier(table.name)?
            );
            let body: String =
                connection.query_row(&row_sql, params_from_iter(parameters), |r| r.get(0))?;
            ensure!(
                body.len() <= MAX_PAYLOAD,
                "encoded source row exceeds payload limit"
            );
            let mutation = Mutation {
                key: serde_json::to_string(&json!([table.name, key]))?,
                old: None,
                new: Some(serde_json::from_str(&body)?),
            };
            let encoded = serde_json::to_vec(&mutation)?.len();
            if encoded > bytes - used {
                ensure!(!output.is_empty(), "source row exceeds page byte limit");
                return Ok((serde_json::to_vec(&cursor)?, output, false));
            }
            used += encoded;
            output.push(mutation);
            cursor.after = Some(key);
            count += 1;
        }
        if count < remaining {
            cursor.table += 1;
            cursor.after = None;
        }
    }
    Ok((
        serde_json::to_vec(&cursor)?,
        output,
        cursor.table == TABLES.len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ivm_agent_physical_extraction_pages_are_complete_unique_and_bounded() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        store
            .connection
            .batched(|tx| super::super::install_capture(tx, 1))
            .unwrap()
            .unwrap();
        for index in 0..3 {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: format!("agent/extract-{index}"),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: std::collections::BTreeMap::from([
                        ("runtime_id".into(), json!(format!("extract-{index}"))),
                        ("status".into(), json!("running")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let connection = store.readers.get();
        let expected: usize = TABLES
            .iter()
            .map(|table| {
                connection
                    .query_row(&format!("SELECT COUNT(*) FROM {}", table.name), [], |r| {
                        r.get::<_, usize>(0)
                    })
                    .unwrap()
            })
            .sum();
        let mut cursor = vec![];
        let mut seen = std::collections::BTreeMap::new();
        for _ in 0..100 {
            let (next, rows, finished) = extract(&connection, &cursor, 2, 4096).unwrap();
            assert!(rows.len() <= 2);
            assert!(
                rows.iter()
                    .map(|row| serde_json::to_vec(row).unwrap().len())
                    .sum::<usize>()
                    <= 4096
            );
            for row in rows {
                assert!(row.old.is_none());
                assert!(seen.insert(row.key, row.new.unwrap()).is_none());
            }
            cursor = next;
            if finished {
                break;
            }
        }
        assert_eq!(seen.len(), expected);
        assert_eq!(
            seen.values()
                .filter(|body| body.get("store_index").is_some())
                .count(),
            3
        );
        assert!(extract(&connection, &cursor, 129, 4096).is_err());
        assert!(extract(&connection, br#"{"table":999,"after":null}"#, 2, 4096).is_err());
        // Native cell sizes/byte budget are inspected before complete source JSON allocation.
        assert!(extract(&connection, &[], 2, 1).is_err());
    }
}
