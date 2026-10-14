//! Prepared canonical work-summary relation, independent of effective work/authorization.
//! Installation and complete admitted-claim capture belong to the shared source owner.
//! Readers never replay history or advance the captured clock.
#![allow(dead_code)]

#[cfg(test)]
#[path = "work_summaries_ivm_tests.rs"]
mod tests;

use super::*;
use smallclaims::ivm::{Contribution, Definition, LocalChange, Readiness, View, Views, source_cut};

pub(crate) const VIEW: &str = "st3.work-summaries.v1";
const CLOCK: &str = "st3.work-summaries.clock";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_work_summary_clock (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), through_ms BLOB NOT NULL CHECK(length(through_ms)=16)
);
INSERT OR IGNORE INTO local_work_summary_clock VALUES(1,zeroblob(16));
CREATE TABLE IF NOT EXISTS local_work_summaries (
 key TEXT PRIMARY KEY, subject TEXT NOT NULL, attempt INTEGER NOT NULL,
 progress TEXT, progress_at TEXT, completion TEXT, next_at BLOB CHECK(next_at IS NULL OR length(next_at)=16)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_work_summaries_deadline
 ON local_work_summaries(next_at,key) WHERE next_at IS NOT NULL;
"#;

// Both maintenance and its query-plan control use these exact indexed queries. Canonical
// ranks begin with a fixed-width big-endian u128 accepted time. The exclusive upper prefix
// therefore includes every tie at the captured millisecond, without a history fold.
const WINNER: &str = "SELECT value FROM ivm_contributions
 WHERE view=?1 AND key=?2 AND register=?3 AND rank<?4 ORDER BY rank DESC,claim_id DESC LIMIT 1";
const LAST: &str = "SELECT value FROM ivm_contributions
 WHERE view=?1 AND key=?2 AND register=?3 ORDER BY rank DESC,claim_id DESC LIMIT 1";
const NEXT: &str = "SELECT rank FROM ivm_contributions
 WHERE view=?1 AND key=?2 AND register=?3 AND rank>=?4 ORDER BY rank ASC,claim_id ASC LIMIT 1";
const DUE: &str = "SELECT key FROM local_work_summaries
 WHERE next_at IS NOT NULL AND next_at<=?1 ORDER BY next_at,key LIMIT ?2";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Summaries {
    pub progress_summary: Option<String>,
    pub progress_at_unix_ms: Option<u128>,
    pub completion_summary: Option<String>,
}

pub(crate) fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(WorkSummaries)]
}
pub(crate) struct WorkSummaries;

fn key(subject: &str, attempt: u32) -> Result<String> {
    Ok(serde_json::to_string(&(subject, attempt))?)
}

impl View for WorkSummaries {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: "work-summaries.v1;canonical-sortable.v1;subject-attempt;trim-nonempty;legacy-fields;ignore-handoff;retain-original;captured-u128-clock;private-attempt-keys;not-full-work",
            kinds: &["work.progress", "work.submitted"],
            local_kinds: &[CLOCK],
            max_contributions: 1,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        Ok(())
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        canonical: &smallclaims::store::canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        let register = match claim.kind.as_str() {
            "work.progress" => "progress",
            "work.submitted" => "completion",
            _ => return Ok(Vec::new()),
        };
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let Some(attempt) = fields
            .get("attempt")
            .and_then(Value::as_u64)
            .and_then(|attempt| u32::try_from(attempt).ok())
        else {
            return Ok(Vec::new());
        };
        if fields.get("handoff_acknowledged").is_some() {
            return Ok(Vec::new());
        }
        let Some(summary) = fields
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
        else {
            return Ok(Vec::new());
        };
        anyhow::ensure!(
            canonical.0 == claim.accepted_at_unix_ms,
            "summary canonical time mismatch"
        );
        Ok(vec![Contribution {
            key: key(&claim.subject, attempt)?,
            register: register.into(),
            value: json!({"summary":summary,"accepted":claim.accepted_at_unix_ms.to_string()}),
            rank: smallclaims::store::canonical::sortable_key(canonical),
        }])
    }
    fn maintain_key(
        &self,
        tx: &Transaction<'_>,
        key: &str,
        _old: Option<&ClaimRecord>,
        _new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        Ok(Some(maintain(tx, key, clock(tx)?)?))
    }
    fn maintain_local_key(
        &self,
        tx: &Transaction<'_>,
        key: &str,
        change: &LocalChange,
    ) -> Result<bool> {
        maintain(tx, key, change.evaluation_time_unix_ms)
    }
}

