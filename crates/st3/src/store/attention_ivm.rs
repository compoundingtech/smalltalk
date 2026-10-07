//! Keyed open attention families. A partial family is never a certificate for all of Now.
//! The collection installer owns source certification; these operators consume its projected
//! sources in the same transaction. Readers never repair or recompute the index.
use super::*;
use smallclaims::ivm::{Definition, LocalChange, View, Views, source_cut};

pub(crate) const PERSON_VIEW: &str = "st3.attention.person-steps.v1";
pub(crate) const CUSTOM_VIEW: &str = "st3.attention.custom.v1";
const PERSON_CHANGE: &str = "st3.attention.person.source";
const CUSTOM_CHANGE: &str = "st3.attention.custom.source";
const PERSON_CLOCK: &str = "st3.attention.person.clock";
const CUSTOM_CLOCK: &str = "st3.attention.custom.clock";

const SCHEMA: &str = r#"
CREATE INDEX IF NOT EXISTS attention_open_person_sources ON step_runs(subject)
 WHERE assignee LIKE 'person/%';
CREATE TABLE IF NOT EXISTS local_attention_open (
 family TEXT NOT NULL, source TEXT NOT NULL, id TEXT NOT NULL, person TEXT NOT NULL,
 priority INTEGER NOT NULL, requested TEXT NOT NULL, eligible TEXT NOT NULL,
 episode TEXT NOT NULL, item TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(family,source), UNIQUE(family,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS attention_open_person ON local_attention_open
 (family,person,priority,length(requested) DESC,requested DESC,source,episode);
CREATE INDEX IF NOT EXISTS attention_open_due ON local_attention_open
 (length(eligible),eligible,family,source);
CREATE TABLE IF NOT EXISTS local_attention_open_dependencies (
 family TEXT NOT NULL, source TEXT NOT NULL, dependency TEXT NOT NULL,
 PRIMARY KEY(family,source,dependency)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS attention_open_reverse ON local_attention_open_dependencies
 (dependency,family,source);
CREATE TABLE IF NOT EXISTS local_attention_open_dirty (
 family TEXT NOT NULL, source TEXT NOT NULL, PRIMARY KEY(family,source)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS attention_open_custom_insert AFTER INSERT ON custom_sources BEGIN
 INSERT INTO local_attention_open_dirty VALUES('custom',NEW.subject) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS attention_open_custom_update AFTER UPDATE ON custom_sources BEGIN
 INSERT INTO local_attention_open_dirty VALUES('custom',OLD.subject) ON CONFLICT DO NOTHING;
 INSERT INTO local_attention_open_dirty VALUES('custom',NEW.subject) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS attention_open_custom_delete AFTER DELETE ON custom_sources BEGIN
 INSERT INTO local_attention_open_dirty VALUES('custom',OLD.subject) ON CONFLICT DO NOTHING;
END;
"#;

pub(crate) fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(Family::Person), Box::new(Family::Custom)]
}

#[derive(Clone, Copy)]
enum Family {
    Person,
    Custom,
}
impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Custom => "custom",
        }
    }
    fn view(self) -> &'static str {
        match self {
            Self::Person => PERSON_VIEW,
            Self::Custom => CUSTOM_VIEW,
        }
    }
}

