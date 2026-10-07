//! Canonically ordered reductions. Each edit touches only the search path; persisted
//! subtree summaries let reads reduce an epoch without visiting its observations.
use super::*;

const MAX_DEPTH: usize = 128;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_harness_nodes (
 namespace TEXT NOT NULL, id INTEGER NOT NULL, claim TEXT NOT NULL,
 subject TEXT NOT NULL, kind TEXT NOT NULL,
 stream TEXT NOT NULL, rank BLOB NOT NULL, priority BLOB NOT NULL,
 event TEXT NOT NULL, left_id INTEGER, right_id INTEGER,
 summary TEXT NOT NULL, machine BLOB NOT NULL,
 PRIMARY KEY(namespace,id), UNIQUE(namespace,claim)
);
CREATE INDEX IF NOT EXISTS local_agent_card_harness_runtime
 ON local_agent_card_harness_nodes(namespace,subject,kind,rank DESC);
CREATE TABLE IF NOT EXISTS local_agent_card_harness_roots (
 namespace TEXT NOT NULL, stream TEXT NOT NULL, root INTEGER NOT NULL,
 PRIMARY KEY(namespace,stream)
);
CREATE TABLE IF NOT EXISTS local_agent_card_harness_sequence (
 namespace TEXT PRIMARY KEY, next_id INTEGER NOT NULL
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

fn read(connection: &Connection, namespace: &str, id: u64) -> Result<Node> {
    let (rank, priority, event, left, right, summary, machine) = connection.query_row(
        "SELECT rank,priority,event,left_id,right_id,summary,machine FROM local_agent_card_harness_nodes WHERE id=?1 AND namespace=?2",
        params![id,namespace], |r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?,r.get(3)?,r.get(4)?,r.get::<_,String>(5)?,r.get::<_,Vec<u8>>(6)?)),
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

fn summary(connection: &Connection, namespace: &str, id: Option<u64>) -> Result<Summary> {
    id.map(|id| read(connection, namespace, id).map(|node| node.summary))
        .transpose()
        .map(|v| v.unwrap_or_default())
}

fn write(connection: &Connection, namespace: &str, node: &mut Node) -> Result<()> {
    node.summary = summary(connection, namespace, node.left)?
        .join(&Summary::event(&node.event, node.id)?)
        .join(&summary(connection, namespace, node.right)?);
    connection.execute("UPDATE local_agent_card_harness_nodes SET left_id=?2,right_id=?3,summary=?4,machine=?5 WHERE id=?1 AND namespace=?6",
        params![node.id,node.left,node.right,serde_json::to_string(&node.summary)?,node.summary.machine.encode(),namespace])?;
    Ok(())
}

fn rotate_left(connection: &Connection, namespace: &str, mut root: Node) -> Result<u64> {
    let mut next = read(
        connection,
        namespace,
        root.right.context("missing right child")?,
    )?;
    root.right = next.left;
    write(connection, namespace, &mut root)?;
    next.left = Some(root.id);
    write(connection, namespace, &mut next)?;
    Ok(next.id)
}
fn rotate_right(connection: &Connection, namespace: &str, mut root: Node) -> Result<u64> {
    let mut next = read(
        connection,
        namespace,
        root.left.context("missing left child")?,
    )?;
    root.left = next.right;
    write(connection, namespace, &mut root)?;
    next.right = Some(root.id);
    write(connection, namespace, &mut next)?;
    Ok(next.id)
}

fn insert(
    connection: &Connection,
    namespace: &str,
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
    let mut node = read(connection, namespace, root)?;
    if rank < node.rank.as_slice() {
        node.left = Some(insert(
            connection,
            namespace,
            node.left,
            id,
            rank,
            priority,
            depth + 1,
        )?);
        write(connection, namespace, &mut node)?;
        if priority < node.priority.as_slice() {
            rotate_right(connection, namespace, node)
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
            namespace,
            node.right,
            id,
            rank,
            priority,
            depth + 1,
        )?);
        write(connection, namespace, &mut node)?;
        if priority < node.priority.as_slice() {
            rotate_left(connection, namespace, node)
        } else {
            Ok(root)
        }
    }
}