fn decode_time(bytes: Vec<u8>) -> Result<u128> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("summary clock width"))?;
    Ok(u128::from_be_bytes(bytes))
}
fn clock(connection: &Connection) -> Result<u128> {
    decode_time(connection.query_row(
        "SELECT through_ms FROM local_work_summary_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )?)
}
fn stored(connection: &Connection, key: &str) -> Result<Summaries> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT progress,progress_at,completion FROM local_work_summaries WHERE key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((progress_summary, at, completion_summary)) = row else {
        return Ok(Summaries::default());
    };
    Ok(Summaries {
        progress_summary,
        progress_at_unix_ms: at.map(|at| at.parse()).transpose()?,
        completion_summary,
    })
}
fn maintain(tx: &Transaction<'_>, key: &str, at: u128) -> Result<bool> {
    let (subject, attempt): (String, u32) = serde_json::from_str(key)?;
    let old = stored(tx, key)?;
    let upper = at.checked_add(1).map(u128::to_be_bytes);
    let mut values = BTreeMap::new();
    let mut next_at: Option<Vec<u8>> = None;
    for register in ["progress", "completion"] {
        let winner: Option<String> = if let Some(upper) = &upper {
            tx.query_row(
                WINNER,
                params![VIEW, key, register, upper.as_slice()],
                |row| row.get(0),
            )
            .optional()?
        } else {
            tx.query_row(LAST, params![VIEW, key, register], |row| row.get(0))
                .optional()?
        };
        if let Some(winner) = winner {
            values.insert(register, serde_json::from_str::<Value>(&winner)?);
        }
        if let Some(upper) = &upper {
            let rank: Option<Vec<u8>> = tx
                .query_row(
                    NEXT,
                    params![VIEW, key, register, upper.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(rank) = rank {
                anyhow::ensure!(rank.len() >= 16, "summary rank width");
                let time = rank[..16].to_vec();
                if next_at.as_ref().is_none_or(|old| time < *old) {
                    next_at = Some(time);
                }
            }
        }
    }
    let summary = |register| -> Option<String> {
        values
            .get(register)?
            .get("summary")?
            .as_str()
            .map(str::to_owned)
    };
    let progress_at: Option<String> = values
        .get("progress")
        .and_then(|v| v.get("accepted"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let new = Summaries {
        progress_summary: summary("progress"),
        progress_at_unix_ms: progress_at.as_ref().map(|at| at.parse()).transpose()?,
        completion_summary: summary("completion"),
    };
    if new == Summaries::default() && next_at.is_none() {
        tx.execute("DELETE FROM local_work_summaries WHERE key=?1", [key])?;
    } else {
        tx.execute("INSERT INTO local_work_summaries VALUES(?1,?2,?3,?4,?5,?6,?7)
          ON CONFLICT(key) DO UPDATE SET progress=excluded.progress,progress_at=excluded.progress_at,
          completion=excluded.completion,next_at=excluded.next_at
          WHERE progress IS NOT excluded.progress OR progress_at IS NOT excluded.progress_at
             OR completion IS NOT excluded.completion OR next_at IS NOT excluded.next_at",
          params![key,subject,attempt,new.progress_summary,progress_at,new.completion_summary,next_at])?;
    }
    Ok(new != old)
}

fn ready(connection: &Connection, views: &Views) -> Result<smallclaims::ivm::SourceCut> {
    let cut = source_cut(connection)?.context("work summary source unavailable")?;
    anyhow::ensure!(
        matches!(
            views.readiness(connection, VIEW, cut.epoch)?,
            Readiness::Ready(_)
        ),
        "work summaries unavailable"
    );
    // This equality is necessary, but only the complete admission/source owner can certify
    // the frontier. Computing a largest applied index does not establish hook coverage.
    anyhow::ensure!(
        cut.admitted == cut.projected && cut.projected == current_index(connection)?,
        "work summary prefix incomplete"
    );
    Ok(cut)
}
fn ready_at(connection: &Connection, views: &Views, at: u128) -> Result<()> {
    ready(connection, views)?;
    anyhow::ensure!(
        at >= clock(connection)?,
        "work summary historical clock unavailable"
    );
    let due = connection
        .prepare_cached(DUE)?
        .exists(params![at.to_be_bytes().as_slice(), 1])?;
    anyhow::ensure!(!due, "work summary captured time pending");
    Ok(())
}

/// A writer-only captured-time page. Incomplete pages remain explicitly unavailable.
/// The same certified claim cut is retained; no durable telemetry claim is synthesized.
pub(crate) fn clock_page(
    tx: &Transaction<'_>,
    views: &Views,
    at: u128,
    limit: usize,
) -> Result<usize> {
    anyhow::ensure!((1..=1024).contains(&limit), "work summary clock page bound");
    let mut cut = ready(tx, views)?;
    anyhow::ensure!(at >= clock(tx)?, "work summary clock regressed");
    let keys = tx
        .prepare_cached(DUE)?
        .query_map(params![at.to_be_bytes().as_slice(), limit], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for key in &keys {
        cut.local_generation = cut
            .local_generation
            .checked_add(1)
            .context("work summary generation overflow")?;
        let changes = views.local_change(
            tx,
            &LocalChange {
                kind: CLOCK.into(),
                old_keys: BTreeSet::from([key.clone()]),
                new_keys: BTreeSet::from([key.clone()]),
                evaluation_time_unix_ms: at,
            },
            cut,
        )?;
        anyhow::ensure!(
            changes.deferred.is_empty(),
            "work summary clock maintenance deferred"
        );
    }
    tx.execute(
        "UPDATE local_work_summary_clock SET through_ms=?1 WHERE singleton=1 AND through_ms<?1",
        [at.to_be_bytes().as_slice()],
    )?;
    Ok(keys.len())
}

pub(crate) fn row(
    connection: &Connection,
    views: &Views,
    subject: &str,
    attempt: u32,
    at: u128,
) -> Result<Summaries> {
    ready_at(connection, views, at)?;
    stored(connection, &key(subject, attempt)?)
}
/// One query for a selected batch. Absence means empty summaries, never proof the step exists.
pub(crate) fn rows(
    connection: &Connection,
    views: &Views,
    selected: &[(String, u32)],
    at: u128,
) -> Result<BTreeMap<(String, u32), Summaries>> {
    anyhow::ensure!(selected.len() <= 501, "work summary selected batch bound");
    ready_at(connection, views, at)?;
    let keys = selected
        .iter()
        .map(|(subject, attempt)| key(subject, *attempt))
        .collect::<Result<Vec<_>>>()?;
    let rows = connection.prepare_cached("SELECT subject,attempt,progress,progress_at,completion FROM local_work_summaries WHERE key IN (SELECT value FROM json_each(?1))")?
        .query_map([serde_json::to_string(&keys)?],|row| Ok((row.get::<_,String>(0)?,row.get::<_,u32>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut output = selected
        .iter()
        .cloned()
        .map(|key| (key, Summaries::default()))
        .collect::<BTreeMap<_, _>>();
    for (subject, attempt, progress_summary, at, completion_summary) in rows {
        output.insert(
            (subject, attempt),
            Summaries {
                progress_summary,
                progress_at_unix_ms: at.map(|at| at.parse()).transpose()?,
                completion_summary,
            },
        );
    }
    Ok(output)
}
pub(crate) fn next_deadline(
    connection: &Connection,
    views: &Views,
    at: u128,
) -> Result<Option<u128>> {
    ready_at(connection, views, at)?;
    connection.query_row("SELECT next_at FROM local_work_summaries WHERE next_at IS NOT NULL ORDER BY next_at,key LIMIT 1",[],|row| row.get::<_,Vec<u8>>(0)).optional()?.map(decode_time).transpose()
}
