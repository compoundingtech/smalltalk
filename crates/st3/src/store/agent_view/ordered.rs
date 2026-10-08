//! Canonically ordered reductions. Each edit touches only the search path; persisted
//! subtree summaries let reads reduce an epoch without visiting its observations.
use super::*;

const MAX_DEPTH: usize = 128;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_ordered_nodes (
 id INTEGER PRIMARY KEY AUTOINCREMENT, claim TEXT NOT NULL UNIQUE,
 stream TEXT NOT NULL, rank BLOB NOT NULL, priority BLOB NOT NULL,
 event TEXT NOT NULL, left_id INTEGER, right_id INTEGER,
 summary TEXT NOT NULL, machine BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS local_agent_ordered_roots (
 stream TEXT PRIMARY KEY, root INTEGER NOT NULL
);
"#;

struct Node {
    id: u64,
    rank: Vec<u8>,
    priority: Vec<u8>,
    event: Event,
    left: Option<u64>,
    right: Option<u64>,
    summary: Summary,
}

fn read(connection: &Connection, id: u64) -> Result<Node> {
    let (rank, priority, event, left, right, summary, machine) = connection.query_row(
        "SELECT rank,priority,event,left_id,right_id,summary,machine FROM local_agent_ordered_nodes WHERE id=?1",
        [id], |r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?,r.get(3)?,r.get(4)?,r.get::<_,String>(5)?,r.get::<_,Vec<u8>>(6)?)),
    )?;
    let mut summary: Summary = serde_json::from_str(&summary)?;
    summary.machine = Machine::decode(&machine)?;
    Ok(Node {
        id,
        rank,
        priority,
        event: serde_json::from_str(&event)?,
        left,
        right,
        summary,
    })
}

fn summary(connection: &Connection, id: Option<u64>) -> Result<Summary> {
    id.map(|id| read(connection, id).map(|node| node.summary))
        .transpose()
        .map(|v| v.unwrap_or_default())
}

fn write(connection: &Connection, node: &mut Node) -> Result<()> {
    node.summary = summary(connection, node.left)?
        .join(&Summary::event(&node.event, node.id)?)
        .join(&summary(connection, node.right)?);
    connection.execute("UPDATE local_agent_ordered_nodes SET left_id=?2,right_id=?3,summary=?4,machine=?5 WHERE id=?1",
        params![node.id,node.left,node.right,serde_json::to_string(&node.summary)?,node.summary.machine.encode()])?;
    Ok(())
}

fn rotate_left(connection: &Connection, mut root: Node) -> Result<u64> {
    let mut next = read(connection, root.right.context("missing right child")?)?;
    root.right = next.left;
    write(connection, &mut root)?;
    next.left = Some(root.id);
    write(connection, &mut next)?;
    Ok(next.id)
}
fn rotate_right(connection: &Connection, mut root: Node) -> Result<u64> {
    let mut next = read(connection, root.left.context("missing left child")?)?;
    root.left = next.right;
    write(connection, &mut root)?;
    next.right = Some(root.id);
    write(connection, &mut next)?;
    Ok(next.id)
}

fn insert(
    connection: &Connection,
    root: Option<u64>,
    id: u64,
    rank: &[u8],
    priority: &[u8],
    depth: usize,
) -> Result<u64> {
    anyhow::ensure!(
        depth < MAX_DEPTH,
        "agent ordered source exceeds depth bound"
    );
    let Some(root) = root else { return Ok(id) };
    let mut node = read(connection, root)?;
    if rank < node.rank.as_slice() {
        node.left = Some(insert(
            connection,
            node.left,
            id,
            rank,
            priority,
            depth + 1,
        )?);
        write(connection, &mut node)?;
        if priority < node.priority.as_slice() {
            rotate_right(connection, node)
        } else {
            Ok(root)
        }
    } else {
        anyhow::ensure!(
            rank != node.rank.as_slice(),
            "duplicate canonical agent position"
        );
        node.right = Some(insert(
            connection,
            node.right,
            id,
            rank,
            priority,
            depth + 1,
        )?);
        write(connection, &mut node)?;
        if priority < node.priority.as_slice() {
            rotate_left(connection, node)
        } else {
            Ok(root)
        }
    }
}