fn merge(
    connection: &Connection,
    namespace: &str,
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
            let mut l = read(connection, namespace, l)?;
            let mut r = read(connection, namespace, r)?;
            if l.priority < r.priority {
                l.right = merge(connection, namespace, l.right, Some(r.id), depth + 1)?;
                write(connection, namespace, &mut l)?;
                Ok(Some(l.id))
            } else {
                r.left = merge(connection, namespace, Some(l.id), r.left, depth + 1)?;
                write(connection, namespace, &mut r)?;
                Ok(Some(r.id))
            }
        }
    }
}
fn remove(
    connection: &Connection,
    namespace: &str,
    root: Option<u64>,
    rank: &[u8],
    depth: usize,
) -> Result<Option<u64>> {
    anyhow::ensure!(
        depth < MAX_DEPTH,
        "agent ordered source exceeds depth bound"
    );
    let Some(root) = root else { return Ok(None) };
    let mut node = read(connection, namespace, root)?;
    match rank.cmp(node.rank.as_slice()) {
        std::cmp::Ordering::Equal => merge(connection, namespace, node.left, node.right, depth + 1),
        std::cmp::Ordering::Less => {
            node.left = remove(connection, namespace, node.left, rank, depth + 1)?;
            write(connection, namespace, &mut node)?;
            Ok(Some(root))
        }
        std::cmp::Ordering::Greater => {
            node.right = remove(connection, namespace, node.right, rank, depth + 1)?;
            write(connection, namespace, &mut node)?;
            Ok(Some(root))
        }
    }
}
fn root(connection: &Connection, namespace: &str, stream: &str) -> Result<Option<u64>> {
    Ok(connection
        .query_row(
            "SELECT root FROM local_agent_card_harness_roots WHERE stream=?1 AND namespace=?2",
            params![stream, namespace],
            |r| r.get(0),
        )
        .optional()?)
}
fn put_root(connection: &Connection, namespace: &str, stream: &str, id: Option<u64>) -> Result<()> {
    match id {
        Some(id) => {
            connection.execute("INSERT INTO local_agent_card_harness_roots VALUES(?1,?2,?3) ON CONFLICT(namespace,stream) DO UPDATE SET root=excluded.root",params![namespace,stream,id])?;
        }
        None => {
            connection.execute(
                "DELETE FROM local_agent_card_harness_roots WHERE stream=?1 AND namespace=?2",
                params![stream, namespace],
            )?;
        }
    }
    Ok(())
}

pub(super) fn retract(connection: &Connection, namespace: &str, claim: &str) -> Result<()> {
    let source: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT stream,rank FROM local_agent_card_harness_nodes WHERE claim=?1 AND namespace=?2",
            params![claim,namespace],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((stream, rank)) = source {
        let next = remove(
            connection,
            namespace,
            root(connection, namespace, &stream)?,
            &rank,
            0,
        )?;
        put_root(connection, namespace, &stream, next)?;
        connection.execute(
            "DELETE FROM local_agent_card_harness_nodes WHERE claim=?1 AND namespace=?2",
            params![claim, namespace],
        )?;
    }
    Ok(())
}

pub(super) fn admit(connection: &Connection, namespace: &str, event: &Event) -> Result<()> {
    retract(connection, namespace, &event.claim)?;
    let stream = stream(
        &event.subject,
        event.fields.get("incarnation_id").and_then(Value::as_str),
    );
    let priority = Sha256::digest(event.claim.as_bytes()).to_vec();
    connection.execute("INSERT INTO local_agent_card_harness_sequence VALUES(?1,1) ON CONFLICT(namespace) DO UPDATE SET next_id=next_id+1",[namespace])?;
    let id: u64 = connection.query_row(
        "SELECT next_id FROM local_agent_card_harness_sequence WHERE namespace=?1",
        [namespace],
        |r| r.get(0),
    )?;
    connection.execute("INSERT INTO local_agent_card_harness_nodes(namespace,id,claim,subject,kind,stream,rank,priority,event,summary,machine) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'{}',X'')",params![namespace,id,event.claim,event.subject,event.kind,stream,event.rank,priority,serde_json::to_string(event)?])?;
    let own = Summary::event(event, id)?;
    connection.execute(
        "UPDATE local_agent_card_harness_nodes SET summary=?2,machine=?3 WHERE id=?1 AND namespace=?4",
        params![id, serde_json::to_string(&own)?, own.machine.encode(),namespace],
    )?;
    let next = insert(
        connection,
        namespace,
        root(connection, namespace, &stream)?,
        id,
        &event.rank,
        &priority,
        0,
    )?;
    put_root(connection, namespace, &stream, Some(next))
}

pub(super) fn stream(subject: &str, incarnation: Option<&str>) -> String {
    serde_json::to_string(&(subject, incarnation)).expect("stream serializes")
}

pub(super) fn all(
    connection: &Connection,
    namespace: &str,
    subject: &str,
    incarnation: Option<&str>,
) -> Result<Summary> {
    summary(
        connection,
        namespace,
        root(connection, namespace, &stream(subject, incarnation))?,
    )
}

fn suffix(
    connection: &Connection,
    namespace: &str,
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
    let node = read(connection, namespace, root)?;
    if node.rank.as_slice() <= after {
        suffix(connection, namespace, node.right, after, depth + 1)
    } else {
        Ok(suffix(connection, namespace, node.left, after, depth + 1)?
            .join(&Summary::event(&node.event, node.id)?)
            .join(&summary(connection, namespace, node.right)?))
    }
}

pub(super) fn unnamed_after(
    connection: &Connection,
    namespace: &str,
    subject: &str,
    after: &[u8],
) -> Result<Summary> {
    suffix(
        connection,
        namespace,
        root(connection, namespace, &stream(subject, None))?,
        after,
        0,
    )
}

pub(super) fn event(connection: &Connection, namespace: &str, id: u64) -> Result<Event> {
    Ok(read(connection, namespace, id)?.event)
}
