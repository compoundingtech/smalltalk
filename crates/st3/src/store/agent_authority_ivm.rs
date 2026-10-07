//! Incremental actual fields and runtime causal authority for the agent card.
//! The card owner certifies extraction, all source hooks and authorization. This module
//! owns neither a registry nor a Publisher; dirty/incomplete ancestry rejects reads.
//! Parent identities are captured namespace inputs, including foreign subjects and known
//! absence. Repair must not read global source tables after an installation source cut.
use super::*;
use smallclaims::ivm::install::Namespace;

pub(crate) const FINGERPRINT: &str = "agent-authority.v2;namespace.v1;captured-parent-identity.v1;actual-patch.v1;schema-reset.v1;canonical-rank.v1;live-runtime-ancestry.v1;bounds128-256.v1";

// Namespace is an opaque Installer context. Escape its value as a SQL literal; all
// source/user fields continue to use bound parameters. No identifier is interpolated.
fn sql(namespace: &Namespace, statement: &str) -> String {
    statement.replace(
        "@NS@",
        &format!("'{}'", namespace.as_str().replace('\'', "''")),
    )
}

const FIELDS: &[&str] = &[
    "status",
    "reachability",
    "reason",
    "runtime_id",
    "incarnation_id",
    "host",
];
const EDGES: usize = 128;
const ORIGINS: usize = 256;

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_agent_authority_nodes(
 namespace TEXT NOT NULL, id TEXT NOT NULL, agent TEXT NOT NULL, kind TEXT NOT NULL, origin TEXT NOT NULL,
 rank BLOB NOT NULL, body TEXT NOT NULL, actual INTEGER NOT NULL,
 runtime INTEGER NOT NULL, fold INTEGER NOT NULL, complete INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(namespace,id)
);
CREATE INDEX IF NOT EXISTS agent_authority_actual
 ON local_agent_authority_nodes(namespace,agent,actual,runtime,rank DESC,id);
CREATE INDEX IF NOT EXISTS agent_authority_presence
 ON local_agent_authority_nodes(namespace,agent,fold,rank DESC,id);
CREATE INDEX IF NOT EXISTS agent_authority_closure
 ON local_agent_authority_nodes(namespace,complete,agent,id);
CREATE INDEX IF NOT EXISTS agent_authority_origin
 ON local_agent_authority_nodes(namespace,agent,origin,runtime,rank DESC,id);
