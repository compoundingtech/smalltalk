use super::*;
pub use smallclaims::store::canonical::*;

pub fn claim_key(connection: &Connection, id: &str) -> Result<ClaimKey> {
    if id.starts_with("local-observation/") {
        let row: Option<(u128, String, String)> = connection
            .query_row(
                "SELECT source_at,origin,source_id FROM latest_values WHERE source_id=?1",
                [id],
                |r| Ok((r.get::<_, u64>(0)? as u128, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((at, origin, id)) = row {
            return Ok((at, origin, at as u64, id.clone(), 0, id));
        }
    }
    smallclaims::store::canonical::claim_key(connection, id)
}
