//! Application-shaped operators. Admission belongs to the application runtime, not these
//! test operators. The historical oracle in ivm_real_views.rs never reads these tables.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimRecord,
    ivm::{Contribution, Definition, View},
    store::canonical,
};
use std::collections::BTreeSet;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS app_rows(view TEXT NOT NULL,id TEXT NOT NULL,scope TEXT NOT NULL,
    payload TEXT NOT NULL,PRIMARY KEY(view,id));
CREATE INDEX IF NOT EXISTS app_rows_scope ON app_rows(view,scope,id);
CREATE TABLE IF NOT EXISTS app_work(actor TEXT NOT NULL,step TEXT NOT NULL,incarnation TEXT NOT NULL,
    payload TEXT NOT NULL,rank BLOB NOT NULL,PRIMARY KEY(actor,step));
CREATE INDEX IF NOT EXISTS app_work_incarnation ON app_work(actor,incarnation,step);
CREATE INDEX IF NOT EXISTS app_work_activity ON app_work(actor,incarnation,rank DESC,step);
CREATE UNIQUE INDEX IF NOT EXISTS app_work_step ON app_work(step);
CREATE TABLE IF NOT EXISTS app_work_inputs(id TEXT PRIMARY KEY,step TEXT NOT NULL,
    rank BLOB NOT NULL,payload TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS app_work_inputs_rank ON app_work_inputs(step,rank DESC,id DESC);
CREATE TABLE IF NOT EXISTS app_unread(recipient TEXT PRIMARY KEY,count INTEGER NOT NULL CHECK(count>=0));
CREATE TABLE IF NOT EXISTS app_prior_hosts(subject TEXT NOT NULL,host TEXT NOT NULL,
    PRIMARY KEY(subject,host));
CREATE INDEX IF NOT EXISTS app_prior_hosts_host ON app_prior_hosts(host,subject);
-- Immutable, admitted mission-definition input, installed explicitly by the fixture owner.
-- This is separate from the derived rows and is never inferred by a read/history replay.
CREATE TABLE IF NOT EXISTS app_steps(subject TEXT PRIMARY KEY,run TEXT NOT NULL,path TEXT NOT NULL,
    parent TEXT,status TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS app_steps_run ON app_steps(run,subject);
"#;

pub fn tuple(parts: &[&str]) -> String {
    serde_json::to_string(parts).unwrap()
}
pub fn fields(claim: &ClaimRecord) -> &Value {
    claim.body.get("fields").unwrap_or(&claim.body)
}
pub fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .with_context(|| format!("missing {field}"))
}
pub fn head(connection: &Connection, view: &str, key: &str, slot: &str) -> Result<Option<Value>> {
    connection
        .query_row(
            "SELECT value FROM ivm_heads WHERE view=?1 AND key=?2 AND register=?3",
            params![view, key, slot],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}
pub fn row(connection: &Connection, view: &str, id: &str) -> Result<Option<Value>> {
    connection
        .query_row(
            "SELECT payload FROM app_rows WHERE view=?1 AND id=?2",
            params![view, id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}
pub fn put(
    tx: &Transaction<'_>,
    view: &str,
    id: &str,
    scope: &str,
    next: Option<Value>,
) -> Result<bool> {
    if row(tx, view, id)? == next {
        return Ok(false);
    }
    if let Some(next) = next {
        tx.execute(
            "INSERT INTO app_rows VALUES(?1,?2,?3,?4) ON CONFLICT(view,id)
            DO UPDATE SET scope=excluded.scope,payload=excluded.payload",
            params![view, id, scope, next.to_string()],
        )?;
    } else {
        tx.execute(
            "DELETE FROM app_rows WHERE view=?1 AND id=?2",
            params![view, id],
        )?;
    }
    Ok(true)
}
pub fn collection(connection: &Connection, view: &str, scope: &str) -> Result<Vec<Value>> {
    connection
        .prepare_cached("SELECT payload FROM app_rows WHERE view=?1 AND scope=?2 ORDER BY id")?
        .query_map(params![view, scope], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}

#[derive(Clone, Copy)]
pub enum Family {
    Card,
    Mailbox,
    Tree,
    Desired,
}
impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Self::Card => "agent-card",
            Self::Mailbox => "mailbox",
            Self::Tree => "mission-tree",
            Self::Desired => "desired-by-host",
        }
    }
    fn kinds(self) -> &'static [&'static str] {
        match self {
            Self::Card => &[
                "runtime.observed",
                "harness.observed",
                "harness.usage",
                "work.claimed",
                "work.progress",
                "work.submitted",
                "work.failed",
                "work.released",
            ],
            Self::Mailbox => &["message.sent", "message.read", "message.closed"],
            Self::Tree => &[
                "mission-run.created",
                "step-run.state",
                "work.claimed",
                "work.progress",
                "work.submitted",
                "work.failed",
                "work.released",
            ],
            Self::Desired => &["intent.desired", "owned-set.revised"],
        }
    }
}

impl View for Family {
    fn definition(&self) -> Definition {
        Definition {
            name: self.name(),
            fingerprint: "real-views.v1;admitted-input.v1;canonical.v1;typed-keys.v1",
            kinds: self.kinds(),
            local_kinds: if matches!(self, Self::Tree) {
                &["mission.layout.install"]
            } else {
                &[]
            },
            max_contributions: 4,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        Ok(())
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        canonical: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        let f = fields(claim);
        if matches!(self, Self::Card) && claim.kind.starts_with("work.") {
            return Ok(vec![]);
        }
        let mapping = match self {
            Self::Card => match claim.kind.as_str() {
                "runtime.observed" => (claim.subject.clone(), "runtime".to_owned()),
                "harness.observed" => (
                    claim.subject.clone(),
                    tuple(&["harness", text(f, "incarnation_id")?]),
                ),
                "harness.usage" => {
                    ensure!(
                        f["semantics"] == "session_cumulative",
                        "unsupported usage aggregation requires its own operator"
                    );
                    (
                        claim.subject.clone(),
                        tuple(&["usage", text(f, "incarnation_id")?]),
                    )
                }
                _ => (
                    claim.actor.clone().context("work actor missing")?,
                    tuple(&["work", &claim.subject]),
                ),
            },
            Self::Mailbox => (
                claim.subject.clone(),
                match claim.kind.as_str() {
                    "message.sent" => "sent",
                    "message.read" => "read",
                    _ => "closed",
                }
                .to_owned(),
            ),
            Self::Tree => (
                claim.subject.clone(),
                if claim.kind == "mission-run.created" {
                    "run"
                } else {
                    "state"
                }
                .to_owned(),
            ),
            Self::Desired => {
                // Owned-set validation/closure cannot be inferred from an unowned declaration.
                ensure!(
                    claim.kind == "intent.desired"
                        && ["owned_set", "owner_run", "owner_step", "owner_generation"]
                            .iter()
                            .all(|k| claim.body.get(*k).is_none_or(Value::is_null)),
                    "owned-set authority requires the reviewed ownership operator"
                );
                (claim.subject.clone(), "desired".to_owned())
            }
        };
        let mut rank = canonical::sortable_key(canonical);
        if matches!(self, Self::Card) && claim.kind == "harness.usage" {
            let mut usage_rank = f["total_tokens"]
                .as_u64()
                .context("missing cumulative total")?
                .to_be_bytes()
                .to_vec();
            usage_rank.extend(rank);
            rank = usage_rank;
        }
        let mut result = vec![Contribution {
            key: mapping.0,
            register: mapping.1,
            value: json!({"fields":f,"claim":claim.id,"actor":claim.actor,"kind":claim.kind}),
            rank,
        }];
        if matches!(self, Self::Card)
            && claim.kind == "harness.observed"
            && f["provider_auth"].is_boolean()
        {
            result.push(Contribution {
                key: claim.subject.clone(),
                register: tuple(&["auth", text(f, "incarnation_id")?]),
                value: json!({"fields":f,"claim":claim.id}),
                rank: canonical::sortable_key(canonical),
            });
        }
        Ok(result)
    }
    fn affected_keys(
        &self,
        tx: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<BTreeSet<String>> {
        let mut actors = BTreeSet::new();
        if !matches!(self, Self::Card) {
            return Ok(actors);
        }
        let steps = old
            .into_iter()
            .chain(new)
            .filter(|c| c.kind.starts_with("work."))
            .map(|c| c.subject.clone())
            .collect::<BTreeSet<_>>();
        if let Some(old) = old.filter(|c| c.kind.starts_with("work.")) {
            tx.execute("DELETE FROM app_work_inputs WHERE id=?1", [&old.id])?;
        }
        if let Some(new) = new.filter(|c| c.kind.starts_with("work.")) {
            tx.execute(
                "INSERT INTO app_work_inputs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO NOTHING",
                params![
                    new.id,
                    new.subject,
                    canonical::sortable_key(&canonical::claim_key(tx, &new.id)?),
                    json!({"fields":fields(new),"actor":new.actor,"kind":new.kind,"claim":new.id})
                        .to_string()
                ],
            )?;
        }
        for step in steps {
            let previous = tx
                .query_row("SELECT payload FROM app_work WHERE step=?1", [&step], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .map(|s| serde_json::from_str::<Value>(&s))
                .transpose()?;
            let selected=tx.query_row("SELECT rank,payload FROM app_work_inputs WHERE step=?1 ORDER BY rank DESC,id DESC LIMIT 1",
                [&step],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?))).optional()?
                .map(|(rank,s)|Ok::<_,anyhow::Error>((rank,serde_json::from_str::<Value>(&s)?))).transpose()?
                .filter(|(_,s)|matches!(s["kind"].as_str(),Some("work.claimed"|"work.progress")));
            // Lower-ranked history contributes provenance, but cannot force a card/list scan.
            if previous.as_ref() == selected.as_ref().map(|(_, s)| s) {
                continue;
            }
            if let Some(previous) = previous {
                actors.insert(text(&previous, "actor")?.to_owned());
            }
            tx.execute("DELETE FROM app_work WHERE step=?1", [&step])?;
            if let Some((rank, selected)) = selected {
                let actor = text(&selected, "actor")?;
                let incarnation = text(&selected["fields"], "claim_incarnation")?;
                tx.execute(
                    "INSERT INTO app_work VALUES(?1,?2,?3,?4,?5)",
                    params![actor, step, incarnation, selected.to_string(), rank],
                )?;
                actors.insert(actor.to_owned());
            }
        }
        Ok(actors)
    }
    fn maintain_key(
        &self,
        tx: &Transaction<'_>,
        key: &str,
        _old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        Ok(Some(match self {
            Self::Card => card(tx, key)?,
            Self::Mailbox => mail(tx, key)?,
            Self::Tree => tree(tx, key)?,
            Self::Desired => desired(tx, key, new)?,
        }))
    }
}

fn card(tx: &Transaction<'_>, actor: &str) -> Result<bool> {
    // Indexed global step selection updates only the prior/new actor's membership. Serialization
    // visits that actor's visible work; it does not scan the roster or historical step claims.
    let Some(runtime) = head(tx, "agent-card", actor, "runtime")? else {
        return put(tx, "agent-card", actor, actor, None);
    };
    let incarnation = text(&runtime["fields"], "incarnation_id")?;
    let harness = head(tx, "agent-card", actor, &tuple(&["harness", incarnation]))?;
    let auth = head(tx, "agent-card", actor, &tuple(&["auth", incarnation]))?;
    let usage = head(tx, "agent-card", actor, &tuple(&["usage", incarnation]))?;
    let last_work=tx.query_row("SELECT rank FROM app_work WHERE actor=?1 AND incarnation=?2 ORDER BY rank DESC,step LIMIT 1",
        params![actor,incarnation],|r|r.get::<_,Vec<u8>>(0)).optional()?;
    let last_harness = tx
        .query_row(
            "SELECT rank FROM ivm_heads WHERE view='agent-card' AND key=?1 AND register=?2",
            params![actor, tuple(&["harness", incarnation])],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let work: Vec<Value> = tx
        .prepare_cached(
            "SELECT step,payload FROM app_work WHERE actor=?1 AND incarnation=?2 ORDER BY step",
        )?
        .query_map(params![actor, incarnation], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|r| {
            let (step, payload) = r?;
            let p: Value = serde_json::from_str(&payload)?;
            Ok(json!({"step":step,"status":p["fields"]["status"],"claim":p["claim"]}))
        })
        .collect::<Result<_>>()?;
    let status = if runtime["fields"]["status"] != "running" {
        runtime["fields"]["status"].clone()
    } else if auth
        .as_ref()
        .is_some_and(|h| h["fields"]["provider_auth"] == false)
    {
        json!("needs-login")
    } else if last_work
        .as_ref()
        .is_some_and(|w| last_harness.as_ref().is_none_or(|h| w > h))
    {
        json!("working")
    } else {
        harness
            .as_ref()
            .map(|h| h["fields"]["state"].clone())
            .unwrap_or(json!("unknown"))
    };
    put(
        tx,
        "agent-card",
        actor,
        actor,
        Some(
            json!({"subject":actor,"incarnation":incarnation,"status":status,
        "usage":usage.map(|u|u["fields"]["total_tokens"].clone()),"work":work}),
        ),
    )
}

fn mail(tx: &Transaction<'_>, message: &str) -> Result<bool> {
    let previous = row(tx, "mailbox", message)?;
    let next = if let Some(sent) = head(tx, "mailbox", message, "sent")? {
        let unread = head(tx, "mailbox", message, "read")?.is_none()
            && head(tx, "mailbox", message, "closed")?.is_none();
        Some(
            json!({"id":message,"to":sent["fields"]["to"],"from":sent["fields"]["from"],
            "body":sent["fields"]["body"],"unread":unread}),
        )
    } else {
        None
    };
    if previous == next {
        return Ok(false);
    }
    for (value, delta) in [(previous.as_ref(), -1), (next.as_ref(), 1)] {
        if let Some(value) = value.filter(|v| v["unread"] == true) {
            let recipient = text(value, "to")?;
            // New recipients start at zero before a positive delta; removal sees an existing row.
            tx.execute("INSERT OR IGNORE INTO app_unread VALUES(?1,0)", [recipient])?;
            tx.execute(
                "UPDATE app_unread SET count=count+?2 WHERE recipient=?1",
                params![recipient, delta],
            )?;
        }
    }
    let scope = next
        .as_ref()
        .map(|v| text(v, "to"))
        .transpose()?
        .unwrap_or("")
        .to_owned();
    put(tx, "mailbox", message, &scope, next)
}

fn tree(tx: &Transaction<'_>, id: &str) -> Result<bool> {
    if let Some(run) = head(tx, "mission-tree", id, "run")? {
        return put(
            tx,
            "mission-tree",
            id,
            id,
            Some(json!({"id":id,
            "state":run["fields"]["status"],"parent":run["fields"]["parent_step_run"],"kind":"run"})),
        );
    }
    let declaration = tx
        .query_row(
            "SELECT run,path,parent,status FROM app_steps WHERE subject=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((run, path, parent, status)) = declaration else {
        return put(tx, "mission-tree", id, "", None);
    };
    let state = head(tx, "mission-tree", id, "state")?;
    put(
        tx,
        "mission-tree",
        id,
        &run,
        Some(json!({"id":id,"parent":parent,
        "kind":"step","path":path,"state":state.map(|s|s["fields"]["status"].clone()).unwrap_or(json!(status))})),
    )
}

fn desired(tx: &Transaction<'_>, subject: &str, new: Option<&ClaimRecord>) -> Result<bool> {
    // The prior-host relation is additive admitted history, independent of the selected head.
    let mut candidates_changed = false;
    if let Some(new) = new {
        let f = fields(new);
        if let Some(host) = f.pointer("/member/host").and_then(Value::as_str) {
            candidates_changed = tx.execute(
                "INSERT OR IGNORE INTO app_prior_hosts VALUES(?1,?2)",
                params![subject, host],
            )? > 0;
        }
    }
    let Some(selected) = head(tx, "desired-by-host", subject, "desired")? else {
        return Ok(put(tx, "desired-by-host", subject, "", None)? || candidates_changed);
    };
    let f = &selected["fields"];
    let host = f
        .pointer("/member/host")
        .and_then(Value::as_str)
        .unwrap_or("");
    ensure!(
        matches!(f["kind"].as_str(), Some("agent" | "stop")),
        "unsupported desired kind"
    );
    Ok(put(
        tx,
        "desired-by-host",
        subject,
        host,
        Some(json!({"subject":subject,"kind":f["kind"],
        "host":f.pointer("/member/host"),"claim":selected["claim"]})),
    )? || candidates_changed)
}

pub fn desired_candidates(connection: &Connection, host: &str) -> Result<Vec<Value>> {
    connection
        .prepare_cached(
            "SELECT payload FROM (
        SELECT r.id,r.payload FROM app_rows r WHERE r.view='desired-by-host' AND r.scope=?1
        UNION SELECT r.id,r.payload FROM app_prior_hosts p JOIN app_rows r
          ON r.view='desired-by-host' AND r.id=p.subject WHERE p.host=?1) ORDER BY id",
        )?
        .query_map([host], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