impl View for Family {
    fn definition(&self) -> Definition {
        Definition {
            name: self.view(),
            fingerprint: "projected-source.v1;canonical-person-fences.v2.ascii-like;custom-source.v1;u128-time.v1;public-row.v1",
            kinds: &[],
            local_kinds: match self {
                Self::Person => &[PERSON_CHANGE, PERSON_CLOCK],
                Self::Custom => &[CUSTOM_CHANGE, CUSTOM_CLOCK],
            },
            max_contributions: 1,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        // Projection updates and raw source corrections use the same reverse index. The
        // OLD edge survives reassignment/removal until maintenance reads the new source.
        for (table, key) in [
            ("step_runs", "subject"),
            ("mission_runs", "id"),
            ("desired", "subject"),
            ("claims", "subject"),
        ] {
            let dependency = |which: &str| {
                if table == "mission_runs" {
                    format!("'mission-run/' || {which}.{key}")
                } else {
                    format!("{which}.{key}")
                }
            };
            for action in ["INSERT", "UPDATE", "DELETE"] {
                let mut body = String::new();
                for which in match action {
                    "INSERT" => vec!["NEW"],
                    "DELETE" => vec!["OLD"],
                    _ => vec!["OLD", "NEW"],
                } {
                    body.push_str(&format!("INSERT INTO local_attention_open_dirty SELECT family,source FROM local_attention_open_dependencies WHERE dependency={} ON CONFLICT DO NOTHING;", dependency(which)));
                    if table == "step_runs" {
                        body.push_str(&format!("INSERT INTO local_attention_open_dirty SELECT 'person',{which}.subject WHERE {which}.assignee LIKE 'person/%' ON CONFLICT DO NOTHING;"));
                    }
                }
                connection.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS attention_open_{table}_{action} AFTER {action} ON {table} BEGIN {body} END;"))?;
            }
        }
        // Canonical tie corrections can change a request/declaration without changing its
        // subject or the frontier. Follow the admitted claim's subject, never arrival order.
        for action in ["INSERT", "UPDATE", "DELETE"] {
            let references = match action {
                "INSERT" => "NEW.claim_id",
                "DELETE" => "OLD.claim_id",
                _ => "OLD.claim_id,NEW.claim_id",
            };
            connection.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS attention_open_record_{action} AFTER {action} ON replica_records BEGIN
              INSERT INTO local_attention_open_dirty SELECT family,source FROM local_attention_open_dependencies
              WHERE dependency IN (SELECT subject FROM claims WHERE id IN ({references})) ON CONFLICT DO NOTHING; END;"))?;
        }
        connection.execute_batch("CREATE TRIGGER IF NOT EXISTS attention_open_batch_update AFTER UPDATE OF origin,replica_sequence ON batches BEGIN
          INSERT INTO local_attention_open_dirty SELECT family,source FROM local_attention_open_dependencies
          WHERE dependency IN (SELECT subject FROM claims WHERE batch_id IN (OLD.id,NEW.id)) ON CONFLICT DO NOTHING; END;")?;
        Ok(())
    }
    fn maintain_local_key(
        &self,
        tx: &Transaction<'_>,
        key: &str,
        change: &LocalChange,
    ) -> Result<bool> {
        let (family, source): (String, String) = serde_json::from_str(key)?;
        if family != self.name() {
            return Ok(false);
        }
        let before: Option<(String, String, String)> = tx
            .query_row(
                "SELECT item,body,eligible FROM local_attention_open WHERE family=?1 AND source=?2",
                params![family, source],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        tx.execute(
            "DELETE FROM local_attention_open_dependencies WHERE family=?1 AND source=?2",
            params![family, source],
        )?;
        let next = match self {
            Family::Person => person_row(tx, &source)?,
            Family::Custom => custom_row(tx, &source)?,
        };
        let Some((item, body, eligible)) = next else {
            tx.execute(
                "DELETE FROM local_attention_open WHERE family=?1 AND source=?2",
                params![family, source],
            )?;
            return Ok(before.is_some());
        };
        let item_text = serde_json::to_string(&item)?;
        let body_text = serde_json::to_string(&body)?;
        let changed =
            before.as_ref() != Some(&(item_text.clone(), body_text.clone(), eligible.to_string()));
        if changed {
            tx.execute(
                "INSERT INTO local_attention_open VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
              ON CONFLICT(family,source) DO UPDATE SET id=excluded.id,person=excluded.person,
              priority=excluded.priority,requested=excluded.requested,eligible=excluded.eligible,
              episode=excluded.episode,item=excluded.item,body=excluded.body",
                params![
                    family,
                    source,
                    body["id"].as_str(),
                    item.person,
                    priority(&item),
                    item.requested_at_unix_ms.to_string(),
                    eligible.to_string(),
                    item.episode,
                    item_text,
                    body_text
                ],
            )?;
        }
        // A scheduled eligibility transition changes window membership even with equal rows.
        Ok(changed || matches!(change.kind.as_str(), PERSON_CLOCK | CUSTOM_CLOCK))
    }
}

fn priority(item: &AttentionItemView) -> u8 {
    match item.priority.as_str() {
        "critical" => 0,
        "high" => 1,
        "normal" => 2,
        _ => 3,
    }
}
fn dependency(tx: &Transaction<'_>, family: &str, source: &str, key: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO local_attention_open_dependencies VALUES(?1,?2,?3)",
        params![family, source, key],
    )?;
    Ok(())
}

fn custom_row(
    tx: &Transaction<'_>,
    source: &str,
) -> Result<Option<(AttentionItemView, Value, u128)>> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT body,requested_at FROM custom_sources WHERE subject=?1 AND active=1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((text, at)) = row else {
        return Ok(None);
    };
    let source: Value = serde_json::from_str(&text)?;
    let at = at.parse().unwrap_or(0);
    let item = custom::attention_item(&source, at)?;
    let mut row = resource(&item)?;
    row["source_kind"] = json!("custom");
    row["revision"] = source["revision"].clone();
    row["custom_form"] = source["attention"]["reply"].clone();
    row["action_parameters"] = json!({"custom.reply":{"target_id":item.subject,"registration":source["registration"],"revision":source["revision"],"episode":item.episode}});
    Ok(Some((item, row, at)))
}

fn person_row(
    tx: &Transaction<'_>,
    source: &str,
) -> Result<Option<(AttentionItemView, Value, u128)>> {
    dependency(tx, "person", source, source)?;
    let Some(step) = person_work::step(tx, source)? else {
        return Ok(None);
    };
    if step.assigned_to.as_deref().is_none_or(|a| {
        !a.get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("person/"))
    }) {
        return Ok(None);
    }
    run_dependencies(tx, source, &step.run)?;
    let ask = person_work::request(tx, source)?;
    if let Some(ask) = &ask {
        if let Some(actor) = ask.actor.as_deref() {
            dependency(tx, "person", source, actor)?;
            if let Some(desired) = current_desired_row(tx, actor)? {
                if desired.kind == "stop" && !person_work::is_update(ask) {
                    anyhow::bail!(
                        "retiring requester requires the owned-set rollout dependency operator"
                    );
                }
                if let Some(run) = desired.owner_run {
                    run_dependencies(tx, source, &run)?;
                }
            }
        }
        for field in ["owner_run", "origin_step"] {
            if let Some(key) = ask.body["fields"][field].as_str() {
                dependency(tx, "person", source, key)?;
                if key.starts_with("mission-run/") {
                    run_dependencies(tx, source, key)?;
                }
            }
        }
    }
    let Some(item) = attention_snapshot::person_attention_item(tx, source, u128::MAX)? else {
        return Ok(None);
    };
    let eligible = ask.as_ref().map_or(item.requested_at_unix_ms, |a| {
        item.requested_at_unix_ms.max(a.accepted_at_unix_ms)
    });
    let mut row = resource(&item)?;
    row["action_parameters"] =
        json!({"work.done":{"target_id":item.subject,"episode":item.episode}});
    if let Some(ask) = &ask
        && let Some(origin) = ask.body["fields"]["origin_step"].as_str()
        && let Some(blocked) = person_work::step(tx, origin)?
    {
        let mission: Option<String> = tx
            .query_row(
                "SELECT mission_id FROM mission_runs WHERE id=?1",
                [step.run.trim_start_matches("mission-run/")],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(mission) = mission {
            row["mission_id"] = json!(if mission.starts_with("mission/") {
                mission
            } else {
                format!("mission/{mission}")
            });
        }
        row["blocked"] = json!({"step_run_id":origin,"step":blocked.step,"goal":blocked.goals.join("\n"),"attempt":blocked.attempt});
    }
    match item.request.as_ref() {
        Some(update) if update["type"] == "update" => {
            row["action_parameters"]["work.done"]["summary"] = json!("Read");
            row["action_parameters"]["work.done"]["answer"] = json!({"id":"read"});
            row["update"] = update.clone();
        }
        Some(request) => row["request"] = request.clone(),
        None => {}
    }
    Ok(Some((item, row, eligible)))
}

/// Record the full liveness chain even when a parent is absent, so later restoration wakes
/// this source. An ancestry exceeding the bound fences the family instead of omitting rows.
fn run_dependencies(tx: &Transaction<'_>, source: &str, initial: &str) -> Result<()> {
    let mut pending = vec![initial.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(run) = pending.pop() {
        if !seen.insert(run.clone()) {
            continue;
        }
        anyhow::ensure!(
            seen.len() <= 256,
            "attention run ancestry exceeds dependency bound"
        );
        dependency(tx, "person", source, &run)?;
        let Some(header) =
            mission_run_header_tx(tx, run.trim_start_matches("mission-run/")).optional()?
        else {
            continue;
        };
        dependency(tx, "person", source, &header.generation)?;
        if header.root_mission_run != run {
            pending.push(header.root_mission_run);
        }
        if let Some(parent) = header.parent_step_run {
            dependency(tx, "person", source, &parent)?;
            if let Some(parent) = person_work::step(tx, &parent)? {
                pending.push(parent.run);
            } else {
                let key = parent.strip_prefix("step-run/").unwrap_or(&parent);
                dependency(tx, "person", source, key)?;
                if let Some(owner) = current_desired_row(tx, key)?
                    && let Some(run) = owner.owner_run
                {
                    pending.push(run);
                }
            }
        }
    }
    Ok(())
}

fn resource(item: &AttentionItemView) -> Result<Value> {
    let id = crate::api::client_attention_id(&item.subject, &item.person, &item.episode)?;
    let mut row = json!({"id":id,"kind":"attention","attention_kind":item.kind,
      "source_id":item.subject,"source_kind":item.kind,"episode":item.episode,"person_id":item.person,
      "revision":item.episode,"updated_at":crate::api::client_timestamp(item.requested_at_unix_ms),
      "title":item.title,"detail":item.detail,"priority":item.priority,"state":"open",
      "requested_at":crate::api::client_timestamp(item.requested_at_unix_ms),"targets":item.targets,
      "actions":crate::api::client_attention_actions(&item.kind,item.review_mode.as_deref()),
      "operational":{"layer":"current","actionable":true,"reasons":[]}});
    for (name, value) in [
        ("requester_id", &item.requester_id),
        ("launch_id", &item.launch_id),
        ("variant_id", &item.variant_id),
        ("message_id", &item.message_id),
        ("mission_id", &item.mission),
        ("mission_run_id", &item.mission_run),
        ("step_run_id", &item.step),
        ("review_mode", &item.review_mode),
    ] {
        if let Some(value) = value {
            row[name] = json!(value);
        }
    }
    Ok(row)
}

/// Called only by the certified installer after source projections, never from a reader.
pub(crate) fn flush(
    tx: &Transaction<'_>,
    views: &Views,
    captured_time: u128,
    limit: usize,
) -> Result<bool> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "attention maintenance page exceeds bound"
    );
    let sources = dirty_page(tx, limit)?;
    for (family, source) in sources {
        let previous =
            source_cut(tx)?.context("attention installer has no certified source cut")?;
        let kind = source_kind(&family)?;
        let key = serde_json::to_string(&(family.as_str(), source.as_str()))?;
        let changes = views.local_change(
            tx,
            &LocalChange {
                kind: kind.into(),
                old_keys: BTreeSet::new(),
                new_keys: BTreeSet::from([key]),
                evaluation_time_unix_ms: captured_time,
            },
            smallclaims::ivm::SourceCut {
                local_generation: previous
                    .local_generation
                    .checked_add(1)
                    .context("local generation exhausted")?,
                ..previous
            },
        )?;
        if changes.deferred.is_empty() {
            tx.execute(
                "DELETE FROM local_attention_open_dirty WHERE family=?1 AND source=?2",
                params![family, source],
            )?;
        }
    }
    clean(tx, None)
}

