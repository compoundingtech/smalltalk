//! Consumer-specific ordered seat-run source, not a card/authority certificate.
//! Normal moves splice one ordered node. Late source changes undo/reapply an affected
//! canonical suffix in bounded durable pages; reads refuse incomplete maintenance.
#![allow(dead_code)]
use super::*;
use smallclaims::store::canonical::{ClaimKey, sortable_key};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_seat_order_joins (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, at BLOB NOT NULL CHECK(length(at)=16), live INTEGER NOT NULL,
 PRIMARY KEY(namespace,agent,run)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_seat_order_moves (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, rank BLOB NOT NULL, run TEXT NOT NULL,
 placement TEXT NOT NULL, anchor TEXT, PRIMARY KEY(namespace,agent,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_seat_order_refs (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, rank BLOB NOT NULL, id TEXT NOT NULL, stage INTEGER NOT NULL,
 PRIMARY KEY(namespace,agent,run,rank,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_seat_order_events (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, rank BLOB NOT NULL, kind TEXT NOT NULL, run TEXT NOT NULL, placement TEXT, anchor TEXT,
 PRIMARY KEY(namespace,agent,rank)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_seat_order_refs_claim ON local_seat_order_refs(namespace,agent,id);
CREATE UNIQUE INDEX IF NOT EXISTS local_seat_order_join_event
 ON local_seat_order_events(namespace,agent,run) WHERE kind='join';
CREATE TABLE IF NOT EXISTS local_seat_order_state (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, ready INTEGER NOT NULL, rewind_from BLOB,
 PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_seat_order_pending ON local_seat_order_state(namespace,agent) WHERE ready=0;
CREATE TABLE IF NOT EXISTS local_seat_order_journal (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, rank BLOB NOT NULL, undo TEXT NOT NULL, PRIMARY KEY(namespace,agent,rank)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_seat_order_nodes (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, position BLOB NOT NULL CHECK(length(position)<=4096), live INTEGER NOT NULL,
 PRIMARY KEY(namespace,agent,run)
) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS local_seat_order_position
 ON local_seat_order_nodes(namespace,agent,position);
CREATE INDEX IF NOT EXISTS local_seat_order_live
 ON local_seat_order_nodes(namespace,agent,position,run) WHERE live=1;
CREATE TABLE IF NOT EXISTS local_seat_order_rank_dirty (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, PRIMARY KEY(namespace,agent,run)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS seat_order_rank_insert AFTER INSERT ON local_seat_order_nodes BEGIN
 INSERT INTO local_seat_order_rank_dirty VALUES(NEW.namespace,NEW.agent,NEW.run) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS seat_order_rank_delete AFTER DELETE ON local_seat_order_nodes BEGIN
 INSERT INTO local_seat_order_rank_dirty VALUES(OLD.namespace,OLD.agent,OLD.run) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS seat_order_rank_update AFTER UPDATE OF position,live ON local_seat_order_nodes
 WHEN OLD.position<>NEW.position OR OLD.live<>NEW.live BEGIN
 INSERT INTO local_seat_order_rank_dirty VALUES(NEW.namespace,NEW.agent,NEW.run) ON CONFLICT DO NOTHING;
END;
"#;
const NEXT: &str = "SELECT rank,kind,run,placement,anchor FROM local_seat_order_events
 WHERE namespace=?1 AND agent=?2 AND rank>?3 ORDER BY rank LIMIT 1";
const LAST_APPLIED: &str = "SELECT rank,undo FROM local_seat_order_journal
 WHERE namespace=?1 AND agent=?2 ORDER BY rank DESC LIMIT 1";
const WINDOW: &str = "SELECT run FROM local_seat_order_nodes
 WHERE namespace=?1 AND agent=?2 AND live=1 ORDER BY position,run LIMIT ?3";

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(crate) fn clean(connection: &Connection, namespace: &str) -> Result<bool> {
    Ok(!connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_seat_order_state WHERE namespace=?1 AND ready=0)",
        [namespace],
        |row| row.get::<_, bool>(0),
    )?)
}

pub(crate) fn position(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    run: &str,
) -> Result<Option<(Vec<u8>, bool)>> {
    Ok(connection
        .query_row(
            "SELECT position,live FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}
fn initialize(tx: &Transaction<'_>, namespace: &str, agent: &str) -> Result<()> {
    anyhow::ensure!(agent.starts_with("agent/"), "seat order agent subject");
    tx.execute(
        "INSERT INTO local_seat_order_state VALUES(?1,?2,1,NULL) ON CONFLICT DO NOTHING",
        [namespace, agent],
    )?;
    Ok(())
}
fn dirty(tx: &Transaction<'_>, namespace: &str, agent: &str, rank: &[u8]) -> Result<()> {
    initialize(tx, namespace, agent)?;
    let last: Option<Vec<u8>> = tx
        .query_row(
            "SELECT rank FROM local_seat_order_journal WHERE namespace=?1 AND agent=?2 ORDER BY rank DESC LIMIT 1",
            [namespace, agent],
            |row| row.get(0),
        )
        .optional()?;
    if last.as_ref().is_some_and(|last| last.as_slice() >= rank) {
        tx.execute("UPDATE local_seat_order_state SET ready=0,rewind_from=CASE WHEN rewind_from IS NULL OR ?3<rewind_from THEN ?3 ELSE rewind_from END WHERE namespace=?1 AND agent=?2",params![namespace, agent,rank])?;
    } else {
        tx.execute(
            "UPDATE local_seat_order_state SET ready=0 WHERE namespace=?1 AND agent=?2 AND ready<>0",
            [namespace, agent],
        )?;
    }
    Ok(())
}
fn move_rank(canonical: &[u8], stage: u8) -> Result<Vec<u8>> {
    anyhow::ensure!(canonical.len() > 16, "seat move canonical rank width");
    let mut rank = canonical[..16].to_vec();
    rank.push(1);
    rank.extend_from_slice(&canonical[16..]);
    rank.push(stage);
    Ok(rank)
}
fn join_rank(at: &[u8], run: &str) -> Vec<u8> {
    let mut rank = at.to_vec();
    rank.push(0);
    rank.extend_from_slice(run.as_bytes());
    rank
}
fn refresh_join(tx: &Transaction<'_>, namespace: &str, agent: &str, run: &str) -> Result<()> {
    let old: Option<Vec<u8>> = tx
        .query_row(
            "SELECT rank FROM local_seat_order_events WHERE namespace=?1 AND agent=?2 AND run=?3 AND kind='join'",
            params![namespace, agent, run],
            |row| row.get(0),
        )
        .optional()?;
    let at: Option<Vec<u8>> = tx
        .query_row(
            "SELECT at FROM local_seat_order_joins WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
            |row| row.get(0),
        )
        .optional()?;
    let new = if let Some(at) = at {
        let reference: Option<(Vec<u8>,u8)>=tx.query_row("SELECT rank,stage FROM local_seat_order_refs WHERE namespace=?1 AND agent=?2 AND run=?3 ORDER BY rank,id LIMIT 1",params![namespace, agent,run],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((reference, stage)) = reference.filter(|(rank, _)| rank[..16] < at[..]) {
            Some(move_rank(&reference, stage)?)
        } else {
            Some(join_rank(&at, run))
        }
    } else {
        None
    };
    if old == new {
        return Ok(());
    }
    if let Some(old) = old {
        dirty(tx, namespace, agent, &old)?;
        tx.execute(
            "DELETE FROM local_seat_order_events WHERE namespace=?1 AND agent=?2 AND rank=?3",
            params![namespace, agent, old],
        )?;
    }
    if let Some(new) = new {
        dirty(tx, namespace, agent, &new)?;
        tx.execute(
            "INSERT INTO local_seat_order_events VALUES(?1,?2,?3,'join',?4,NULL,NULL)",
            params![namespace, agent, new, run],
        )?;
    }
    Ok(())
}

/// The owner supplies the exact existing join time and current live membership. This
/// operation neither scans steps nor infers source coverage; removal is explicit None.
pub(crate) fn set_join(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    run: &str,
    join: Option<(u128, bool)>,
) -> Result<bool> {
    initialize(tx, namespace, agent)?;
    anyhow::ensure!(run.starts_with("mission-run/"), "seat join run subject");
    let old: Option<(Vec<u8>, bool)> = tx
        .query_row(
            "SELECT at,live FROM local_seat_order_joins WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let new = join.map(|(at, live)| (at.to_be_bytes().to_vec(), live));
    if old == new {
        return Ok(false);
    }
    if let Some((at, live)) = &new {
        tx.execute("INSERT INTO local_seat_order_joins VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent,run) DO UPDATE SET at=excluded.at,live=excluded.live",params![namespace, agent,run,at,live])?;
    } else {
        tx.execute(
            "DELETE FROM local_seat_order_joins WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
        )?;
    }
    let live = new.as_ref().is_some_and(|(_, live)| *live);
    tx.execute(
        "UPDATE local_seat_order_nodes SET live=?4 WHERE namespace=?1 AND agent=?2 AND run=?3 AND live<>?4",
        params![namespace, agent, run, live],
    )?;
    // Membership can change without changing an event's rank. The source owner publishes
    // a boundary only after the same transaction/page has reached this component's clean cut.
    tx.execute(
        "UPDATE local_seat_order_state SET ready=0 WHERE namespace=?1 AND agent=?2 AND ready<>0",
        [namespace, agent],
    )?;
    refresh_join(tx, namespace, agent, run)?;
    Ok(true)
}

/// Capture an admitted canonical move or its explicit retraction. Source ownership,
/// admission/repair eligibility and invocation in the certified writer cut are caller-owned.
pub(crate) fn set_move(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    id: &str,
    movement: Option<(&ClaimKey, &QueueMove)>,
) -> Result<bool> {
    let normalized = movement
        .map(|(key, movement)| {
            anyhow::ensure!(key.5 == id, "seat move canonical identity mismatch");
            Ok::<_, anyhow::Error>((sortable_key(key), movement))
        })
        .transpose()?;
    set_move_rank(
        tx,
        namespace,
        agent,
        id,
        normalized
            .as_ref()
            .map(|(rank, movement)| (rank.as_slice(), *movement)),
    )
}

// The source owner attests complete canonical rank and input identity before this call.
pub(crate) fn set_move_rank(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    id: &str,
    movement: Option<(&[u8], &QueueMove)>,
) -> Result<bool> {
    initialize(tx, namespace, agent)?;
    let old: Option<(Vec<u8>, String, String, Option<String>)> = tx
        .query_row(
            "SELECT rank,run,placement,anchor FROM local_seat_order_moves WHERE namespace=?1 AND agent=?2 AND id=?3",
            params![namespace, agent, id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let new = movement
        .map(|(canonical, movement)| {
            anyhow::ensure!(
                canonical.len() > 16 && canonical[..16] == movement.at_unix_ms.to_be_bytes(),
                "seat move canonical identity/time mismatch"
            );
            Ok::<_, anyhow::Error>((
                canonical.to_vec(),
                movement.run.clone(),
                movement.placement.as_str().to_owned(),
                movement.anchor.clone(),
            ))
        })
        .transpose()?;
    if old == new {
        return Ok(false);
    }
    let mut affected = BTreeSet::new();
    for source in [old.as_ref(), new.as_ref()].into_iter().flatten() {
        affected.insert(source.1.clone());
        affected.extend(source.3.iter().cloned());
    }
    if let Some((rank, _, _, _)) = &old {
        let event = move_rank(rank, 2)?;
        dirty(tx, namespace, agent, &event)?;
        tx.execute(
            "DELETE FROM local_seat_order_events WHERE namespace=?1 AND agent=?2 AND rank=?3",
            params![namespace, agent, event],
        )?;
        tx.execute(
            "DELETE FROM local_seat_order_refs WHERE namespace=?1 AND agent=?2 AND id=?3",
            params![namespace, agent, id],
        )?;
        tx.execute(
            "DELETE FROM local_seat_order_moves WHERE namespace=?1 AND agent=?2 AND id=?3",
            params![namespace, agent, id],
        )?;
    }
    if let Some((rank, run, placement, anchor)) = &new {
        tx.execute(
            "INSERT INTO local_seat_order_moves VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![namespace, agent, id, rank, run, placement, anchor],
        )?;
        for (stage, named) in std::iter::once(run).chain(anchor.iter()).enumerate() {
            tx.execute("INSERT INTO local_seat_order_refs VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,agent,run,rank,id) DO UPDATE SET stage=MIN(stage,excluded.stage)",params![namespace, agent,named,rank,id,stage])?;
        }
        let event = move_rank(rank, 2)?;
        dirty(tx, namespace, agent, &event)?;
        tx.execute(
            "INSERT INTO local_seat_order_events VALUES(?1,?2,?3,'move',?4,?5,?6)",
            params![namespace, agent, event, run, placement, anchor],
        )?;
    }
    for run in affected {
        refresh_join(tx, namespace, agent, &run)?;
    }
    Ok(true)
}

#[derive(Debug)]
pub(crate) struct PositionExhausted;
impl std::fmt::Display for PositionExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("seat position key bound exhausted")
    }
}
impl std::error::Error for PositionExhausted {}

/// A finite binary fractional key strictly between its neighbors. Keys never end in zero,
/// so extending a shared prefix always leaves room. Exhaustion fences rather than rebases.
fn between(left: Option<&[u8]>, right: Option<&[u8]>) -> Result<Vec<u8>> {
    anyhow::ensure!(
        left.is_none_or(|l| right.is_none_or(|r| l < r)),
        "seat position bounds"
    );
    let mut output = Vec::new();
    let mut right = right;
    for i in 0..4096 {
        let lower = left.and_then(|key| key.get(i)).copied().unwrap_or(0) as u16;
        let upper = match right {
            Some(key) => *key.get(i).context("seat position right prefix")? as u16,
            None => 256,
        };
        anyhow::ensure!(lower <= upper, "seat position interval");
        if upper - lower > 1 {
            output.push(((lower + upper) / 2) as u8);
            return Ok(output);
        }
        output.push(lower as u8);
        if lower < upper {
            right = None;
        }
    }
    Err(PositionExhausted.into())
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Undo {
    run: String,
    position: Option<Vec<u8>>,
}
fn node(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    run: &str,
) -> Result<Option<Vec<u8>>> {
    Ok(connection
        .query_row(
            "SELECT position FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
            |row| row.get(0),
        )
        .optional()?)
}
fn live(connection: &Connection, namespace: &str, agent: &str, run: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT live FROM local_seat_order_joins WHERE namespace=?1 AND agent=?2 AND run=?3",
            params![namespace, agent, run],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}
fn neighbor(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    run: &str,
    position: Option<&[u8]>,
    after: bool,
) -> Result<Option<Vec<u8>>> {
    let sql = match (position.is_some(), after) {
        (false, false) => {
            "SELECT position FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run<>?3 ORDER BY position LIMIT 1"
        }
        (false, true) => {
            "SELECT position FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run<>?3 ORDER BY position DESC LIMIT 1"
        }
        (true, false) => {
            "SELECT position FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run<>?3 AND position<?4 ORDER BY position DESC LIMIT 1"
        }
        (true, true) => {
            "SELECT position FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run<>?3 AND position>?4 ORDER BY position LIMIT 1"
        }
    };
    let mut statement = connection.prepare_cached(sql)?;
    let value = if position.is_some() {
        statement
            .query_row(params![namespace, agent, run, position], |row| row.get(0))
            .optional()?
    } else {
        statement
            .query_row(params![namespace, agent, run], |row| row.get(0))
            .optional()?
    };
    Ok(value)
}
fn apply(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    kind: &str,
    run: &str,
    placement: Option<&str>,
    anchor: Option<&str>,
) -> Result<Option<Undo>> {
    let old = node(tx, namespace, agent, run)?;
    let (left, right) = if kind == "join" {
        if old.is_some() {
            return Ok(None);
        }
        (neighbor(tx, namespace, agent, run, None, true)?, None)
    } else {
        if old.is_none() {
            return Ok(None);
        }
        match placement
            .and_then(Placement::parse)
            .context("seat event placement")?
        {
            Placement::Top => (None, neighbor(tx, namespace, agent, run, None, false)?),
            Placement::Bottom => (neighbor(tx, namespace, agent, run, None, true)?, None),
            Placement::Before | Placement::After => {
                let Some(anchor) = anchor.filter(|anchor| *anchor != run) else {
                    return Ok(None);
                };
                let Some(at) = node(tx, namespace, agent, anchor)? else {
                    return Ok(None);
                };
                if placement == Some("before") {
                    (
                        neighbor(tx, namespace, agent, run, Some(&at), false)?,
                        Some(at),
                    )
                } else {
                    (
                        Some(at.clone()),
                        neighbor(tx, namespace, agent, run, Some(&at), true)?,
                    )
                }
            }
        }
    };
    // A move that already occupies its requested interval changes no node.
    if old.as_ref().is_some_and(|old| {
        left.as_ref().is_none_or(|left| left < old)
            && right.as_ref().is_none_or(|right| old < right)
    }) {
        return Ok(None);
    }
    let position = between(left.as_deref(), right.as_deref())?;
    tx.execute("INSERT INTO local_seat_order_nodes VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent,run) DO UPDATE SET position=excluded.position,live=excluded.live",params![namespace, agent,run,position,live(tx, namespace,agent,run)?])?;
    Ok(Some(Undo {
        run: run.into(),
        position: old,
    }))
}
fn undo(tx: &Transaction<'_>, namespace: &str, agent: &str, undo: Option<Undo>) -> Result<()> {
    if let Some(undo) = undo {
        if let Some(position) = undo.position {
            tx.execute("INSERT INTO local_seat_order_nodes VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent,run) DO UPDATE SET position=excluded.position,live=excluded.live",params![namespace, agent,undo.run,position,live(tx, namespace,agent,&undo.run)?])?;
        } else {
            tx.execute(
                "DELETE FROM local_seat_order_nodes WHERE namespace=?1 AND agent=?2 AND run=?3",
                params![namespace, agent, undo.run],
            )?;
        }
    }
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Page {
    pub processed: usize,
    pub ready: bool,
}

/// At most limit undo/apply events (1..=1024), with durable journal and readiness.
/// The enclosing writer transaction makes each page and concurrent source mutation atomic.
pub(crate) fn page(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    limit: usize,
) -> Result<Page> {
    anyhow::ensure!((1..=1024).contains(&limit), "seat event page bound");
    initialize(tx, namespace, agent)?;
    let mut processed = 0;
    while processed < limit {
        let rewind: Option<Vec<u8>> = tx.query_row(
            "SELECT rewind_from FROM local_seat_order_state WHERE namespace=?1 AND agent=?2",
            [namespace, agent],
            |row| row.get(0),
        )?;
        let last: Option<(Vec<u8>, String)> = tx
            .query_row(LAST_APPLIED, [namespace, agent], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some(rewind) = rewind {
            if let Some((rank, body)) = last.as_ref().filter(|(rank, _)| rank >= &rewind) {
                undo(tx, namespace, agent, serde_json::from_str(body)?)?;
                tx.execute(
                    "DELETE FROM local_seat_order_journal WHERE namespace=?1 AND agent=?2 AND rank=?3",
                    params![namespace, agent, rank],
                )?;
                processed += 1;
                continue;
            }
            tx.execute(
                "UPDATE local_seat_order_state SET rewind_from=NULL WHERE namespace=?1 AND agent=?2",
                [namespace, agent],
            )?;
        }
        let from = last
            .as_ref()
            .map(|(rank, _)| rank.as_slice())
            .unwrap_or(&[]);
        let next: Option<(Vec<u8>, String, String, Option<String>, Option<String>)> = tx
            .query_row(NEXT, params![namespace, agent, from], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?;
        let Some((rank, kind, run, placement, anchor)) = next else {
            break;
        };
        let undo = apply(
            tx,
            namespace,
            agent,
            &kind,
            &run,
            placement.as_deref(),
            anchor.as_deref(),
        )?;
        tx.execute(
            "INSERT INTO local_seat_order_journal VALUES(?1,?2,?3,?4)",
            params![namespace, agent, rank, serde_json::to_string(&undo)?],
        )?;
        processed += 1;
    }
    let rewind: Option<Vec<u8>> = tx.query_row(
        "SELECT rewind_from FROM local_seat_order_state WHERE namespace=?1 AND agent=?2",
        [namespace, agent],
        |row| row.get(0),
    )?;
    let last: Option<Vec<u8>> = tx
        .query_row(
            "SELECT rank FROM local_seat_order_journal WHERE namespace=?1 AND agent=?2 ORDER BY rank DESC LIMIT 1",
            [namespace, agent],
            |row| row.get(0),
        )
        .optional()?;
    let needs_undo = rewind
        .as_ref()
        .is_some_and(|rewind| last.as_ref().is_some_and(|last| last >= rewind));
    let next: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_seat_order_events WHERE namespace=?1 AND agent=?2 AND rank>?3)",
        params![namespace, agent, last.as_deref().unwrap_or(&[])],
        |row| row.get(0),
    )?;
    let ready = !needs_undo && !next;
    if ready {
        tx.execute("UPDATE local_seat_order_state SET ready=1,rewind_from=NULL WHERE namespace=?1 AND agent=?2 AND (ready<>1 OR rewind_from IS NOT NULL)",[namespace, agent])?;
    }
    Ok(Page { processed, ready })
}
pub(crate) fn window(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    limit: usize,
) -> Result<Vec<String>> {
    anyhow::ensure!((1..=501).contains(&limit), "seat run window bound");
    let ready: Option<bool> = connection
        .query_row(
            "SELECT ready FROM local_seat_order_state WHERE namespace=?1 AND agent=?2",
            [namespace, agent],
            |row| row.get(0),
        )
        .optional()?;
    anyhow::ensure!(ready == Some(true), "seat run source unavailable");
    Ok(connection
        .prepare_cached(WINDOW)?
        .query_map(params![namespace, agent, limit], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests;
