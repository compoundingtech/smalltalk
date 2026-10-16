//! Retained source resolutions for HTTP history/detail, never the open Home projection.
//! Reads use the caller's pinned snapshot and never repair or populate a projection.
use super::*;

pub(crate) const MAX_CANDIDATES: usize = 32;
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_DECODE_BYTES: usize = 2_097_152;

/// A scanned index survives even when decoding is refused, so paging can advance.
pub(crate) struct Candidate {
    pub store_index: u64,
    pub owner: Option<String>,
    pub resolution: Option<Resolution>,
    pub omitted: bool,
}

pub(crate) struct Resolution {
    pub claim: ClaimRecord,
    pub original: Option<Original>,
}

pub(crate) struct Original {
    pub id: String,
    pub subject: String,
    pub kind: String,
    pub actor: Option<String>,
    pub at: u128,
    pub body: Value,
}

fn safe_body(alias: &str) -> String {
    format!(
        "CASE WHEN octet_length({alias}.body)<={MAX_BODY_BYTES} THEN CASE WHEN json_valid({alias}.body) THEN {alias}.body ELSE '{{}}' END ELSE '{{}}' END"
    )
}

fn select_rows(from: &str, cut: &str) -> String {
    let bounded = |alias: &str, column: &str, limit: usize| {
        format!("CASE WHEN octet_length({alias}.{column})<={limit} THEN {alias}.{column} END")
    };
    let metadata = |alias: &str| {
        [
            "id",
            "batch_id",
            "subject",
            "kind",
            "origin",
            "actor",
            "accepted_at_unix_ms",
        ]
        .iter()
        .map(|column| format!("({alias}.{column} IS NULL OR octet_length({alias}.{column})<=4096)"))
        .collect::<Vec<_>>()
        .join(" AND ")
    };
    let current_body = safe_body("c");
    let original_body = safe_body("e");
    let owner = format!("CASE WHEN c.kind='work.person-cancelled' THEN json_extract({original_body},'$.fields.person')
        WHEN json_extract({current_body},'$.fields.code')='native-prompt-answered'
        AND json_extract({current_body},'$.fields.status')='dismiss'
        THEN json_extract({current_body},'$.fields.acted_for') END");
    let columns = [
        bounded("c", "id", 4096),
        "c.store_index".into(),
        bounded("c", "batch_id", 4096),
        bounded("c", "subject", 4096),
        bounded("c", "kind", 4096),
        bounded("c", "origin", 4096),
        bounded("c", "actor", 4096),
        bounded("c", "body", MAX_BODY_BYTES),
        bounded("c", "predecessors", 16384),
        bounded("c", "accepted_at_unix_ms", 4096),
        bounded("e", "id", 4096),
        bounded("e", "subject", 4096),
        bounded("e", "kind", 4096),
        bounded("e", "actor", 4096),
        bounded("e", "accepted_at_unix_ms", 4096),
        bounded("e", "body", MAX_BODY_BYTES),
        format!(
            "({} AND octet_length(c.predecessors)<=16384 AND (e.id IS NULL OR ({})))",
            metadata("c"),
            metadata("e")
        ),
        // Owner selection precedes body retrieval/validation in decode_rows.
        // This is a filter only; resource() still checks the complete source binding.
        format!("CASE WHEN typeof(({owner}))='text' AND octet_length(({owner}))<=4096 THEN ({owner}) END"),
        "coalesce(octet_length(c.body),0)+coalesce(octet_length(e.body),0)".into(),
        // NULL means the diagnostic body is not safely classifiable. Count an
        // anonymous omission conservatively; never decode an unbounded body to
        // decide relevance. Known unrelated diagnostics are always false.
        format!("CASE WHEN c.kind='work.person-cancelled' THEN 1
            WHEN c.kind!='harness.diagnostic' THEN 0
            WHEN octet_length(c.body)>{MAX_BODY_BYTES} THEN NULL
            WHEN NOT json_valid(c.body) THEN NULL
            WHEN json_extract({current_body},'$.fields.code')='native-prompt-answered'
             AND json_extract({current_body},'$.fields.status')='dismiss' THEN 1 ELSE 0 END"),
    ]
    .join(",");
    format!(
        "SELECT {columns} {from}
        LEFT JOIN claims e ON e.id=coalesce(
            json_extract({current_body},'$.fields.episode'),
            json_extract({current_body},'$.fields.delegation.episode'))
        AND e.store_index<={cut}
        ORDER BY c.store_index DESC"
    )
}

fn decode_rows(mut rows: rusqlite::Rows<'_>, person: Option<&str>) -> Result<Vec<Candidate>> {
    let mut result = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        smallclaims::read_budget::check()?;
        let store_index = row.get(1)?;
        let mut candidate = Candidate {
            store_index,
            owner: None,
            resolution: None,
            omitted: false,
        };
        match row.get::<_, Option<bool>>(19)? {
            Some(false) => {
                // Keep its index for sparse paging, without retrieving/decoding
                // a noise body, charging decode budget or reporting an omission.
                result.push(candidate);
                continue;
            }
            None => {
                candidate.omitted = true;
                result.push(candidate);
                continue;
            }
            Some(true) => {}
        }
        candidate.owner = row.get(17)?;
        // An unrelated person's missing/wide source cannot fail this person's page,
        // consume its decode budget or contribute to its omission count.
        if person
            .zip(candidate.owner.as_deref())
            .is_some_and(|(person, owner)| person != owner)
        {
            result.push(candidate);
            continue;
        }
        let size: usize = row.get(18)?;
        if size > MAX_DECODE_BYTES.saturating_sub(bytes) {
            candidate.omitted = true;
            result.push(candidate);
            continue;
        }
        bytes += size;
        let decoded = (|| -> Result<Resolution> {
            anyhow::ensure!(
                row.get::<_, bool>(16)?,
                "history source identity exceeds read limit"
            );
            // These CASE-bounded columns cannot allocate an oversized body in Rust.
            let body: String = row
                .get(7)
                .context("history resolution body exceeds read limit")?;
            let original_id: Option<String> = row.get(10)?;
            let original_body: Option<String> = row.get(15)?;
            anyhow::ensure!(
                original_id.is_none() || original_body.is_some(),
                "history original body exceeds read limit"
            );
            let original = original_id
                .map(|id| -> Result<Original> {
                    Ok(Original {
                        id,
                        subject: row.get(11)?,
                        kind: row.get(12)?,
                        actor: row.get(13)?,
                        at: row.get::<_, String>(14)?.parse()?,
                        body: serde_json::from_str(original_body.as_deref().unwrap())?,
                    })
                })
                .transpose()?;
            // Validate before claim_from_row uses this same bounded body column.
            let _: serde::de::IgnoredAny = serde_json::from_str(&body)?;
            let _: Vec<String> = serde_json::from_str(&row.get::<_, String>(8)?)?;
            let _: u128 = row.get::<_, String>(9)?.parse()?;
            Ok(Resolution {
                claim: claim_from_row(row)?,
                original,
            })
        })();
        // Cancellation/deadline errors retain the existing whole-request failure;
        // they are never relabelled as corrupt-row omissions.
        smallclaims::read_budget::check()?;
        match decoded {
            Ok(resolution) => candidate.resolution = Some(resolution),
            Err(_) => candidate.omitted = true,
        }
        result.push(candidate);
    }
    smallclaims::read_budget::check()?;
    Ok(result)
}

fn candidates_sql() -> String {
    let prefix="WITH withdrawals AS MATERIALIZED (
        SELECT store_index FROM claims INDEXED BY claims_kind_index
        WHERE kind='work.person-cancelled' AND store_index<=?1 AND store_index<?2
        ORDER BY store_index DESC LIMIT ?3),
    answers AS MATERIALIZED (
        SELECT store_index FROM claims INDEXED BY claims_kind_index
        WHERE kind='harness.diagnostic' AND store_index<=?1 AND store_index<?2
        ORDER BY store_index DESC LIMIT ?3),
    candidates AS MATERIALIZED (
        SELECT store_index FROM (SELECT store_index FROM withdrawals UNION ALL SELECT store_index FROM answers)
        ORDER BY store_index DESC LIMIT ?3) ";
    format!(
        "{prefix}{}",
        select_rows(
            "FROM candidates JOIN claims c ON c.store_index=candidates.store_index",
            "?1"
        )
    )
}

