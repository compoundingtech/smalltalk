//! One total order for shared claim folds. Arrival indexes are used only to recover the
//! relative wire position of legacy batches that predate replica_records.
use super::*;

pub struct CanonicalOrder(pub bool);

impl std::fmt::Display for CanonicalOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&order_sql("claims", self.0))
    }
}

pub const CANONICAL_ORDER: CanonicalOrder = CanonicalOrder(false);
pub const CANONICAL_ORDER_DESC: CanonicalOrder = CanonicalOrder(true);

pub fn components(alias: &str) -> Vec<String> {
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

pub fn order_sql(alias: &str, descending: bool) -> String {
    let direction = if descending { " DESC" } else { "" };
    components(alias)
        .into_iter()
        .map(|part| format!("{part}{direction}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn after_sql(left: &str, right: &str) -> String {
    format!(
        "({}) > ({})",
        components(left).join(", "),
        components(right).join(", ")
    )
}

pub fn position_sql(alias: &str) -> String {
    components(alias).remove(5)
}

pub fn key_from_record(
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
pub fn canonical_sql(query: &str) -> String {
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

pub type ClaimKey = (u128, String, u64, String, u64, String);

/// SQLite BLOB ordering equivalent to ClaimKey's total order. Fixed-width numbers sort
/// numerically; escaped zeroes and a double-zero terminator preserve string prefix ordering.
pub fn sortable_key(key: &ClaimKey) -> Vec<u8> {
    fn string(output: &mut Vec<u8>, value: &str) {
        for byte in value.bytes() {
            output.push(byte);
            if byte == 0 {
                output.push(255);
            }
        }
        output.extend_from_slice(&[0, 0]);
    }
    let mut output = key.0.to_be_bytes().to_vec();
    string(&mut output, &key.1);
    output.extend_from_slice(&key.2.to_be_bytes());
    string(&mut output, &key.3);
    output.extend_from_slice(&key.4.to_be_bytes());
    string(&mut output, &key.5);
    output
}

pub fn claim_key(connection: &Connection, id: &str) -> Result<ClaimKey> {
    static QUERY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let position = position_sql("claims");
        format!("SELECT claims.accepted_at_unix_ms, batches.origin, batches.replica_sequence,
            claims.batch_id, {position}, claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
            WHERE claims.id=?1")
    });
    let (time, writer, sequence, batch, position, id) =
        connection.prepare_cached(&QUERY)?.query_row([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
    Ok((time.parse()?, writer, sequence, batch, position, id))
}

#[cfg(test)]
#[test]
fn claim_keys_preserve_numeric_time_and_record_or_legacy_position_order() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(
        "CREATE TABLE batches(id TEXT, origin TEXT, replica_sequence INTEGER);
         CREATE TABLE claims(id TEXT, batch_id TEXT, accepted_at_unix_ms TEXT, store_index INTEGER);
         CREATE TABLE replica_records(claim_id TEXT, position INTEGER);
         INSERT INTO batches VALUES ('batch', 'writer', 2);
         INSERT INTO claims VALUES ('a', 'batch', '10', 3), ('b', 'batch', '9', 2), ('c', 'batch', '10', 1);
         INSERT INTO replica_records VALUES ('b', 8), ('b', 4);"
    ).unwrap();
    let mut keys: Vec<_> = ["a", "b", "c"]
        .map(|id| claim_key(&connection, id).unwrap())
        .into();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            (9, "writer".into(), 2, "batch".into(), 4, "b".into()),
            (10, "writer".into(), 2, "batch".into(), 0, "c".into()),
            (10, "writer".into(), 2, "batch".into(), 2, "a".into()),
        ]
    );
}