fn merge(
    connection: &Connection,
    left: Option<u64>,
    right: Option<u64>,
    depth: usize,
) -> Result<Option<u64>> {
    anyhow::ensure!(
        depth < MAX_DEPTH,
        "agent ordered source exceeds depth bound"
    );
    match (left, right) {
        (None, r) => Ok(r),
        (l, None) => Ok(l),
        (Some(l), Some(r)) => {
            let mut l = read(connection, l)?;
            let mut r = read(connection, r)?;
            if l.priority < r.priority {
                l.right = merge(connection, l.right, Some(r.id), depth + 1)?;
                write(connection, &mut l)?;
                Ok(Some(l.id))
            } else {
                r.left = merge(connection, Some(l.id), r.left, depth + 1)?;
                write(connection, &mut r)?;
                Ok(Some(r.id))
            }
        }
    }
}
fn remove(
    connection: &Connection,
    root: Option<u64>,
    rank: &[u8],
    depth: usize,
) -> Result<Option<u64>> {
    anyhow::ensure!(
        depth < MAX_DEPTH,
        "agent ordered source exceeds depth bound"
    );
    let Some(root) = root else { return Ok(None) };
    let mut node = read(connection, root)?;
    match rank.cmp(node.rank.as_slice()) {
        std::cmp::Ordering::Equal => merge(connection, node.left, node.right, depth + 1),
        std::cmp::Ordering::Less => {
            node.left = remove(connection, node.left, rank, depth + 1)?;
            write(connection, &mut node)?;
            Ok(Some(root))
        }
        std::cmp::Ordering::Greater => {
            node.right = remove(connection, node.right, rank, depth + 1)?;
            write(connection, &mut node)?;
            Ok(Some(root))
        }
    }
}
fn root(connection: &Connection, stream: &str) -> Result<Option<u64>> {
    Ok(connection
        .query_row(
            "SELECT root FROM local_agent_ordered_roots WHERE stream=?1",
            [stream],
            |r| r.get(0),
        )
        .optional()?)
}
fn put_root(connection: &Connection, stream: &str, id: Option<u64>) -> Result<()> {
    match id {
        Some(id) => {
            connection.execute("INSERT INTO local_agent_ordered_roots VALUES(?1,?2) ON CONFLICT(stream) DO UPDATE SET root=excluded.root",params![stream,id])?;
        }
        None => {
            connection.execute(
                "DELETE FROM local_agent_ordered_roots WHERE stream=?1",
                [stream],
            )?;
        }
    }
    Ok(())
}

pub(super) fn retract(connection: &Connection, claim: &str) -> Result<()> {
    let source: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT stream,rank FROM local_agent_ordered_nodes WHERE claim=?1",
            [claim],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((stream, rank)) = source {
        let next = remove(connection, root(connection, &stream)?, &rank, 0)?;
        put_root(connection, &stream, next)?;
        connection.execute(
            "DELETE FROM local_agent_ordered_nodes WHERE claim=?1",
            [claim],
        )?;
    }
    Ok(())
}

pub(super) fn admit(connection: &Connection, event: &Event) -> Result<()> {
    retract(connection, &event.claim)?;
    let stream = stream(
        &event.subject,
        event.fields.get("incarnation_id").and_then(Value::as_str),
    );
    let priority = Sha256::digest(event.claim.as_bytes()).to_vec();
    connection.execute("INSERT INTO local_agent_ordered_nodes(claim,stream,rank,priority,event,summary,machine) VALUES(?1,?2,?3,?4,?5,'{}',X'')",
        params![event.claim,stream,event.rank,priority,serde_json::to_string(event)?])?;
    let id = connection.last_insert_rowid() as u64;
    let own = Summary::event(event, id)?;
    connection.execute(
        "UPDATE local_agent_ordered_nodes SET summary=?2,machine=?3 WHERE id=?1",
        params![id, serde_json::to_string(&own)?, own.machine.encode()],
    )?;
    let next = insert(
        connection,
        root(connection, &stream)?,
        id,
        &event.rank,
        &priority,
        0,
    )?;
    put_root(connection, &stream, Some(next))
}

pub(super) fn stream(subject: &str, incarnation: Option<&str>) -> String {
    serde_json::to_string(&(subject, incarnation)).expect("stream serializes")
}

pub(super) fn all(
    connection: &Connection,
    subject: &str,
    incarnation: Option<&str>,
) -> Result<Summary> {
    summary(connection, root(connection, &stream(subject, incarnation))?)
}

fn suffix(
    connection: &Connection,
    root: Option<u64>,
    after: &[u8],
    depth: usize,
) -> Result<Summary> {
    anyhow::ensure!(
        depth < MAX_DEPTH,
        "agent ordered source exceeds depth bound"
    );
    let Some(root) = root else {
        return Ok(Summary::default());
    };
    let node = read(connection, root)?;
    if node.rank.as_slice() <= after {
        suffix(connection, node.right, after, depth + 1)
    } else {
        Ok(suffix(connection, node.left, after, depth + 1)?
            .join(&Summary::event(&node.event, node.id)?)
            .join(&summary(connection, node.right)?))
    }
}

pub(super) fn unnamed_after(
    connection: &Connection,
    subject: &str,
    after: &[u8],
) -> Result<Summary> {
    suffix(
        connection,
        root(connection, &stream(subject, None))?,
        after,
        0,
    )
}

pub(super) fn event(connection: &Connection, id: u64) -> Result<Event> {
    Ok(read(connection, id)?.event)
}
