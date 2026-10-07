//! Captured finite deadline/repair input. A due or pending indexed work item owns each tick.
//! This never scans recipients, advances a source certificate, or writes an Installer root.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub enum Reason {
    Deadline,
    Kernel,
    ProducerAck,
}
impl Reason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Kernel => "kernel",
            Self::ProducerAck => "producer-ack",
        }
    }
}

pub fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS local_agent_card_clock(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),at_ms TEXT NOT NULL,
        revision INTEGER NOT NULL CHECK(revision>=0),snapshot_index INTEGER NOT NULL CHECK(snapshot_index>=0),reason TEXT NOT NULL
        CHECK(reason IN ('deadline','kernel','producer-ack')))",
    )?;
    Ok(())
}

pub fn tick(tx: &Transaction<'_>, at_ms: u128, reason: Reason) -> Result<()> {
    let previous: Option<(String, u64)> = tx
        .query_row(
            "SELECT at_ms,revision FROM local_agent_card_clock WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let revision = if let Some((at, revision)) = previous {
        ensure!(
            at_ms >= at.parse::<u128>()?,
            "agent captured clock moved backwards"
        );
        revision
            .checked_add(1)
            .filter(|revision| *revision <= i64::MAX as u64)
            .ok_or_else(|| anyhow::anyhow!("agent captured clock revision exhausted"))?
    } else {
        1
    };
    tx.execute("INSERT INTO local_agent_card_clock(singleton,at_ms,revision,reason,snapshot_index) VALUES(1,?1,?2,?3,?4)
        ON CONFLICT(singleton) DO UPDATE SET at_ms=excluded.at_ms,revision=excluded.revision,reason=excluded.reason,snapshot_index=excluded.snapshot_index",
        params![at_ms.to_string(),revision,reason.as_str(),smallclaims::store::current_index(tx)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_clock_is_monotone_and_rollback_does_not_advance_revision() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        connection.execute_batch("CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO claims DEFAULT VALUES").unwrap();
        let tx = connection.transaction().unwrap();
        tick(&tx, 100, Reason::Kernel).unwrap();
        tick(&tx, 100, Reason::ProducerAck).unwrap();
        assert!(tick(&tx, 99, Reason::Deadline).is_err());
        assert_eq!(
            tx.query_row("SELECT revision FROM local_agent_card_clock", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            2
        );
        assert_eq!(tx.query_row("SELECT snapshot_index FROM local_agent_card_clock", [], |r| r.get::<_, u64>(0)).unwrap(),1);
        tx.rollback().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM local_agent_card_clock", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
}
