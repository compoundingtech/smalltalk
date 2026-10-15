//! Rebuildable latest observations. Dirty subjects are local work, never replicated identity.
use super::*;

const VERSION: &str = "2";
const PENDING_TRIGGER_VERSION: &str = "1";
const SCHEMA: &str = /* sqlite-large-value: allow-for-now lv-h9ee719c350ef owner=st3::store::resources TODO=chunk-or-reference-variable-columns */ r#"
CREATE TABLE IF NOT EXISTS resource_observations (
    subject TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    facts TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    opened_by TEXT,
    opened_by_run TEXT
);
CREATE INDEX IF NOT EXISTS resource_observations_kind ON resource_observations(kind, subject);
CREATE INDEX IF NOT EXISTS resource_observations_agent_subject ON resource_observations(opened_by, subject);
CREATE INDEX IF NOT EXISTS resource_observations_run_subject ON resource_observations(opened_by_run, subject);
CREATE TABLE IF NOT EXISTS local_resource_projection_pending (subject TEXT PRIMARY KEY);
"#;

// The existence probes use the pending subject primary key. Avoiding the duplicate INSERT
// entirely also works when the statement firing a trigger has its own conflict policy.
const PENDING_TRIGGERS: &str = /* sqlite-large-value: allow-for-now lv-hf047366caefc owner=st3::store::resources TODO=chunk-or-reference-exact-wire-values */ r#"
CREATE TRIGGER IF NOT EXISTS resource_observations_insert AFTER INSERT ON claims
WHEN NEW.kind='resource.observed' BEGIN
    INSERT INTO local_resource_projection_pending(subject)
    SELECT NEW.subject WHERE NOT EXISTS (
        SELECT 1 FROM local_resource_projection_pending WHERE subject=NEW.subject
    );
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_delete AFTER DELETE ON claims
WHEN OLD.kind='resource.observed' BEGIN
    INSERT INTO local_resource_projection_pending(subject)
    SELECT OLD.subject WHERE NOT EXISTS (
        SELECT 1 FROM local_resource_projection_pending WHERE subject=OLD.subject
    );
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT INTO local_resource_projection_pending(subject)
    SELECT claims.subject FROM claims WHERE claims.id=NEW.claim_id AND claims.kind='resource.observed'
        AND NOT EXISTS (
            SELECT 1 FROM local_resource_projection_pending WHERE subject=claims.subject
        );
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_record_update AFTER UPDATE ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT INTO local_resource_projection_pending(subject)
    SELECT claims.subject FROM claims WHERE claims.id=NEW.claim_id AND claims.kind='resource.observed'
        AND NOT EXISTS (
            SELECT 1 FROM local_resource_projection_pending WHERE subject=claims.subject
        );
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(/* sqlite-large-value: allow-for-now lv-h19d4181a4ef0 owner=st3::store::resources TODO=trace-statement-and-value-representation */ SCHEMA)
        .context("creating resource observation projection")?;
    connection
        .execute_batch(/* sqlite-large-value: allow-for-now lv-h2aa8751a32a3 owner=st3::store::resources TODO=trace-statement-and-value-representation */ PENDING_TRIGGERS)
        .context("creating resource observation pending triggers")
}

fn replace_pending_triggers(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        /* sqlite-large-value: allow-for-now lv-hc99d2e30ef7d owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "DROP TRIGGER IF EXISTS resource_observations_insert;
         DROP TRIGGER IF EXISTS resource_observations_delete;
         DROP TRIGGER IF EXISTS resource_observations_record_insert;
         DROP TRIGGER IF EXISTS resource_observations_record_update;",
    )?;
    transaction.execute_batch(/* sqlite-large-value: allow-for-now lv-haa580e36dc07 owner=st3::store::resources TODO=trace-statement-and-value-representation */ PENDING_TRIGGERS)?;
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let trigger_version: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key='resource_pending_trigger_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if trigger_version.as_deref() != Some(PENDING_TRIGGER_VERSION) {
        // CREATE TRIGGER IF NOT EXISTS in create_schema cannot upgrade a persisted trigger.
        // Replace all four with their marker in the same opening transaction.
        replace_pending_triggers(transaction)?;
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-hbcaf86516593 owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "INSERT OR REPLACE INTO meta(key,value) VALUES('resource_pending_trigger_version',?1)",
            [PENDING_TRIGGER_VERSION],
        )?;
    }
    let version: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key='resource_observations_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version.as_deref() != Some(VERSION) {
        rebuild(transaction)?;
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-h1a1bf8f88983 owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "INSERT OR REPLACE INTO meta(key,value) VALUES('resource_observations_version',?1)",
            [VERSION],
        )?;
    } else {
        flush(transaction)?;
    }
    Ok(())
}