fn source_kind(family: &str) -> Result<&'static str> {
    match family {
        "person" => Ok(PERSON_CHANGE),
        "custom" => Ok(CUSTOM_CHANGE),
        _ => anyhow::bail!("unknown attention family"),
    }
}
fn family(view: &str) -> Result<Family> {
    match view {
        PERSON_VIEW => Ok(Family::Person),
        CUSTOM_VIEW => Ok(Family::Custom),
        _ => anyhow::bail!("unknown attention family"),
    }
}
fn dirty_page(connection: &Connection, limit: usize) -> Result<Vec<(String, String)>> {
    Ok(connection
        .prepare_cached(
            "SELECT family,source FROM local_attention_open_dirty ORDER BY family,source LIMIT ?1",
        )?
        .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
pub(crate) fn clean(connection: &Connection, view: Option<&str>) -> Result<bool> {
    let name = view.map(family).transpose()?.map(Family::name);
    Ok(!connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_attention_open_dirty WHERE (?1 IS NULL OR family=?1))",
        [name],
        |r| r.get::<_, bool>(0),
    )?)
}

/// Bounded explicit extraction for a fresh installation, outside GET/startup paths. The
/// installer supplies the source snapshot/cursor, owns an unpublished lifetime, and fences
/// all writes until catch-up and dependency certification finish. This is not publication.
pub(crate) fn seed_page(
    tx: &Transaction<'_>,
    view: &str,
    after: &str,
    limit: usize,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "attention installation page exceeds bound"
    );
    let family = family(view)?;
    let query = match family {
        Family::Person => {
            "SELECT subject FROM step_runs WHERE assignee LIKE 'person/%' AND subject>?1 ORDER BY subject LIMIT ?2"
        }
        Family::Custom => {
            "SELECT subject FROM custom_sources WHERE subject>?1 ORDER BY subject LIMIT ?2"
        }
    };
    let keys = tx
        .prepare_cached(query)?
        .query_map(params![after, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for source in &keys {
        tx.execute(
            "INSERT OR IGNORE INTO local_attention_open_dirty VALUES(?1,?2)",
            params![family.name(), source],
        )?;
    }
    Ok(keys)
}

/// Build a bounded page in the installer's unpublished lifetime. The owner must reject
/// concurrent serving, finish all pages, verify complete source capture and then publish.
pub(crate) fn backfill_page(
    tx: &Transaction<'_>,
    captured_time: u128,
    limit: usize,
) -> Result<bool> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "attention backfill page exceeds bound"
    );
    for (name, source) in dirty_page(tx, limit)? {
        let operator = match name.as_str() {
            "person" => Family::Person,
            "custom" => Family::Custom,
            _ => anyhow::bail!("unknown attention family"),
        };
        let ready: bool = tx.query_row(
            "SELECT COALESCE((SELECT ready FROM ivm_views WHERE name=?1),0)",
            [operator.view()],
            |r| r.get(0),
        )?;
        anyhow::ensure!(!ready, "cannot backfill a published attention family");
        let key = serde_json::to_string(&(name.as_str(), source.as_str()))?;
        operator.maintain_local_key(
            tx,
            &key,
            &LocalChange {
                kind: source_kind(&name)?.into(),
                old_keys: BTreeSet::new(),
                new_keys: BTreeSet::from([key.clone()]),
                evaluation_time_unix_ms: captured_time,
            },
        )?;
        tx.execute(
            "DELETE FROM local_attention_open_dirty WHERE family=?1 AND source=?2",
            params![name, source],
        )?;
    }
    clean(tx, None)
}

