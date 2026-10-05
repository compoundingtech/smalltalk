//! Rebuildable latest observations. Dirty subjects are local work, never replicated identity.
use super::*;

const VERSION: &str = "2";
const SCHEMA: &str = r#"
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
CREATE TRIGGER IF NOT EXISTS resource_observations_insert AFTER INSERT ON claims
WHEN NEW.kind='resource.observed' BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_delete AFTER DELETE ON claims
WHEN OLD.kind='resource.observed' BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending VALUES(OLD.subject);
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending
    SELECT subject FROM claims WHERE id=NEW.claim_id AND kind='resource.observed';
END;
CREATE TRIGGER IF NOT EXISTS resource_observations_record_update AFTER UPDATE ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_resource_projection_pending
    SELECT subject FROM claims WHERE id=NEW.claim_id AND kind='resource.observed';
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(SCHEMA)
        .context("creating resource observation projection")
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
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
            "INSERT OR REPLACE INTO meta(key,value) VALUES('resource_observations_version',?1)",
            [VERSION],
        )?;
    } else {
        flush(transaction)?;
    }
    Ok(())
}

pub(super) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute("DELETE FROM resource_observations", [])?;
    transaction.execute("INSERT OR IGNORE INTO local_resource_projection_pending SELECT DISTINCT subject FROM claims WHERE kind='resource.observed'", [])?;
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
    let claim = transaction.query_row(&canonical_sql(
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
                &format!("SELECT body FROM claims WHERE subject=?1 AND kind='resource.observed'
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
            "INSERT INTO resource_observations(subject,claim_id,kind,facts,observed_at,opened_by,opened_by_run)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(subject) DO UPDATE SET
             claim_id=excluded.claim_id,kind=excluded.kind,facts=excluded.facts,observed_at=excluded.observed_at,
             opened_by=excluded.opened_by,opened_by_run=excluded.opened_by_run",
            params![subject, id, kind, canonical_json_text(&json!(facts))?, observed_at,
                facts.get("opened_by").and_then(Value::as_str), facts.get("opened_by_run").and_then(Value::as_str)],
        )?;
    } else {
        transaction.execute(
            "DELETE FROM resource_observations WHERE subject=?1",
            [subject],
        )?;
    }
    transaction.execute(
        "DELETE FROM local_resource_projection_pending WHERE subject=?1",
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
            .prepare(&query)?
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
