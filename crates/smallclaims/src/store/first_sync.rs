//! First sync: a node that joins a fleet syncs claims from its sponsor until it holds the same
//! envelopes as a peer and has admitted them. That ends the first sync. Projections are this
//! node's own pure functions of those claims, so sync never compares or repairs them.

use super::*;
use crate::replication::ReplicationFirstSync;

impl Store {
    /// This node's first sync after it joined, if it joined.
    pub fn first_sync(&self) -> Result<Option<ReplicationFirstSync>> {
        let connection = self.readers.get();
        first_sync_tx(&connection)
    }

    /// Start this node's first sync: it ends at the first exchange at which this node holds the
    /// same envelopes as a peer.
    pub fn begin_first_sync(&self, sponsor: &str) -> Result<()> {
        let connection = self.connection.write();
        save_first_sync(
            &connection,
            &ReplicationFirstSync {
                state: "syncing".into(),
                started_at_unix_ms: now_ms(),
                peer: Some(sponsor.to_owned()),
                ..Default::default()
            },
        )
    }

    /// End a mixed-build first sync at a verified equal wire log. Like any first sync it settles
    /// on claims; the log digest records what it verified.
    pub fn observe_first_sync_log(
        &self,
        peer: &str,
        envelopes: u64,
        authority_digest: &str,
    ) -> Result<()> {
        let connection = self.connection.write();
        let Some(mut first) = first_sync_tx(&connection)? else {
            return Ok(());
        };
        if matches!(first.state.as_str(), "syncing" | "failed") {
            first.state = "verified".into();
            first.ended_at_unix_ms = Some(now_ms());
            first.peer = Some(peer.to_owned());
            first.envelopes = Some(envelopes);
            first.graph_digest = None;
            first.peer_graph_digest = None;
            first.authority_digest = Some(authority_digest.to_owned());
            first.message = None;
            save_first_sync(&connection, &first)?;
        }
        Ok(())
    }

    /// End the first sync at an exchange where this node holds the same envelopes as `peer`
    /// and has admitted and projected them all. It settles on claims alone: comparing
    /// projections is not part of sync.
    pub fn observe_first_sync(&self, peer: &str, envelopes: u64) -> Result<()> {
        let connection = self.connection.write();
        let Some(mut first) = first_sync_tx(&connection)? else {
            return Ok(());
        };
        // A mixed-build verification stays settled once builds match.
        if first.state == "verified" {
            return Ok(());
        }
        first.state = "verified".into();
        first.ended_at_unix_ms = Some(now_ms());
        first.peer = Some(peer.to_owned());
        first.envelopes = Some(envelopes);
        first.message = None;
        save_first_sync(&connection, &first)
    }
}

pub fn first_sync_tx(connection: &Connection) -> Result<Option<ReplicationFirstSync>> {
    let value = connection
        .query_row("SELECT value FROM meta WHERE key='first_sync'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    Ok(value.and_then(|value| serde_json::from_str(&value).ok()))
}

pub fn save_first_sync(connection: &Connection, first: &ReplicationFirstSync) -> Result<()> {
    connection.execute(
        /* sqlite-large-value: allow-for-now lv-he36db82473fc owner=smallclaims::store::first_sync TODO=bound-or-reference-values-and-reclamation */ "INSERT OR REPLACE INTO meta(key, value) VALUES ('first_sync', ?1)",
        [serde_json::to_string(first)?],
    )?;
    Ok(())
}
