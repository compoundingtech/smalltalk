//! Point reads used only by observer/subscription detail adapters. Call inside read_snapshot.
//! Metadata checks precede payload copies; no declaration/member or claim-history fold runs.

use super::*;

pub(crate) const SNAPSHOT_METADATA_SQL: &str =
    "SELECT store_index, octet_length(accepted_at_unix_ms),
                    typeof(accepted_at_unix_ms)='text'
             FROM claims WHERE store_index<=?1 ORDER BY store_index DESC LIMIT 1";

pub(crate) const DECLARATION_METADATA_SQL: &str =
    "SELECT octet_length(subject) + octet_length(kind) + octet_length(revision)
                    + octet_length(body) + coalesce(octet_length(owner_run),0)
                    + coalesce(octet_length(owner_generation),0)
                    + coalesce(octet_length(owner_step),0),
                    typeof(subject)='text' AND typeof(kind)='text'
                    AND typeof(revision)='text' AND typeof(body)='text'
                    AND typeof(owner_run) IN ('null','text')
                    AND typeof(owner_generation) IN ('null','text')
                    AND typeof(owner_step) IN ('null','text')
             FROM desired WHERE subject=?1 AND (kind=?2 OR (kind='stop' AND ?3))";

pub(crate) const STATE_METADATA_SQL: &str =
    "SELECT store_index, octet_length(body) + octet_length(accepted_at_unix_ms),
                    typeof(body)='text' AND typeof(accepted_at_unix_ms)='text'
             FROM claims INDEXED BY claims_subject_kind_index
             WHERE subject=?1 AND kind=?2 AND store_index<=?3
             ORDER BY store_index DESC LIMIT 1";

pub(crate) const MAX_DETAIL_SOURCE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_DETAIL_KEY_BYTES: usize = 4096;

pub(crate) struct DetailBudget(usize);

impl DetailBudget {
    pub(crate) fn new() -> Self {
        Self(MAX_DETAIL_SOURCE_BYTES)
    }

    pub(crate) fn charge(&mut self, bytes: i64) -> Result<()> {
        let Some(bytes) = usize::try_from(bytes).ok().filter(|bytes| *bytes <= self.0) else {
            return Err(St3Error::new(
                "projection-detail-too-large",
                "observer/subscription detail source exceeds the 256 KiB read budget",
            )
            .into());
        };
        self.0 -= bytes;
        Ok(())
    }
}

pub(crate) struct DetailDeclaration {
    pub(crate) desired: DesiredSubject,
    pub(crate) revision: String,
}

impl Store {
    // client_snapshot_at reads this column too. Check its width before that helper
    // copies it; the caller holds the same snapshot through both reads.
    pub(crate) fn observer_subscription_detail_snapshot_guard(
        &self,
        index: u64,
        budget: &mut DetailBudget,
    ) -> Result<()> {
        let connection = self.readers.get();
        let metadata: Option<(u64, i64, bool)> = connection
            .prepare_cached(SNAPSHOT_METADATA_SQL)?
            .query_row([index], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        if let Some((position, bytes, valid)) = metadata {
            valid_metadata(valid)?;
            // The guarded validation and client_snapshot_at each copy this column.
            budget.charge(bytes.saturating_mul(2))?;
            connection
                .prepare_cached("SELECT accepted_at_unix_ms FROM claims WHERE store_index=?1")?
                .query_row([position], |row| decode_time(row.get(0)?, 0))
                .map_err(detail_read_error)?;
        }
        Ok(())
    }

    pub(crate) fn observer_subscription_detail_declaration(
        &self,
        subject: &str,
        kind: &str,
        budget: &mut DetailBudget,
    ) -> Result<Option<DetailDeclaration>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        // Wrong-kind rows do not copy/decode payloads. Stops are admitted by the same
        // subject-prefix rule as observer_subscription_resources.
        let stop = subject.starts_with(&format!("{kind}/"));
        let metadata: Option<(i64, bool)> = connection
            .prepare_cached(DECLARATION_METADATA_SQL)?
            .query_row(params![subject, kind, stop], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some((bytes, valid)) = metadata else {
            return Ok(None);
        };
        valid_metadata(valid)?;
        budget.charge(bytes)?;
        smallclaims::read_budget::check()?;
        connection
            .prepare_cached(
                "SELECT kind, revision, body, owner_run, owner_generation, owner_step
             FROM desired WHERE subject=?1",
            )?
            .query_row([subject], |row| {
                let body = row.get::<_, String>(2)?;
                Ok(DetailDeclaration {
                    desired: DesiredSubject {
                        subject: subject.into(),
                        kind: row.get(0)?,
                        desired: decode_body(&body, 2)?,
                        member: None, // Neither item renderer consumes membership.
                        owner_run: row.get(3)?,
                        owner_generation: row.get(4)?,
                        owner_step: row.get(5)?,
                    },
                    revision: row.get(1)?,
                })
            })
            .optional()
            .map_err(detail_read_error)
    }

    pub(crate) fn observer_subscription_detail_state(
        &self,
        subject: &str,
        kind: &str,
        index: u64,
        budget: &mut DetailBudget,
    ) -> Result<Option<(Value, u128)>> {
        let connection = self.readers.get();
        // An inclusive bound is the old exclusive (index + 1) bound without overflow.
        // The full index order needs no correlated subquery or temporary sort.
        let metadata: Option<(u64, i64, bool)> = connection
            .prepare_cached(STATE_METADATA_SQL)?
            .query_row(params![subject, kind, index], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let Some((position, bytes, valid)) = metadata else {
            return Ok(None);
        };
        valid_metadata(valid)?;
        budget.charge(bytes)?;
        smallclaims::read_budget::check()?;
        // Ignore unrelated claim metadata and predecessors, which the renderer never uses.
        connection
            .prepare_cached("SELECT body, accepted_at_unix_ms FROM claims WHERE store_index=?1")?
            .query_row([position], |row| {
                let body = row.get::<_, String>(0)?;
                let accepted = row.get::<_, String>(1)?;
                Ok((decode_body(&body, 0)?, decode_time(accepted, 1)?))
            })
            .optional()
            .map_err(detail_read_error)
    }
}

fn valid_metadata(valid: bool) -> Result<()> {
    if !valid {
        return Err(St3Error::new(
            "projection-detail-invalid-source",
            "observer/subscription detail has invalid stored metadata types",
        )
        .into());
    }
    Ok(())
}

fn decode_body(body: &str, column: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(body).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn decode_time(value: String, column: usize) -> rusqlite::Result<u128> {
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn detail_read_error(error: rusqlite::Error) -> anyhow::Error {
    match error {
        rusqlite::Error::FromSqlConversionFailure(..) | rusqlite::Error::InvalidColumnType(..) => {
            St3Error::new(
                "projection-detail-invalid-source",
                "observer/subscription detail has invalid stored encoding or types",
            )
            .into()
        }
        error => error.into(),
    }
}
