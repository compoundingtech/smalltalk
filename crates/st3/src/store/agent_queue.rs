//! Namespace-owned dependency relation for the existing agent_work_queues public fields.
//! Complete replacement capture and source publication belong to the shared installer owner.
#![allow(dead_code)]
use super::seat_queue_order as order;
use super::*;
use smallclaims::ivm::install::Namespace;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_queue_steps (
 namespace TEXT NOT NULL, subject TEXT NOT NULL, run TEXT NOT NULL, generation TEXT NOT NULL,
 path TEXT NOT NULL, status TEXT NOT NULL, assignee TEXT, claimant TEXT, available TEXT NOT NULL,
 created TEXT NOT NULL, created_num BLOB NOT NULL, join_at INTEGER NOT NULL, agentless INTEGER NOT NULL,
 expiry TEXT, deadline BLOB, body TEXT NOT NULL, PRIMARY KEY(namespace,subject)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_agent_queue_run_generation ON local_agent_queue_steps(namespace,run,generation,subject);
CREATE INDEX IF NOT EXISTS local_agent_queue_join_min ON local_agent_queue_steps(namespace,assignee,run,join_at,subject) WHERE agentless=0 AND assignee IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_queue_deadline ON local_agent_queue_steps(namespace,deadline,subject) WHERE deadline IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_agent_queue_generations (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, generation TEXT NOT NULL,
 count INTEGER NOT NULL CHECK(count>0), PRIMARY KEY(namespace,agent,run,generation)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_runs (
 namespace TEXT NOT NULL, run TEXT NOT NULL, generation TEXT NOT NULL, status TEXT NOT NULL, phase TEXT NOT NULL,
 body TEXT NOT NULL, PRIMARY KEY(namespace,run)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_run_work (
 namespace TEXT NOT NULL, run TEXT NOT NULL, generation TEXT NOT NULL, cursor TEXT,
 PRIMARY KEY(namespace,run,generation)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_join_work (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, PRIMARY KEY(namespace,agent,run)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_step_work (
 namespace TEXT NOT NULL, subject TEXT NOT NULL, PRIMARY KEY(namespace,subject)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_claims (
 namespace TEXT NOT NULL, id TEXT NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL,
 rank BLOB NOT NULL, body TEXT NOT NULL, PRIMARY KEY(namespace,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_agent_queue_claim_rank ON local_agent_queue_claims(namespace,subject,kind,rank,id);
CREATE TABLE IF NOT EXISTS local_agent_queue_associations (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, subject TEXT NOT NULL, run TEXT NOT NULL, path TEXT NOT NULL,
 status TEXT NOT NULL, selector TEXT NOT NULL, created BLOB NOT NULL, created_text TEXT NOT NULL,
 carried INTEGER NOT NULL, held INTEGER NOT NULL, ready INTEGER NOT NULL, category INTEGER NOT NULL, position BLOB NOT NULL,
 PRIMARY KEY(namespace,agent,subject)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_agent_queue_by_subject ON local_agent_queue_associations(namespace,subject,agent);
CREATE INDEX IF NOT EXISTS local_agent_queue_by_run ON local_agent_queue_associations(namespace,agent,run,subject);
CREATE INDEX IF NOT EXISTS local_agent_queue_context ON local_agent_queue_associations(namespace,agent,run,selector,path,subject);
CREATE INDEX IF NOT EXISTS local_agent_queue_ready_context ON local_agent_queue_associations(namespace,agent,run,selector,path,subject) WHERE status='ready';
CREATE INDEX IF NOT EXISTS local_agent_queue_held ON local_agent_queue_associations(namespace,agent,length(created_text),created_text,subject) WHERE held=1;
CREATE INDEX IF NOT EXISTS local_agent_queue_ready ON local_agent_queue_associations(namespace,agent,carried DESC,category,position,created,subject) WHERE ready=1;
CREATE TABLE IF NOT EXISTS local_agent_queue_flag_work (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, subject TEXT NOT NULL, PRIMARY KEY(namespace,agent,subject)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_scope_work (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, selector TEXT NOT NULL,
 prefix TEXT NOT NULL, cursor_path TEXT, cursor_subject TEXT, PRIMARY KEY(namespace,agent,run,selector,prefix)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_rank_work (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, run TEXT NOT NULL, cursor TEXT,
 PRIMARY KEY(namespace,agent,run)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_counts (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, active INTEGER NOT NULL CHECK(active>=0), queued INTEGER NOT NULL CHECK(queued>=0),
 PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_dirty (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_queue_clock (
 namespace TEXT PRIMARY KEY, through BLOB NOT NULL CHECK(length(through)=16)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS local_agent_queue_reclaiming (namespace TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS agent_queue_step_insert AFTER INSERT ON local_agent_queue_steps
 WHEN NEW.agentless=0 AND NEW.assignee IS NOT NULL AND NOT EXISTS(SELECT 1 FROM local_agent_queue_reclaiming WHERE namespace=NEW.namespace) BEGIN
 INSERT INTO local_agent_queue_generations VALUES(NEW.namespace,NEW.assignee,NEW.run,NEW.generation,1)
 ON CONFLICT(namespace,agent,run,generation) DO UPDATE SET count=count+1;
 INSERT INTO local_agent_queue_join_work VALUES(NEW.namespace,NEW.assignee,NEW.run) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS agent_queue_step_delete AFTER DELETE ON local_agent_queue_steps
 WHEN OLD.agentless=0 AND OLD.assignee IS NOT NULL AND NOT EXISTS(SELECT 1 FROM local_agent_queue_reclaiming WHERE namespace=OLD.namespace) BEGIN
 DELETE FROM local_agent_queue_generations WHERE namespace=OLD.namespace AND agent=OLD.assignee AND run=OLD.run AND generation=OLD.generation AND count=1;
 UPDATE local_agent_queue_generations SET count=count-1 WHERE namespace=OLD.namespace AND agent=OLD.assignee AND run=OLD.run AND generation=OLD.generation;
 INSERT INTO local_agent_queue_join_work VALUES(OLD.namespace,OLD.assignee,OLD.run) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS agent_queue_step_update AFTER UPDATE OF assignee,run,generation,join_at,agentless ON local_agent_queue_steps
 WHEN (OLD.assignee IS NOT NEW.assignee OR OLD.run<>NEW.run OR OLD.generation<>NEW.generation OR OLD.join_at<>NEW.join_at OR OLD.agentless<>NEW.agentless) AND NOT EXISTS(SELECT 1 FROM local_agent_queue_reclaiming WHERE namespace=NEW.namespace) BEGIN
 DELETE FROM local_agent_queue_generations WHERE namespace=OLD.namespace AND agent=OLD.assignee AND run=OLD.run AND generation=OLD.generation AND count=1 AND OLD.agentless=0;
 UPDATE local_agent_queue_generations SET count=count-1 WHERE namespace=OLD.namespace AND agent=OLD.assignee AND run=OLD.run AND generation=OLD.generation AND OLD.agentless=0;
 INSERT INTO local_agent_queue_generations SELECT NEW.namespace,NEW.assignee,NEW.run,NEW.generation,1 WHERE NEW.agentless=0 AND NEW.assignee IS NOT NULL
 ON CONFLICT(namespace,agent,run,generation) DO UPDATE SET count=count+1;
 INSERT INTO local_agent_queue_join_work SELECT OLD.namespace,OLD.assignee,OLD.run WHERE OLD.agentless=0 AND OLD.assignee IS NOT NULL ON CONFLICT DO NOTHING;
 INSERT INTO local_agent_queue_join_work SELECT NEW.namespace,NEW.assignee,NEW.run WHERE NEW.agentless=0 AND NEW.assignee IS NOT NULL ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS agent_queue_association_insert AFTER INSERT ON local_agent_queue_associations BEGIN
 INSERT INTO local_agent_queue_counts VALUES(NEW.namespace,NEW.agent,NEW.held,NEW.ready)
 ON CONFLICT(namespace,agent) DO UPDATE SET active=active+NEW.held,queued=queued+NEW.ready;
 INSERT INTO local_agent_queue_dirty VALUES(NEW.namespace,NEW.agent) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS agent_queue_association_delete AFTER DELETE ON local_agent_queue_associations WHEN NOT EXISTS(SELECT 1 FROM local_agent_queue_reclaiming WHERE namespace=OLD.namespace) BEGIN
 UPDATE local_agent_queue_counts SET active=active-OLD.held,queued=queued-OLD.ready WHERE namespace=OLD.namespace AND agent=OLD.agent;
 INSERT INTO local_agent_queue_dirty VALUES(OLD.namespace,OLD.agent) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS agent_queue_association_flags AFTER UPDATE OF held,ready,carried,category,position,created,created_text ON local_agent_queue_associations
 WHEN OLD.held<>NEW.held OR OLD.ready<>NEW.ready OR OLD.carried<>NEW.carried OR OLD.category<>NEW.category OR OLD.position<>NEW.position OR OLD.created<>NEW.created OR OLD.created_text<>NEW.created_text BEGIN
 UPDATE local_agent_queue_counts SET active=active+NEW.held-OLD.held,queued=queued+NEW.ready-OLD.ready WHERE namespace=NEW.namespace AND agent=NEW.agent AND (OLD.held<>NEW.held OR OLD.ready<>NEW.ready);
 INSERT INTO local_agent_queue_dirty VALUES(NEW.namespace,NEW.agent) ON CONFLICT DO NOTHING;
END;
"#;
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Coverage {
    Complete,
    Unsupported(String),
}
#[derive(Debug)]
pub(crate) struct Drain {
    pub processed: usize,
    pub clean: bool,
    pub coverage: Coverage,
}

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    anyhow::ensure!(
        AGENT_WORK_PREVIEW_LIMIT == 5,
        "agent queue prefix schema requires the qualified preview bound"
    );
    order::create_schema(connection)?;
    connection.execute_batch(SCHEMA)?;
    Ok(())
}
fn clock(tx: &Transaction<'_>, namespace: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO local_agent_queue_clock VALUES(?1,zeroblob(16)) ON CONFLICT DO NOTHING",
        [namespace],
    )?;
    Ok(())
}
fn step_work(tx: &Transaction<'_>, namespace: &str, subject: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO local_agent_queue_step_work VALUES(?1,?2) ON CONFLICT DO NOTHING",
        params![namespace, subject],
    )?;
    Ok(())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("queue source text {key}"))
}
fn optional_text(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Null) => Ok(None),
        _ => anyhow::bail!("queue source optional text {key}"),
    }
}

/// Projected complete source replacements, not invalidations. The caller supplies the
/// installer namespace and attests exact old/new PK identity and captured SQL row types.
pub(crate) fn replace_step(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    subject: &str,
    new: Option<&Value>,
) -> Result<Coverage> {
    let namespace = namespace.as_str();
    clock(tx, namespace)?;
    let old: Option<String> = tx
        .query_row(
            "SELECT body FROM local_agent_queue_steps WHERE namespace=?1 AND subject=?2",
            params![namespace, subject],
            |row| row.get(0),
        )
        .optional()?;
    let body = new.map(serde_json::to_string).transpose()?;
    if old == body {
        return Ok(Coverage::Complete);
    }
    if let Some(new) = new {
        let decoded = (|| -> Result<_> {
            anyhow::ensure!(
                text(new, "subject")? == subject,
                "queue step PK reassignment must retract then insert"
            );
            // Reject values the existing label SQL accessor cannot decode at the writer
            // seam, so the source owner can fence without rejecting source admission.
            optional_text(new, "title")?;
            let _: Vec<String> = serde_json::from_str(text(new, "goals")?)?;
            let _: u128 = text(new, "updated_at_unix_ms")?.parse()?;
            let path = text(new, "step_path")?;
            anyhow::ensure!(
                path.len() <= 1024,
                "queue ancestor path exceeds bounded supported domain"
            );
            Ok((
                path,
                text(new, "created_at_unix_ms")?,
                new.get("agentless")
                    .and_then(Value::as_i64)
                    .context("queue source agentless integer")?,
                optional_text(new, "assignee")?,
                optional_text(new, "lease_owner")?,
                optional_text(new, "lease_expires_at_unix_ms")?,
                text(new, "run_id")?,
                text(new, "generation_id")?,
                text(new, "status")?,
                text(new, "available_to")?,
            ))
        })();
        let (
            path,
            created,
            agentless,
            assignee,
            claimant,
            expiry,
            run,
            generation,
            status,
            available,
        ) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => return Ok(Coverage::Unsupported(error.to_string())),
        };
        let numeric = created.parse::<u128>().unwrap_or_default().to_be_bytes();
        let join_at: i64 = tx.query_row("SELECT MAX(CAST(?1 AS INTEGER),0)", [created], |row| {
            row.get(0)
        })?;
        tx.execute("INSERT INTO local_agent_queue_steps VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,NULL,?15)
   ON CONFLICT(namespace,subject) DO UPDATE SET run=excluded.run,generation=excluded.generation,path=excluded.path,status=excluded.status,assignee=excluded.assignee,claimant=excluded.claimant,available=excluded.available,created=excluded.created,created_num=excluded.created_num,join_at=excluded.join_at,agentless=excluded.agentless,expiry=excluded.expiry,deadline=NULL,body=excluded.body",
   params![namespace,subject,run,generation,path,status,assignee,claimant,available,created,numeric.as_slice(),join_at,agentless,expiry,body])?;
    } else {
        tx.execute(
            "DELETE FROM local_agent_queue_steps WHERE namespace=?1 AND subject=?2",
            params![namespace, subject],
        )?;
    }
    step_work(tx, namespace, subject)?;
    Ok(Coverage::Complete)
}

pub(crate) fn replace_run(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    run: &str,
    new: Option<&Value>,
) -> Result<Coverage> {
    let namespace = namespace.as_str();
    clock(tx, namespace)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT generation,body FROM local_agent_queue_runs WHERE namespace=?1 AND run=?2",
            params![namespace, run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let body = new.map(serde_json::to_string).transpose()?;
    if old.as_ref().map(|(_, body)| body) == body.as_ref() {
        return Ok(Coverage::Complete);
    }
    let mut generations = BTreeSet::new();
    if let Some((generation, _)) = old {
        generations.insert(generation);
    }
    if let Some(new) = new {
        let decoded = (|| -> Result<_> {
            anyhow::ensure!(
                text(new, "id")? == run,
                "queue run PK reassignment must retract then insert"
            );
            text(new, "mission_id")?;
            Ok((
                text(new, "current_generation_id")?,
                text(new, "status")?,
                text(new, "phase")?,
            ))
        })();
        let (generation, status, phase) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => return Ok(Coverage::Unsupported(error.to_string())),
        };
        generations.insert(generation.to_owned());
        tx.execute("INSERT INTO local_agent_queue_runs VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,run) DO UPDATE SET generation=excluded.generation,status=excluded.status,phase=excluded.phase,body=excluded.body",params![namespace,run,generation,status,phase,body])?;
    } else {
        tx.execute(
            "DELETE FROM local_agent_queue_runs WHERE namespace=?1 AND run=?2",
            params![namespace, run],
        )?;
    }
    for generation in generations {
        tx.execute("INSERT INTO local_agent_queue_run_work VALUES(?1,?2,?3,NULL) ON CONFLICT(namespace,run,generation) DO UPDATE SET cursor=NULL",params![namespace,run,generation])?;
    }
    Ok(Coverage::Complete)
}

/// The source owner normalizes admitted canonical ranks and exact repair policy: carried
/// and claimed retain originals; moves exclude repaired originals. False eligibility is
/// an explicit retraction, not a rejected input treated as a default row.
pub(crate) fn replace_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    id: &str,
    new: Option<&Value>,
) -> Result<Coverage> {
    let namespace = namespace.as_str();
    clock(tx, namespace)?;
    let old:Option<(String,String,Vec<u8>,String)>=tx.query_row("SELECT subject,kind,rank,body FROM local_agent_queue_claims WHERE namespace=?1 AND id=?2",params![namespace,id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let decoded = new
        .map(|new| -> Result<_> {
            anyhow::ensure!(text(new, "id")? == id, "queue canonical claim identity");
            let rank: Vec<u8> =
                serde_json::from_value(new.get("rank").context("queue canonical rank")?.clone())?;
            anyhow::ensure!(rank.len() > 16, "queue canonical rank width");
            let eligible = new
                .get("eligible")
                .and_then(Value::as_bool)
                .context("queue canonical repair eligibility")?;
            let body = new.get("body").context("queue canonical body")?.clone();
            Ok((
                text(new, "subject")?.to_owned(),
                text(new, "kind")?.to_owned(),
                rank,
                serde_json::to_string(&body)?,
                body,
                eligible,
            ))
        })
        .transpose();
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(error) => return Ok(Coverage::Unsupported(error.to_string())),
    };
    let new = decoded.as_ref().filter(|row| {
        row.5
            && (row.1 != "step-run.carried" || {
                let fields = row.4.get("fields");
                fields.and_then(|f| f.get("status")).and_then(Value::as_str) == Some("ready")
                    && fields
                        .and_then(|f| f.get("claimant"))
                        .is_some_and(|v| !v.is_null())
            })
    });
    if old.as_ref().map(|row| (&row.0, &row.1, &row.2, &row.3))
        == new.map(|row| (&row.0, &row.1, &row.2, &row.3))
    {
        return Ok(Coverage::Complete);
    }
    if let Some((subject, kind, _, _)) = &old {
        if kind == seat_queue::MOVED_CLAIM && subject.starts_with("agent/") {
            order::set_move_rank(tx, namespace, subject, id, None)?;
        } else {
            step_work(tx, namespace, subject)?;
        }
        tx.execute(
            "DELETE FROM local_agent_queue_claims WHERE namespace=?1 AND id=?2",
            params![namespace, id],
        )?;
    }
    if let Some((subject, kind, rank, body, parsed, _)) = new {
        if !matches!(
            kind.as_str(),
            "step-run.carried" | "work.claimed" | seat_queue::MOVED_CLAIM
        ) {
            return Ok(Coverage::Unsupported(
                "queue canonical kind outside exact registry".into(),
            ));
        }
        tx.execute(
            "INSERT INTO local_agent_queue_claims VALUES(?1,?2,?3,?4,?5,?6)",
            params![namespace, id, subject, kind, rank, body],
        )?;
        if kind == seat_queue::MOVED_CLAIM {
            if !subject.starts_with("agent/") {
                return Ok(Coverage::Complete);
            }
            let fields = parsed.get("fields").unwrap_or(parsed);
            if let (Some(run), Some(placement)) = (
                fields.get("run").and_then(Value::as_str),
                fields
                    .get("placement")
                    .and_then(Value::as_str)
                    .and_then(Placement::parse),
            ) {
                let at = u128::from_be_bytes(rank[..16].try_into().unwrap());
                let movement = QueueMove {
                    run: run.into(),
                    placement,
                    anchor: fields
                        .get("anchor")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    at_unix_ms: at,
                };
                order::set_move_rank(tx, namespace, subject, id, Some((rank, &movement)))?;
            }
        } else {
            step_work(tx, namespace, subject)?;
        }
    }
    Ok(Coverage::Complete)
}

fn carried(
    connection: &Connection,
    namespace: &str,
    subject: &str,
) -> Result<Result<Option<String>, String>> {
    let claimed:Option<Vec<u8>>=connection.query_row("SELECT rank FROM local_agent_queue_claims WHERE namespace=?1 AND subject=?2 AND kind='work.claimed' ORDER BY rank DESC,id DESC LIMIT 1",params![namespace,subject],|row|row.get(0)).optional()?;
    let from = claimed.as_deref().unwrap_or(&[]);
    // Eligibility follows the exact canonical after relation; this query is a bounded seek.
    // There is one native carry per new-generation subject. Legacy ambiguous multiple eligible
    // carries require explicit Unsupported rather than choosing SQLite's unspecified row order.
    let candidates=connection.prepare_cached("SELECT body FROM local_agent_queue_claims WHERE namespace=?1 AND subject=?2 AND kind='step-run.carried' AND rank>?3 ORDER BY rank,id LIMIT 2")?.query_map(params![namespace,subject,from],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut eligible = Vec::new();
    for body in candidates {
        let body: Value = serde_json::from_str(&body)?;
        let fields = body.get("fields"); // Existing carried reader deliberately has no legacy-body fallback.
        if fields.and_then(|f| f.get("status")).and_then(Value::as_str) == Some("ready")
            && let Some(claimant) = fields
                .and_then(|f| f.get("claimant"))
                .filter(|value| !value.is_null())
        {
            let Some(claimant) = claimant.as_str() else {
                return Ok(Err("non-text carried claimant".into()));
            };
            eligible.push(claimant.to_owned());
        }
    }
    if eligible.len() > 1 {
        return Ok(Err(
            "ambiguous carried history exceeds qualified bounded domain".into(),
        ));
    }
    Ok(Ok(eligible.pop()))
}

#[derive(Clone, Debug)]
struct Association {
    agent: String,
    subject: String,
    run: String,
    path: String,
    status: String,
    selector: String,
    created: Vec<u8>,
    created_text: String,
    carried: bool,
}
const ASSOCIATIONS: &str = "SELECT agent,subject,run,path,status,selector,created,created_text,carried FROM local_agent_queue_associations WHERE namespace=?1 AND subject=?2 ORDER BY agent";
fn association_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Association> {
    Ok(Association {
        agent: row.get(0)?,
        subject: row.get(1)?,
        run: row.get(2)?,
        path: row.get(3)?,
        status: row.get(4)?,
        selector: row.get(5)?,
        created: row.get(6)?,
        created_text: row.get(7)?,
        carried: row.get(8)?,
    })
}
fn ancestors(path: &str) -> Vec<String> {
    path.match_indices('/')
        .map(|(at, _)| path[..at].to_owned())
        .collect()
}
fn mark_context(tx: &Transaction<'_>, namespace: &str, row: &Association) -> Result<()> {
    tx.execute(
        "INSERT INTO local_agent_queue_dirty VALUES(?1,?2) ON CONFLICT DO NOTHING",
        params![namespace, row.agent],
    )?;
    tx.execute(
        "INSERT INTO local_agent_queue_flag_work VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
        params![namespace, row.agent, row.subject],
    )?;
    let parents = ancestors(&row.path);
    tx.execute("INSERT INTO local_agent_queue_flag_work SELECT namespace,agent,subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND path IN (SELECT value FROM json_each(?5)) ON CONFLICT DO NOTHING",params![namespace,row.agent,row.run,row.selector,serde_json::to_string(&parents)?])?;
    tx.execute("INSERT INTO local_agent_queue_scope_work VALUES(?1,?2,?3,?4,?5,NULL,NULL) ON CONFLICT(namespace,agent,run,selector,prefix) DO UPDATE SET cursor_path=NULL,cursor_subject=NULL",params![namespace,row.agent,row.run,row.selector,row.path])?;
    Ok(())
}
type StepCandidate = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Vec<u8>,
    Option<String>,
);
type ScopeRepair = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn refresh_step(
    tx: &Transaction<'_>,
    namespace: &str,
    subject: &str,
    at: u128,
) -> Result<Coverage> {
    let old = tx
        .prepare_cached(ASSOCIATIONS)?
        .query_map(params![namespace, subject], association_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // This shadow predicate and the inclusive Rust expiry transform match SEAT_STEP_ROWS.
    let candidate:Option<StepCandidate>=tx.query_row(
  "SELECT s.run,s.path,s.status,s.assignee,s.claimant,s.available,s.created,s.created_num,s.expiry
   FROM local_agent_queue_steps s JOIN local_agent_queue_runs r ON r.namespace=s.namespace AND r.run=s.run
   WHERE s.namespace=?1 AND s.subject=?2 AND s.agentless=0
     AND s.status IN ('ready','claimed','working','verifying') AND s.generation=r.generation
     AND NOT ((s.status='ready' OR (s.status IN ('claimed','working','verifying') AND s.expiry IS NOT NULL AND CAST(s.expiry AS INTEGER)<=?3)) AND r.phase='revision-draining')",
  params![namespace,subject,at.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?))).optional()?;
    let mut new = Vec::new();
    if let Some((
        run,
        path,
        mut status,
        assignee,
        mut claimant,
        available,
        created_text,
        created,
        expiry,
    )) = candidate
    {
        let available: Vec<String> = serde_json::from_str(&available).unwrap_or_default();
        if matches!(status.as_str(), "claimed" | "working" | "verifying")
            && expiry
                .as_ref()
                .and_then(|expiry| expiry.parse::<u128>().ok())
                .is_some_and(|expiry| expiry <= at)
        {
            status = "ready".into();
            claimant = None;
        }
        let carried = if status == "ready" && assignee.is_some() {
            match carried(tx, namespace, subject)? {
                Ok(value) => value,
                Err(reason) => return Ok(Coverage::Unsupported(reason)),
            }
        } else {
            None
        };
        let selector = serde_json::to_string(&(assignee.as_deref(), &available))?;
        for agent in [assignee.as_deref(), claimant.as_deref()]
            .into_iter()
            .flatten()
            .filter(|agent| agent.starts_with("agent/"))
            .collect::<BTreeSet<_>>()
        {
            new.push(Association {
                agent: agent.into(),
                subject: subject.into(),
                run: run.clone(),
                path: path.clone(),
                status: status.clone(),
                selector: selector.clone(),
                created: created.clone(),
                created_text: created_text.clone(),
                carried: carried.as_deref() == Some(agent),
            });
        }
    }
    for row in &old {
        mark_context(tx, namespace, row)?;
    }
    for row in &old {
        if !new.iter().any(|candidate| candidate.agent == row.agent) {
            tx.execute("DELETE FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND subject=?3",params![namespace,row.agent,subject])?;
        }
    }
    for row in &new {
        let rank = order::position(
            tx,
            namespace,
            &row.agent,
            &format!("mission-run/{}", row.run),
        )?;
        let (category, position) = rank
            .filter(|(_, live)| *live)
            .map(|(position, _)| (0i64, position))
            .unwrap_or((1, Vec::new()));
        tx.execute("INSERT INTO local_agent_queue_associations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,0,?11,?12)
    ON CONFLICT(namespace,agent,subject) DO UPDATE SET run=excluded.run,path=excluded.path,status=excluded.status,selector=excluded.selector,created=excluded.created,created_text=excluded.created_text,carried=excluded.carried,category=excluded.category,position=excluded.position
    WHERE run<>excluded.run OR path<>excluded.path OR status<>excluded.status OR selector<>excluded.selector OR created<>excluded.created OR created_text<>excluded.created_text OR carried<>excluded.carried OR category<>excluded.category OR position<>excluded.position",
    params![namespace,row.agent,subject,row.run,row.path,row.status,row.selector,row.created,row.created_text,row.carried,category,position])?;
        mark_context(tx, namespace, row)?;
    }
    let expiry:Option<String>=tx.query_row("SELECT expiry FROM local_agent_queue_steps s WHERE namespace=?1 AND subject=?2 AND status IN ('claimed','working','verifying') AND generation=(SELECT generation FROM local_agent_queue_runs WHERE namespace=s.namespace AND run=s.run)",params![namespace,subject],|row|row.get(0)).optional()?.flatten();
    let deadline = expiry
        .and_then(|expiry| expiry.parse::<u128>().ok())
        .filter(|expiry| *expiry > at)
        .map(u128::to_be_bytes);
    tx.execute("UPDATE local_agent_queue_steps SET deadline=?3 WHERE namespace=?1 AND subject=?2 AND deadline IS NOT ?3",params![namespace,subject,deadline.map(|d|d.to_vec())])?;
    Ok(Coverage::Complete)
}

fn refresh_flags(tx: &Transaction<'_>, namespace: &str, agent: &str, subject: &str) -> Result<()> {
    let row:Option<Association>=tx.query_row("SELECT agent,subject,run,path,status,selector,created,created_text,carried FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND subject=?3",params![namespace,agent,subject],association_from_row).optional()?;
    if let Some(row) = row {
        let parents = ancestors(&row.path);
        let parent:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND path IN (SELECT value FROM json_each(?5)) AND status<>'verifying')",params![namespace,agent,row.run,row.selector,serde_json::to_string(&parents)?],|row|row.get(0))?;
        let child:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND status='ready' AND path>=?5 AND path<?6)",params![namespace,agent,row.run,row.selector,format!("{}/",row.path),format!("{}0",row.path)],|row|row.get(0))?;
        // Ready membership is only assigned work; claimant-only readiness is not offered work.
        let (assignee, _): (Option<String>, Vec<String>) = serde_json::from_str(&row.selector)?;
        let ready = row.status == "ready" && assignee.as_deref() == Some(agent) && !parent;
        let held = matches!(row.status.as_str(), "claimed" | "working" | "verifying")
            && !(row.status == "verifying" && child);
        tx.execute("UPDATE local_agent_queue_associations SET held=?4,ready=?5 WHERE namespace=?1 AND agent=?2 AND subject=?3 AND (held<>?4 OR ready<>?5)",params![namespace,agent,subject,held,ready])?;
    }
    Ok(())
}

fn ns_clean(connection: &Connection, namespace: &str, at: u128) -> Result<bool> {
    if !order::clean(connection, namespace)? {
        return Ok(false);
    }
    for table in [
        "local_agent_queue_run_work",
        "local_agent_queue_join_work",
        "local_agent_queue_step_work",
        "local_agent_queue_flag_work",
        "local_agent_queue_scope_work",
        "local_agent_queue_rank_work",
        "local_seat_order_rank_dirty",
    ] {
        // Fixed repository-owned identifiers, never source/user SQL.
        if connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE namespace=?1)"),
            [namespace],
            |row| row.get::<_, bool>(0),
        )? {
            return Ok(false);
        }
    }
    let due:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_queue_steps WHERE namespace=?1 AND deadline IS NOT NULL AND deadline<=?2)",params![namespace,at.to_be_bytes().as_slice()],|row|row.get(0))?;
    Ok(!due)
}
pub(crate) fn clean(connection: &Connection, namespace: &Namespace, at: u128) -> Result<bool> {
    ns_clean(connection, namespace.as_str(), at)
}

fn join(tx: &Transaction<'_>, namespace: &str, agent: &str, run: &str) -> Result<()> {
    if !agent.starts_with("agent/") {
        return Ok(());
    }
    let first:Option<i64>=tx.query_row("SELECT join_at FROM local_agent_queue_steps WHERE namespace=?1 AND assignee=?2 AND run=?3 AND agentless=0 AND assignee IS NOT NULL ORDER BY join_at,subject LIMIT 1",params![namespace,agent,run],|row|row.get(0)).optional()?;
    let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_queue_runs r JOIN local_agent_queue_generations g ON g.namespace=r.namespace AND g.run=r.run AND g.generation=r.generation WHERE r.namespace=?1 AND g.agent=?2 AND r.run=?3 AND r.status NOT IN ('completed','failed','cancelled') AND r.phase<>'terminal')",params![namespace,agent,run],|row|row.get(0))?;
    order::set_join(
        tx,
        namespace,
        agent,
        &format!("mission-run/{run}"),
        first.map(|first| (first as u128, live)),
    )?;
    Ok(())
}

/// At most limit affected source/context/flag/rank or undo/apply items (1..=1024).
/// Empty work-item retirement also consumes one item. The source owner calls this before
/// publication and includes logical Unsupported in its source fence without rejecting admission.
pub(crate) fn drain(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    at: u128,
    limit: usize,
) -> Result<Drain> {
    anyhow::ensure!((1..=1024).contains(&limit), "agent queue drain page bound");
    let namespace = namespace.as_str();
    clock(tx, namespace)?;
    let previous: Vec<u8> = tx.query_row(
        "SELECT through FROM local_agent_queue_clock WHERE namespace=?1",
        [namespace],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        previous.as_slice() <= at.to_be_bytes().as_slice(),
        "agent queue clock regressed"
    );
    let mut processed = 0;
    while processed < limit {
        let due:Option<String>=tx.query_row("SELECT subject FROM local_agent_queue_steps WHERE namespace=?1 AND deadline IS NOT NULL AND deadline<=?2 ORDER BY deadline,subject LIMIT 1",params![namespace,at.to_be_bytes().as_slice()],|row|row.get(0)).optional()?;
        if let Some(subject) = due {
            tx.execute("UPDATE local_agent_queue_steps SET deadline=NULL WHERE namespace=?1 AND subject=?2",params![namespace,subject])?;
            step_work(tx, namespace, &subject)?;
            processed += 1;
            continue;
        }
        let remaining = limit - processed;
        let run_work:Option<(String,String,Option<String>)>=tx.query_row("SELECT run,generation,cursor FROM local_agent_queue_run_work WHERE namespace=?1 ORDER BY run,generation LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        if let Some((run, generation, cursor)) = run_work {
            let query = if cursor.is_some() {
                "SELECT subject,assignee,agentless FROM local_agent_queue_steps WHERE namespace=?1 AND run=?2 AND generation=?3 AND subject>?4 ORDER BY subject LIMIT ?5"
            } else {
                "SELECT subject,assignee,agentless FROM local_agent_queue_steps WHERE namespace=?1 AND run=?2 AND generation=?3 AND ?4 IS NULL ORDER BY subject LIMIT ?5"
            };
            let mut keys = tx
                .prepare_cached(query)?
                .query_map(
                    params![namespace, run, generation, cursor, remaining + 1],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, bool>(2)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let done = keys.len() <= remaining;
            keys.truncate(remaining);
            let selected = serde_json::to_string(
                &keys
                    .iter()
                    .map(|(subject, _, _)| subject)
                    .collect::<Vec<_>>(),
            )?;
            tx.execute("INSERT INTO local_agent_queue_step_work SELECT ?1,value FROM json_each(?2) WHERE 1 ON CONFLICT DO NOTHING",params![namespace,selected])?;
            tx.execute("INSERT INTO local_agent_queue_join_work SELECT namespace,assignee,run FROM local_agent_queue_steps WHERE namespace=?1 AND subject IN (SELECT value FROM json_each(?2)) AND agentless=0 AND assignee IS NOT NULL ON CONFLICT DO NOTHING",params![namespace,selected])?;
            if done {
                tx.execute("DELETE FROM local_agent_queue_run_work WHERE namespace=?1 AND run=?2 AND generation=?3",params![namespace,run,generation])?;
            } else {
                tx.execute("UPDATE local_agent_queue_run_work SET cursor=?4 WHERE namespace=?1 AND run=?2 AND generation=?3",params![namespace,run,generation,keys.last().map(|row|&row.0)])?;
            }
            processed += keys.len().max(1);
            continue;
        }
        let step:Option<String>=tx.query_row("SELECT subject FROM local_agent_queue_step_work WHERE namespace=?1 ORDER BY subject LIMIT 1",[namespace],|row|row.get(0)).optional()?;
        if let Some(subject) = step {
            let coverage = refresh_step(tx, namespace, &subject, at)?;
            if coverage != Coverage::Complete {
                return Ok(Drain {
                    processed,
                    clean: false,
                    coverage,
                });
            }
            // Owning run changes can change live/current membership without a step tuple delta.
            tx.execute("INSERT INTO local_agent_queue_join_work SELECT namespace,assignee,run FROM local_agent_queue_steps WHERE namespace=?1 AND subject=?2 AND agentless=0 AND assignee IS NOT NULL ON CONFLICT DO NOTHING",params![namespace,subject])?;
            tx.execute(
                "DELETE FROM local_agent_queue_step_work WHERE namespace=?1 AND subject=?2",
                params![namespace, subject],
            )?;
            processed += 1;
            continue;
        }
        let pair:Option<(String,String)>=tx.query_row("SELECT agent,run FROM local_agent_queue_join_work WHERE namespace=?1 ORDER BY agent,run LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((agent, run)) = pair {
            join(tx, namespace, &agent, &run)?;
            tx.execute("DELETE FROM local_agent_queue_join_work WHERE namespace=?1 AND agent=?2 AND run=?3",params![namespace,agent,run])?;
            processed += 1;
            continue;
        }
        let agent:Option<String>=tx.query_row("SELECT agent FROM local_seat_order_state WHERE namespace=?1 AND ready=0 ORDER BY agent LIMIT 1",[namespace],|row|row.get(0)).optional()?;
        if let Some(agent) = agent {
            let result = match order::page(tx, namespace, &agent, remaining) {
                Ok(result) => result,
                Err(error) if error.downcast_ref::<order::PositionExhausted>().is_some() => {
                    return Ok(Drain {
                        processed,
                        clean: false,
                        coverage: Coverage::Unsupported(error.to_string()),
                    });
                }
                Err(error) => return Err(error),
            };
            processed += result.processed.max(1);
            continue;
        }
        let pair:Option<(String,String)>=tx.query_row("SELECT agent,run FROM local_seat_order_rank_dirty WHERE namespace=?1 ORDER BY agent,run LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((agent, run)) = pair {
            tx.execute("INSERT INTO local_agent_queue_rank_work VALUES(?1,?2,?3,NULL) ON CONFLICT(namespace,agent,run) DO UPDATE SET cursor=NULL",params![namespace,agent,run])?;
            tx.execute("DELETE FROM local_seat_order_rank_dirty WHERE namespace=?1 AND agent=?2 AND run=?3",params![namespace,agent,run])?;
            processed += 1;
            continue;
        }
        let rank_work:Option<(String,String,Option<String>)>=tx.query_row("SELECT agent,run,cursor FROM local_agent_queue_rank_work WHERE namespace=?1 ORDER BY agent,run LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        if let Some((agent, run, cursor)) = rank_work {
            let bare = run
                .strip_prefix("mission-run/")
                .context("queue rank run subject")?;
            let query = if cursor.is_some() {
                "SELECT subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND subject>?4 ORDER BY subject LIMIT ?5"
            } else {
                "SELECT subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND ?4 IS NULL ORDER BY subject LIMIT ?5"
            };
            let mut keys = tx
                .prepare_cached(query)?
                .query_map(
                    params![namespace, agent, bare, cursor, remaining + 1],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let done = keys.len() <= remaining;
            keys.truncate(remaining);
            let (category, position) = order::position(tx, namespace, &agent, &run)?
                .filter(|(_, live)| *live)
                .map(|(position, _)| (0i64, position))
                .unwrap_or((1, Vec::new()));
            tx.execute("UPDATE local_agent_queue_associations SET category=?4,position=?5 WHERE namespace=?1 AND agent=?2 AND subject IN (SELECT value FROM json_each(?3)) AND (category<>?4 OR position<>?5)",params![namespace,agent,serde_json::to_string(&keys)?,category,position])?;
            if done {
                tx.execute("DELETE FROM local_agent_queue_rank_work WHERE namespace=?1 AND agent=?2 AND run=?3",params![namespace,agent,run])?;
            } else {
                tx.execute("UPDATE local_agent_queue_rank_work SET cursor=?4 WHERE namespace=?1 AND agent=?2 AND run=?3",params![namespace,agent,run,keys.last()])?;
            }
            processed += keys.len().max(1);
            continue;
        }
        let scope:Option<ScopeRepair>=tx.query_row("SELECT agent,run,selector,prefix,cursor_path,cursor_subject FROM local_agent_queue_scope_work WHERE namespace=?1 ORDER BY agent,run,selector,prefix LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?;
        if let Some((agent, run, selector, prefix, cursor_path, cursor_subject)) = scope {
            let query = if cursor_path.is_some() {
                "SELECT path,subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND status='ready' AND path>=?5 AND path<?6 AND (path,subject)>(?7,?8) ORDER BY path,subject LIMIT ?9"
            } else {
                "SELECT path,subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND status='ready' AND path>=?5 AND path<?6 AND ?7 IS NULL AND ?8 IS NULL ORDER BY path,subject LIMIT ?9"
            };
            let mut keys = tx
                .prepare_cached(query)?
                .query_map(
                    params![
                        namespace,
                        agent,
                        run,
                        selector,
                        format!("{prefix}/"),
                        format!("{prefix}0"),
                        cursor_path,
                        cursor_subject,
                        remaining + 1
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let done = keys.len() <= remaining;
            keys.truncate(remaining);
            tx.execute("INSERT INTO local_agent_queue_flag_work SELECT ?1,?2,value FROM json_each(?3) WHERE 1 ON CONFLICT DO NOTHING",params![namespace,agent,serde_json::to_string(&keys.iter().map(|(_,subject)|subject).collect::<Vec<_>>())?])?;
            if done {
                tx.execute("DELETE FROM local_agent_queue_scope_work WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND prefix=?5",params![namespace,agent,run,selector,prefix])?;
            } else {
                tx.execute("UPDATE local_agent_queue_scope_work SET cursor_path=?6,cursor_subject=?7 WHERE namespace=?1 AND agent=?2 AND run=?3 AND selector=?4 AND prefix=?5",params![namespace,agent,run,selector,prefix,keys.last().map(|row|&row.0),keys.last().map(|row|&row.1)])?;
            }
            processed += keys.len().max(1);
            continue;
        }
        let flag:Option<(String,String)>=tx.query_row("SELECT agent,subject FROM local_agent_queue_flag_work WHERE namespace=?1 ORDER BY agent,subject LIMIT 1",[namespace],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((agent, subject)) = flag {
            refresh_flags(tx, namespace, &agent, &subject)?;
            tx.execute("DELETE FROM local_agent_queue_flag_work WHERE namespace=?1 AND agent=?2 AND subject=?3",params![namespace,agent,subject])?;
            processed += 1;
            continue;
        }
        break;
    }
    let at = at.to_be_bytes();
    tx.execute(
        "UPDATE local_agent_queue_clock SET through=?2 WHERE namespace=?1 AND through<>?2",
        params![namespace, at.as_slice()],
    )?;
    Ok(Drain {
        processed,
        clean: ns_clean(tx, namespace, u128::from_be_bytes(at))?,
        coverage: Coverage::Complete,
    })
}

const ROWS: &str = r#"SELECT requested.value,COALESCE(c.active,0),COALESCE(c.queued,0),
 (SELECT json_group_array(subject) FROM (SELECT subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=requested.value AND held=1 ORDER BY length(created_text),created_text,subject LIMIT 5)),
 (SELECT json_group_array(subject) FROM (SELECT subject FROM local_agent_queue_associations WHERE namespace=?1 AND agent=requested.value AND ready=1 ORDER BY carried DESC,category,position,created,subject LIMIT 5))
 FROM json_each(?2) requested LEFT JOIN local_agent_queue_counts c ON c.namespace=?1 AND c.agent=requested.value"#;
/// One query with bounded indexed prefixes per selected public agent. This proves neither
/// namespace/source authority nor complete card readiness: the caller verifies the current
/// Installer Root, certified source cut and all other card dependencies in the same snapshot.
pub(crate) fn rows(
    connection: &Connection,
    namespace: &Namespace,
    agents: &[String],
    at: u128,
) -> Result<BTreeMap<String, AgentWorkQueue>> {
    anyhow::ensure!(agents.len() <= 501, "agent queue selected batch bound");
    anyhow::ensure!(
        agents.iter().all(|agent| agent.starts_with("agent/")),
        "agent queue requires public agent IDs"
    );
    anyhow::ensure!(
        ns_clean(connection, namespace.as_str(), at)?,
        "agent queue dependency maintenance incomplete"
    );
    let clock: Vec<u8> = connection.query_row(
        "SELECT through FROM local_agent_queue_clock WHERE namespace=?1",
        [namespace.as_str()],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        clock.as_slice() <= at.to_be_bytes().as_slice(),
        "agent queue snapshot clock regressed"
    );
    let tuples = connection
        .prepare_cached(ROWS)?
        .query_map(
            params![namespace.as_str(), serde_json::to_string(agents)?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tuples
        .into_iter()
        .filter_map(|(agent, active, queued, held, ready)| {
            if active == 0 && queued == 0 {
                return None;
            }
            Some((|| -> Result<_> {
                let current: Vec<String> = serde_json::from_str(&held)?;
                let upcoming: Vec<String> = serde_json::from_str(&ready)?;
                Ok((
                    agent,
                    AgentWorkQueue {
                        current_work_ids: current,
                        active_work_count: active,
                        next_work_id: upcoming.first().cloned(),
                        upcoming_work_ids: upcoming,
                        queued_work_count: queued,
                    },
                ))
            })())
        })
        .collect()
}
pub(crate) fn next_deadline(
    connection: &Connection,
    namespace: &Namespace,
) -> Result<Option<u128>> {
    let deadline:Option<Vec<u8>>=connection.query_row("SELECT deadline FROM local_agent_queue_steps WHERE namespace=?1 AND deadline IS NOT NULL ORDER BY deadline,subject LIMIT 1",[namespace.as_str()],|row|row.get(0)).optional()?;
    deadline
        .map(|bytes| {
            Ok(u128::from_be_bytes(
                bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("queue deadline width"))?,
            ))
        })
        .transpose()
}
pub(crate) fn dirty_agents(
    connection: &Connection,
    namespace: &Namespace,
    limit: usize,
) -> Result<Vec<String>> {
    anyhow::ensure!((1..=1024).contains(&limit), "queue dirty agent page bound");
    Ok(connection
        .prepare_cached(
            "SELECT agent FROM local_agent_queue_dirty WHERE namespace=?1 ORDER BY agent LIMIT ?2",
        )?
        .query_map(params![namespace.as_str(), limit], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
pub(crate) fn acknowledge(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agents: &[String],
) -> Result<()> {
    anyhow::ensure!(agents.len() <= 1024, "queue dirty acknowledgment bound");
    tx.execute("DELETE FROM local_agent_queue_dirty WHERE namespace=?1 AND agent IN (SELECT value FROM json_each(?2))",params![namespace.as_str(),serde_json::to_string(agents)?])?;
    Ok(())
}

/// Complete selected labels from this same installable namespace, including full raw
/// projected fields. The source owner verifies current Root/source/authority before serving.
pub(crate) fn labels(
    connection: &Connection,
    namespace: &Namespace,
    subjects: &[String],
) -> Result<BTreeMap<String, StepLabel>> {
    anyhow::ensure!(
        subjects.len() <= 501 * AGENT_WORK_PREVIEW_LIMIT * 2,
        "namespace label batch bound"
    );
    let bodies=connection.prepare_cached("SELECT s.subject,s.body,r.body FROM local_agent_queue_steps s JOIN local_agent_queue_runs r ON r.namespace=s.namespace AND r.run=s.run WHERE s.namespace=?1 AND s.subject IN (SELECT value FROM json_each(?2))")?
  .query_map(params![namespace.as_str(),serde_json::to_string(subjects)?],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    bodies
        .into_iter()
        .map(|(subject, step, run)| -> Result<_> {
            let step: Value = serde_json::from_str(&step)?;
            let run: Value = serde_json::from_str(&run)?;
            let goals: Vec<String> = serde_json::from_str(text(&step, "goals")?)?;
            Ok((
                subject,
                StepLabel {
                    run: format!("mission-run/{}", text(&step, "run_id")?),
                    mission: format!("mission/{}", text(&run, "mission_id")?),
                    path: text(&step, "step_path")?.into(),
                    title: optional_text(&step, "title")?,
                    goal: goals.into_iter().next(),
                    status: text(&step, "status")?.into(),
                    updated_at_unix_ms: text(&step, "updated_at_unix_ms")?.parse()?,
                },
            ))
        })
        .collect()
}

/// Namespace reclamation is explicit and globally bounded across all component tables.
/// The Installer never passes a ready namespace. Fixed table identifiers are repository-owned.
pub(crate) fn reclaim(tx: &Transaction<'_>, namespace: &Namespace, limit: usize) -> Result<bool> {
    anyhow::ensure!((1..=1024).contains(&limit), "queue reclaim page bound");
    let namespace = namespace.as_str();
    let mut remaining = limit;
    tx.execute(
        "INSERT INTO local_agent_queue_reclaiming VALUES(?1) ON CONFLICT DO NOTHING",
        [namespace],
    )?;
    // Retired-source triggers are suppressed; order-node deletion dirties are reclaimed later.
    let tables = [
        "local_agent_queue_scope_work",
        "local_agent_queue_flag_work",
        "local_agent_queue_rank_work",
        "local_agent_queue_step_work",
        "local_agent_queue_run_work",
        "local_agent_queue_join_work",
        "local_agent_queue_associations",
        "local_agent_queue_counts",
        "local_agent_queue_steps",
        "local_agent_queue_runs",
        "local_agent_queue_claims",
        "local_agent_queue_generations",
        "local_agent_queue_dirty",
        "local_agent_queue_clock",
        "local_seat_order_refs",
        "local_seat_order_moves",
        "local_seat_order_events",
        "local_seat_order_journal",
        "local_seat_order_nodes",
        "local_seat_order_rank_dirty",
        "local_seat_order_joins",
        "local_seat_order_state",
        "local_agent_queue_reclaiming",
    ];
    // All tables are WITHOUT ROWID except clock's single namespace PK; full PK tuples are
    // required here. Source/card owners need these exact component keys for safe bounded cleanup.
    for table in tables {
        if remaining == 0 {
            break;
        }
        let keys: &[&str] = match table {
            "local_agent_queue_scope_work" => &["agent", "run", "selector", "prefix"],
            "local_agent_queue_flag_work" | "local_agent_queue_associations" => {
                &["agent", "subject"]
            }
            "local_agent_queue_rank_work"
            | "local_agent_queue_join_work"
            | "local_seat_order_joins"
            | "local_seat_order_nodes"
            | "local_seat_order_rank_dirty" => &["agent", "run"],
            "local_agent_queue_step_work" | "local_agent_queue_steps" => &["subject"],
            "local_agent_queue_run_work" => &["run", "generation"],
            "local_agent_queue_runs" => &["run"],
            "local_agent_queue_claims" => &["id"],
            "local_agent_queue_generations" => &["agent", "run", "generation"],
            "local_agent_queue_counts" | "local_agent_queue_dirty" | "local_seat_order_state" => {
                &["agent"]
            }
            "local_seat_order_refs" => &["agent", "run", "rank", "id"],
            "local_seat_order_moves" => &["agent", "id"],
            "local_seat_order_events" | "local_seat_order_journal" => &["agent", "rank"],
            "local_agent_queue_clock" | "local_agent_queue_reclaiming" => &[],
            _ => unreachable!(),
        };
        let affected = if keys.is_empty() {
            tx.execute(
                &format!("DELETE FROM {table} WHERE namespace=?1"),
                [namespace],
            )?
        } else {
            let columns = keys.join(",");
            tx.execute(&format!("DELETE FROM {table} WHERE namespace=?1 AND ({columns}) IN (SELECT {columns} FROM {table} WHERE namespace=?1 ORDER BY {columns} LIMIT ?2)"),params![namespace,remaining])?
        };
        remaining -= affected;
    }
    for table in tables {
        if tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE namespace=?1)"),
            [namespace],
            |row| row.get::<_, bool>(0),
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