pub(super) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(/* sqlite-large-value: allow-for-now lv-h0225de8fb1a0 owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM resource_observations", [])?;
    transaction.execute(/* sqlite-large-value: allow-for-now lv-hd60991382db6 owner=st3::store::resources TODO=prepare-body-and-predecessor-values */ "INSERT OR IGNORE INTO local_resource_projection_pending SELECT DISTINCT subject FROM claims WHERE kind='resource.observed'", [])?;
    flush(transaction)
}

pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let subjects = transaction
        .prepare("SELECT subject FROM local_resource_projection_pending ORDER BY subject")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in subjects {
        refresh(transaction, &subject)?;
    }
    Ok(())
}

/// Receipts add identity and the first opener; only observations replace resource facts.
pub(super) fn merge_observation(
    current: &mut serde_json::Map<String, Value>,
    fields: &serde_json::Map<String, Value>,
) {
    let carries_opener = fields.get("kind").and_then(Value::as_str).is_some_and(super::carries_opener);
    if carries_opener && fields.get("attribution_only") == Some(&Value::Bool(true)) {
        if let Some(kind) = fields.get("kind") {
            current.entry("kind").or_insert_with(|| kind.clone());
        }
        let nested = current.contains_key("facts") || current.len() == 1;
        let current = if nested {
            current.entry("facts").or_insert_with(|| json!({})).as_object_mut().unwrap()
        } else {
            current
        };
        if let Some(incoming) = fields.get("facts").and_then(Value::as_object) {
            if current.contains_key("opened_by") || current.contains_key("opened_by_run") {
                return;
            }
            let initial = current.is_empty();
            for name in ["repository", "number", "url", "opened_by", "opened_by_run"] {
                if (initial || matches!(name, "opened_by" | "opened_by_run")) && let Some(value) = incoming.get(name) {
                    current.entry(name).or_insert_with(|| value.clone());
                }
            }
        }
        return;
    }
    let mut opener = [None, None];
    if carries_opener {
        let previous = if current.get("facts").is_some_and(Value::is_object) {
            current.get_mut("facts").unwrap().as_object_mut().unwrap()
        } else {
            &mut *current
        };
        for (slot, name) in opener.iter_mut().zip(["opened_by", "opened_by_run"]) {
            *slot = previous.remove(name);
        }
    }
    for (key, value) in fields {
        if key != "attribution_only" {
            current.insert(key.clone(), value.clone());
        }
    }
    if opener.iter().any(Option::is_some) {
        let facts = if current.get("facts").is_some_and(Value::is_object) {
            current.get_mut("facts").unwrap().as_object_mut().unwrap()
        } else {
            current
        };
        facts.remove("opened_by");
        facts.remove("opened_by_run");
        for (value, name) in opener.into_iter().zip(["opened_by", "opened_by_run"]) {
            if let Some(value) = value {
                facts.insert(name.into(), value);
            }
        }
    }
}