CREATE TABLE IF NOT EXISTS local_agent_authority_parent_identities(
 namespace TEXT NOT NULL, id TEXT NOT NULL, subject TEXT, PRIMARY KEY(namespace,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_authority_edges(
 namespace TEXT NOT NULL, child TEXT NOT NULL, parent TEXT NOT NULL, PRIMARY KEY(namespace,child,parent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_authority_children
 ON local_agent_authority_edges(namespace,parent,child);
CREATE TABLE IF NOT EXISTS local_agent_authority_ancestors(
 namespace TEXT NOT NULL, id TEXT NOT NULL, origin TEXT NOT NULL, ancestor TEXT NOT NULL, rank BLOB NOT NULL,
 PRIMARY KEY(namespace,id,origin)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_authority_fields(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, field TEXT NOT NULL, id TEXT NOT NULL, rank BLOB NOT NULL,
 value TEXT, PRIMARY KEY(namespace,agent,field,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_authority_field_head
 ON local_agent_authority_fields(namespace,agent,field,rank DESC,id);
CREATE INDEX IF NOT EXISTS agent_authority_field_source
 ON local_agent_authority_fields(namespace,id,agent,field);
CREATE TABLE IF NOT EXISTS local_agent_authority_runtime_heads(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, origin TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,origin)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_authority_dirty(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(namespace,agent,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_authority_fences(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, reason TEXT NOT NULL, PRIMARY KEY(namespace,agent)
);
"#,
    )?;
    Ok(())
}

fn actual(kind: &str) -> bool {
    !("harness.".."harness/").contains(&kind)
        && !matches!(
            kind,
            "intent.desired" | "runtime.readiness-deadline-reached" | "reconcile.fault"
        )
}
fn fold(kind: &str) -> bool {
    actual(kind)
        && !kind
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("harness."))
}
fn fence(tx: &Transaction<'_>, namespace: &Namespace, agent: &str, reason: &str) -> Result<()> {
    tx.execute(
        &sql(
            namespace,
            "INSERT INTO local_agent_authority_fences VALUES(@NS@,?1,?2)
      ON CONFLICT(namespace,agent) DO UPDATE SET reason=excluded.reason",
        ),
        params![agent, reason],
    )?;
    Ok(())
}
fn queue(tx: &Transaction<'_>, namespace: &Namespace, agent: &str, id: &str) -> Result<()> {
    tx.execute(
        &sql(
            namespace,
            "INSERT INTO local_agent_authority_dirty VALUES(@NS@,?1,?2) ON CONFLICT DO NOTHING",
        ),
        params![agent, id],
    )?;
    Ok(())
}
fn children(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agent: &str,
    id: &str,
) -> Result<BTreeSet<String>> {
    let children = tx.prepare_cached(&sql(namespace, "SELECT n.agent,e.child FROM local_agent_authority_edges e
      JOIN local_agent_authority_nodes n ON n.namespace=e.namespace AND n.id=e.child WHERE e.namespace=@NS@ AND e.parent=?1 ORDER BY e.child LIMIT ?2"))?
      .query_map(params![id,EDGES+1], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    if children.len() > EDGES {
        // Unvisited children may belong to other agents. A namespace-wide fence prevents
        // their indexed reads from silently using stale ancestry outside this bounded page.
        fence(
            tx,
            namespace,
            "",
            "runtime authority child fanout exceeds bound",
        )?;
        fence(
            tx,
            namespace,
            agent,
            "runtime authority child fanout exceeds bound",
        )?;
    }
    let mut affected = BTreeSet::new();
    for (agent, child) in children.into_iter().take(EDGES) {
        queue(tx, namespace, &agent, &child)?;
        affected.insert(agent);
    }
    Ok(affected)
}

/// Capture the claim/checkpoint catalog's subject at the namespace's certified source cut.
/// `None` records known absence; a missing catalog row is an uncaptured lookup. Both keep
/// an unresolved parent pending. Different-subject edges can be ignored only after an
/// explicit captured identity. No body/history is read here or during ancestry repair.
/// The source owner must deliver corrections/removals even without an agent-node mutation.
pub(crate) fn apply_parent_identity(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    id: &str,
    subject: Option<&str>,
) -> Result<BTreeSet<String>> {
    let prior: Option<Option<String>>=tx.query_row(&sql(namespace,"SELECT subject FROM local_agent_authority_parent_identities WHERE namespace=@NS@ AND id=?1"),[id],|r|r.get(0)).optional()?;
    if prior.as_ref().is_some_and(|old| old.as_deref() == subject) {
        return Ok(BTreeSet::new());
    }
    tx.execute(
        &sql(
            namespace,
            "INSERT INTO local_agent_authority_parent_identities VALUES(@NS@,?1,?2)
      ON CONFLICT(namespace,id) DO UPDATE SET subject=excluded.subject",
        ),
        params![id, subject],
    )?;
    children(tx, namespace, subject.unwrap_or(""), id)
}

fn refresh_origin(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agent: &str,
    origin: &str,
) -> Result<()> {
    tx.execute(
        &sql(namespace, "DELETE FROM local_agent_authority_runtime_heads WHERE namespace=@NS@ AND agent=?1 AND origin=?2"),
        params![agent, origin],
    )?;
    tx.execute(
        &sql(
            namespace,
            "INSERT INTO local_agent_authority_runtime_heads
      SELECT @NS@,agent,origin,id,body FROM local_agent_authority_nodes
      WHERE namespace=@NS@ AND agent=?1 AND origin=?2 AND runtime=1 ORDER BY rank DESC,id LIMIT 1",
        ),
        params![agent, origin],
    )?;
    let count: usize=tx.query_row(&sql(namespace,"SELECT COUNT(*) FROM
      (SELECT 1 FROM local_agent_authority_runtime_heads WHERE namespace=@NS@ AND agent=?1 LIMIT ?2)"),params![agent,ORIGINS+1],|r|r.get(0))?;
    if count > ORIGINS {
        fence(
            tx,
            namespace,
            agent,
            "runtime authority origin head count exceeds bound",
        )?;
    }
    Ok(())
}

/// Call for every same-agent source claim, including intermediate harness claims, and
/// canonical-rank corrections. Source removal also needs its checkpoint tombstone hook.
pub(crate) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut changed = BTreeSet::new();
    if let Some(old) = old.filter(|c| c.subject.starts_with("agent/")) {
        changed.insert(old.subject.clone());
        remove(tx, namespace, &old.id, &old.subject, &old.origin)?;
    }
    if let Some((claim, key)) = new.filter(|(c, _)| c.subject.starts_with("agent/")) {
        anyhow::ensure!(
            key.5 == claim.id && key.0 == claim.accepted_at_unix_ms && key.3 == claim.batch_id,
            "authority canonical key belongs to another claim"
        );
        changed.extend(apply_parent_identity(
            tx,
            namespace,
            &claim.id,
            Some(&claim.subject),
        )?);
        changed.insert(claim.subject.clone());
        // Replacement callers may supply only new; retire this source's previous metadata.
        let prior: Option<(String, String)> = tx
            .query_row(
                &sql(namespace, "SELECT agent,origin FROM local_agent_authority_nodes WHERE namespace=@NS@ AND id=?1"),
                [&claim.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((agent, origin)) = prior {
            changed.insert(agent.clone());
            remove(tx, namespace, &claim.id, &agent, &origin)?;
        }
        let rank = canonical::sortable_key(key);
        tx.execute(
            &sql(namespace, "INSERT INTO local_agent_authority_nodes VALUES(@NS@,?1,?2,?3,?4,?5,?6,?7,?8,?9,0)"),
            params![
                claim.id,
                claim.subject,
                claim.kind,
                claim.origin,
                rank,
                serde_json::to_string(&json!({"fields":{"status":claim.body.get("fields").unwrap_or(&claim.body).get("status"),"host":claim.body.get("fields").unwrap_or(&claim.body).get("host")}}))?,
                actual(&claim.kind),
                claim.kind == "runtime.observed",
                fold(&claim.kind)
            ],
        )?;
        set_edges(
            tx,
            namespace,
            &claim.subject,
            &claim.id,
            &claim.predecessors,
        )?;
        let source = claim.body.get("fields").unwrap_or(&claim.body);
        // The field fold excludes ASCII case variants of harness.*, while the selected
        // source predicate deliberately retains the canonical reducer's binary ranges.
        if fold(&claim.kind)
            && let Some(fields) = source.as_object()
            && !(claim.kind == "resource.observed"
                && fields
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(carries_opener)
                && fields.get("attribution_only") == Some(&Value::Bool(true)))
        {
            let registry = st3_schema::registry();
            let spec = registry.claim(&claim.kind);
            for field in FIELDS {
                let reset = claim.kind != "resource.observed"
                    && spec.is_some_and(|s| {
                        s.cardinality == st3_schema::Cardinality::StateTransition
                            && s.fields.contains_key(*field)
                    });
                if let Some(value) = fields.get(*field) {
                    tx.execute(
                        &sql(
                            namespace,
                            "INSERT INTO local_agent_authority_fields VALUES(@NS@,?1,?2,?3,?4,?5)",
                        ),
                        params![
                            claim.subject,
                            field,
                            claim.id,
                            rank,
                            serde_json::to_string(value)?
                        ],
                    )?;
                } else if reset {
                    tx.execute(
                        &sql(namespace, "INSERT INTO local_agent_authority_fields VALUES(@NS@,?1,?2,?3,?4,NULL)"),
                        params![claim.subject, field, claim.id, rank],
                    )?;
                }
            }
        }
        refresh_origin(tx, namespace, &claim.subject, &claim.origin)?;
        queue(tx, namespace, &claim.subject, &claim.id)?;
    }
    Ok(changed)
}
fn remove(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    id: &str,
    agent: &str,
    origin: &str,
) -> Result<()> {
    children(tx, namespace, agent, id)?;
    tx.execute(
        &sql(
            namespace,
            "DELETE FROM local_agent_authority_nodes WHERE namespace=@NS@ AND id=?1",
        ),
        [id],
    )?;
    tx.execute(
        &sql(
            namespace,
            "DELETE FROM local_agent_authority_fields WHERE namespace=@NS@ AND id=?1",
        ),
        [id],
    )?;
    tx.execute(
        &sql(
            namespace,
            "DELETE FROM local_agent_authority_ancestors WHERE namespace=@NS@ AND id=?1",
        ),
        [id],
    )?;
    tx.execute(
        &sql(
            namespace,
            "DELETE FROM local_agent_authority_edges WHERE namespace=@NS@ AND child=?1",
        ),
        [id],
    )?;
    tx.execute(
        &sql(
            namespace,
            "DELETE FROM local_agent_authority_dirty WHERE namespace=@NS@ AND agent=?1 AND id=?2",
        ),
        params![agent, id],
    )?;
    refresh_origin(tx, namespace, agent, origin)
}
fn set_edges(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agent: &str,
    id: &str,
    parents: &[String],
) -> Result<()> {
    if parents.len() > EDGES {
        fence(
            tx,
            namespace,
            agent,
            "runtime authority predecessor count exceeds bound",
        )?;
    }
    for parent in parents.iter().take(EDGES) {
        tx.execute(
            &sql(
                namespace,
                "INSERT INTO local_agent_authority_edges VALUES(@NS@,?1,?2) ON CONFLICT DO NOTHING",
            ),
            params![id, parent],
        )?;
    }
    Ok(())
}

/// Project one admitted checkpoint link. Tombstones preserve causal edges, never actual
/// fields or runtime heads. The adapter must also deliver removals/adoption and source ranks.
pub(crate) fn apply_tombstone(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    id: &str,
    agent: &str,
    parents: &[String],
) -> Result<BTreeSet<String>> {
    if !agent.starts_with("agent/") {
        return Ok(BTreeSet::new());
    }
    let mut changed = apply_parent_identity(tx, namespace, id, Some(agent))?;
    changed.insert(agent.to_owned());
    let prior: Option<String> = tx
        .query_row(
            &sql(
                namespace,
                "SELECT origin FROM local_agent_authority_nodes WHERE namespace=@NS@ AND id=?1",
            ),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(origin) = prior {
        remove(tx, namespace, id, agent, &origin)?;
    }
    tx.execute(
        &sql(namespace, "INSERT INTO local_agent_authority_nodes VALUES(@NS@,?1,?2,'checkpoint','',X'','null',0,0,0,0)"),
        params![id, agent],
    )?;
    set_edges(tx, namespace, agent, id, parents)?;
    queue(tx, namespace, agent, id)?;
    Ok(changed)
}

type Ancestors = BTreeMap<String, (Vec<u8>, String)>;
fn ancestors(connection: &Connection, namespace: &Namespace, id: &str) -> Result<Ancestors> {
    let rows=connection.prepare_cached(&sql(namespace, "SELECT origin,rank,ancestor FROM local_agent_authority_ancestors WHERE namespace=@NS@ AND id=?1 ORDER BY origin LIMIT ?2"))?
      .query_map(params![id,ORIGINS+1],|r|Ok((r.get::<_,String>(0)?,(r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?))))?
      .collect::<rusqlite::Result<Ancestors>>()?;
    anyhow::ensure!(
        rows.len() <= ORIGINS,
        "runtime authority origin ancestry exceeds bound"
    );
    Ok(rows)
}
fn close_node(tx: &Transaction<'_>, namespace: &Namespace, agent: &str, id: &str) -> Result<bool> {
    let node: Option<(String,Vec<u8>,bool,bool)>=tx.query_row(
      &sql(namespace, "SELECT origin,rank,runtime,complete FROM local_agent_authority_nodes WHERE namespace=@NS@ AND id=?1 AND agent=?2"),params![id,agent],
      |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((origin, rank, runtime, was_complete)) = node else {
        return Ok(true);
    };
    let parents=tx.prepare_cached(&sql(namespace, "SELECT parent FROM local_agent_authority_edges WHERE namespace=@NS@ AND child=?1 ORDER BY parent LIMIT ?2"))?
      .query_map(params![id,EDGES+1],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut next = Ancestors::new();
    if runtime {
        next.insert(origin, (rank, id.to_owned()));
    }
    for parent in parents {
        let identity: Option<Option<String>> = tx
            .query_row(
                &sql(
                    namespace,
                    "SELECT subject FROM local_agent_authority_parent_identities
          WHERE namespace=@NS@ AND id=?1",
                ),
                [&parent],
                |r| r.get(0),
            )
            .optional()?;
        let Some(Some(owner)) = identity else {
            tx.execute(&sql(namespace,"UPDATE local_agent_authority_nodes SET complete=0 WHERE namespace=@NS@ AND id=?1"),[id])?;
            return Ok(false);
        };
        if owner != agent {
            continue;
        }
        let state: Option<(bool,bool)>=tx.query_row(
          &sql(namespace, "SELECT complete,EXISTS(SELECT 1 FROM local_agent_authority_dirty d WHERE d.namespace=n.namespace AND d.agent=n.agent AND d.id=n.id)
           FROM local_agent_authority_nodes n WHERE n.namespace=@NS@ AND id=?1 AND agent=?2"),params![parent,agent],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((complete, dirty)) = state else {
            tx.execute(&sql(namespace,"UPDATE local_agent_authority_nodes SET complete=0 WHERE namespace=@NS@ AND id=?1"),[id])?;
            return Ok(false);
        };
        if !complete || dirty {
            tx.execute(
                &sql(namespace, "UPDATE local_agent_authority_nodes SET complete=0 WHERE namespace=@NS@ AND id=?1"),
                [id],
            )?;
            return Ok(false);
        }
        for (origin, value) in ancestors(tx, namespace, &parent)? {
            if next.get(&origin).is_none_or(|old| old < &value) {
                next.insert(origin, value);
            }
        }
        if next.len() > ORIGINS {
            fence(
                tx,
                namespace,
                agent,
                "runtime authority origin ancestry exceeds bound",
            )?;
            return Ok(false);
        }
    }
    let previous = ancestors(tx, namespace, id)?;
    let changed = previous != next;
    if changed {
        tx.execute(
            &sql(
                namespace,
                "DELETE FROM local_agent_authority_ancestors WHERE namespace=@NS@ AND id=?1",
            ),
            [id],
        )?;
        for (origin, (rank, ancestor)) in next {
            tx.execute(
                &sql(
                    namespace,
                    "INSERT INTO local_agent_authority_ancestors VALUES(@NS@,?1,?2,?3,?4)",
                ),
                params![id, origin, ancestor, rank],
            )?;
        }
    }
    tx.execute(
        &sql(
            namespace,
            "UPDATE local_agent_authority_nodes SET complete=1 WHERE namespace=@NS@ AND id=?1",
        ),
        [id],
    )?;
    if !was_complete || changed {
        children(tx, namespace, agent, id)?;
    }
    Ok(true)
}

/// Bounded source maintenance; retain continuation after commit and wrap each pass while
/// dirty sources remain. Pending parents and unsupported bounds never become Ready.
pub(crate) fn flush_ancestry(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    after: Option<(&str, &str)>,
    limit: usize,
) -> Result<(Option<(String, String)>, bool)> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "authority maintenance page exceeds bound"
    );
    let (agent, id) = after.unwrap_or(("", ""));
    let page=tx.prepare_cached(&sql(namespace, "SELECT agent,id FROM local_agent_authority_dirty WHERE namespace=@NS@ AND (agent,id)>(?1,?2) ORDER BY agent,id LIMIT ?3"))?
      .query_map(params![agent,id,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let last = page.last().cloned();
    for (agent, id) in page {
        if close_node(tx, namespace, &agent, &id)? {
            tx.execute(
                &sql(namespace, "DELETE FROM local_agent_authority_dirty WHERE namespace=@NS@ AND agent=?1 AND id=?2"),
                params![agent, id],
            )?;
        }
    }
    let clean = !tx.query_row(
        &sql(
            namespace,
            "SELECT EXISTS(SELECT 1 FROM local_agent_authority_dirty WHERE namespace=@NS@)",
        ),
        [],
        |r| r.get::<_, bool>(0),
    )?;
    Ok((last, clean))
}

#[derive(Debug, PartialEq)]
pub(crate) struct Authority {
    pub actual: Option<Value>,
    pub status: Option<String>,
    pub reachability: Option<String>,
    pub reason: Option<String>,
    pub runtime_id: Option<String>,
    pub incarnation_id: Option<String>,
    pub actual_claim: Option<String>,
    pub actual_origin: Option<String>,
    pub runtime_origin_conflict: bool,
    pub actual_presence: bool,
}

/// Causal completion only. This does not certify source extraction, authority, epochs,
/// capture hooks, or the whole public card. The full operator supplies those proofs.
pub(crate) fn ensure_closed(connection: &Connection, namespace: &Namespace) -> Result<()> {
    let incomplete: bool = connection.query_row(
        &sql(
            namespace,
            "SELECT
      EXISTS(SELECT 1 FROM local_agent_authority_dirty WHERE namespace=@NS@)
      OR EXISTS(SELECT 1 FROM local_agent_authority_nodes WHERE namespace=@NS@ AND complete=0)
      OR EXISTS(SELECT 1 FROM local_agent_authority_fences WHERE namespace=@NS@)",
        ),
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        !incomplete,
        "agent authority namespace has pending or fenced sources"
    );
    Ok(())
}

/// Delete a total bounded number of rows from an obsolete namespace. The Installer owns
/// replacement/reclamation authorization; a namespace currently marked Ready is refused.
pub(crate) fn reclaim_namespace(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    limit: usize,
) -> Result<bool> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "authority reclamation page exceeds bound"
    );
    let ready: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1 AND ready=1)",
        [namespace.as_str()],
        |r| r.get(0),
    )?;
    anyhow::ensure!(!ready, "cannot reclaim a ready authority namespace");
    let tables = [
        ("local_agent_authority_nodes", "id"),
        ("local_agent_authority_parent_identities", "id"),
        ("local_agent_authority_edges", "child,parent"),
        ("local_agent_authority_ancestors", "id,origin"),
        ("local_agent_authority_fields", "agent,field,id"),
        ("local_agent_authority_runtime_heads", "agent,origin"),
        ("local_agent_authority_dirty", "agent,id"),
        ("local_agent_authority_fences", "agent"),
    ];
    let mut remaining = limit;
    for (table, keys) in tables {
        if remaining == 0 {
            break;
        }
        // Table/column names are fixed internal constants; only the opaque namespace value
        // is escaped, and the row bound is supplied as a parameter.
        let query = format!(
            "DELETE FROM {table} WHERE namespace=@NS@ AND ({keys}) IN
          (SELECT {keys} FROM {table} WHERE namespace=@NS@ ORDER BY {keys} LIMIT ?1)"
        );
        remaining -= tx.execute(&sql(namespace, &query), [remaining])?;
    }
    for (table, _) in tables {
        let query = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE namespace=@NS@)");
        if tx.query_row(&sql(namespace, &query), [], |r| r.get::<_, bool>(0))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Indexed read in the caller's authorized, fully certified source snapshot. Reachability
/// and reason are raw actual fields; the card owner applies unknown/conflict/default rules.
pub(crate) fn read_authority(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
    desired_host: Option<&str>,
) -> Result<Authority> {
    anyhow::ensure!(
        agent.starts_with("agent/"),
        "authority source is not an agent"
    );
    let failure: Option<String> = connection
        .query_row(
            &sql(
                namespace,
                "SELECT reason FROM local_agent_authority_fences WHERE namespace=@NS@ AND agent IN (?1,'') ORDER BY agent LIMIT 1",
            ),
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    anyhow::ensure!(
        failure.is_none(),
        "agent authority unavailable: {}",
        failure.unwrap_or_default()
    );
    let pending = connection.query_row(
        &sql(namespace, "SELECT EXISTS(SELECT 1 FROM local_agent_authority_dirty WHERE namespace=@NS@ AND agent=?1)"),
        [agent],
        |r| r.get::<_, bool>(0),
    )?;
    anyhow::ensure!(!pending, "agent authority ancestry pending");
    // Both seeks name the maintained actual predicate; runtime selection takes precedence
    // even when a generic actual claim is canonically later.
    let selected = |runtime: bool| -> Result<Option<(String, String, String)>> {
        Ok(connection
            .query_row(
                &sql(namespace, "SELECT id,origin,body FROM local_agent_authority_nodes
          WHERE namespace=@NS@ AND agent=?1 AND actual=1 AND runtime=?2 ORDER BY rank DESC,id LIMIT 1"),
                params![agent, runtime],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    };
    let runtime = selected(true)?;
    let selected = if runtime.is_some() {
        runtime
    } else {
        selected(false)?
    };
    let mut fields = serde_json::Map::new();
    for field in FIELDS {
        let value: Option<Option<String>> = connection
            .query_row(
                &sql(
                    namespace,
                    "SELECT value FROM local_agent_authority_fields
          WHERE namespace=@NS@ AND agent=?1 AND field=?2 ORDER BY rank DESC,id LIMIT 1",
                ),
                params![agent, field],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(value)) = value {
            fields.insert((*field).into(), serde_json::from_str(&value)?);
        }
    }
    let origins=connection.prepare_cached(&sql(namespace, "SELECT origin,id,body FROM local_agent_authority_runtime_heads WHERE namespace=@NS@ AND agent=?1 ORDER BY origin LIMIT ?2"))?
      .query_map(params![agent,ORIGINS+1],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        origins.len() <= ORIGINS,
        "runtime authority origin head count exceeds bound"
    );
    let mut conflict = false;
    if let Some((id, origin, body)) = &selected {
        let body: Value = serde_json::from_str(body)?;
        let ancestry = ancestors(connection, namespace, id)?;
        for (other, head, body_other) in origins {
            if other == *origin || head == *id {
                continue;
            }
            if nonowner_terminal_observation(
                desired_host,
                origin,
                &body,
                &other,
                &serde_json::from_str(&body_other)?,
            ) {
                continue;
            }
            if !ancestry
                .get(&other)
                .is_some_and(|(_, ancestor)| ancestor == &head)
            {
                conflict = true;
                break;
            }
        }
    }
    let string = |field: &str| fields.get(field).and_then(Value::as_str).map(str::to_owned);
    let present = connection.query_row(
        &sql(namespace, "SELECT EXISTS(SELECT 1 FROM local_agent_authority_nodes WHERE namespace=@NS@ AND agent=?1 AND fold=1)"),
        [agent],
        |r| r.get::<_, bool>(0),
    )?;
    Ok(Authority {
        status: string("status"),
        reachability: string("reachability"),
        reason: string("reason"),
        runtime_id: string("runtime_id"),
        incarnation_id: string("incarnation_id"),
        actual_claim: selected.as_ref().map(|v| v.0.clone()),
        actual_origin: selected.map(|v| v.1),
        runtime_origin_conflict: conflict,
        actual_presence: present,
        actual: present.then_some(Value::Object(fields)),
    })
}

#[cfg(test)]
mod tests;