impl Store {
    /// Cap both indexed kind scans BEFORE filtering or joining. A sparse page can
    /// have no owner-visible rows and still carry a cursor; never refill by scanning.
    pub(crate) fn attention_history_candidates(
        &self,
        cut: u64,
        before: u64,
        limit: usize,
        person: Option<&str>,
    ) -> Result<Vec<Candidate>> {
        let limit = limit.clamp(1, MAX_CANDIDATES) + 1;
        let connection = self.readers.get();
        let sql = candidates_sql();
        let mut statement = connection.prepare_cached(&sql)?;
        decode_rows(
            statement.query(params![cut, before.min(i64::MAX as u64), limit])?,
            person,
        )
    }

    /// A closed resource names its immutable resolution claim, so selection never
    /// reverses a hash or scans older history. The API checks the recorded recipient.
    pub(crate) fn attention_history_resolution(
        &self,
        id: &str,
        cut: u64,
        person: Option<&str>,
    ) -> Result<Option<Candidate>> {
        let connection = self.readers.get();
        let sql = select_rows(
            "FROM (SELECT * FROM claims WHERE id=?1 AND store_index<=?2) c",
            "?2",
        );
        let mut statement = connection.prepare_cached(&sql)?;
        Ok(decode_rows(statement.query(params![id, cut])?, person)?.pop())
    }
}

#[cfg(test)]
pub(super) fn candidate_query_plan(store: &Store, cut: u64) -> Result<Vec<(i64, i64, String)>> {
    let connection = store.readers.get();
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {}", candidates_sql()))?;
    Ok(statement
        .query_map(params![cut, i64::MAX, MAX_CANDIDATES + 1], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<(i64, i64, String)>>>()?)
}