pub(super) fn refresh(transaction: &Transaction<'_>, subject: &str) -> Result<()> {
    let claim = transaction.query_row(/* sqlite-large-value: allow-for-now lv-h54054df79be2 owner=st3::store::resources TODO=trace-statement-and-value-representation */ &canonical_sql(
        "SELECT id, body, accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='resource.observed'
         AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')
         ORDER BY COALESCE(json_extract(body, '$.fields.attribution_only'), 0), CANONICAL_DESC(claims) LIMIT 1"), [subject],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    ).optional()?;
    if let Some((id, body, accepted)) = claim {
        let mut body: Value = serde_json::from_str(&body)?;
        let fields: BTreeMap<String, Value> = match body.get_mut("fields").map(Value::take) {
            Some(Value::Null) | None => BTreeMap::new(),
            Some(fields) => serde_json::from_value(fields)?,
        };
        let mut facts = resource_facts(&fields).map_err(anyhow::Error::new)?;
        if fields.get("kind").and_then(Value::as_str).is_some_and(super::carries_opener) {
            let first_opener = transaction.query_row(
                /* sqlite-large-value: allow-for-now lv-h502d23053f3c owner=st3::store::resources TODO=trace-statement-and-value-representation */ &format!("SELECT body FROM claims WHERE subject=?1 AND kind='resource.observed'
                    AND (COALESCE(json_extract(body, '$.fields.facts.opened_by'), json_extract(body, '$.fields.opened_by')) IS NOT NULL
                         OR COALESCE(json_extract(body, '$.fields.facts.opened_by_run'), json_extract(body, '$.fields.opened_by_run')) IS NOT NULL)
                    AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')
                    ORDER BY {} LIMIT 1", canonical::order_sql("claims", false)),
                [subject], |row| row.get::<_, String>(0),
            ).optional()?;
            if let Some(opener) = first_opener {
                let mut opener: Value = serde_json::from_str(&opener)?;
                let opener_fields = serde_json::from_value(opener["fields"].take())?;
                let mut opener_facts = resource_facts(&opener_fields).map_err(anyhow::Error::new)?;
                facts.remove("opened_by");
                facts.remove("opened_by_run");
                for name in ["opened_by", "opened_by_run"] {
                    if let Some(value) = opener_facts.remove(name) {
                        facts.insert(name.into(), value);
                    }
                }
            }
        }
        let kind = fields
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // `fields.observed_at` has no declared unit and no producer; the admitted claim time is the
        // observation time every other resource reader uses.
        let observed_at = chrono::DateTime::from_timestamp_millis(accepted.parse()?)
            .context("resource observation timestamp outside supported range")?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-haf24edd52f45 owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO resource_observations(subject,claim_id,kind,facts,observed_at,opened_by,opened_by_run)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(subject) DO UPDATE SET
             claim_id=excluded.claim_id,kind=excluded.kind,facts=excluded.facts,observed_at=excluded.observed_at,
             opened_by=excluded.opened_by,opened_by_run=excluded.opened_by_run",
            params![subject, id, kind, canonical_json_text(&json!(facts))?, observed_at,
                facts.get("opened_by").and_then(Value::as_str), facts.get("opened_by_run").and_then(Value::as_str)],
        )?;
    } else {
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-hb483f28647aa owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM resource_observations WHERE subject=?1",
            [subject],
        )?;
    }
    transaction.execute(
        /* sqlite-large-value: allow-for-now lv-h6ac3a5b3ce70 owner=st3::store::resources TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM local_resource_projection_pending WHERE subject=?1",
        [subject],
    )?;
    Ok(())
}

fn prefix_successor(prefix: &str) -> Option<String> {
    for (index, character) in prefix.char_indices().rev() {
        let scalar = u32::from(character);
        if scalar == 0x10_ffff {
            continue;
        }
        let next = if scalar == 0xd7ff { 0xe000 } else { scalar + 1 };
        let mut upper = prefix[..index].to_owned();
        upper.push(char::from_u32(next).expect("incremented Unicode scalar"));
        return Some(upper);
    }
    None
}

impl Store {
    /// The cursor of the observer that last recorded into `resource`, for `observer` when it has
    /// none of its own: it starts where that observer left the resource, so what changed between
    /// their polls is read as a change rather than taken as already known.
    pub(crate) fn inherited_observer_cursor(
        &self,
        resource: &str,
        observer: &str,
    ) -> Result<Option<String>> {
        let prefix = format!("{resource}/");
        let upper = prefix_successor(&prefix).unwrap_or_default();
        let recorder = {
            let connection = self.readers.get();
            connection
                .query_row(
                    "SELECT json_extract(claims.body, '$.fields.observer')
                     FROM resource_observations JOIN claims ON claims.id=resource_observations.claim_id
                     WHERE resource_observations.subject=?1
                        OR (resource_observations.subject>=?2 AND resource_observations.subject<?3)
                     ORDER BY resource_observations.observed_at DESC LIMIT 1",
                    params![resource, prefix, upper],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
        };
        let Some(recorder) = recorder.filter(|recorder| recorder != observer) else {
            return Ok(None);
        };
        Ok(self
            .latest_claim(&recorder, Some("observer.observed"))?
            .and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/cursor")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }))
    }

    /// Constant-work version, independent of claims that do not change resource rows.
    pub(crate) fn resource_collection_version(&self) -> Result<String> {
        let connection = self.readers.get();
        let (columns, count, accumulator) = connection.query_row(
            "SELECT columns_json,row_count,accumulator FROM projection_digest_state WHERE table_name='resource_observations'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?, row.get::<_, Vec<u8>>(2)?)),
        )?;
        Ok(projection_digest::table_digest(
            "resource_observations",
            &columns,
            count,
            &accumulator,
        ))
    }

    /// Read at most `limit` current observations; callers request one extra row for pagination.
    pub(crate) fn resource_collection_page(
        &self,
        opened_by: Option<&str>,
        kind: Option<&str>,
        subject_prefix: Option<&str>,
        after_subject: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let mut query = "SELECT subject,kind,facts,observed_at,opened_by,opened_by_run FROM resource_observations WHERE 1=1".to_owned();
        let mut values = Vec::<rusqlite::types::Value>::new();
        if let Some(owner) = opened_by {
            query.push_str(if owner.starts_with("mission-run/") {
                " AND opened_by_run=?"
            } else {
                " AND opened_by=?"
            });
            values.push(owner.to_owned().into());
        }
        if let Some(kind) = kind {
            query.push_str(" AND kind=?");
            values.push(kind.to_owned().into());
        }
        if let Some(prefix) = subject_prefix {
            // BINARY UTF-8 ordering preserves Unicode scalar order, without LIKE wildcards.
            query.push_str(" AND subject>=?");
            values.push(prefix.to_owned().into());
            if let Some(upper) = prefix_successor(prefix) {
                query.push_str(" AND subject<?");
                values.push(upper.into());
            }
        }
        if let Some(after) = after_subject {
            query.push_str(" AND subject>?");
            values.push(after.to_owned().into());
        }
        query.push_str(" ORDER BY subject LIMIT ?");
        values.push(i64::try_from(limit)?.into());
        let connection = self.readers.get();
        connection
            .prepare(/* sqlite-large-value: allow-for-now lv-h88db06e93431 owner=st3::store::resources TODO=trace-statement-and-value-representation */ &query)?
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .map(|row| {
                let (id, kind, facts, observed_at, opened_by, opened_by_run) = row?;
                Ok(
                    json!({"id":id,"kind":kind,"facts":serde_json::from_str::<Value>(&facts)?,
                "observed_at":observed_at,"opened_by":opened_by,"opened_by_run":opened_by_run}),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(store: &Store, subject: &str, state: &str, owner: Option<&str>) {
        let mut facts = json!({"state": state, "opened_by_run": "mission-run/example"});
        if let Some(owner) = owner {
            facts["opened_by"] = json!(owner);
        }
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), json!("vcs.pull-request")),
                    ("facts".into(), facts),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[test]
    fn latest_resources_replace_rows_and_filter_conjunctively() {
        let store = Store::open_memory("resources").unwrap();
        observe(&store, "resource/pr/b", "open", Some("agent/one"));
        observe(&store, "resource/pr/a", "open", Some("agent/two"));
        observe(&store, "resource/pr/b", "closed", Some("agent/one"));
        let page = store
            .resource_collection_page(
                Some("agent/one"),
                Some("vcs.pull-request"),
                Some("resource/pr/"),
                None,
                10,
            )
            .unwrap();
        assert_eq!(
            page,
            vec![json!({
                "id": "resource/pr/b", "kind": "vcs.pull-request",
                "facts": {"state": "closed", "opened_by": "agent/one", "opened_by_run": "mission-run/example"},
                "observed_at": page[0]["observed_at"], "opened_by": "agent/one", "opened_by_run": "mission-run/example",
            })]
        );
        chrono::DateTime::parse_from_rfc3339(page[0]["observed_at"].as_str().unwrap()).unwrap();
        let page = store
            .resource_collection_page(Some("mission-run/example"), None, None, None, 1)
            .unwrap();
        assert_eq!(page[0]["id"], "resource/pr/a");
        let page = store
            .resource_collection_page(None, None, None, Some("resource/pr/a"), 10)
            .unwrap();
        assert_eq!(page[0]["id"], "resource/pr/b");
        assert!(
            store
                .resource_collection_page(Some("agent/one"), Some("ci.run"), None, None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .resource_collection_page(None, None, Some("resource/%"), None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resource_prefix_upper_bounds_follow_unicode_scalar_order() {
        assert_eq!(
            prefix_successor("resource/a").as_deref(),
            Some("resource/b")
        );
        assert_eq!(prefix_successor("a\u{d7ff}").as_deref(), Some("a\u{e000}"));
        assert_eq!(prefix_successor("a\u{10ffff}").as_deref(), Some("b"));
        assert_eq!(prefix_successor("\u{10ffff}"), None);
    }

    #[test]
    fn resource_projection_backfills_old_stores_and_rebuilds_lost_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.sqlite3");
        let store = Store::open(&path, "resources").unwrap();
        observe(&store, "resource/pr/a", "open", None);
        observe(&store, "resource/pr/a", "closed", None);
        let expected = store
            .resource_collection_page(None, None, None, None, 10)
            .unwrap();
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction
                .execute("DELETE FROM resource_observations", [])
                .unwrap();
            rebuild(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(
            store
                .resource_collection_page(None, None, None, None, 10)
                .unwrap(),
            expected
        );
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction
                .execute("DELETE FROM resource_observations", [])
                .unwrap();
            transaction
                .execute(
                    "DELETE FROM meta WHERE key='resource_observations_version'",
                    [],
                )
                .unwrap();
            transaction.commit().unwrap();
        }
        drop(store);
        let reopened = Store::open(&path, "resources").unwrap();
        assert_eq!(
            reopened
                .resource_collection_page(None, None, None, None, 10)
                .unwrap(),
            expected
        );
        assert_eq!(
            projection_digest::tables(&reopened.readers.get()).unwrap(),
            projection_digest::oracle(&reopened.readers.get()).unwrap()
        );
    }
}

#[cfg(test)]
mod replication_tests {
    use super::*;

    #[test]
    fn receipt_before_replicated_observation_keeps_opener_and_observed_facts() {
        const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
        let source = Store::open_memory("observer").unwrap();
        let target = Store::open_memory("recorder").unwrap();
        source.bind_fleet(FLEET).unwrap();
        target.bind_fleet(FLEET).unwrap();
        let subject = "resource/github/acme/demo/pull-request/9";
        let spool = tempfile::tempdir().unwrap();
        std::fs::write(spool.path().join("1.json"), serde_json::to_vec(&crate::recorder::Receipt {
            schema: "st3.recorder.receipt.v1".into(),
            url: "https://github.com/acme/demo/pull/9".into(),
            actor: "agent/node.builder".into(), mission_run: Some("created".into()),
            exit_code: Some(0), at: "2026-10-03T12:00:00Z".into(),
        }).unwrap()).unwrap();
        assert_eq!(crate::recorder_receipts::ingest_once(&target, spool.path()).unwrap(), 1);
        source.append_claim(&ClaimInput {
            subject: subject.into(), kind: "resource.observed".into(), actor: None,
            fields: BTreeMap::from([
                ("kind".into(), json!("vcs.pull-request")),
                ("facts".into(), json!({"number": 9, "state": "open", "title": "Observed",
                    "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})),
            ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let exchange = source.export_replication_exchange_answering(
            FLEET, &target.replication_inventory().unwrap(),
            &target.replication_signature_requests().unwrap(),
        ).unwrap();
        target.receive_replication_exchange(&source.origin, FLEET, &exchange).unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
        let facts = target.latest_actual_value(subject).unwrap().unwrap()["facts"].clone();
        assert_eq!(facts["opened_by"], "agent/node.builder");
        assert_eq!(facts["opened_by_run"], "mission-run/created");
        assert_eq!(facts["state"], "open");
        assert_eq!(facts["title"], "Observed");
        assert_eq!(facts["head_sha"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(target.resource_collection_page(None, None, Some(subject), None, 10).unwrap()[0]["facts"], facts);
        target.replay_replication_graph().unwrap();
        assert_eq!(target.latest_actual_value(subject).unwrap().unwrap()["facts"], facts);
        assert_eq!(target.resource_collection_page(None, None, Some(subject), None, 10).unwrap()[0]["facts"], facts);
    }

    #[test]
    fn resource_latest_converges_when_newer_envelopes_arrive_first() {
        const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
        let source = Store::open_memory("resource-source").unwrap();
        let target = Store::open_memory("resource-target").unwrap();
        source.bind_fleet(FLEET).unwrap();
        target.bind_fleet(FLEET).unwrap();
        for state in ["open", "closed"] {
            source
                .append_claim(&ClaimInput {
                    subject: "resource/pr/out-of-order".into(),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("kind".into(), json!("vcs.pull-request")),
                        ("facts".into(), json!({"state": state})),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let exchange = source
            .export_replication_exchange_answering(
                FLEET,
                &target.replication_inventory().unwrap(),
                &target.replication_signature_requests().unwrap(),
            )
            .unwrap();
        for envelope in exchange.envelopes.iter().rev() {
            let mut single = exchange.clone();
            single.envelopes = vec![envelope.clone()];
            target
                .receive_replication_exchange(&source.origin, FLEET, &single)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.project_replication_backlog().unwrap();
        }
        let expected = source
            .resource_collection_page(None, None, None, None, 10)
            .unwrap();
        assert_eq!(
            target
                .resource_collection_page(None, None, None, None, 10)
                .unwrap(),
            expected
        );
        assert_eq!(expected[0]["facts"]["state"], "closed");
        let version = target.resource_collection_version().unwrap();
        target.replay_replication_graph().unwrap();
        assert_eq!(target.resource_collection_version().unwrap(), version);
        assert_eq!(
            target
                .resource_collection_page(None, None, None, None, 10)
                .unwrap(),
            expected
        );
        assert_eq!(
            projection_digest::tables(&target.readers.get()).unwrap(),
            projection_digest::oracle(&target.readers.get()).unwrap()
        );
    }
}

#[cfg(test)]
mod pending_trigger_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const OLD_PENDING_TRIGGERS: &str = r#"
CREATE TRIGGER resource_observations_insert AFTER INSERT ON claims
WHEN NEW.kind='resource.observed' BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending VALUES(NEW.subject);
END;
CREATE TRIGGER resource_observations_delete AFTER DELETE ON claims
WHEN OLD.kind='resource.observed' BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending VALUES(OLD.subject);
END;
CREATE TRIGGER resource_observations_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending
    SELECT subject FROM claims WHERE id=NEW.claim_id AND kind='resource.observed';
END;
CREATE TRIGGER resource_observations_record_update AFTER UPDATE ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending
    SELECT subject FROM claims WHERE id=NEW.claim_id AND kind='resource.observed';
END;
"#;

    fn resource_claim(store: &Store, subject: &str) -> ClaimRecord {
        store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), json!("vcs.pull-request")),
                ("facts".into(), json!({"state": "open"})),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap()
    }

    fn pending_count(connection: &Connection, subject: &str) -> i64 {
        connection.query_row(
            "SELECT COUNT(*) FROM local_resource_projection_pending WHERE subject=?1",
            [subject], |row| row.get(0),
        ).unwrap()
    }

    fn record_upsert(connection: &Connection, claim_id: &str) -> rusqlite::Result<usize> {
        connection.execute(
            "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,
                 claim_id,updated_at_unix_ms)
             VALUES('record/pending-control','writer',1,'hash',0,X'00','valid',?1,'1')
             ON CONFLICT(record_ref) DO UPDATE SET claim_id=excluded.claim_id,
                 state=excluded.state,updated_at_unix_ms=excluded.updated_at_unix_ms",
            [claim_id],
        )
    }

    #[test]
    fn pending_subject_is_idempotent_across_claim_and_record_triggers() {
        let store = Store::open_memory("pending-control").unwrap();
        let subject = "resource/pending-control";
        let first = resource_claim(&store, subject);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute(
                "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)", [subject],
            ).unwrap();
            // A physical claim deletion fires the OLD-subject trigger while the key is pending.
            transaction.execute("DELETE FROM claims WHERE id=?1", [&first.id]).unwrap();
            assert_eq!(pending_count(&transaction, subject), 1);
            transaction.commit().unwrap();
        }
        let second = resource_claim(&store, subject);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute(
                "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)", [subject],
            ).unwrap();
            record_upsert(&transaction, &second.id).unwrap(); // AFTER INSERT
            record_upsert(&transaction, &second.id).unwrap(); // AFTER UPDATE
            assert_eq!(pending_count(&transaction, subject), 1);
            transaction.rollback().unwrap();
        }
        assert_eq!(store.resource_collection_page(None, None, Some(subject), None, 10).unwrap().len(), 1);
        assert_eq!(projection_digest::tables(&store.readers.get()).unwrap(),
                   projection_digest::oracle(&store.readers.get()).unwrap());
    }

    #[test]
    fn old_trigger_control_then_reopen_replaces_all_four_definitions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.sqlite3");
        let store = Store::open(&path, "old-pending-control").unwrap();
        let subject = "resource/old-pending-control";
        let claim = resource_claim(&store, subject);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute_batch(
                "DROP TRIGGER resource_observations_insert;
                 DROP TRIGGER resource_observations_delete;
                 DROP TRIGGER resource_observations_record_insert;
                 DROP TRIGGER resource_observations_record_update;
                 DELETE FROM meta WHERE key='resource_pending_trigger_version';",
            ).unwrap();
            transaction.execute_batch(OLD_PENDING_TRIGGERS).unwrap();
            for name in ["resource_observations_insert", "resource_observations_delete",
                         "resource_observations_record_insert", "resource_observations_record_update"] {
                let sql: String = transaction.query_row(
                    "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name=?1",
                    [name], |row| row.get(0),
                ).unwrap();
                assert!(sql.contains("INSERT OR IGNORE INTO local_resource_projection_pending"),
                    "old trigger missing its original INSERT OR IGNORE: {name}: {sql}");
                assert!(!sql.contains("NOT EXISTS"), "old trigger was already replaced: {name}: {sql}");
            }
            transaction.execute("INSERT INTO local_resource_projection_pending(subject) VALUES(?1)", [subject]).unwrap();
            assert_eq!(pending_count(&transaction, subject), 1);
            let prior_record: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM replica_records WHERE record_ref='record/pending-control'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(prior_record, 0);
            // Hosted #2352 at 553ee13d returned Ok(1) here on all three tries. This rejects
            // the proposed conflict mechanism for the old record INSERT branch.
            assert_eq!(record_upsert(&transaction, &claim.id).unwrap(), 1);
            assert_eq!(pending_count(&transaction, subject), 1);
            let record_claim: String = transaction.query_row(
                "SELECT claim_id FROM replica_records WHERE record_ref='record/pending-control'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(record_claim, claim.id);

            // This is a distinct, UNRUN old-trigger UPDATE hypothesis. A success must fail
            // this control and reject that mechanism; no either-outcome fallback is allowed.
            let update = record_upsert(&transaction, &claim.id);
            assert!(update.as_ref().is_err_and(|error| error.to_string().contains(
                "UNIQUE constraint failed: local_resource_projection_pending.subject")),
                "old UPDATE trigger did not reproduce the hypothesized pending conflict: {update:?}");

            // These old-trigger claim INSERT/DELETE expectations have not run on this source.
            // The lower-level graph append keeps pending seeded until the whole probe rolls back.
            let inserted = append_claim_record_tx(
                &transaction, &store.origin, subject, "resource.observed", None,
                &claim.body, &[], None,
            ).unwrap();
            assert_ne!(inserted.id, claim.id);
            assert_eq!(pending_count(&transaction, subject), 1);
            assert_eq!(transaction.execute("DELETE FROM claims WHERE id=?1", [&inserted.id]).unwrap(), 1);
            assert_eq!(pending_count(&transaction, subject), 1);

            transaction.rollback().unwrap();
        }
        // Persist the old definitions and missing marker separately from the rolled-back probe.
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute_batch(
                "DROP TRIGGER resource_observations_insert;
                 DROP TRIGGER resource_observations_delete;
                 DROP TRIGGER resource_observations_record_insert;
                 DROP TRIGGER resource_observations_record_update;
                 DELETE FROM meta WHERE key='resource_pending_trigger_version';",
            ).unwrap();
            transaction.execute_batch(OLD_PENDING_TRIGGERS).unwrap();
            transaction.commit().unwrap();
        }
        drop(store);
        let reopened = Store::open(&path, "old-pending-control").unwrap();
        let connection = reopened.readers.get();
        let version: String = connection.query_row(
            "SELECT value FROM meta WHERE key='resource_pending_trigger_version'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(version, PENDING_TRIGGER_VERSION);
        for name in ["resource_observations_insert", "resource_observations_delete",
                     "resource_observations_record_insert", "resource_observations_record_update"] {
            let sql: String = connection.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name=?1", [name], |row| row.get(0),
            ).unwrap();
            assert!(sql.contains("NOT EXISTS"), "{name}: {sql}");
        }
        assert_eq!(projection_digest::tables(&connection).unwrap(), projection_digest::oracle(&connection).unwrap());
    }

    #[test]
    fn fresh_store_reopen_keeps_trigger_marker_and_projection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fresh-resources.sqlite3");
        let first = Store::open(&path, "fresh-pending-control").unwrap();
        resource_claim(&first, "resource/fresh-pending-control");
        drop(first);
        let reopened = Store::open(&path, "fresh-pending-control").unwrap();
        let connection = reopened.readers.get();
        let version: String = connection.query_row(
            "SELECT value FROM meta WHERE key='resource_pending_trigger_version'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(version, PENDING_TRIGGER_VERSION);
        assert_eq!(reopened.resource_collection_page(None, None, Some("resource/fresh-pending-control"), None, 10).unwrap().len(), 1);
        assert_eq!(projection_digest::tables(&connection).unwrap(), projection_digest::oracle(&connection).unwrap());
    }

    #[test]
    fn old_trigger_open_revalidates_held_writer_observer_without_fault() {
        #[derive(Default)]
        struct ClaimsObserver {
            hints: AtomicU64,
            faults: Mutex<Vec<Arc<str>>>,
        }
        impl smallclaims::sqlite::WriterObserver for ClaimsObserver {
            fn tables(&self) -> &'static [(&'static str, &'static str)] {
                &[("main", "claims")]
            }
            fn mutated(&self, _database: &str, _table: &str) {}
            fn unobserved(&self, database: &str, table: &str) {
                assert_eq!((database, table), ("main", "claims"));
                self.hints.fetch_add(1, Ordering::Relaxed);
            }
            fn failed(&self, error: Arc<str>) {
                self.faults.lock().unwrap().push(error);
            }
            fn published(&self, connection: &Connection) {
                assert!(connection.is_autocommit());
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observed-old-resources.sqlite3");
        let old_store = Store::open(&path, "observed-pending-control").unwrap();
        resource_claim(&old_store, "resource/observed-pending-control");
        {
            let mut connection = old_store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute_batch(
                "DROP TRIGGER resource_observations_insert;
                 DROP TRIGGER resource_observations_delete;
                 DROP TRIGGER resource_observations_record_insert;
                 DROP TRIGGER resource_observations_record_update;
                 DELETE FROM meta WHERE key='resource_pending_trigger_version';",
            ).unwrap();
            transaction.execute_batch(OLD_PENDING_TRIGGERS).unwrap();
            transaction.commit().unwrap();
        }
        drop(old_store);

        // Use the held WriterConnection authorizer and publication path, not a second hook.
        let observer = Arc::new(ClaimsObserver::default());
        let writer = smallclaims::sqlite::WriterConnection::new_with_observer(
            Connection::open(&path).unwrap(), Arc::new(AtomicU64::new(0)), observer.clone(),
        ).unwrap();
        {
            let mut connection = writer.write();
            create_schema(&connection).unwrap();
            let transaction = connection.transaction().unwrap();
            open(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(observer.hints.load(Ordering::Relaxed), 1);
        assert!(observer.faults.lock().unwrap().is_empty());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(projection_digest::tables(&connection).unwrap(), projection_digest::oracle(&connection).unwrap());
    }

    #[test]
    fn envelope_savepoint_rollback_discards_pending_and_record_before_position_zero_label() {
        let store = Store::open_memory("pending-rollback-control").unwrap();
        let claim = resource_claim(&store, "resource/pending-rollback-control");
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        transaction.execute_batch("SAVEPOINT envelope_control").unwrap();
        record_upsert(&transaction, &claim.id).unwrap();
        assert_eq!(pending_count(&transaction, &claim.subject), 1);
        transaction.execute_batch("ROLLBACK TO envelope_control; RELEASE envelope_control").unwrap();
        assert_eq!(pending_count(&transaction, &claim.subject), 0);
        let retained: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM replica_records WHERE record_ref='record/pending-control'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(retained, 0);

        // This is the existing failure-label helper, not a signed receive or a claim attribution.
        let envelope = ReplicaEnvelope {
            writer: "writer".into(), sequence: 1, previous_hash: None,
            hash: "hash".into(), accepted_at_unix_ms: 1,
            payload: vec![1, 2, 3].into(), member_key: None, signature: None,
        };
        record_invalid_replica_envelope(
            &transaction, &envelope,
            &St3Error::new("internal", "pending control failure"),
        ).unwrap();
        let (position, claim_id, state): (i64, Option<String>, String) = transaction.query_row(
            "SELECT position,claim_id,state FROM replica_records WHERE writer='writer' AND sequence=1 AND envelope_hash='hash'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!((position, claim_id, state), (0, None, "invalid".into()));
        transaction.commit().unwrap();
    }

    #[test]
    #[ignore = "finite SQLite work cohort; source control only until governed execution"]
    fn pending_trigger_work_stays_indexed_as_unrelated_subjects_grow() {
        fn measured(unrelated: i64) -> smallclaims::sqlite::work::SqliteWork {
            let store = Store::open_memory("pending-work-control").unwrap();
            let claim = resource_claim(&store, "resource/pending-work-control");
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            transaction.execute(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1)
                 INSERT INTO local_resource_projection_pending(subject)
                 SELECT printf('resource/unrelated/%08d',x) FROM n",
                [unrelated],
            ).unwrap();
            transaction.execute(
                "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)",
                [&claim.subject],
            ).unwrap();
            let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
            record_upsert(&transaction, &claim.id).unwrap();
            let work = scope.finish();
            assert_eq!(pending_count(&transaction, &claim.subject), 1);
            transaction.rollback().unwrap();
            work
        }
        let small = measured(1_000);
        let large = measured(10_000);
        assert_eq!(small.statements, large.statements);
        assert_eq!(small.fullscan_steps, 0);
        assert_eq!(large.fullscan_steps, 0);
        assert!(large.vm_steps <= small.vm_steps * 2,
            "target answer is fixed; 1k={small:?}, 10k={large:?}");
    }
}
