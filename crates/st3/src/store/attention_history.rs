//! Disposable source history. The daemon indexes bounded pages after it starts serving.
//! Local versions pin pages; epochs fence replay/trim without renumbering graph indexes.
use super::*;
use std::time::{Duration, Instant};

const CLAIM_PAGE: usize = 128;
const DIRTY_PAGE: usize = 16;
// Cap retained input and generated rows before taking a write lock. Oversized sources
// are quarantined like malformed sources, with incomplete coverage visible to clients.
const SOURCE_CLAIMS: usize = 256;
const SOURCE_ROWS: usize = 256;
const SOURCE_BUDGET: Duration = Duration::from_millis(250);
const PUBLISH_BUDGET: Duration = Duration::from_millis(250);
const KINDS: &[&str] = &[
    "work.person-asked",
    "work.person-done",
    "work.person-cancelled",
    "gate.requested",
    "gate.result",
    "planning-session.started",
    "planning-session.candidate-submitted",
    "planning-session.previewed",
    "planning-session.approved",
    "planning-session.cancelled",
    "planning-session.revision-requested",
    "revision-proposal.created",
    "revision-proposal.approved",
    "revision-proposal.cancelled",
];
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_attention_history_v2 (
 id TEXT NOT NULL, source TEXT NOT NULL, person TEXT NOT NULL,
 sort_ms INTEGER NOT NULL, valid_from INTEGER NOT NULL, valid_to INTEGER,
 body TEXT NOT NULL, epoch INTEGER NOT NULL, source_index INTEGER NOT NULL,
 PRIMARY KEY(id,valid_from)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_attention_history_person_v2
 ON local_attention_history_v2(epoch,person,sort_ms DESC,id DESC,valid_from DESC);
CREATE INDEX IF NOT EXISTS local_attention_history_source_v2
 ON local_attention_history_v2(epoch,source) WHERE valid_to IS NULL;
CREATE TABLE IF NOT EXISTS local_attention_history_state (
 id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL DEFAULT 0,
 epoch INTEGER NOT NULL DEFAULT 1, backfill_after INTEGER NOT NULL DEFAULT 0,
 backfill_done INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 0, degraded INTEGER NOT NULL DEFAULT 0,
 indexed_through INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS local_attention_history_dirty (subject TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_attention_history_errors (
 epoch INTEGER NOT NULL, subject TEXT NOT NULL, error TEXT NOT NULL,
 PRIMARY KEY(epoch,subject)
) WITHOUT ROWID;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    // No new claim-log index: an upgrade must not scan claims while opening. Backfill uses
    // the existing store_index key in fixed pages, including irrelevant rows, then filters.
    let kinds = KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    for name in [
        "claim_insert",
        "claim_delete",
        "cut_delete",
        "claim_update",
        "custom_insert",
        "custom_update",
        "custom_delete",
        "batch_update",
        "record_insert",
        "record_update",
        "record_delete",
    ] {
        connection.execute_batch(&format!("DROP TRIGGER IF EXISTS attention_history_{name}"))?;
    }
    for (event, record) in [("INSERT", "NEW"), ("DELETE", "OLD")] {
        connection.execute_batch(&format!("CREATE TRIGGER attention_history_claim_{} AFTER {event} ON claims
            WHEN {record}.kind IN ({kinds}) AND (SELECT enabled FROM local_attention_history_state WHERE id=1)=1 BEGIN INSERT INTO local_attention_history_dirty VALUES({record}.subject) ON CONFLICT DO NOTHING; END;", event.to_lowercase()))?;
    }
    // A real trim need not renumber surviving claims, but dropping the highest position
    // can lower the retained frontier below cached rows. Fence it once, in constant work.
    // Unused history never reads the claim maximum; the state lookup is a singleton key.
    connection.execute_batch("CREATE TRIGGER attention_history_cut_delete AFTER DELETE ON claims
        WHEN OLD.store_index >= (SELECT indexed_through FROM local_attention_history_state WHERE id=1 AND enabled=1 AND indexed_through>0)
        AND COALESCE((SELECT MAX(store_index) FROM claims),0) < (SELECT indexed_through FROM local_attention_history_state WHERE id=1)
        BEGIN UPDATE local_attention_history_state SET epoch=epoch+1,version=version+1,
            backfill_after=0,backfill_done=0,degraded=0,indexed_through=0 WHERE id=1; END;")?;
    connection.execute_batch(&format!("CREATE TRIGGER attention_history_claim_update AFTER UPDATE OF body,actor ON claims
        WHEN (SELECT enabled FROM local_attention_history_state WHERE id=1)=1 AND NEW.kind IN ({kinds}) AND (NEW.body IS NOT OLD.body OR NEW.actor IS NOT OLD.actor)
        BEGIN INSERT INTO local_attention_history_dirty VALUES(NEW.subject) ON CONFLICT DO NOTHING; END;"))?;
    for (event, record, change) in [
        ("INSERT", "NEW", ""),
        ("DELETE", "OLD", ""),
        ("UPDATE", "NEW", "AND NEW.body IS NOT OLD.body"),
    ] {
        connection.execute_batch(&format!("CREATE TRIGGER attention_history_custom_{} AFTER {event} ON custom_sources
            WHEN (SELECT enabled FROM local_attention_history_state WHERE id=1)=1
            AND {record}.subject GLOB 'custom/*' AND {record}.subject NOT GLOB 'custom/client/*' {change}
            AND EXISTS(SELECT 1 FROM custom_registrations WHERE kind={record}.kind AND hash={record}.registration AND json_type(manifest,'$.attention')='object')
            BEGIN INSERT INTO local_attention_history_dirty VALUES({record}.subject) ON CONFLICT DO NOTHING; END;", event.to_lowercase()))?;
    }
    connection.execute_batch(&format!("CREATE TRIGGER attention_history_batch_update AFTER UPDATE OF origin,replica_sequence ON batches
        WHEN (SELECT enabled FROM local_attention_history_state WHERE id=1)=1 AND (NEW.origin IS NOT OLD.origin OR NEW.replica_sequence IS NOT OLD.replica_sequence)
        BEGIN INSERT INTO local_attention_history_dirty SELECT subject FROM claims WHERE batch_id=NEW.id
        AND (kind IN ({kinds}) OR (subject GLOB 'custom/*' AND subject NOT GLOB 'custom/client/*' AND EXISTS(SELECT 1 FROM custom_sources s JOIN custom_registrations r ON r.kind=s.kind AND r.hash=s.registration WHERE s.subject=claims.subject AND json_type(r.manifest,'$.attention')='object'))) ON CONFLICT DO NOTHING; END;"))?;
    for (event, ids, change) in [
        (
            "INSERT",
            "NEW.claim_id",
            "NEW.state='repaired' AND NEW.claim_id IS NOT NULL",
        ),
        (
            "DELETE",
            "OLD.claim_id",
            "OLD.state='repaired' AND OLD.claim_id IS NOT NULL",
        ),
        (
            "UPDATE OF claim_id,state",
            "OLD.claim_id,NEW.claim_id",
            "(NEW.state='repaired' OR OLD.state='repaired') AND (NEW.claim_id IS NOT OLD.claim_id OR NEW.state IS NOT OLD.state)",
        ),
    ] {
        let name = event
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase();
        connection.execute_batch(&format!("CREATE TRIGGER attention_history_record_{name} AFTER {event} ON replica_records
            WHEN {change} AND (SELECT enabled FROM local_attention_history_state WHERE id=1)=1 BEGIN INSERT INTO local_attention_history_dirty SELECT subject FROM claims WHERE id IN ({ids})
            AND (kind IN ({kinds}) OR (subject GLOB 'custom/*' AND subject NOT GLOB 'custom/client/*' AND EXISTS(SELECT 1 FROM custom_sources s JOIN custom_registrations r ON r.kind=s.kind AND r.hash=s.registration WHERE s.subject=claims.subject AND json_type(r.manifest,'$.attention')='object'))) ON CONFLICT DO NOTHING; END;"))?;
    }
    Ok(())
}

pub(super) fn open(tx: &Transaction<'_>) -> Result<()> {
    // Constant work, no source reads. Existing progress resumes in the daemon worker.
    tx.execute(
        "INSERT OR IGNORE INTO local_attention_history_state(id) VALUES(1)",
        [],
    )?;
    Ok(())
}

pub(super) fn invalidate(tx: &Transaction<'_>) -> Result<()> {
    // Old rows are invisible immediately and are removed in bounded background pages.
    tx.execute(
        "UPDATE local_attention_history_state SET epoch=epoch+1,version=version+1,
        backfill_after=0,backfill_done=0,degraded=0,indexed_through=0 WHERE id=1",
        [],
    )?;
    Ok(())
}

#[derive(Clone, Debug, serde::Deserialize, Serialize, PartialEq)]
pub(crate) struct HistorySnapshot {
    pub epoch: u64,
    pub version: u64,
    pub source_index: u64,
}

fn snapshot(connection: &Connection, source_index: u64) -> Result<HistorySnapshot> {
    connection
        .query_row(
            "SELECT epoch,version FROM local_attention_history_state WHERE id=1",
            [],
            |r| {
                Ok(HistorySnapshot {
                    epoch: r.get(0)?,
                    version: r.get(1)?,
                    source_index,
                })
            },
        )
        .map_err(Into::into)
}

fn availability(connection: &Connection) -> Result<st3_client::AttentionHistoryAvailability> {
    let (done, degraded, dirty): (bool,bool,bool) = connection.query_row(
        "SELECT backfill_done,degraded,EXISTS(SELECT 1 FROM local_attention_history_dirty) FROM local_attention_history_state WHERE id=1", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let note = if degraded {
        "Recorded history is incomplete because some sources could not be indexed."
    } else if !done || dirty {
        "Recorded history is still indexing; some closed items are not available yet."
    } else {
        "Recorded closures only; login and some custom or implicit closures are unavailable."
    };
    Ok(st3_client::AttentionHistoryAvailability {
        complete: false,
        since: None,
        note: Some(note.into()),
    })
}

fn claims(connection: &Connection, subject: &str) -> Result<Vec<ClaimRecord>> {
    connection.prepare_cached(&canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
         FROM claims WHERE subject=?1 AND NOT EXISTS (SELECT 1 FROM replica_records
         WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')
         AND (kind IN ('work.person-asked','work.person-done','work.person-cancelled','gate.requested','gate.result')
         OR kind LIKE 'planning-session.%' OR kind LIKE 'revision-proposal.%' OR json_type(body,'$._custom_reply')='object')
         ORDER BY CANONICAL_ASC(claims)"))?
        .query_map([subject], claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub(super) fn resource(
    subject: &str,
    person: &str,
    episode: &str,
    kind: &str,
    title: &str,
    detail: &str,
    at: u128,
) -> Result<Value> {
    Ok(json!({
        "id": crate::api::client_attention_id(subject,person,episode)?, "kind":"attention",
        "source_id":subject,"source_kind":kind,"attention_kind":kind,"person_id":person,
        "episode":episode,"revision":episode,"title":title,"detail":detail,"priority":"normal",
        "state":"resolved","requested_at":crate::api::client_timestamp(at),
        "updated_at":crate::api::client_timestamp(at),"targets":[subject],"actions":[],
        "operational":{"layer":"history","actionable":false,"reasons":["resolved"]}
    }))
}

pub(super) fn resolve(row: &mut Value, claim: &ClaimRecord, kind: &str, label: Option<&str>) {
    let fields = &claim.body["fields"];
    let mut resolution =
        json!({"kind":kind,"at":crate::api::client_timestamp(claim.accepted_at_unix_ms)});
    if let Some(actor) = &claim.actor {
        resolution["by"] = json!(actor);
    }
    if let Some(reason) = fields["reason"]
        .as_str()
        .or_else(|| fields["summary"].as_str())
        .filter(|s| !s.is_empty())
    {
        resolution["reason"] = json!(reason);
    }
    if let Some(label) = label.filter(|s| !s.is_empty()) {
        resolution["answer_label"] = json!(label);
    }
    row["updated_at"] = resolution["at"].clone();
    row["resolution"] = resolution;
}

fn person_rows(connection: &Connection, records: &[ClaimRecord]) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    // The person-work source consumes its canonical first request for a subject. Public
    // ask keys reuse that request even after closure; a new key/attempt creates a new subject.
    let ask = records.iter().find(|c| c.kind == "work.person-asked");
    let mut episodes = BTreeSet::new();
    for closure in records.iter().filter(|c| {
        matches!(
            c.kind.as_str(),
            "work.person-done" | "work.person-cancelled"
        )
    }) {
        let f = &closure.body["fields"];
        let episode = f["episode"].as_str().or_else(|| ask.map(|a| a.id.as_str()));
        let Some(episode) = episode else { continue };
        if episodes.contains(episode) {
            continue;
        }
        if let Some(ask) = ask {
            if episode != ask.id || f["attempt"] != ask.body["fields"]["attempt"] {
                continue;
            }
            let a = &ask.body["fields"];
            let Some(person) = a["person"].as_str() else {
                continue;
            };
            // Match the source's authority, including exact delegated episode fences.
            let done = closure.kind == "work.person-done";
            let delegated = closure
                .actor
                .as_deref()
                .is_some_and(|a| a.starts_with("agent/"))
                && f["acted_for"] == person
                && f["delegation"]["episode"] == episode;
            if done && closure.actor.as_deref() != Some(person) && !delegated {
                continue;
            }
            if !done
                && closure.actor != ask.actor
                && closure.actor.as_deref() != Some("daemon/runtime")
            {
                continue;
            }
            episodes.insert(episode.to_owned());
            let since = a["waiting_since"]
                .as_str()
                .and_then(|v| v.parse().ok())
                .unwrap_or(ask.accepted_at_unix_ms);
            let mut row = resource(
                &ask.subject,
                person,
                episode,
                "person-step",
                a["title"].as_str().unwrap_or("A step needs your response"),
                a["reason"].as_str().unwrap_or_default(),
                since,
            )?;
            row["requester_id"] = json!(ask.actor);
            row["mission_run_id"] = a["run"].clone();
            row["step_run_id"] = json!(ask.subject);
            if a["request"].is_object() {
                let field = if a["request"]["type"] == "update" {
                    "update"
                } else {
                    "request"
                };
                row[field] = a["request"].clone();
            }
            if let Some(view) = person_work::step(connection, &ask.subject)? {
                add_work_context(connection, &view, &mut row)?;
            }
            if let Some(origin) = a["origin_step"].as_str()
                && let Some(blocked) = person_work::step(connection, origin)?
            {
                row["blocked"] = json!({"step_run_id":origin,"step":blocked.step,"goal":blocked.goals.join("\n"),"attempt":a["origin_attempt"]});
            }
            let kind = if done {
                if a["request"]["type"] == "update" {
                    "closed"
                } else {
                    "answered"
                }
            } else if closure.actor == ask.actor {
                "withdrawn"
            } else {
                "cancelled"
            };
            // A named label is resolved by the source when accepting the answer. Free text is its summary.
            let label = done
                .then(|| {
                    f["answer"]["label"]
                        .as_str()
                        .or_else(|| f["summary"].as_str())
                })
                .flatten();
            resolve(&mut row, closure, kind, label);
            rows.push(row);
        } else if let Some(view) = person_work::step(connection, &closure.subject)? {
            let Some(person) = closure
                .actor
                .as_deref()
                .filter(|p| p.starts_with("person/"))
            else {
                continue;
            };
            episodes.insert(episode.to_owned());
            let mut row = resource(
                &view.subject,
                person,
                episode,
                "person-step",
                view.title
                    .as_deref()
                    .unwrap_or("A step needs your response"),
                &view.goals.join("\n"),
                view.created_at_unix_ms,
            )?;
            add_work_context(connection, &view, &mut row)?;
            resolve(&mut row, closure, "answered", f["summary"].as_str());
            rows.push(row);
        }
    }
    Ok(rows)
}

fn add_work_context(connection: &Connection, view: &StepRunView, row: &mut Value) -> Result<()> {
    row["mission_run_id"] = json!(view.run);
    row["step_run_id"] = json!(view.subject);
    let mission: Option<String> = connection
        .query_row(
            "SELECT mission_id FROM mission_runs WHERE id=?1",
            [view.run.trim_start_matches("mission-run/")],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(mission) = mission {
        row["mission_id"] = json!(format!(
            "mission/{}",
            mission.trim_start_matches("mission/")
        ));
    }
    Ok(())
}

fn gate_rows(connection: &Connection, records: &[ClaimRecord]) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for request in records.iter().filter(|c| c.kind == "gate.requested") {
        let f = &request.body["fields"];
        let (Some(person), Some(owner)) = (f["reviewer"].as_str(), f["owner"].as_str()) else {
            continue;
        };
        let Some(answer) = human_review_answer_tx(connection, &request.id)? else {
            continue;
        };
        let step = if owner.starts_with("step-run/") {
            person_work::step(connection, owner)?
        } else {
            None
        };
        let mut row = resource(
            owner,
            person,
            &request.id,
            "human-gate",
            step.as_ref()
                .map(|s| s.title.as_deref().unwrap_or(&s.step))
                .unwrap_or("Human review"),
            f["question"].as_str().unwrap_or("Approve this work?"),
            request.accepted_at_unix_ms,
        )?;
        row["review_mode"] = f["mode"].clone();
        row["targets"] = f["review_targets"].clone();
        if let Some(step) = &step {
            add_work_context(connection, step, &mut row)?;
        } else if owner.starts_with("mission-run/") {
            row["mission_run_id"] = json!(owner);
            if let Some(run) =
                mission_run_header_tx(connection, owner.trim_start_matches("mission-run/"))
                    .optional()?
            {
                row["mission_id"] = json!(run.mission);
            }
        }
        let label = answer.body["fields"]["decision"]
            .as_str()
            .or_else(|| answer.body["fields"]["verdict"].as_str());
        resolve(&mut row, &answer, "answered", label);
        rows.push(row);
    }
    Ok(rows)
}

fn planning_rows(records: &[ClaimRecord]) -> Result<Vec<Value>> {
    let Some(start) = records
        .iter()
        .find(|c| c.kind == "planning-session.started")
    else {
        return Ok(Vec::new());
    };
    let s = &start.body["fields"];
    let Some(person) = s["requester"]
        .as_str()
        .or(start.actor.as_deref())
        .filter(|p| p.starts_with("person/"))
    else {
        return Ok(Vec::new());
    };
    let mut candidates = BTreeMap::<String, &ClaimRecord>::new();
    let mut waiting = BTreeMap::<String, Value>::new();
    let mut rows = Vec::new();
    for claim in records {
        let f = &claim.body["fields"];
        let variant = f["variant"].as_str().unwrap_or("default");
        match claim.kind.as_str() {
            "planning-session.candidate-submitted" => {
                if let Some(mut prior) = waiting.remove(variant) {
                    resolve(&mut prior, claim, "closed", None);
                    rows.push(prior);
                }
                candidates.insert(variant.into(), claim);
            }
            "planning-session.previewed" => {
                let Some(candidate) = candidates.get(variant) else {
                    continue;
                };
                let c = &candidate.body["fields"];
                if f["candidate_revision"] != c["candidate_revision"]
                    && f["candidate_revision"] != c["revision"]
                {
                    continue;
                }
                if f["mission"]["blockers"]
                    .as_array()
                    .is_none_or(|a| !a.is_empty())
                {
                    continue;
                }
                let Some(hash) = f["preview_hash"].as_str() else {
                    continue;
                };
                if waiting.get(variant).is_some_and(|r| r["episode"] == hash) {
                    continue;
                }
                if let Some(mut prior) = waiting.remove(variant) {
                    resolve(&mut prior, claim, "closed", None);
                    rows.push(prior);
                }
                let mission = s["mission"].as_str().unwrap_or_default();
                let mut row = resource(
                    &start.subject,
                    person,
                    hash,
                    "launch-approval",
                    &format!("Approve {mission}"),
                    "The current launch preview is ready for approval.",
                    claim.accepted_at_unix_ms,
                )?;
                row["requester_id"] = json!(person);
                row["launch_id"] = json!(format!(
                    "launch/{}",
                    start.subject.trim_start_matches("planning-session/")
                ));
                row["variant_id"] = json!(format!(
                    "launch-variant/{}/{variant}",
                    start.subject.trim_start_matches("planning-session/")
                ));
                row["mission_id"] = json!(mission);
                if s["target_run"].is_string() {
                    row["mission_run_id"] = s["target_run"].clone();
                }
                row["targets"] = json!([s["request"], c["markdown"], c["kdl"]]);
                waiting.insert(variant.into(), row);
            }
            "planning-session.approved"
            | "planning-session.cancelled"
            | "planning-session.revision-requested" => {
                let kind = if claim.kind == "planning-session.cancelled" {
                    "cancelled"
                } else {
                    "answered"
                };
                let label = match claim.kind.as_str() {
                    "planning-session.approved" => Some("Approved"),
                    "planning-session.revision-requested" => Some("Changes requested"),
                    _ => None,
                };
                for (_, mut row) in std::mem::take(&mut waiting) {
                    let selected = claim.kind != "planning-session.approved"
                        || row["episode"] == f["preview_hash"];
                    resolve(
                        &mut row,
                        claim,
                        if selected { kind } else { "closed" },
                        if selected { label } else { None },
                    );
                    rows.push(row);
                }
            }
            _ => {}
        }
    }
    Ok(rows)
}

fn revision_rows(connection: &Connection, records: &[ClaimRecord]) -> Result<Vec<Value>> {
    let Some(start) = records
        .iter()
        .find(|c| c.kind == "revision-proposal.created")
    else {
        return Ok(Vec::new());
    };
    let f = &start.body["fields"];
    if f["status"] != "pending-approval" {
        return Ok(Vec::new());
    }
    let (Some(run), Some(generation), Some(hash)) = (
        f["run"].as_str(),
        f["source_generation"].as_str(),
        f["preview_hash"].as_str(),
    ) else {
        return Ok(Vec::new());
    };
    let header =
        mission_run_header_tx(connection, run.trim_start_matches("mission-run/")).optional()?;
    let mission = header
        .as_ref()
        .map(|r| r.mission.as_str())
        .unwrap_or("mission/unknown");
    let mut rows = Vec::new();
    for person in f["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let closure = records.iter().find(|c| {
            c.kind == "revision-proposal.cancelled"
                || (c.kind == "revision-proposal.approved"
                    && c.actor.as_deref() == Some(person)
                    && c.body["fields"]["preview_hash"] == hash)
        });
        let Some(closure) = closure else { continue };
        let mut row = resource(
            &start.subject,
            person,
            &format!("{generation}:{hash}"),
            "revision-approval",
            &format!("Approve a revision of {mission}"),
            f["reason"].as_str().unwrap_or_default(),
            start.accepted_at_unix_ms,
        )?;
        if header.is_some() {
            row["mission_id"] = json!(mission);
        }
        row["mission_run_id"] = json!(run);
        row["targets"] = json!([format!(
            "{mission}@{}",
            f["candidate_revision"].as_str().unwrap_or_default()
        )]);
        let approved = closure.kind == "revision-proposal.approved";
        resolve(
            &mut row,
            closure,
            if approved {
                "answered"
            } else if closure.actor == start.actor {
                "withdrawn"
            } else {
                "cancelled"
            },
            approved.then_some("Approved"),
        );
        rows.push(row);
    }
    Ok(rows)
}

#[derive(Debug, Default)]
pub(crate) struct Maintenance {
    pub examined: usize,
    pub processed: usize,
    pub more: bool,
}

struct IndexedRow {
    id: String,
    person: String,
    sort_ms: i64,
    body: String,
}

struct PreparedPage {
    data_version: u64,
    at: HistorySnapshot,
    backfill: Option<(u64, bool)>,
    discovered: BTreeSet<String>,
    sources: Vec<(String, std::result::Result<Vec<IndexedRow>, String>)>,
    examined: usize,
}

fn custom_has_attention(connection: &Connection, subject: &str) -> Result<bool> {
    if !subject.starts_with("custom/") || subject.starts_with("custom/client/") {
        return Ok(false);
    }
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM custom_sources s JOIN custom_registrations r
        ON r.kind=s.kind AND r.hash=s.registration WHERE s.subject=?1 AND json_type(r.manifest,'$.attention')='object')",
        [subject], |r| r.get(0))?)
}

/// Reconstruct against one read snapshot, without holding SQLite's write lock. Capture
/// data_version BEFORE opening this transaction: any intervening commit invalidates the
/// entire plan, including admission metadata changes with an unchanged claim frontier.
fn prepare_page(tx: &Transaction<'_>, data_version: u64) -> Result<PreparedPage> {
    let (after, done): (u64, bool) = tx.query_row(
        "SELECT backfill_after,backfill_done FROM local_attention_history_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let source_index =
        tx.query_row("SELECT COALESCE(MAX(store_index),0) FROM claims", [], |r| {
            r.get(0)
        })?;
    let at = snapshot(tx, source_index)?;
    let mut discovered = BTreeSet::new();
    let mut examined = 0;
    let backfill = if !done {
        let candidates = tx.prepare_cached("SELECT store_index,subject,kind FROM claims WHERE store_index>?1 ORDER BY store_index LIMIT ?2")?
            .query_map(params![after,CLAIM_PAGE],|r| Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        examined = candidates.len();
        for (_, subject, kind) in &candidates {
            if KINDS.contains(&kind.as_str()) || custom_has_attention(tx, subject)? {
                let quarantined: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_attention_history_errors WHERE epoch=?1 AND subject=?2)",
                    params![at.epoch,subject],|r|r.get(0))?;
                if !quarantined {
                    discovered.insert(subject.clone());
                }
            }
        }
        Some((
            candidates.last().map(|c| c.0).unwrap_or(after),
            examined < CLAIM_PAGE,
        ))
    } else {
        None
    };
    let mut subjects = tx
        .prepare_cached(
            "SELECT subject FROM local_attention_history_dirty ORDER BY subject LIMIT ?1",
        )?
        .query_map([DIRTY_PAGE], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    subjects.extend(discovered.iter().cloned());
    let mut sources = Vec::new();
    for subject in subjects.into_iter().take(DIRTY_PAGE) {
        let deadline = Instant::now() + SOURCE_BUDGET;
        tx.progress_handler(1000, Some(move || Instant::now() >= deadline));
        let rows = reconstruct(tx, &subject, deadline).map_err(|e| e.to_string());
        tx.progress_handler(0, None::<fn() -> bool>);
        sources.push((subject, rows));
    }
    Ok(PreparedPage {
        data_version,
        at,
        backfill,
        discovered,
        sources,
        examined,
    })
}

fn reconstruct(
    connection: &Connection,
    subject: &str,
    deadline: Instant,
) -> Result<Vec<IndexedRow>> {
    // Count via the existing subject index, stopping before decoding/sorting any bodies.
    let count: usize = connection.query_row(
        "SELECT COUNT(*) FROM (SELECT store_index FROM claims WHERE subject=?1 LIMIT ?2)",
        params![subject, SOURCE_CLAIMS + 1],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        count <= SOURCE_CLAIMS,
        "history source exceeds {SOURCE_CLAIMS} retained claims"
    );
    let records = claims(connection, subject)?;
    let mut rows = person_rows(connection, &records)?;
    rows.extend(gate_rows(connection, &records)?);
    rows.extend(planning_rows(&records)?);
    rows.extend(revision_rows(connection, &records)?);
    rows.extend(custom::attention_history(connection, subject, deadline)?);
    anyhow::ensure!(
        rows.len() <= SOURCE_ROWS,
        "history source exceeds {SOURCE_ROWS} rows"
    );
    let mut kept = BTreeSet::new();
    let mut indexed = Vec::new();
    for row in rows {
        anyhow::ensure!(
            Instant::now() < deadline,
            "history source exceeded reconstruction deadline"
        );
        let id = row["id"]
            .as_str()
            .context("history row has no ID")?
            .to_owned();
        if !kept.insert(id.clone()) {
            continue;
        }
        let sort = row["resolution"]["at"]
            .as_str()
            .unwrap_or(row["requested_at"].as_str().unwrap_or_default());
        indexed.push(IndexedRow {
            id,
            person: row["person_id"]
                .as_str()
                .context("history row has no person")?
                .to_owned(),
            sort_ms: chrono::DateTime::parse_from_rfc3339(sort)?.timestamp_millis(),
            body: canonical_json_text(&row)?,
        });
    }
    anyhow::ensure!(
        Instant::now() < deadline,
        "history source exceeded reconstruction deadline"
    );
    Ok(indexed)
}

#[cfg(test)]
fn publish_page(tx: &Transaction<'_>, page: PreparedPage) -> Result<Maintenance> {
    publish_page_until(tx, page, Instant::now() + PUBLISH_BUDGET)
}

fn publication_deadline(deadline: Instant) -> Result<()> {
    anyhow::ensure!(
        Instant::now() < deadline,
        "history publication deadline exceeded"
    );
    Ok(())
}

fn publish_page_until(
    tx: &Transaction<'_>,
    page: PreparedPage,
    deadline: Instant,
) -> Result<Maintenance> {
    publication_deadline(deadline)?;
    let data_version: u64 = tx.query_row("PRAGMA data_version", [], |r| r.get(0))?;
    let mut at = snapshot(tx, page.at.source_index)?;
    if data_version != page.data_version || at != page.at {
        // Leave progress and dirty entries untouched. A subsequent read computes fresh rows.
        return Ok(Maintenance {
            more: true,
            ..Maintenance::default()
        });
    }
    let mut result = Maintenance {
        examined: page.examined,
        ..Maintenance::default()
    };
    for subject in page.discovered {
        publication_deadline(deadline)?;
        tx.execute(
            "INSERT INTO local_attention_history_dirty VALUES(?1) ON CONFLICT DO NOTHING",
            [subject],
        )?;
    }
    if let Some((through, done)) = page.backfill {
        tx.execute("UPDATE local_attention_history_state SET backfill_after=?1,backfill_done=?2 WHERE id=1", params![through,done])?;
    }
    if !page.sources.is_empty() {
        at.version += 1;
    }
    tx.execute(
        "UPDATE local_attention_history_state SET version=?1,indexed_through=?2 WHERE id=1",
        params![at.version, at.source_index],
    )?;
    for (subject, rows) in page.sources {
        publication_deadline(deadline)?;
        tx.execute_batch("SAVEPOINT history_subject")?;
        let published = match rows {
            Ok(rows) => publish_source(tx, &subject, &at, rows, deadline),
            Err(error) => Err(anyhow::anyhow!(error)),
        };
        match published {
            Ok(()) => {
                tx.execute_batch("RELEASE history_subject")?;
                tx.execute(
                    "DELETE FROM local_attention_history_errors WHERE epoch=?1 AND subject=?2",
                    params![at.epoch, subject],
                )?;
            }
            Err(error) => {
                // SQLite may already have rolled back the entire transaction on interrupt.
                // Preserve that error; the outer worker clears the handler and retries safely.
                if error.downcast_ref::<rusqlite::Error>().is_some_and(|e| matches!(e,
                    rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::OperationInterrupted)) {
                    return Err(error);
                }
                tx.execute_batch("ROLLBACK TO history_subject; RELEASE history_subject")?;
                tx.execute("INSERT INTO local_attention_history_errors VALUES(?1,?2,?3) ON CONFLICT(epoch,subject) DO UPDATE SET error=excluded.error",
                    params![at.epoch,subject,error.to_string()])?;
                tx.execute("UPDATE local_attention_history_v2 SET valid_to=?1 WHERE epoch=?2 AND source=?3 AND valid_to IS NULL",params![at.version,at.epoch,subject])?;
            }
        }
        tx.execute(
            "DELETE FROM local_attention_history_dirty WHERE subject=?1",
            [subject],
        )?;
        result.processed += 1;
    }
    tx.execute("UPDATE local_attention_history_state SET degraded=EXISTS(
        SELECT 1 FROM local_attention_history_errors WHERE epoch=local_attention_history_state.epoch) WHERE id=1", [])?;
    tx.execute(
        "DELETE FROM local_attention_history_v2 WHERE (id,valid_from) IN (
        SELECT id,valid_from FROM local_attention_history_v2 WHERE epoch<?1 LIMIT ?2)",
        params![at.epoch, CLAIM_PAGE],
    )?;
    result.more = tx.query_row("SELECT NOT backfill_done OR EXISTS(SELECT 1 FROM local_attention_history_dirty)
        OR EXISTS(SELECT 1 FROM local_attention_history_v2 WHERE epoch<local_attention_history_state.epoch)
        FROM local_attention_history_state WHERE id=1",[],|r|r.get(0))?;
    Ok(result)
}

fn publish_source(
    tx: &Transaction<'_>,
    subject: &str,
    at: &HistorySnapshot,
    rows: Vec<IndexedRow>,
    deadline: Instant,
) -> Result<()> {
    let current = tx.prepare_cached("SELECT id,body FROM local_attention_history_v2 WHERE epoch=?1 AND source=?2 AND valid_to IS NULL")?
        .query_map(params![at.epoch,subject],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<_,_>>>()?;
    let mut kept = BTreeSet::new();
    for row in rows {
        publication_deadline(deadline)?;
        kept.insert(row.id.clone());
        if current.get(&row.id).is_some_and(|prior| prior == &row.body) {
            continue;
        }
        retire(tx, &row.id, at.version)?;
        tx.execute(
            "INSERT INTO local_attention_history_v2 VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,?8)",
            params![
                row.id,
                subject,
                row.person,
                row.sort_ms,
                at.version,
                row.body,
                at.epoch,
                at.source_index
            ],
        )?;
    }
    for id in current.keys().filter(|id| !kept.contains(*id)) {
        publication_deadline(deadline)?;
        retire(tx, id, at.version)?;
    }
    Ok(())
}

fn maintain(connection: &mut Connection) -> Result<Maintenance> {
    maintain_with_budget(connection, PUBLISH_BUDGET)
}

fn maintain_with_budget(connection: &mut Connection, budget: Duration) -> Result<Maintenance> {
    let data_version = connection.query_row("PRAGMA data_version", [], |r| r.get(0))?;
    let tx = connection.transaction()?;
    let page = prepare_page(&tx, data_version)?;
    tx.commit()?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let deadline = Instant::now() + budget;
    tx.progress_handler(100, Some(move || Instant::now() >= deadline));
    let work = publish_page_until(&tx, page, deadline);
    // An interrupt aborts the local transaction; remove the deadline before rollback/commit.
    tx.progress_handler(0, None::<fn() -> bool>);
    let work = work?;
    publication_deadline(deadline)?;
    tx.commit()?;
    Ok(work)
}

fn retire(tx: &Transaction<'_>, id: &str, version: u64) -> Result<()> {
    tx.execute(
        "DELETE FROM local_attention_history_v2 WHERE id=?1 AND valid_from=?2",
        params![id, version],
    )?;
    tx.execute(
        "UPDATE local_attention_history_v2 SET valid_to=?2 WHERE id=?1 AND valid_to IS NULL",
        params![id, version],
    )?;
    Ok(())
}

pub(crate) type HistoryKey = (i64, String, u64);
#[derive(Debug)]
pub(crate) struct AttentionHistoryPage {
    pub items: Vec<Value>,
    pub keys: Vec<HistoryKey>,
    pub next: Option<HistoryKey>,
    pub snapshot: HistorySnapshot,
    pub availability: st3_client::AttentionHistoryAvailability,
}

impl Store {
    /// History-only writes use a separate connection: they must not signal a canonical
    /// store commit or invalidate open stream windows. Reconstruction uses a read snapshot; publication
    /// takes a short write transaction and discards plans invalidated by intervening commits.
    fn history_connection(&self) -> Result<Connection> {
        let mut connection = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        smallclaims::sqlite::observe(&mut connection);
        connection.busy_timeout(Duration::from_millis(100))?;
        connection.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
            use rusqlite::hooks::{AuthAction, Authorization};
            match context.action {
                AuthAction::Insert { table_name }
                | AuthAction::Delete { table_name }
                | AuthAction::Update { table_name, .. }
                    if table_name.starts_with("local_attention_history_") =>
                {
                    Authorization::Allow
                }
                AuthAction::Read { .. }
                | AuthAction::Select
                | AuthAction::Function { .. }
                | AuthAction::Recursive
                | AuthAction::Transaction { .. }
                | AuthAction::Savepoint { .. } => Authorization::Allow,
                AuthAction::Pragma {
                    pragma_name,
                    pragma_value: None,
                } if pragma_name.eq_ignore_ascii_case("data_version") => Authorization::Allow,
                _ => Authorization::Deny,
            }
        }));
        Ok(connection)
    }

    /// Start only on an authorized explicit history read, then resume after process restarts.
    pub(crate) fn request_attention_history(&self) -> Result<()> {
        let enabled: bool = self.readers.get().query_row(
            "SELECT enabled FROM local_attention_history_state WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        if !enabled {
            let connection = self.history_connection()?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.execute(
                "UPDATE local_attention_history_state SET enabled=1 WHERE id=1 AND enabled=0",
                [],
            )?;
        }
        Ok(())
    }

    fn maintain_history_connection(
        &self,
        connection: &mut Option<Connection>,
    ) -> Result<Maintenance> {
        let needed: bool = self.readers.get().query_row(
            "SELECT enabled AND (NOT backfill_done OR EXISTS(SELECT 1 FROM local_attention_history_dirty)
            OR EXISTS(SELECT 1 FROM local_attention_history_v2 WHERE epoch<local_attention_history_state.epoch))
            FROM local_attention_history_state WHERE id=1", [], |r|r.get(0))?;
        if !needed {
            return Ok(Maintenance::default());
        }
        if connection.is_none() {
            *connection = Some(self.history_connection()?);
        }
        maintain(connection.as_mut().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn maintain_attention_history(&self) -> Result<Maintenance> {
        self.maintain_history_connection(&mut None)
    }

    /// The daemon owns this task. Each read reconstructs at most DIRTY_PAGE capped sources.
    /// Brief, deadline-fenced publication never reconstructs or aborts a canonical claim write.
    pub async fn run_attention_history(self: Arc<Self>) {
        let connection = Arc::new(Mutex::new(None));
        loop {
            let store = self.clone();
            let connection = connection.clone();
            let delay = match tokio::task::spawn_blocking(move || {
                store.maintain_history_connection(
                    &mut connection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
            })
            .await
            {
                Ok(Ok(work)) if work.more => Duration::from_millis(100),
                Ok(Ok(_)) => Duration::from_secs(1),
                result => {
                    tracing::warn!(?result, "attention history maintenance failed");
                    Duration::from_secs(5)
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_attention_history(&self) {
        self.request_attention_history().unwrap();
        let mut connection = None;
        for _ in 0..10_000 {
            if !self
                .maintain_history_connection(&mut connection)
                .unwrap()
                .more
            {
                return;
            }
        }
        panic!("history maintenance did not converge");
    }

    #[cfg(test)]
    pub(crate) fn attention_history_test_page(
        &self,
        person: &str,
        limit: usize,
    ) -> Result<AttentionHistoryPage> {
        self.drain_attention_history();
        self.attention_history_page(person, self.index()?, None, limit, None)
    }

    pub(crate) fn attention_history_item(
        &self,
        id: &str,
        person: &str,
        source_index: u64,
    ) -> Result<Option<Value>> {
        let connection = self.readers.get();
        let tx = connection.unchecked_transaction()?;
        let at = snapshot(&tx, source_index)?;
        let body:Option<String> = tx.query_row("SELECT body FROM local_attention_history_v2 WHERE epoch=?1 AND id=?2 AND person=?3
            AND valid_from<=?4 AND (valid_to IS NULL OR valid_to>?4) AND source_index<=?5 ORDER BY valid_from DESC LIMIT 1",
            params![at.epoch,id,person,at.version,source_index],|r|r.get(0)).optional()?;
        body.map(|body| serde_json::from_str(&body).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn attention_history_page(
        &self,
        person: &str,
        source_index: u64,
        pinned: Option<&HistorySnapshot>,
        limit: usize,
        after: Option<&HistoryKey>,
    ) -> Result<AttentionHistoryPage> {
        let connection = self.readers.get();
        let tx = connection.unchecked_transaction()?;
        let current = snapshot(&tx, source_index)?;
        let at = pinned.unwrap_or(&current);
        anyhow::ensure!(
            at.epoch == current.epoch && at.version <= current.version,
            "attention-history-epoch-changed"
        );
        let seek = if after.is_some() {
            "AND (sort_ms,id,valid_from)<(?3,?4,?5)"
        } else {
            ""
        };
        let sql = format!("SELECT sort_ms,id,valid_from,valid_to,body,source_index FROM local_attention_history_v2 INDEXED BY local_attention_history_person_v2
            WHERE epoch=?1 AND person=?2 {seek} ORDER BY sort_ms DESC,id DESC,valid_from DESC LIMIT ?6");
        let (ms, id, version) =
            after
                .cloned()
                .unwrap_or((i64::MAX, String::new(), i64::MAX as u64));
        let budget = limit.saturating_mul(2).max(32);
        let candidates = tx
            .prepare_cached(&sql)?
            .query_map(
                params![at.epoch, person, ms, id, version, budget + 1],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u64>(2)?,
                        r.get::<_, Option<u64>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, u64>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = Vec::new();
        let mut keys = Vec::new();
        let mut next = None;
        for (position, (ms, id, from, to, body, source)) in
            candidates.iter().enumerate().take(budget)
        {
            next = Some((*ms, id.clone(), *from));
            if *from <= at.version
                && to.is_none_or(|to| at.version < to)
                && *source <= at.source_index
            {
                items.push(serde_json::from_str(body)?);
                keys.push((*ms, id.clone(), *from));
            }
            if items.len() == limit {
                if position + 1 == candidates.len() {
                    next = None;
                }
                break;
            }
            if position + 1 == candidates.len() {
                next = None;
            }
        }
        Ok(AttentionHistoryPage {
            items,
            keys,
            next,
            snapshot: at.clone(),
            availability: availability(&tx)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PersonAskRequest, PersonStepResponse};
    use std::sync::atomic::AtomicUsize;

    fn response(subject: &str, actor: &str, summary: &str) -> PersonStepResponse {
        PersonStepResponse {
            delegation: None,
            subject: subject.into(),
            actor: actor.into(),
            summary: summary.into(),
            evidence: vec![],
            episode: None,
            idempotency_key: format!("{subject}:{actor}:{summary}"),
            answer: None,
        }
    }
    fn history(store: &Store) -> Vec<Value> {
        store
            .attention_history_test_page("person/avery", 100)
            .unwrap()
            .items
    }

    fn claim(
        store: &Store,
        subject: &str,
        kind: &str,
        actor: Option<&str>,
        fields: Value,
        key: &str,
    ) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: actor.map(str::to_owned),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    }

    #[test]
    fn human_gate_first_answer_and_reopened_episode_history_follow_source_rules() {
        let (store, origin, _) = person_work::tests::fixture();
        let run = store.mission_run(&origin.run).unwrap().unwrap();
        let step = run.steps.iter().find(|s| s.step == "review").unwrap();
        let ask = |key: &str| {
            claim(
                &store,
                if key == "history-gate-reopened" {
                    "gate-operation/history-reopened"
                } else {
                    "gate-operation/history"
                },
                "gate.requested",
                None,
                json!({
            "owner":step.subject,"reviewer":"person/avery","question":"Approve the release?","mode":"approve",
            "review_targets":[step.subject],"decisions":["approved","rejected"],"operation":"gate-operation/history",
            "mission_revision":run.revision,"step_definition":step.definition_hash,"attempt":step.attempt}),
                key,
            )
        };
        let first = ask("history-gate-first");
        let open = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|r| r.kind == "human-gate")
            .unwrap();
        claim(
            &store,
            "gate-operation/history",
            "gate.result",
            Some("person/other"),
            json!({"request":first.id,"verdict":"pass","decision":"approved"}),
            "wrong-reviewer",
        );
        assert!(history(&store).is_empty());
        let answered = claim(
            &store,
            "gate-operation/history",
            "gate.result",
            Some("person/avery"),
            json!({"request":first.id,"verdict":"fail","decision":"rejected","reason":"Needs another test"}),
            "history-gate-answer",
        );
        let rows = history(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["id"],
            crate::api::client_attention_id(&open.subject, &open.person, &open.episode).unwrap()
        );
        assert_eq!(rows[0]["resolution"]["answer_label"], "rejected");
        assert_eq!(
            rows[0]["resolution"]["at"],
            crate::api::client_timestamp(answered.accepted_at_unix_ms)
        );
        claim(
            &store,
            "gate-operation/history",
            "gate.result",
            Some("person/avery"),
            json!({"request":first.id,"verdict":"pass","decision":"approved"}),
            "later-racing-answer",
        );
        assert_eq!(
            history(&store),
            rows,
            "the source consumes its canonical first answer"
        );
        let reopened = ask("history-gate-reopened");
        assert_ne!(reopened.id, first.id);
        claim(
            &store,
            "gate-operation/history-reopened",
            "gate.result",
            Some("person/avery"),
            json!({"request":reopened.id,"verdict":"pass","decision":"approved"}),
            "history-gate-second-answer",
        );
        let rows = history(&store);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0]["id"], rows[1]["id"]);
        let replica = Store::open_memory("birch").unwrap();
        replica
            .import_replication(store.origin(), &store.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(history(&replica), rows);
    }

    #[test]
    fn answered_and_withdrawn_asks_keep_episode_and_provenance() {
        let (store, origin, mut input) = person_work::tests::fixture();
        store
            .set_step_state(
                &origin.subject,
                "completed",
                Some("Fixture setup is complete"),
            )
            .unwrap();
        // Independent asks allow repeated episodes while the same source requester remains live.
        input.step = None;
        input.new_run = Some("history-answer".into());
        let ask = store.ask_person(&input).unwrap();
        let open = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|r| r.subject == ask.subject)
            .unwrap();
        store
            .finish_person_step(
                &response(&ask.subject, "person/avery", "Friday, after the review"),
                false,
            )
            .unwrap();
        let first = history(&store);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0]["id"],
            crate::api::client_attention_id(&open.subject, &open.person, &open.episode).unwrap()
        );
        assert_eq!(first[0]["resolution"]["kind"], "answered");
        assert_eq!(first[0]["resolution"]["by"], "person/avery");
        assert_eq!(
            first[0]["resolution"]["answer_label"],
            "Friday, after the review"
        );
        assert_eq!(first[0]["actions"], json!([]));
        assert_eq!(first[0]["operational"]["actionable"], false);
        assert_eq!(first[0]["title"], open.title);
        assert_eq!(first[0]["detail"], open.detail);
        let closure = store
            .claims_for(&ask.subject, Some("work.person-done"))
            .unwrap()
            .remove(0);
        assert_eq!(
            first[0]["resolution"]["at"],
            crate::api::client_timestamp(closure.accepted_at_unix_ms)
        );
        input.new_run = Some("history-withdrawal".into());
        input.idempotency_key = "withdrawal".into();
        let withdrawn = store.ask_person(&input).unwrap();
        store
            .finish_person_step(
                &response(
                    &withdrawn.subject,
                    &input.actor,
                    "The question was withdrawn",
                ),
                true,
            )
            .unwrap();
        let second = history(&store);
        assert_eq!(second[0]["resolution"]["kind"], "withdrawn");
        assert_eq!(second[0]["resolution"]["by"], input.actor);
        assert_eq!(
            second[0]["resolution"]["reason"],
            "The question was withdrawn"
        );
        assert_eq!(second.len(), 2);
        assert!(
            store
                .attention_history_test_page("person/other", 100)
                .unwrap()
                .items
                .is_empty()
        );
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|r| r.subject != ask.subject && r.subject != withdrawn.subject)
        );
    }

    #[test]
    fn structured_answer_label_and_recorded_owner_cancellation() {
        let (store, origin, mut input) = person_work::tests::fixture();
        store
            .set_step_state(
                &origin.subject,
                "completed",
                Some("Fixture setup is complete"),
            )
            .unwrap();
        input.step = None;
        input.new_run = Some("history-choice".into());
        input.request = Some(
            json!({"version":1,"type":"choice","question":"Which day?","why_person":"Choose the day","answers":[{"id":"fri","label":"Friday","consequence":"Ship Friday"},{"id":"mon","label":"Monday","consequence":"Ship Monday"}]}),
        );
        let ask = store.ask_person(&input).unwrap();
        let mut done = response(&ask.subject, "person/avery", "");
        done.answer = Some(crate::person_request::AnswerInput {
            id: Some("fri".into()),
            text: None,
        });
        store.finish_person_step(&done, false).unwrap();
        assert_eq!(history(&store)[0]["resolution"]["answer_label"], "Friday");
        input.request = None;
        input.new_run = Some("history-cancellation".into());
        input.idempotency_key = "cancel-owner".into();
        let cancelled = store.ask_person(&input).unwrap();
        let stop =
            crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
                .unwrap();
        store.apply_internal(&stop, "history-stop").unwrap();
        // Hiding an ask is not evidence of a closure. Reconciliation records the source cancellation.
        assert!(
            !history(&store)
                .iter()
                .any(|r| r["source_id"] == cancelled.subject)
        );
        store.reconcile_person_asks().unwrap();
        let rows = history(&store);
        let row = rows
            .iter()
            .find(|r| r["source_id"] == cancelled.subject)
            .unwrap();
        assert_eq!(row["resolution"]["kind"], "cancelled");
        assert_eq!(row["resolution"]["by"], "daemon/runtime");
        assert_eq!(
            row["resolution"]["reason"],
            "the requester, origin or owning run ended"
        );
    }

    #[test]
    fn historical_source_backfill_rebuilds_from_claims_and_keeps_reopened_asks() {
        let (store, origin, mut input) = person_work::tests::fixture();
        store
            .set_step_state(
                &origin.subject,
                "completed",
                Some("Fixture setup is complete"),
            )
            .unwrap();
        input.step = None;
        input.new_run = Some("history-one".into());
        let first = store.ask_person(&input).unwrap();
        store
            .finish_person_step(&response(&first.subject, "person/avery", "One"), false)
            .unwrap();
        input.new_run = Some("history-two".into());
        input.idempotency_key = "second-episode".into();
        let second = store.ask_person(&input).unwrap();
        store
            .finish_person_step(&response(&second.subject, "person/avery", "Two"), false)
            .unwrap();
        let original = history(&store);
        store.connection.batched(|tx|->Result<()> {
            tx.execute_batch("DELETE FROM local_attention_history_v2; DELETE FROM local_attention_history_dirty;")?;
            invalidate(tx)
        }).unwrap().unwrap();
        assert_eq!(history(&store), original);
        assert_ne!(original[0]["id"], original[1]["id"]);
    }

    #[test]
    fn history_pages_bound_sql_steps_and_preserve_snapshot_versions() {
        let store = Store::open_memory("alder").unwrap();
        store
            .connection
            .batched(|tx| -> Result<()> {
                let mut insert = tx.prepare(
                    "INSERT INTO local_attention_history_v2 VALUES(?1,'source',?2,?3,1,NULL,?4,1,0)",
                )?;
                for n in 0..20_000 {
                    let id = format!("attention/{n:08}");
                    let person = if n % 2 == 0 {
                        "person/avery"
                    } else {
                        "person/other"
                    };
                    insert.execute(params![
                        id,
                        person,
                        n / 5,
                        serde_json::to_string(&json!({"id":id,"person_id":person}))?
                    ])?;
                }
                tx.execute("UPDATE local_attention_history_state SET version=1 WHERE id=1", [])?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let pinned = HistorySnapshot {
            epoch: 1,
            version: 1,
            source_index: 0,
        };
        let first = store
            .attention_history_page("person/avery", 0, Some(&pinned), 5, None)
            .unwrap();
        assert_eq!(first.items.len(), 5);
        let key = first.next.unwrap();
        let connection = store.readers.get();
        let sql="SELECT sort_ms,id,valid_from,valid_to,body FROM local_attention_history_v2 INDEXED BY local_attention_history_person_v2
            WHERE epoch=1 AND person=?1 AND (sort_ms,id,valid_from)<(?2,?3,?4) ORDER BY sort_ms DESC,id DESC,valid_from DESC LIMIT 33";
        let mut statement = connection.prepare(sql).unwrap();
        let rows = statement
            .query_map(params!["person/avery", key.0, key.1, key.2], |r| {
                r.get::<_, String>(1)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 33);
        eprintln!(
            "history keyset seek across 20,000 rows: {} SQLite VM steps",
            statement.get_status(rusqlite::StatementStatus::VmStep)
        );
        assert!(
            statement.get_status(rusqlite::StatementStatus::VmStep) < 1_500,
            "history seek must not scan earlier person rows"
        );
        assert_eq!(statement.get_status(rusqlite::StatementStatus::Sort), 0);
        drop(statement);
        drop(connection);
        let before = store
            .attention_history_page("person/avery", 0, Some(&pinned), 5, Some(&key))
            .unwrap()
            .items;
        store.connection.batched(|tx|->Result<()> {
            for row in &before {
                let id=row["id"].as_str().unwrap();
                retire(tx,id,2)?;
                tx.execute("INSERT INTO local_attention_history_v2 VALUES(?1,'source','person/avery',999999,2,NULL,?2,1,0)",params![id,serde_json::to_string(&json!({"id":id,"changed":true}))?])?;
            }
            tx.execute("UPDATE local_attention_history_state SET version=2 WHERE id=1", [])?;
            Ok(())
        }).unwrap().unwrap();
        assert_eq!(
            store
                .attention_history_page("person/avery", 0, Some(&pinned), 5, Some(&key))
                .unwrap()
                .items,
            before
        );
        let current = store
            .attention_history_page("person/avery", 0, None, 5, None)
            .unwrap();
        assert!(current.items.iter().all(|r| r["changed"] == true));
    }

    fn closed_fixture() -> (Store, PersonAskRequest, String) {
        let (store, origin, mut input) = person_work::tests::fixture();
        store
            .set_step_state(
                &origin.subject,
                "completed",
                Some("Fixture setup is complete"),
            )
            .unwrap();
        input.step = None;
        input.new_run = Some("history-lifecycle".into());
        let ask = store.ask_person(&input).unwrap();
        store
            .finish_person_step(
                &response(&ask.subject, "person/avery", "Recorded answer"),
                false,
            )
            .unwrap();
        (store, input, ask.subject)
    }

    fn padding(store: &Store, count: usize) {
        store.connection.batched(|tx| -> Result<()> {
            let batch: String = tx.query_row("SELECT id FROM batches LIMIT 1", [], |r| r.get(0))?;
            let mut insert = tx.prepare("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                VALUES(?1,?2,?1,'test.irrelevant','alder','{}','[]','1')")?;
            for n in 0..count { insert.execute(params![format!("padding/{n}"),batch])?; }
            Ok(())
        }).unwrap().unwrap();
    }

    #[test]
    fn reconstruction_allows_canonical_writes_and_rejects_stale_publication() {
        let (original, _, _) = closed_fixture();
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history.db"), "birch").unwrap();
        store
            .import_replication(original.origin(), &original.export_replication(0).unwrap())
            .unwrap();
        store.request_attention_history().unwrap();
        let (prepared_tx, prepared_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let store = &store;
            let worker = scope.spawn(move || {
                let mut connection = store.history_connection().unwrap();
                let data_version = connection
                    .query_row("PRAGMA data_version", [], |r| r.get(0))
                    .unwrap();
                let tx = connection.transaction().unwrap();
                let page = prepare_page(&tx, data_version).unwrap();
                assert!(!page.sources.is_empty());
                prepared_tx.send(()).unwrap();
                // Simulate a slow source fold while its read snapshot remains open.
                resume_rx.recv().unwrap();
                tx.commit().unwrap();
                let tx = connection
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                let result = publish_page(&tx, page).unwrap();
                tx.commit().unwrap();
                result
            });
            prepared_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let started = Instant::now();
            let write = store.append_claim(&ClaimInput {
                subject: "message/history-concurrent-write".into(),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: serde_json::from_value(json!({"from":"person/avery","to":"agent/alder.asker","content":"Concurrent diagnostic","status":"sent"})).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some("history-concurrent-write".into()),
            });
            resume_tx.send(()).unwrap();
            write.unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "a history read blocked the canonical writer"
            );
            let work = worker.join().unwrap();
            assert!(work.more);
            assert_eq!(work.processed, 0, "stale reconstruction was published");
        });
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT backfill_after FROM local_attention_history_state",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(history(&store), history(&original));

        // Admission metadata can change without moving the claim frontier. data_version,
        // rather than MAX(store_index) alone, must invalidate such computed plans too.
        let mut connection = store.history_connection().unwrap();
        let data_version = connection
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap();
        let tx = connection.transaction().unwrap();
        let page = prepare_page(&tx, data_version).unwrap();
        tx.commit().unwrap();
        let cut = store.index().unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE batches SET replica_sequence=COALESCE(replica_sequence,0)+1",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(store.index().unwrap(), cut);
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let result = publish_page(&tx, page).unwrap();
        assert!(result.more);
        assert_eq!(result.processed, 0);
        tx.commit().unwrap();
        store.drain_attention_history();
    }

    #[test]
    fn publication_deadline_rolls_back_local_progress_and_releases_writer() {
        let (store, _, _) = closed_fixture();
        // Force a statement to cross the progress-handler interval. Otherwise this small
        // fixture's individual statements finish before the callback is invoked.
        store.connection.batched(|tx|tx.execute_batch("CREATE TRIGGER history_slow_publication BEFORE INSERT ON local_attention_history_v2
            BEGIN SELECT SUM(n) FROM (WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<10000000) SELECT n FROM numbers); END;")).unwrap().unwrap();
        store.request_attention_history().unwrap();
        let mut connection = store.history_connection().unwrap();
        let error = maintain_with_budget(&mut connection, PUBLISH_BUDGET).unwrap_err();
        assert!(error.to_string().contains("interrupt"), "{error:#}");
        assert!(
            connection.is_autocommit(),
            "failed publication retained a write lock"
        );
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT backfill_after FROM local_attention_history_state",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
        store
            .connection
            .batched(|tx| tx.execute_batch("DROP TRIGGER history_slow_publication"))
            .unwrap()
            .unwrap();
        claim(
            &store,
            "message/history-after-deadline",
            "message.sent",
            Some("person/avery"),
            json!({"from":"person/avery","to":"agent/alder.asker","content":"After deadline","status":"sent"}),
            "after-deadline",
        );
        assert_eq!(history(&store).len(), 1);
    }

    #[test]
    fn oversized_sources_are_quarantined_without_blocking_claim_writes() {
        let (store, _, subject) = closed_fixture();
        store.connection.batched(|tx| -> Result<()> {
            let batch: String = tx.query_row("SELECT id FROM batches LIMIT 1",[],|r|r.get(0))?;
            let mut insert = tx.prepare("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                VALUES(?1,?2,?3,'test.irrelevant','alder','{}','[]','1')")?;
            for n in 0..SOURCE_CLAIMS {insert.execute(params![format!("cap-padding/{n}"),batch,subject])?;}
            Ok(())
        }).unwrap().unwrap();
        store.drain_attention_history();
        let error: String = store
            .readers
            .get()
            .query_row(
                "SELECT error FROM local_attention_history_errors WHERE subject=?1",
                [&subject],
                |r| r.get(0),
            )
            .unwrap();
        assert!(error.contains("256 retained claims"));
        let page = store
            .attention_history_page("person/avery", store.index().unwrap(), None, 5, None)
            .unwrap();
        assert!(page.items.is_empty());
        assert!(!page.availability.complete);
        assert!(
            page.availability
                .note
                .unwrap()
                .contains("could not be indexed")
        );
        claim(
            &store,
            "message/history-after-cap",
            "message.sent",
            Some("person/avery"),
            json!({"from":"person/avery","to":"agent/alder.asker","content":"After cap","status":"sent"}),
            "after-cap",
        );
        assert!(
            !store.maintain_attention_history().unwrap().more,
            "quarantine retried without a source change"
        );
    }

    #[test]
    fn unused_history_skips_dirty_marks_and_record_hooks_only_track_repairs() {
        let (store, _, subject) = closed_fixture();
        let dirty = || {
            store
                .readers
                .get()
                .query_row(
                    "SELECT COUNT(*) FROM local_attention_history_dirty",
                    [],
                    |r| r.get::<_, usize>(0),
                )
                .unwrap()
        };
        assert_eq!(dirty(), 0, "unused history queued canonical writes");
        store.drain_attention_history();
        store.connection.batched(|tx| -> Result<()> {
            let id: String = tx.query_row("SELECT id FROM claims WHERE subject=?1 AND kind='work.person-done'",[&subject],|r|r.get(0))?;
            tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                VALUES('history-record','alder',999999,'history-envelope',0,X'00','valid',?1,'1')",[id])?;
            Ok(())
        }).unwrap().unwrap();
        assert_eq!(
            dirty(),
            0,
            "valid replicated insert performed a history lookup"
        );
        store.connection.batched(|tx|tx.execute("UPDATE replica_records SET position=position+1 WHERE record_ref='history-record'",[])).unwrap().unwrap();
        assert_eq!(
            dirty(),
            0,
            "ordinary record position update dirtied history"
        );
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE replica_records SET state='repaired' WHERE record_ref='history-record'",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(dirty(), 1);
        assert!(
            history(&store).is_empty(),
            "repaired closure remained visible"
        );
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE replica_records SET state='valid' WHERE record_ref='history-record'",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            history(&store).len(),
            1,
            "restoring a repaired record did not rebuild history"
        );
    }

    #[test]
    fn custom_sources_without_attention_do_not_queue_history() {
        let (store, _, _) = closed_fixture();
        store.drain_attention_history();
        store.connection.batched(|tx| -> Result<()> {
            tx.execute("INSERT INTO custom_registrations VALUES('custom-kind/history-none','history-none',1,'custom/history-none/','hash','{}','claim','ready',NULL)",[])?;
            tx.execute("INSERT INTO custom_sources VALUES('custom/history-none/1','hash','history-none','r1','ready','{}',NULL,NULL,0,'1')",[])?;
            tx.execute("UPDATE custom_sources SET body='{\"changed\":true}' WHERE subject='custom/history-none/1'",[])?;
            tx.execute("DELETE FROM custom_sources WHERE subject='custom/history-none/1'",[])?;
            let dirty:usize=tx.query_row("SELECT COUNT(*) FROM local_attention_history_dirty",[],|r|r.get(0))?;
            assert_eq!(dirty,0,"non-attention custom projections queued history");
            Ok(())
        }).unwrap().unwrap();
    }

    #[test]
    fn normal_replication_insert_cost_is_bounded_without_source_lookup() {
        let (store, _, subject) = closed_fixture();
        padding(&store, 20_000);
        store.request_attention_history().unwrap();
        store.connection.batched(|tx| -> Result<()> {
            let id: String = tx.query_row("SELECT id FROM claims WHERE subject=?1 AND kind='work.person-done'",[subject],|r|r.get(0))?;
            let sql = "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                VALUES(?1,'alder',999998,'history-cost',?2,X'00','valid',?3,'1')";
            let mut insert=tx.prepare(sql)?;
            insert.execute(params!["history-cost-with",0,id])?;
            let with=insert.get_status(rusqlite::StatementStatus::VmStep);
            drop(insert);
            tx.execute_batch("DROP TRIGGER attention_history_record_insert")?;
            let mut insert=tx.prepare(sql)?;
            insert.execute(params!["history-cost-without",1,id])?;
            let without=insert.get_status(rusqlite::StatementStatus::VmStep);
            drop(insert);
            eprintln!("normal replica record insert on 20,000 claims: history={with}, baseline={without} SQLite VM steps");
            assert!(with-without<50,"normal records paid a source lookup: {with}-{without}");
            create_schema(tx)?;
            Ok(())
        }).unwrap().unwrap();
    }

    #[test]
    fn fenced_history_connection_denies_schema_attach_and_source_mutations() {
        let (store, _, _) = closed_fixture();
        let connection = store.history_connection().unwrap();
        for sql in [
            "DELETE FROM claims",
            "DROP TABLE claims",
            "CREATE TABLE history_escape(id)",
            "ATTACH DATABASE ':memory:' AS escape",
            "PRAGMA writable_schema=ON",
            "PRAGMA journal_mode=DELETE",
        ] {
            assert!(
                connection.execute_batch(sql).is_err(),
                "history connection permitted {sql}"
            );
        }
        assert!(store.index().unwrap() > 0);
    }

    #[test]
    fn opening_is_constant_and_background_backfill_is_bounded_and_resumes() {
        let (store, _, _) = closed_fixture();
        let baseline = store
            .connection
            .batched(|tx| -> Result<usize> {
                let steps = Arc::new(AtomicUsize::new(0));
                let observed = steps.clone();
                tx.progress_handler(
                    1,
                    Some(move || {
                        observed.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                );
                create_schema(tx)?;
                tx.progress_handler(0, None::<fn() -> bool>);
                Ok(steps.load(Ordering::Relaxed))
            })
            .unwrap()
            .unwrap();
        padding(&store, 20_000);
        // Simulate the upgrade: the source graph predates an empty disposable cache.
        store.connection.batched(|tx| -> Result<()> {
            tx.execute_batch("DELETE FROM local_attention_history_dirty; DELETE FROM local_attention_history_state")?;
            let steps = Arc::new(AtomicUsize::new(0));
            let observed = steps.clone();
            tx.progress_handler(1, Some(move || { observed.fetch_add(1, Ordering::Relaxed); false }));
            create_schema(tx)?;
            let schema = steps.swap(0, Ordering::Relaxed);
            open(tx)?;
            tx.progress_handler(0, None::<fn() -> bool>);
            let opening = steps.load(Ordering::Relaxed);
            assert!(schema.abs_diff(baseline) < 1000, "schema opening grew with claims: baseline={baseline}, large={schema}");
            assert!(opening < 100, "history state opening scanned claims: {opening} VM steps");
            eprintln!("history schema on small/20,000-claim graph: {baseline}/{schema}; state open: {opening} SQLite VM steps");
            Ok(())
        }).unwrap().unwrap();
        assert_eq!(
            store.maintain_attention_history().unwrap().examined,
            0,
            "unused history does not backfill"
        );
        store.request_attention_history().unwrap();
        let source_cut: u64 = store
            .readers
            .get()
            .query_row("SELECT MAX(store_index) FROM claims", [], |r| r.get(0))
            .unwrap();
        let initial = store
            .attention_history_page("person/avery", source_cut, None, 5, None)
            .unwrap();
        assert!(initial.items.is_empty());
        assert!(!initial.availability.complete);
        assert!(
            initial
                .availability
                .note
                .unwrap()
                .contains("still indexing")
        );
        let first = store.maintain_attention_history().unwrap();
        assert_eq!(first.examined, CLAIM_PAGE);
        assert!(first.processed <= DIRTY_PAGE && first.more);
        let progress: u64 = store
            .readers
            .get()
            .query_row(
                "SELECT backfill_after FROM local_attention_history_state",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store.connection.batched(open).unwrap().unwrap();
        assert_eq!(
            progress,
            store
                .readers
                .get()
                .query_row(
                    "SELECT backfill_after FROM local_attention_history_state",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            "restart preserves progress"
        );
        let mut connection = store.history_connection().unwrap();
        let data_version = connection
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap();
        let tx = connection.transaction().unwrap();
        let steps = Arc::new(AtomicUsize::new(0));
        let observed = steps.clone();
        tx.progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let page = prepare_page(&tx, data_version).unwrap();
        tx.progress_handler(0, None::<fn() -> bool>);
        let count = steps.load(Ordering::Relaxed);
        assert!(
            count < 10000,
            "an irrelevant backfill page scanned the graph: {count} VM steps"
        );
        eprintln!(
            "128-claim irrelevant read preparation on 20,000 claims: {count} SQLite VM steps"
        );
        tx.commit().unwrap();
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let second = publish_page(&tx, page).unwrap();
        tx.commit().unwrap();
        assert_eq!(second.examined, CLAIM_PAGE);
        assert!(second.processed <= DIRTY_PAGE);
        let pinned = store
            .attention_history_page("person/avery", source_cut, None, 5, None)
            .unwrap();
        store.drain_attention_history();
        let old = store
            .attention_history_page("person/avery", source_cut, Some(&initial.snapshot), 5, None)
            .unwrap();
        assert!(
            old.items.is_empty(),
            "later background materialization cannot shift a pinned page"
        );
        let rows = store
            .attention_history_page("person/avery", source_cut, None, 100, None)
            .unwrap()
            .items;
        assert_eq!(rows.len(), 1);
        assert_eq!(pinned.items, rows);
    }

    #[test]
    fn checkpoint_renumbering_fences_cursors_and_rebuilds_without_lost_rows() {
        let (store, _, _) = closed_fixture();
        // An index built with large graph positions must survive a later canonical renumber.
        store
            .connection
            .batched(|tx| tx.execute("UPDATE claims SET store_index=store_index+100000", []))
            .unwrap()
            .unwrap();
        store.drain_attention_history();
        let before = store
            .attention_history_page("person/avery", 200000, None, 5, None)
            .unwrap()
            .items;
        let old = store
            .attention_history_page("person/avery", 200000, None, 5, None)
            .unwrap();
        store.connection.batched(|tx| -> Result<()> {
            smallclaims::Runtime::clear_checkpoint_projections(store.smalltalk.as_ref(), tx)?;
            tx.execute_batch("CREATE TEMP TABLE history_renumber AS SELECT store_index AS old,ROW_NUMBER() OVER(ORDER BY store_index) AS new FROM claims;
                UPDATE claims SET store_index=-(SELECT new FROM history_renumber WHERE old=claims.store_index);
                UPDATE claims SET store_index=-store_index; DROP TABLE history_renumber;")?;
            smallclaims::Runtime::replay_checkpoint_projections(store.smalltalk.as_ref(), tx)?;
            Ok(())
        }).unwrap().unwrap();
        assert!(
            store
                .attention_history_page(
                    "person/avery",
                    store.index().unwrap(),
                    Some(&old.snapshot),
                    5,
                    None
                )
                .unwrap_err()
                .to_string()
                .contains("attention-history-epoch-changed")
        );
        store.drain_attention_history();
        let low_cut: u64 = store
            .readers
            .get()
            .query_row("SELECT MAX(store_index) FROM claims", [], |r| r.get(0))
            .unwrap();
        assert!(low_cut < 100000);
        assert_eq!(
            store
                .attention_history_page("person/avery", low_cut, None, 5, None)
                .unwrap()
                .items,
            before
        );
        assert_eq!(before.len(), 1);
        let invalid: u64 = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM local_attention_history_v2 WHERE valid_to<valid_from",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(invalid, 0);
    }

    #[test]
    fn failed_subject_is_quarantined_without_poisoning_claim_writes_and_can_recover() {
        let (store, mut input, poisoned) = closed_fixture();
        store.connection.batched(|tx|tx.execute_batch(&format!("CREATE TRIGGER history_poison BEFORE INSERT ON local_attention_history_v2
            WHEN NEW.source='{poisoned}' BEGIN SELECT RAISE(ABORT,'injected history index failure'); END;"))).unwrap().unwrap();
        input.new_run = Some("history-healthy".into());
        input.idempotency_key = "history-healthy".into();
        let healthy = store.ask_person(&input).unwrap();
        store
            .finish_person_step(
                &response(&healthy.subject, "person/avery", "Healthy answer"),
                false,
            )
            .unwrap();
        store.drain_attention_history();
        let page = store
            .attention_history_page("person/avery", store.index().unwrap(), None, 10, None)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0]["source_id"], healthy.subject);
        assert!(
            page.availability
                .note
                .unwrap()
                .contains("could not be indexed")
        );
        assert_eq!(
            store
                .claims_for(&poisoned, Some("work.person-done"))
                .unwrap()
                .len(),
            1,
            "closure committed independently of cache failure"
        );
        assert_eq!(
            store.maintain_attention_history().unwrap().processed,
            0,
            "quarantined source is not retried on every write"
        );
        store
            .connection
            .batched(|tx| tx.execute_batch("DROP TRIGGER history_poison"))
            .unwrap()
            .unwrap();
        // A new canonical source write is the retry signal; later answers do not replace the first answer.
        claim(
            &store,
            &poisoned,
            "work.person-done",
            Some("person/avery"),
            json!({"summary":"Later answer"}),
            "retry-poison",
        );
        store.drain_attention_history();
        let page = store
            .attention_history_page("person/avery", store.index().unwrap(), None, 10, None)
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(
            !page
                .availability
                .note
                .unwrap()
                .contains("could not be indexed")
        );
    }

    #[test]
    fn repeating_closed_ask_key_preserves_the_single_canonical_request() {
        let (store, input, subject) = closed_fixture();
        let repeated = store.ask_person(&input).unwrap();
        assert_eq!(repeated.subject, subject);
        assert_eq!(
            store
                .claims_for(&subject, Some("work.person-asked"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(history(&store).len(), 1);
    }

    #[test]
    fn unused_history_does_not_query_or_dirty_on_irrelevant_writes() {
        let (store, _, _) = closed_fixture();
        padding(&store, 20000);
        store.connection.batched(|tx| -> Result<()> {
            tx.execute("DELETE FROM local_attention_history_dirty", [])?;
            let batch: String = tx.query_row("SELECT id FROM batches LIMIT 1", [], |r|r.get(0))?;
            let sql = "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                VALUES(?1,?2,'custom/client/irrelevant','custom.client.pairing-completed','alder','{}','[]','1')";
            let mut insert = tx.prepare(sql)?;
            insert.execute(params!["cost-with-history",batch])?;
            let with = insert.get_status(rusqlite::StatementStatus::VmStep);
            drop(insert);
            // A baseline on the same large graph, with just history's claim-insert hook removed.
            tx.execute_batch("DROP TRIGGER attention_history_claim_insert")?;
            let mut insert = tx.prepare(sql)?;
            insert.execute(params!["cost-without-history",batch])?;
            let without = insert.get_status(rusqlite::StatementStatus::VmStep);
            drop(insert);
            create_schema(tx)?;
            eprintln!("irrelevant claim insert on 20,000 claims: history={with}, baseline={without} SQLite VM steps");
            assert!(with - without < 100, "irrelevant writes must only check exact kind membership, never query history");
            let dirty: u64 = tx.query_row("SELECT COUNT(*) FROM local_attention_history_dirty", [], |r|r.get(0))?;
            assert_eq!(dirty, 0, "custom client sources do not dirty attention history");
            Ok(())
        }).unwrap().unwrap();
        assert_eq!(store.maintain_attention_history().unwrap().examined, 0);
    }

    #[test]
    fn disposable_history_commits_do_not_invalidate_source_windows() {
        let (store, mut input, _) = closed_fixture();
        let commits = Arc::new(AtomicUsize::new(0));
        let observed = commits.clone();
        let _observer = store.observe_commits(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        store.drain_attention_history();
        assert_eq!(history(&store).len(), 1);
        assert_eq!(
            commits.load(Ordering::Relaxed),
            0,
            "history-only maintenance must not notify canonical commit observers"
        );
        input.new_run = Some("history-observer".into());
        input.idempotency_key = "history-observer".into();
        store.ask_person(&input).unwrap();
        assert!(
            commits.load(Ordering::Relaxed) > 0,
            "canonical source commits still notify observers"
        );
        let connection = store.history_connection().unwrap();
        assert!(
            connection
                .execute("UPDATE claims SET actor='person/other'", [])
                .is_err(),
            "maintenance connection cannot write source authority"
        );
    }

    #[test]
    fn actual_trim_lowering_retained_frontier_expires_pages_and_rebuilds_untouched_sources() {
        let (store, _, _) = closed_fixture();
        let diagnostic = claim(
            &store,
            "message/history-diagnostic",
            "message.sent",
            Some("person/avery"),
            json!({"from":"person/avery","to":"agent/alder.asker","content":"An unrelated diagnostic","status":"sent"}),
            "history-diagnostic",
        );
        let before = store
            .attention_history_test_page("person/avery", 5)
            .unwrap();
        assert_eq!(before.items.len(), 1);
        let sealed = store.checkpoint_sealed_set(now_ms() + 1000).unwrap();
        let source = sealed
            .claims
            .iter()
            .find(|c| c.claim.id == diagnostic.id)
            .unwrap();
        let dropped = smallclaims::store::checkpoint::claim_tombstone(source);
        let envelope = smallclaims::store::checkpoint::EnvelopeTombstone {
            writer: dropped.writer.clone(),
            sequence: dropped.sequence,
            envelope_hash: dropped.envelope_hash.clone(),
            accepted_at_unix_ms: dropped.accepted_at_unix_ms,
        };
        assert_eq!(
            sealed
                .claims
                .iter()
                .filter(|c| c.envelope == source.envelope)
                .count(),
            1
        );
        // The trim helper commits tombstones and removes the diagnostic's complete envelope.
        store
            .apply_checkpoint_drop("checkpoint/history-diagnostic", &[envelope], &[dropped])
            .unwrap();
        let retained: u64 = store
            .readers
            .get()
            .query_row("SELECT MAX(store_index) FROM claims", [], |r| r.get(0))
            .unwrap();
        assert!(retained < before.snapshot.source_index);
        assert!(
            store.index().unwrap() >= before.snapshot.source_index,
            "ordinary trim preserves the client's AUTOINCREMENT high water"
        );
        assert!(
            store
                .attention_history_page(
                    "person/avery",
                    store.index().unwrap(),
                    Some(&before.snapshot),
                    5,
                    None
                )
                .is_err()
        );
        let after = store
            .attention_history_test_page("person/avery", 5)
            .unwrap();
        assert_eq!(
            after.items, before.items,
            "an untouched source must not vanish when an unrelated latest claim is trimmed"
        );
        assert!(after.snapshot.epoch > before.snapshot.epoch);
    }
}
