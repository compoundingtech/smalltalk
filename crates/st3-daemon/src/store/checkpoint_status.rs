//! Startup advertisements supplement checkpoint status without changing agreement inputs.
use super::*;

impl Store {
    pub fn checkpoint_status(
        &self,
        now_unix_ms: u128,
        configured_peers: &[String],
    ) -> Result<CheckpointStatusView> {
        let mut status = self
            .graph
            .checkpoint_status(now_unix_ms, configured_peers)?;
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&canonical_sql(
            "SELECT json_extract(claims.body,'$.fields.version')
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='daemon.started' AND claims.origin=?2
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        ))?;
        for member in &status.participants {
            let version: Option<String> = statement
                .query_row(params![format!("daemon/{member}"), member], |row| {
                    row.get(0)
                })
                .optional()?
                .flatten();
            if let Some(version) = version
                && version.len() <= 128
                && version
                    .split_once('+')
                    .is_some_and(|(_, revision)| !revision.is_empty() && revision != "dev")
            {
                status.machine_version.insert(member.clone(), version);
            }
        }
        Ok(status)
    }
}
