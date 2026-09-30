//! One total order for shared claim folds. Arrival indexes are used only to recover the
//! relative wire position of legacy batches that predate replica_records.
use super::*;

pub(super) struct CanonicalOrder(pub bool);

impl std::fmt::Display for CanonicalOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&order_sql("claims", self.0))
    }
}

pub(super) const CANONICAL_ORDER: CanonicalOrder = CanonicalOrder(false);
pub(super) const CANONICAL_ORDER_DESC: CanonicalOrder = CanonicalOrder(true);

fn components(alias: &str) -> Vec<String> {
    vec![
        format!("length({alias}.accepted_at_unix_ms)"),
        format!("{alias}.accepted_at_unix_ms"),
        format!("(SELECT origin FROM batches WHERE id={alias}.batch_id)"),
        format!("(SELECT replica_sequence FROM batches WHERE id={alias}.batch_id)"),
        format!("{alias}.batch_id"),
        format!(
            "COALESCE((SELECT MIN(position) FROM replica_records WHERE claim_id={alias}.id), \
            (SELECT COUNT(*) FROM claims legacy_position WHERE legacy_position.batch_id={alias}.batch_id \
            AND legacy_position.store_index<{alias}.store_index))"
        ),
        format!("{alias}.id"),
    ]
}

pub(super) fn order_sql(alias: &str, descending: bool) -> String {
    let direction = if descending { " DESC" } else { "" };
    components(alias)
        .into_iter()
        .map(|part| format!("{part}{direction}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn after_sql(left: &str, right: &str) -> String {
    format!(
        "({}) > ({})",
        components(left).join(", "),
        components(right).join(", ")
    )
}

pub(super) fn position_sql(alias: &str) -> String {
    components(alias).remove(5)
}

pub(super) fn key_from_record(
    claim: &ClaimRecord,
    writer: String,
    sequence: u64,
    position: u64,
) -> ClaimKey {
    (
        claim.accepted_at_unix_ms,
        writer,
        sequence,
        claim.batch_id.clone(),
        position,
        claim.id.clone(),
    )
}

/// Expand explicit canonical ordering markers without introducing joins or ambiguous column
/// names in the caller's SQL. Aliases are fixed source identifiers, never user input.
pub(super) fn canonical_sql(query: &str) -> String {
    let mut query = query.to_owned();
    for alias in ["claims", "request", "created", "sent", "resolution"] {
        for (marker, descending) in [("CANONICAL_ASC", false), ("CANONICAL_DESC", true)] {
            let marker = format!("{marker}({alias})");
            if query.contains(&marker) {
                query = query.replace(&marker, &order_sql(alias, descending));
            }
        }
    }
    query
}

pub(super) type ClaimKey = (u128, String, u64, String, u64, String);

pub(super) fn claim_key(connection: &Connection, id: &str) -> Result<ClaimKey> {
    let position = position_sql("claims");
    let (time, writer, sequence, batch, position, id) = connection.query_row(
        &format!("SELECT claims.accepted_at_unix_ms, batches.origin, batches.replica_sequence,
            claims.batch_id, {position}, claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
            WHERE claims.id=?1"), [id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?,
            row.get::<_, String>(3)?, row.get::<_, u64>(4)?, row.get::<_, String>(5)?,
        )),
    )?;
    Ok((time.parse()?, writer, sequence, batch, position, id))
}
