//! Bounded committed public-row keys. This observes namespace output only; it never writes
//! an Installer root, certifies a source cut, or authorizes a public row.
use anyhow::Result;
use rusqlite::{Connection, Transaction};
use smallclaims::ivm::install::Namespace;

const LIMIT: usize = 1024;
pub fn create_schema(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS st3_agent_row_changes(namespace TEXT NOT NULL,agent TEXT NOT NULL,PRIMARY KEY(namespace,agent));
    CREATE TABLE IF NOT EXISTS st3_agent_row_change_state(namespace TEXT PRIMARY KEY,refresh INTEGER NOT NULL CHECK(refresh IN(0,1)));")?;
    for (event, reference, condition) in [
        ("INSERT", "NEW", "1"),
        ("UPDATE", "NEW", "NEW.key_generation<>OLD.key_generation"),
        ("DELETE", "OLD", "1"),
    ] {
        let offset = LIMIT - 1;
        c.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS st3_agent_row_changes_{event} AFTER {event} ON local_agent_card_rows
        WHEN {condition} AND EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace={reference}.namespace AND ready=1)
        BEGIN
          INSERT INTO st3_agent_row_change_state VALUES({reference}.namespace,0) ON CONFLICT DO NOTHING;
          INSERT INTO st3_agent_row_changes SELECT {reference}.namespace,{reference}.agent
            WHERE NOT EXISTS(SELECT 1 FROM st3_agent_row_changes WHERE namespace={reference}.namespace LIMIT 1 OFFSET {offset}) ON CONFLICT DO NOTHING;
          UPDATE st3_agent_row_change_state SET refresh=1 WHERE namespace={reference}.namespace
            AND NOT EXISTS(SELECT 1 FROM st3_agent_row_changes WHERE namespace={reference}.namespace AND agent={reference}.agent);
        END"))?;
    }
    Ok(())
}

/// None means the complete key set exceeded its bound: use an explicit bounded-window refresh.
pub fn page(c: &Connection, ns: &Namespace) -> Result<Option<Vec<String>>> {
    let refresh: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM st3_agent_row_change_state WHERE namespace=?1 AND refresh=1)",
        [ns.as_str()],
        |r| r.get(0),
    )?;
    if refresh {
        return Ok(None);
    }
    c.prepare_cached(
        "SELECT agent FROM st3_agent_row_changes WHERE namespace=?1 ORDER BY agent LIMIT 1024",
    )?
    .query_map([ns.as_str()], |r| r.get(0))?
    .collect::<rusqlite::Result<Vec<String>>>()
    .map(Some)
    .map_err(Into::into)
}

/// Acknowledge in the same transaction as successful Views synchronization and certificate
/// persistence. Rollback keeps both output keys and the prior delivered source boundary.
pub fn ack(tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
    tx.execute("DELETE FROM st3_agent_row_changes WHERE namespace=?1 AND agent IN (SELECT agent FROM st3_agent_row_changes WHERE namespace=?1 ORDER BY agent LIMIT 1024)",[ns.as_str()])?;
    tx.execute(
        "DELETE FROM st3_agent_row_change_state WHERE namespace=?1",
        [ns.as_str()],
    )?;
    Ok(())
}
