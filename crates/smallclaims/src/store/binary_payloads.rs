//! Forward conversion of legacy TEXT payloads, bounded on the daemon's writer queue.
use super::*;
use crate::claim::EnvelopePayload;
use std::time::{Duration, Instant};

const CURSOR: &str = "binary_envelope_payload_cursor";
const PAGE: usize = 64;
const BUDGET: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub struct PayloadConversion {
    pub scanned: usize,
    pub converted: usize,
    pub done: bool,
}

impl Store {
    /// Convert one bounded page. Cursor and bytes commit together; interrupted conversions
    /// resume at the committed row. No schema or index rebuild is needed.
    pub fn convert_envelope_payloads(&self) -> Result<PayloadConversion> {
        self.connection
            .batched(|tx| convert_page(tx))
            .map_err(anyhow::Error::msg)?
    }
}

fn convert_page(tx: &Connection) -> Result<PayloadConversion> {
    let cursor: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key=?1", [CURSOR], |r| {
            r.get(0)
        })
        .optional()?;
    if cursor.as_deref() == Some("done") {
        return Ok(PayloadConversion {
            done: true,
            ..Default::default()
        });
    }
    let mut after = cursor.map(|v| v.parse::<i64>()).transpose()?.unwrap_or(0);
    let started = Instant::now();
    let mut report = PayloadConversion::default();
    let mut statement = tx.prepare_cached(
        "SELECT rowid,payload FROM replica_envelopes WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
    )?;
    let mut rows = statement.query(params![after, PAGE])?;
    while let Some(row) = rows.next()? {
        after = row.get(0)?;
        report.scanned += 1;
        if matches!(row.get_ref(1)?, rusqlite::types::ValueRef::Text(_)) {
            let payload: EnvelopePayload = row.get(1)?;
            // Bad base64 is forensic evidence, not something a storage conversion repairs.
            if let Ok(bytes) = payload.bytes() {
                tx.execute(
                    "UPDATE replica_envelopes SET payload=?1 WHERE rowid=?2",
                    params![bytes, after],
                )?;
                report.converted += 1;
            }
        }
        if started.elapsed() >= BUDGET {
            break;
        }
    }
    report.done = report.scanned == 0;
    tx.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![CURSOR, if report.done { "done".into() } else { after.to_string() }],
    )?;
    Ok(report)
}
