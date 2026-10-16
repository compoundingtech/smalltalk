//! Invocation-local validation of the existing message-cache keys. No key survives this call.
use super::*;

pub(super) const CHUNK: usize = 128;

#[derive(Debug, PartialEq)]
pub(super) struct Version {
    pub latest_claim_index: u64,
    pub desired_claim_id: Option<String>,
}

// Each supplied subject drives the same two existing indexed lookups as the point reader.
// Keep duplicate subjects and their positions; no graph scan or membership filtering.
const QUERY: &str = "SELECT requested.key,
    (SELECT COALESCE(MAX(store_index), 0) FROM claims WHERE subject=requested.value),
    (SELECT claim_id FROM desired WHERE subject=requested.value)
    FROM json_each(?1) AS requested ORDER BY requested.key";

pub(super) fn read(connection: &Connection, subjects: &[&str]) -> Result<Vec<Version>> {
    anyhow::ensure!(
        subjects.len() <= CHUNK,
        "message version batch exceeds 128 subjects"
    );
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(QUERY)?;
    let mut rows = statement.query([serde_json::to_string(subjects)?])?;
    let mut versions = Vec::with_capacity(subjects.len());
    for ordinal in 0..subjects.len() {
        let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        anyhow::ensure!(
            row.get::<_, usize>(0)? == ordinal,
            "message version batch changed input order"
        );
        versions.push(Version {
            latest_claim_index: row.get(1)?,
            desired_claim_id: row.get(2)?,
        });
    }
    anyhow::ensure!(
        rows.next()?.is_none(),
        "message version batch returned extra rows"
    );
    Ok(versions)
}

#[cfg(test)]
mod tests;