/// Scheduled captured-time membership transitions. The owner passes its prior clock cut;
/// backwards clocks require a replacement snapshot. Each page is bounded and advances only
/// after commit/delivery. No timer, wall-clock read or polling fallback lives in this operator.
pub(crate) fn clock_page(
    tx: &Transaction<'_>,
    views: &Views,
    from: u128,
    to: u128,
    after: Option<(u128, String, String)>,
    limit: usize,
) -> Result<Vec<(u128, String, String)>> {
    anyhow::ensure!(
        to >= from,
        "attention clock moved backwards; recapture snapshot"
    );
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "attention clock page exceeds bound"
    );
    let (cursor, cf, cs) = after.unwrap_or((from, String::new(), String::new()));
    let lo = from.to_string();
    let hi = to.to_string();
    let cursor = cursor.to_string();
    let keys = tx
        .prepare_cached(
            "SELECT eligible,family,source FROM local_attention_open
      WHERE (length(eligible)>length(?1) OR (length(eligible)=length(?1) AND eligible>?1))
      AND (length(eligible)<length(?2) OR (length(eligible)=length(?2) AND eligible<=?2))
      AND (length(eligible),eligible,family,source)>(length(?3),?3,?4,?5)
      ORDER BY length(eligible),eligible,family,source LIMIT ?6",
        )?
        .query_map(params![lo, hi, cursor, cf, cs, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut output = Vec::new();
    for (at, name, source) in keys {
        let previous = source_cut(tx)?.context("attention clock source is unavailable")?;
        let key = serde_json::to_string(&(name.as_str(), source.as_str()))?;
        let changes = views.local_change(
            tx,
            &LocalChange {
                kind: match name.as_str() {
                    "person" => PERSON_CLOCK,
                    "custom" => CUSTOM_CLOCK,
                    _ => anyhow::bail!("unknown attention family"),
                }
                .into(),
                old_keys: BTreeSet::new(),
                new_keys: BTreeSet::from([key]),
                evaluation_time_unix_ms: to,
            },
            smallclaims::ivm::SourceCut {
                local_generation: previous
                    .local_generation
                    .checked_add(1)
                    .context("local generation exhausted")?,
                ..previous
            },
        )?;
        anyhow::ensure!(
            changes.deferred.is_empty(),
            "attention clock operator is unavailable"
        );
        output.push((at.parse()?, name, source));
    }
    Ok(output)
}

fn require_ready(connection: &Connection, views: &Views, view: &str) -> Result<()> {
    let source = source_cut(connection)?.context("attention source unavailable")?;
    anyhow::ensure!(
        matches!(
            views.readiness(connection, view, source.epoch)?,
            smallclaims::ivm::Readiness::Ready(_)
        ),
        "attention family unavailable"
    );
    anyhow::ensure!(
        clean(connection, Some(view))?,
        "attention family has unflushed sources"
    );
    Ok(())
}

/// This is a family window, not the whole attention collection. Call inside the authorized
/// snapshot, after checking that this family's availability is Ready at the same boundary.
pub(crate) fn window(
    connection: &Connection,
    views: &Views,
    view: &str,
    person: &str,
    at: u128,
    limit: usize,
) -> Result<Vec<Value>> {
    require_ready(connection, views, view)?;
    let family = match view {
        PERSON_VIEW => "person",
        CUSTOM_VIEW => "custom",
        _ => anyhow::bail!("unknown attention family"),
    };
    anyhow::ensure!(limit <= 501, "attention window exceeds bound");
    let at = at.to_string();
    let rows = connection
        .prepare_cached(
            "SELECT body FROM local_attention_open WHERE family=?1 AND person=?2
      AND (length(eligible)<length(?3) OR (length(eligible)=length(?3) AND eligible<=?3))
      ORDER BY priority,length(requested) DESC,requested DESC,source,person,episode LIMIT ?4",
        )?
        .query_map(params![family, person, at, limit], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|text| serde_json::from_str(&text).map_err(Into::into))
        .collect()
}

pub(crate) fn row(
    connection: &Connection,
    views: &Views,
    view: &str,
    person: &str,
    id: &str,
    at: u128,
) -> Result<Option<Value>> {
    require_ready(connection, views, view)?;
    let family = match view {
        PERSON_VIEW => "person",
        CUSTOM_VIEW => "custom",
        _ => anyhow::bail!("unknown attention family"),
    };
    let at = at.to_string();
    let text: Option<String> = connection
        .query_row(
            "SELECT body FROM local_attention_open WHERE family=?1 AND person=?2 AND id=?3
      AND (length(eligible)<length(?4) OR (length(eligible)=length(?4) AND eligible<=?4))",
            params![family, person, id, at],
            |r| r.get(0),
        )
        .optional()?;
    text.map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}

#[cfg(test)]
mod tests;
