//! Declared conditions, the state the graph holds for each instance, and what the evaluator
//! writes: one `condition.state` claim when something changed, and a message to an agent owner on
//! a transition.
//!
//! Local tables serve the readers and the evaluator. `local_condition_heads` points at the
//! newest state claim of each instance, so a read finds it with one lookup however long the
//! history; the evaluator folds new claims into it, its own and replicated ones, each tick.
//! `local_condition_claim_bytes` counts, per hour, the bytes of claims this member wrote, for
//! `db.authored-bytes-per-day`. The notification queue retries transitions. These tables do not replicate.

use super::*;
use crate::conditions::{ConditionDecl, Phase, Recorded, Tracker, Transition, parse_condition};

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_condition_observations (
    subject TEXT NOT NULL,
    instance TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY(subject, instance)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS desired_condition_index ON desired(subject) WHERE kind='condition';
CREATE TABLE IF NOT EXISTS local_condition_heads (
    subject TEXT NOT NULL,
    instance TEXT NOT NULL,
    store_index INTEGER NOT NULL,
    PRIMARY KEY (subject, instance)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_condition_notifications (
    store_index INTEGER PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS local_condition_claim_bytes (
    hour_unix_ms INTEGER PRIMARY KEY,
    claims INTEGER NOT NULL,
    bytes INTEGER NOT NULL
);
"#;

const HEADS_CURSOR: &str = "condition_heads_cursor";
const BYTES_CURSOR: &str = "condition_bytes_cursor";
const BYTES_SINCE: &str = "condition_bytes_since";
/// State claims one fold reads at most.
const HEADS_PAGE: i64 = 500;
/// Claims one byte-count read covers, by store index. A read of this many rows takes a few
/// milliseconds, so no read holds its connection long.
const BYTES_PAGE: i64 = 2_000;
/// Reads one tick makes at most while catching up on byte counts.
const BYTES_PAGES_PER_TICK: usize = 25;
const HOUR_MS: u128 = 3_600_000;
const DAY_MS: u128 = 24 * HOUR_MS;

/// A declared condition, or why its body no longer parses.
#[derive(Clone, Debug)]
pub struct DeclaredCondition {
    pub subject: String,
    pub decl: Result<ConditionDecl, String>,
}

/// The newest state the graph holds for one instance of a condition.
#[derive(Clone, Debug, Serialize)]
pub struct ConditionInstanceView {
    pub instance: String,
    pub host: String,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    pub values: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breach_since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<u64>,
    /// The state claim this view reads.
    pub claim: String,
    /// True only for this daemon's periodically refreshed observation.
    pub local_observation: bool,
}

/// A condition and the state of each of its instances.
#[derive(Clone, Debug, Serialize)]
pub struct ConditionView {
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    pub instances: Vec<ConditionInstanceView>,
}

/// What one evaluation writes for an instance.
pub struct ConditionRecord<'a> {
    pub decl: &'a ConditionDecl,
    pub host: &'a str,
    pub instance: &'a str,
    pub tracker: &'a Tracker,
    pub transition: Option<Transition>,
    pub now: u128,
}

#[derive(Clone, Copy)]
struct Notification<'a> {
    condition: &'a str,
    owner: &'a str,
    instance: &'a str,
    transition: Transition,
    breach_since: u128,
    title: &'a str,
    body: &'a str,
    evidence: Option<&'a str>,
}

fn meta_integer(connection: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(connection
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|value| value.parse().ok()))
}

fn set_meta_integer(transaction: &Transaction<'_>, key: &str, value: i64) -> rusqlite::Result<()> {
    transaction
        .execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value.to_string()],
        )
        .map(|_| ())
}

fn condition_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn condition_fields(record: &ConditionRecord<'_>) -> BTreeMap<String, Value> {
    let ConditionRecord {
        decl,
        host,
        instance,
        tracker,
        transition,
        now,
    } = record;
    let number = |value: f64| {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    };
    let mut fields = BTreeMap::from([
        ("condition".to_owned(), Value::String(decl.subject())),
        ("instance".to_owned(), Value::String((*instance).into())),
        ("host".to_owned(), Value::String((*host).into())),
        (
            "phase".to_owned(),
            Value::String(tracker.phase.as_str().into()),
        ),
        (
            "metric".to_owned(),
            Value::String(decl.metric.as_str().into()),
        ),
        (
            "comparison".to_owned(),
            Value::String(decl.comparison.as_str().into()),
        ),
        ("threshold".to_owned(), number(decl.threshold)),
        ("recover_at".to_owned(), number(decl.recover_at)),
        ("owner".to_owned(), Value::String(decl.owner.clone())),
        (
            "phase_since".to_owned(),
            Value::from(u64::try_from(tracker.phase_since).unwrap_or(u64::MAX)),
        ),
        (
            "measured_at".to_owned(),
            Value::from(
                u64::try_from(tracker.values.back().map(|(at, _)| *at).unwrap_or(*now))
                    .unwrap_or(u64::MAX),
            ),
        ),
        (
            "values".to_owned(),
            Value::Array(
                tracker
                    .values
                    .iter()
                    .map(|(at, value)| {
                        json!([u64::try_from(*at).unwrap_or(u64::MAX), number(*value)])
                    })
                    .collect(),
            ),
        ),
    ]);
    if let Some((_, value)) = tracker.values.back() {
        fields.insert("value".into(), number(*value));
    }
    if let Some(since) = tracker.breach_since {
        fields.insert(
            "breach_since".into(),
            Value::from(u64::try_from(since).unwrap_or(u64::MAX)),
        );
    }
    if let Some(transition) = transition {
        fields.insert(
            "transition".into(),
            Value::String(transition.as_str().into()),
        );
    }
    if let Some(transition) = transition {
        let (title, body) =
            crate::conditions::transition_text(decl, *transition, host, instance, tracker, *now);
        fields.insert("notification_title".into(), title.into());
        fields.insert("notification_body".into(), body.into());
    }
    fields
}

fn valid_notification_owner(owner: &str) -> bool {
    let name = owner.strip_prefix("agent/").unwrap_or("");
    !name.is_empty()
        && owner.len() <= 256
        && name.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        })
}

/// Keep the newest eight transition heads of each origin. A new mount must replace an
/// older retired mount, rather than being hidden forever behind its host's old heads.
fn upsert_condition_head(
    tx: &Transaction<'_>,
    condition: &str,
    instance: &str,
    index: i64,
) -> rusqlite::Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_condition_heads WHERE subject=?1 AND instance=?2)",
        params![condition, instance],
        |row| row.get(0),
    )?;
    if !exists {
        let origin: String = tx.query_row(
            "SELECT origin FROM claims WHERE store_index=?1",
            [index],
            |row| row.get(0),
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM local_condition_heads h JOIN claims c ON c.store_index=h.store_index WHERE h.subject=?1 AND c.origin=?2", params![condition, origin], |row| row.get(0))?;
        if count >= crate::conditions::MAX_INSTANCES as i64 {
            let (victim, victim_index): (String, i64) = tx.query_row("SELECT h.instance,h.store_index FROM local_condition_heads h JOIN claims c ON c.store_index=h.store_index WHERE h.subject=?1 AND c.origin=?2 ORDER BY length(c.accepted_at_unix_ms),c.accepted_at_unix_ms,c.id LIMIT 1", params![condition, origin], |row| Ok((row.get(0)?,row.get(1)?)))?;
            let newest: i64 = tx.query_row(&canonical_sql("SELECT claims.store_index FROM claims WHERE claims.store_index IN (?1,?2) ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![victim_index,index], |row| row.get(0))?;
            if newest != index {
                return Ok(());
            }
            tx.execute(
                "DELETE FROM local_condition_heads WHERE subject=?1 AND instance=?2",
                params![condition, victim],
            )?;
        } else {
            let total: i64 = tx.query_row(
                "SELECT COUNT(*) FROM local_condition_heads WHERE subject=?1",
                [condition],
                |row| row.get(0),
            )?;
            if total >= crate::conditions::MAX_REMOTE_INSTANCES as i64 {
                return Ok(());
            }
        }
    }
    tx.execute(&canonical_sql("INSERT INTO local_condition_heads(subject,instance,store_index) VALUES (?1,?2,?3) ON CONFLICT(subject,instance) DO UPDATE SET store_index=(SELECT claims.store_index FROM claims WHERE claims.store_index IN (local_condition_heads.store_index,excluded.store_index) ORDER BY CANONICAL_DESC(claims) LIMIT 1)"), params![condition,instance,index])?;
    Ok(())
}

impl Store {
    /// Every declared condition, in name order.
    pub fn declared_conditions(&self) -> Result<Vec<DeclaredCondition>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, body FROM desired WHERE kind='condition' ORDER BY subject LIMIT 33",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut conditions = Vec::new();
        for row in rows {
            let (subject, body) = row?;
            let mut decl = serde_json::from_str::<Value>(&body)
                .map_err(|error| error.to_string())
                .and_then(|desired| parse_condition(&subject, &desired));
            if conditions.len() >= crate::conditions::MAX_CONDITIONS {
                decl = Err(
                    "fleet condition limit (32) exceeded; this declaration is not evaluated".into(),
                );
            }
            conditions.push(DeclaredCondition { subject, decl });
        }
        Ok(conditions)
    }

    /// Seek directly to the newest claim of each bounded instance, skipping historical samples.
    /// State subjects have one instance each; existing subject indexes skip its entire history.
    pub fn seed_condition_heads(&self, refresh: bool) -> Result<()> {
        let declarations = self.declared_conditions()?;
        let digest = hex::encode(sha2::Sha256::digest(format!("{declarations:?}").as_bytes()));
        let seeded: Option<String> = self
            .readers
            .get()
            .query_row(
                "SELECT value FROM meta WHERE key='condition_heads_declared_digest_v3'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if !refresh && seeded.as_deref() == Some(&digest) {
            return Ok(());
        }
        let captured = i64::try_from(self.index()?)?;
        let mut heads = Vec::new();
        let now = condition_now();
        for condition in declarations
            .into_iter()
            .take(crate::conditions::MAX_CONDITIONS)
        {
            let prefix = crate::conditions::instance_subject_prefix(&condition.subject);
            let upper = format!("{prefix}~");
            let mut previous_origin = prefix.clone();
            // Skip an origin's entire namespace after eight instances. Excess claims from
            // one authenticated member cannot consume another member's startup seek budget.
            for _ in 0..(crate::conditions::MAX_REMOTE_INSTANCES / crate::conditions::MAX_INSTANCES)
            {
                let first: Option<String> = self.readers.get().query_row(
                    "SELECT subject FROM claims INDEXED BY claims_subject_kind_accepted_index
                     WHERE subject>?1 AND subject<?2 AND kind='condition.state' ORDER BY subject LIMIT 1",
                    params![previous_origin, upper], |row| row.get(0)).optional()?;
                let Some(first) = first else { break };
                let Some((origin_hash, _)) = first
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.split_once('/'))
                else {
                    break;
                };
                let origin_prefix = format!("{prefix}{origin_hash}/");
                let origin_upper = format!("{origin_prefix}~");
                previous_origin = origin_upper.clone();
                let mut previous_instance = origin_prefix;
                let mut candidates = Vec::new();
                // A bounded discovery allowance includes retired instance namespaces.
                for _ in 0..64 {
                    let subject: Option<String> = self.readers.get().query_row(
                        "SELECT subject FROM claims INDEXED BY claims_subject_kind_accepted_index
                         WHERE subject>?1 AND subject<?2 AND kind='condition.state' ORDER BY subject LIMIT 1",
                        params![previous_instance, origin_upper], |row| row.get(0)).optional()?;
                    let Some(subject) = subject else { break };
                    previous_instance = subject.clone();
                    let Some(claim) = self.latest_claim(&subject, Some("condition.state"))? else {
                        continue;
                    };
                    let fields = &claim.body["fields"];
                    let Some(instance) = fields["instance"].as_str() else {
                        continue;
                    };
                    if !crate::conditions::valid_state_identity(&subject, &claim.origin, fields)
                        || fields["condition"].as_str() != Some(condition.subject.as_str())
                    {
                        continue;
                    }
                    let notify = claim.origin == self.origin()
                        && fields["transition"].is_string()
                        && fields["owner"]
                            .as_str()
                            .is_some_and(|owner| owner.starts_with("agent/"))
                        && claim.accepted_at_unix_ms <= now
                        && now.saturating_sub(claim.accepted_at_unix_ms) <= 3_600_000;
                    candidates.push((
                        claim.accepted_at_unix_ms,
                        claim.id.clone(),
                        (
                            condition.subject.clone(),
                            instance.to_owned(),
                            i64::try_from(claim.store_index)?,
                            notify,
                        ),
                    ));
                }
                candidates.sort_by(|left, right| (right.0, &right.1).cmp(&(left.0, &left.1)));
                heads.extend(
                    candidates
                        .into_iter()
                        .take(crate::conditions::MAX_INSTANCES)
                        .map(|(_, _, head)| head),
                );
            }
        }
        self.connection
            .batched(|tx| tx.execute("DELETE FROM local_condition_heads WHERE subject NOT IN (SELECT subject FROM desired WHERE kind='condition')", []))
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        for page in heads.chunks(256) {
            let page = page.to_vec();
            self.connection.batched(move |tx| {
                for (subject, instance, index, notify) in page {
                    upsert_condition_head(tx, &subject, &instance, index)?;
                    if notify { tx.execute("INSERT OR IGNORE INTO local_condition_notifications(store_index) VALUES (?1)", [index])?; }
                }
                Ok::<_, rusqlite::Error>(())
            }).map_err(|error| anyhow::anyhow!("{error}"))??;
        }
        self.connection
            .batched(move |tx| {
                set_meta_integer(tx, HEADS_CURSOR, captured)?;
                tx.execute("INSERT INTO meta(key,value) VALUES ('condition_heads_declared_digest_v3',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [digest]).map(|_| ())
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// Latest routine values are local, fixed-size rows rather than replicated claims.
    pub fn record_condition_observations(&self, records: &[ConditionRecord<'_>]) -> Result<()> {
        anyhow::ensure!(
            records.len() <= crate::conditions::MAX_CONDITIONS * crate::conditions::MAX_INSTANCES,
            "condition sample budget exceeded"
        );
        let applicable = self
            .declared_conditions()?
            .into_iter()
            .filter(|condition| {
                condition
                    .decl
                    .as_ref()
                    .is_ok_and(|decl| decl.applies_to(self.origin()))
            })
            .map(|condition| condition.subject)
            .collect::<BTreeSet<_>>();
        let existing = self
            .readers
            .get()
            .prepare_cached("SELECT DISTINCT subject FROM local_condition_observations")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let retired = existing
            .into_iter()
            .filter(|subject| !applicable.contains(subject))
            .collect::<Vec<_>>();
        let rows = records
            .iter()
            .map(|record| {
                (
                    record.decl.subject(),
                    record.instance.to_owned(),
                    json!({"fields":condition_fields(record)}).to_string(),
                )
            })
            .collect::<Vec<_>>();
        self.connection.batched(move |tx| {
            for subject in retired { tx.execute("DELETE FROM local_condition_observations WHERE subject=?1", [subject])?; }
            for (subject, instance, body) in rows {
                tx.execute("INSERT INTO local_condition_observations(subject,instance,body) VALUES (?1,?2,?3)
                    ON CONFLICT(subject,instance) DO UPDATE SET body=excluded.body", params![subject,instance,body])?;
            }
            tx.execute("DELETE FROM local_condition_observations WHERE subject NOT IN (SELECT subject FROM desired WHERE kind='condition')", [])?;
            tx.execute("DELETE FROM local_condition_observations WHERE (subject,instance) IN (
                SELECT subject,instance FROM (SELECT subject,instance,ROW_NUMBER() OVER (PARTITION BY subject ORDER BY json_extract(body,'$.fields.measured_at') DESC,instance) AS rank FROM local_condition_observations) WHERE rank>8)", [])?;
            tx.execute("DELETE FROM local_condition_heads WHERE subject NOT IN (SELECT subject FROM desired WHERE kind='condition')", [])?;
            Ok::<_, rusqlite::Error>(())
        }).map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    pub fn condition_evaluator_status(&self) -> Result<(Option<i64>, Option<String>)> {
        let connection = self.readers.get();
        let at = meta_integer(&connection, "condition_evaluator_completed")?;
        let error = connection.query_row("SELECT value FROM meta WHERE key IN ('condition_evaluator_error','condition_notification_last_error') ORDER BY key LIMIT 1", [], |row| row.get(0)).optional()?;
        Ok((at, error))
    }

    pub fn note_condition_evaluator_status(&self, now: u128, error: Option<&str>) -> Result<()> {
        let at = i64::try_from(now)?;
        let error = error.map(|error| error.chars().take(1024).collect::<String>());
        self.connection.batched(move |tx| {
            set_meta_integer(tx, "condition_evaluator_completed", at)?;
            if let Some(error) = error {
                tx.execute("INSERT INTO meta(key,value) VALUES ('condition_evaluator_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [error])?;
            } else { tx.execute("DELETE FROM meta WHERE key='condition_evaluator_error'", [])?; }
            Ok::<_, rusqlite::Error>(())
        }).map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// Point each instance's head at its newest state claim, a page of new claims at a time.
    /// Returns how many claims it read; fewer than a page means it has caught up.
    pub fn fold_condition_heads(&self) -> Result<usize> {
        let (cursor, rows) = {
            let connection = self.readers.get();
            let cursor = meta_integer(&connection, HEADS_CURSOR)?.unwrap_or(0);
            let mut statement = connection.prepare_cached(
                "SELECT store_index, json_extract(body, '$.fields.condition'), json_extract(body, '$.fields.instance'), body, origin, accepted_at_unix_ms
                   FROM claims
                  WHERE kind='condition.state' AND store_index > ?1
                  ORDER BY store_index LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![cursor, HEADS_PAGE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            (cursor, rows)
        };
        let Some(last) = rows.last().map(|(index, ..)| *index) else {
            return Ok(0);
        };
        let count = rows.len();
        let host = self.origin().to_owned();
        let declared = self
            .declared_conditions()?
            .into_iter()
            .take(crate::conditions::MAX_CONDITIONS)
            .map(|condition| condition.subject)
            .collect::<BTreeSet<_>>();
        self.connection
            .batched(move |transaction| {
                for (index, subject, instance, body, origin, accepted) in &rows {
                    let Some(instance) = instance else { continue };
                    if !declared.contains(subject) { continue; }
                    if let Ok(body) = serde_json::from_str::<Value>(body) {
                        let fields = &body["fields"];
                        if !crate::conditions::valid_state_identity(&crate::conditions::instance_subject_with_origin(subject, origin, instance), origin, fields) { continue; }
                        if origin == &host && fields["host"] == host
                            && accepted.parse::<u128>().is_ok_and(|at| at <= condition_now() && condition_now().saturating_sub(at) <= 3_600_000) && fields["transition"].is_string()
                            && fields["owner"].as_str().is_some_and(|owner| owner.starts_with("agent/")) {
                            transaction.execute("INSERT OR IGNORE INTO local_condition_notifications(store_index) VALUES (?1)", [index])?;
                        }
                    }
                    upsert_condition_head(transaction, subject, instance, *index)?;
                }
                set_meta_integer(transaction, HEADS_CURSOR, last.max(cursor))
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(count)
    }

    /// The newest state claim of every instance of `subject`, from the heads the evaluator keeps.
    fn condition_instances(
        &self,
        connection: &Connection,
        subject: &str,
        local: bool,
        host: Option<&str>,
        breached_only: bool,
    ) -> Result<Vec<ConditionInstanceView>> {
        let mut statement = connection.prepare_cached(
            "SELECT claims.id, claims.body
               FROM local_condition_heads heads
               JOIN claims ON claims.store_index=heads.store_index
              WHERE heads.subject=?1 AND claims.origin=json_extract(claims.body,'$.fields.host') AND (heads.instance=claims.origin OR substr(heads.instance,1,length(claims.origin)+1)=claims.origin||':') AND (?2 IS NULL OR heads.instance=?2 OR (heads.instance>=?2||':' AND heads.instance<?2||';'))
              AND (?3=0 OR json_extract(claims.body,'$.fields.phase') IN ('breach','recovering')) ORDER BY heads.instance LIMIT 256",
        )?;
        let mut rows = statement
            .query_map(params![subject, host, breached_only], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if local {
            let mut local_rows = connection.prepare_cached("SELECT body FROM local_condition_observations WHERE subject=?1 ORDER BY instance LIMIT 8")?;
            for body in local_rows.query_map([subject], |row| row.get::<_, String>(0))? {
                rows.push((String::new(), body?));
            }
        }
        let mut instances = BTreeMap::new();
        for row in rows {
            let (claim, body) = row;
            let Ok(body) = serde_json::from_str::<Value>(&body) else {
                continue;
            };
            let fields = &body["fields"];
            let text = |name: &str| fields[name].as_str().map(str::to_owned);
            let instance = text("instance").unwrap_or_default();
            let local_observation = claim.is_empty();
            let claim = if claim.is_empty() {
                instances
                    .get(&instance)
                    .map(|view: &ConditionInstanceView| view.claim.clone())
                    .unwrap_or_default()
            } else {
                claim
            };
            instances.insert(
                instance,
                ConditionInstanceView {
                    instance: text("instance").unwrap_or_default(),
                    host: text("host").unwrap_or_default(),
                    phase: text("phase").unwrap_or_default(),
                    transition: text("transition"),
                    value: fields["value"].as_f64(),
                    values: fields["values"].as_array().cloned().unwrap_or_default(),
                    phase_since: fields["phase_since"].as_u64(),
                    breach_since: fields["breach_since"].as_u64(),
                    measured_at: fields["measured_at"].as_u64(),
                    claim,
                    local_observation,
                },
            );
        }
        let mut candidates = instances.into_values().collect::<Vec<_>>();
        candidates.sort_by_key(|view| {
            (
                view.host.clone(),
                !view.local_observation,
                std::cmp::Reverse(view.measured_at),
            )
        });
        let mut counts = BTreeMap::<String, usize>::new();
        candidates.retain(|view| {
            let count = counts.entry(view.host.clone()).or_default();
            *count += 1;
            *count <= crate::conditions::MAX_INSTANCES
        });
        candidates.sort_by(|left, right| left.instance.cmp(&right.instance));
        Ok(candidates)
    }

    /// Every declared condition with the newest state of each instance. A read: it writes
    /// nothing, and an instance whose claim has not been folded yet shows once the evaluator has.
    pub fn conditions(&self) -> Result<Vec<ConditionView>> {
        self.conditions_for(None, true, false, None)
    }

    pub fn conditions_local(&self, host: &str) -> Result<Vec<ConditionView>> {
        self.conditions_for(None, true, false, Some(host))
    }

    fn conditions_for(
        &self,
        person: Option<&str>,
        local: bool,
        person_owners_only: bool,
        host: Option<&str>,
    ) -> Result<Vec<ConditionView>> {
        let declared = self.declared_conditions()?;
        let connection = self.readers.get();
        let mut views = Vec::new();
        for condition in declared {
            if person_owners_only
                && !condition
                    .decl
                    .as_ref()
                    .is_ok_and(|decl| decl.owner.starts_with("person/"))
            {
                continue;
            }
            if person.is_some_and(|person| {
                condition
                    .decl
                    .as_ref()
                    .is_ok_and(|decl| decl.owner != person)
            }) {
                continue;
            }
            let mut instances = if host.is_some_and(|host| {
                condition
                    .decl
                    .as_ref()
                    .is_ok_and(|decl| !decl.applies_to(host))
            }) {
                Vec::new()
            } else {
                self.condition_instances(
                    &connection,
                    &condition.subject,
                    local,
                    host,
                    person_owners_only,
                )?
            };
            if let Ok(decl) = &condition.decl {
                instances.retain(|instance| decl.applies_to(&instance.host));
            }
            let view = match &condition.decl {
                Ok(decl) => ConditionView {
                    subject: condition.subject.clone(),
                    metric: Some(decl.metric.as_str().into()),
                    scope: Some(decl.scope.as_str().into()),
                    rule: Some(decl.describe_rule()),
                    owner: Some(decl.owner.clone()),
                    hosts: (!decl.hosts.is_empty()).then(|| decl.hosts.clone()),
                    invalid: None,
                    instances,
                },
                Err(error) => ConditionView {
                    subject: condition.subject.clone(),
                    metric: None,
                    scope: None,
                    rule: None,
                    owner: None,
                    hosts: None,
                    invalid: Some(error.clone()),
                    instances,
                },
            };
            views.push(view);
        }
        Ok(views)
    }

    /// What the graph last recorded for each instance `host` evaluates, so a restarted
    /// evaluator continues instead of announcing a breach again.
    pub fn condition_trackers(&self, host: &str) -> Result<BTreeMap<(String, String), Tracker>> {
        self.condition_trackers_at(host, condition_now())
    }

    pub fn condition_trackers_at(
        &self,
        host: &str,
        now: u128,
    ) -> Result<BTreeMap<(String, String), Tracker>> {
        let mut trackers = BTreeMap::new();
        for condition in self.conditions_for(None, false, false, Some(host))? {
            for instance in condition.instances {
                if instance.host != host {
                    continue;
                }
                let Some(phase) = Phase::parse(&instance.phase) else {
                    continue;
                };
                let recorded = Recorded {
                    at: u128::from(instance.measured_at.unwrap_or(0)).min(now),
                    phase,
                    value: instance.value.unwrap_or(f64::NAN),
                };
                trackers.insert(
                    (condition.subject.clone(), instance.instance.clone()),
                    Tracker::restore(phase, instance.breach_since.map(u128::from), recorded),
                );
            }
        }
        Ok(trackers)
    }

    pub fn condition_tracker(&self, condition: &str, instance: &str, now: u128) -> Result<Tracker> {
        let subject =
            crate::conditions::instance_subject_with_origin(condition, self.origin(), instance);
        let Some(claim) = self.latest_claim(&subject, Some("condition.state"))? else {
            return Ok(Tracker::default());
        };
        let fields = &claim.body["fields"];
        if claim.origin != self.origin()
            || !crate::conditions::valid_state_identity(&subject, &claim.origin, fields)
        {
            return Ok(Tracker::default());
        }
        let phase =
            Phase::parse(fields["phase"].as_str().unwrap_or("clear")).unwrap_or(Phase::Clear);
        Ok(Tracker::restore(
            phase,
            fields["breach_since"].as_u64().map(u128::from),
            Recorded {
                at: u128::from(fields["measured_at"].as_u64().unwrap_or(0)).min(now),
                phase,
                value: fields["value"].as_f64().unwrap_or(f64::NAN),
            },
        ))
    }

    /// Write one instance's state, with its transition when it has one. Returns the claim.
    pub fn record_condition_state(&self, record: &ConditionRecord<'_>) -> Result<ClaimRecord> {
        let ConditionRecord {
            decl,
            instance,
            now,
            ..
        } = record;
        let fields = condition_fields(record);
        let subject = crate::conditions::instance_subject_with_origin(
            &decl.subject(),
            self.origin(),
            instance,
        );
        anyhow::ensure!(
            crate::conditions::valid_state_identity(&subject, self.origin(), &json!(fields)),
            "invalid condition state identity"
        );
        let claim = self.append_claim(&ClaimInput {
            subject,
            kind: "condition.state".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            // A retried write after a timeout lands once.
            idempotency_key: Some(format!(
                "condition-state:{}:{instance}:{now}",
                decl.subject()
            )),
        })?;
        Ok(claim)
    }

    /// Tell the recorded owner once. Historical text and owner remain stable across retries.
    fn send_condition_notification(
        &self,
        notification: &Notification<'_>,
    ) -> Result<Option<String>> {
        let Notification {
            condition,
            owner,
            instance,
            transition,
            breach_since,
            title,
            body,
            evidence,
        } = *notification;
        if !owner.starts_with("agent/") {
            return Ok(None);
        }
        anyhow::ensure!(
            valid_notification_owner(owner),
            "invalid condition notification owner"
        );
        let key = format!(
            "condition:{}:{instance}:{}:{breach_since}",
            condition,
            transition.as_str()
        );
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let subject = format!("message/condition-{}", &digest[..20]);
        if self.latest_claim(&subject, Some("message.sent"))?.is_some() {
            return Ok(None);
        }
        self.append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("daemon/runtime".into())),
                ("to".into(), Value::String(owner.to_owned())),
                ("content".into(), Value::String(body.into())),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String(title.into())),
                ("in_reply_to".into(), Value::Null),
                (
                    "tags".into(),
                    json!([
                        format!("st3-condition:{}", condition),
                        format!("st3-condition-transition:{}", transition.as_str()),
                    ]),
                ),
            ]),
            evidence: evidence.into_iter().map(str::to_owned).collect(),
            expected_subject: None,
            idempotency_key: Some(key),
        })?;
        Ok(Some(subject))
    }

    /// Replay recorded transitions after a restart. Sending and queue removal may be interrupted;
    /// the deterministic message key prevents a second wake when that happens.
    pub fn flush_condition_notifications(&self) -> Result<Vec<String>> {
        let rows = {
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(
                "SELECT c.store_index, c.id, json_extract(c.body,'$.fields.condition'), c.body FROM local_condition_notifications n
                 JOIN claims c ON c.store_index=n.store_index ORDER BY c.store_index LIMIT 16",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut messages = Vec::new();
        let mut had_error = false;
        let mut owner_counts = BTreeMap::<String, usize>::new();
        for (index, claim, subject, body) in rows {
            if let Ok(parsed) = serde_json::from_str::<Value>(&body) {
                let owner = parsed["fields"]["owner"].as_str().unwrap_or("").to_owned();
                let count = owner_counts.entry(owner).or_default();
                if *count >= 4 {
                    continue;
                }
                *count += 1;
            }
            let mut malformed = false;
            let outcome = (|| -> Result<Option<String>> {
                let body: Value = match serde_json::from_str(&body) {
                    Ok(body) => body,
                    Err(error) => {
                        malformed = true;
                        return Err(error.into());
                    }
                };
                let fields = &body["fields"];
                let origin: String = self.readers.get().query_row(
                    "SELECT origin FROM claims WHERE store_index=?1",
                    [index],
                    |row| row.get(0),
                )?;
                if origin != self.origin() || fields["host"].as_str() != Some(self.origin()) {
                    malformed = true;
                    anyhow::bail!("notification origin is not this member");
                }
                let transition = match fields["transition"].as_str() {
                    Some("enter") => Transition::Enter,
                    Some("recover") => Transition::Recover,
                    _ => {
                        malformed = true;
                        anyhow::bail!("invalid transition in notification queue");
                    }
                };
                let owner = fields["owner"].as_str().unwrap_or("");
                if owner.starts_with("agent/") && !valid_notification_owner(owner) {
                    malformed = true;
                    anyhow::bail!("invalid condition notification owner");
                }
                self.send_condition_notification(&Notification {
                    condition: &subject,
                    owner: fields["owner"].as_str().unwrap_or(""),
                    instance: fields["instance"].as_str().unwrap_or(""),
                    transition,
                    breach_since: u128::from(fields["breach_since"].as_u64().unwrap_or(0)),
                    title: fields["notification_title"]
                        .as_str()
                        .unwrap_or("Condition changed"),
                    body: fields["notification_body"]
                        .as_str()
                        .unwrap_or("Inspect the recorded condition state."),
                    evidence: Some(&claim),
                })
            })();
            let attempt_key = format!("condition_notification_attempt:{index}");
            let mut remove = true;
            match outcome {
                Ok(message) => messages.extend(message),
                Err(error) => {
                    had_error = true;
                    if !malformed {
                        // Storage failures cannot establish that this notification's data
                        // is poison. Retain it and report the failure for the next tick.
                        return Err(error);
                    }
                    let attempts =
                        meta_integer(&self.readers.get(), &attempt_key)?.unwrap_or(0) + 1;
                    remove = attempts >= 3;
                    let detail = format!(
                        "condition notification {claim}: {error:#}; attempt {attempts}/3{}",
                        if remove {
                            "; discarded; inspect condition state"
                        } else {
                            ""
                        }
                    );
                    tracing::warn!(%detail, "condition notification");
                    let key = attempt_key.clone();
                    self.connection.batched(move |tx| {
                        set_meta_integer(tx, &key, attempts)?;
                        tx.execute("INSERT INTO meta(key,value) VALUES ('condition_notification_last_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [detail])?;
                        Ok::<_, rusqlite::Error>(())
                    }).map_err(|error| anyhow::anyhow!("{error}"))??;
                }
            }
            if remove {
                self.connection
                    .batched(move |tx| {
                        tx.execute(
                            "DELETE FROM local_condition_notifications WHERE store_index=?1",
                            [index],
                        )?;
                        tx.execute("DELETE FROM meta WHERE key=?1", [attempt_key])?;
                        Ok::<_, rusqlite::Error>(())
                    })
                    .map_err(|error| anyhow::anyhow!("{error}"))??;
            }
        }
        if !had_error {
            self.connection
                .batched(|tx| {
                    tx.execute(
                        "DELETE FROM meta WHERE key='condition_notification_last_error'",
                        [],
                    )
                })
                .map_err(|error| anyhow::anyhow!("{error}"))??;
        }
        Ok(messages)
    }

    /// A breach owned by a person is an alert on their home while it lasts. It is derived from
    /// the newest state claim, so it clears when the condition recovers, and a read never writes.
    pub(crate) fn condition_attention_items(
        &self,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        let mut items = Vec::new();
        for condition in self.conditions_for(person, true, true, None)? {
            let Some(owner) = condition.owner.as_deref() else {
                continue;
            };
            if !owner.starts_with("person/") || person.is_some_and(|person| person != owner) {
                continue;
            }
            for instance in &condition.instances {
                if !Phase::parse(&instance.phase).is_some_and(Phase::in_breach) {
                    continue;
                }
                let since = instance.breach_since.unwrap_or_default();
                let episode = hex::encode(sha2::Sha256::digest(
                    format!("{}:{}:{since}", condition.subject, instance.instance).as_bytes(),
                ));
                let value = instance
                    .value
                    .map(|value| format!("{value}"))
                    .unwrap_or_else(|| "unknown".into());
                let name = condition
                    .subject
                    .strip_prefix("condition/")
                    .unwrap_or(&condition.subject);
                let stale = instance.local_observation
                    && instance.measured_at.is_some_and(|at| {
                        condition_now().saturating_sub(u128::from(at))
                            > crate::conditions::STALE_AFTER_MS
                    });
                items.push(AttentionItemView {
                    episode: format!("condition/{}", &episode[..32]),
                    priority: "high".into(),
                    kind: "condition".into(),
                    review_mode: None,
                    subject: condition.subject.clone(),
                    person: owner.to_owned(),
                    requester_id: None,
                    launch_id: None,
                    variant_id: None,
                    message_id: None,
                    conversation: None,
                    title: format!("Condition breached: {name} on {}", crate::conditions::display_text(&instance.instance)),
                    detail: format!(
                        "{} is {value} on {} since {}. Rule: {}{}. It clears once the condition recovers; `st conditions show {name}` shows the recent values.",
                        condition.metric.as_deref().unwrap_or("the metric"),
                        instance.instance,
                        crate::conditions::utc(u128::from(since)),
                        condition.rule.as_deref().unwrap_or("unknown"),
                        if stale { "; this observation is stale; check the evaluating host" } else { "" },
                    ),
                    request: None,
                    answers: Vec::new(),
                    mission: None,
                    mission_run: None,
                    step: None,
                    targets: vec![condition.subject.clone()],
                    requested_at_unix_ms: u128::from(since),
                    actions: Vec::new(),
                });
            }
        }
        Ok(items)
    }

    /// Count the bytes of claims this member wrote since the last count, a bounded number of short
    /// reads per call. The first count starts a day back.
    pub fn fold_condition_claim_bytes(&self, now: u128) -> Result<()> {
        let origin = self.origin().to_owned();
        let (mut cursor, since) = {
            let connection = self.readers.get();
            (
                meta_integer(&connection, BYTES_CURSOR)?,
                meta_integer(&connection, BYTES_SINCE)?,
            )
        };
        let mut since = since;
        if cursor.is_none() {
            let start = now.saturating_sub(DAY_MS);
            let connection = self.readers.get();
            // SQLite cannot turn a row-value comparison containing an expression into a
            // two-column range seek. Seek the matching digit length first, then longer
            // timestamps only when that range is empty.
            let same_length = connection
                .query_row(
                    "SELECT store_index FROM claims INDEXED BY claims_accepted_order_index
                 WHERE length(accepted_at_unix_ms)=length(?1) AND accepted_at_unix_ms>=?1
                 ORDER BY length(accepted_at_unix_ms),accepted_at_unix_ms,store_index LIMIT 1",
                    [start.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            let first = match same_length {
                Some(index) => Some(index),
                None => connection
                    .query_row(
                        "SELECT store_index FROM claims INDEXED BY claims_accepted_order_index
                     WHERE length(accepted_at_unix_ms)>length(?1)
                     ORDER BY length(accepted_at_unix_ms),accepted_at_unix_ms,store_index LIMIT 1",
                        [start.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?,
            };
            cursor = Some(first.map_or(self.index()? as i64, |index| index - 1));
            since = Some(i64::try_from(start).unwrap_or(i64::MAX));
        }
        let mut cursor = cursor.unwrap_or(0);
        let top = self.index()? as i64;
        let mut hours = BTreeMap::<i64, (i64, i64)>::new();
        for _ in 0..BYTES_PAGES_PER_TICK {
            if cursor >= top {
                break;
            }
            let end = (cursor + BYTES_PAGE).min(top);
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(
                "SELECT accepted_at_unix_ms,
                        length(CAST(body AS BLOB)) + length(CAST(id AS BLOB)) + length(CAST(subject AS BLOB)) + length(CAST(kind AS BLOB)) + length(CAST(predecessors AS BLOB))
                   FROM claims
                  WHERE store_index > ?1 AND store_index <= ?2 AND origin=?3",
            )?;
            let rows = statement.query_map(params![cursor, end, origin], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (accepted, bytes) = row?;
                let accepted = accepted.parse::<i64>().unwrap_or(0);
                let hour = accepted - accepted.rem_euclid(HOUR_MS as i64);
                let entry = hours.entry(hour).or_default();
                entry.0 += 1;
                entry.1 += bytes;
            }
            cursor = end;
        }
        let since = since.unwrap_or(0);
        let oldest = i64::try_from(now.saturating_sub(2 * DAY_MS)).unwrap_or(0);
        self.connection
            .batched(move |transaction| {
                for (hour, (claims, bytes)) in &hours {
                    transaction.execute(
                        "INSERT INTO local_condition_claim_bytes(hour_unix_ms, claims, bytes)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(hour_unix_ms) DO UPDATE
                           SET claims=claims+excluded.claims, bytes=bytes+excluded.bytes",
                        params![hour, claims, bytes],
                    )?;
                }
                transaction.execute(
                    "DELETE FROM local_condition_claim_bytes WHERE hour_unix_ms < ?1",
                    [oldest],
                )?;
                set_meta_integer(transaction, BYTES_CURSOR, cursor)?;
                set_meta_integer(transaction, BYTES_SINCE, since)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// Bytes of claims this member wrote in the last 24 hours. Until a day has been counted, the
    /// hours counted so far are scaled to a day; under an hour of counting has no reading, and
    /// neither has a count still catching up.
    pub fn condition_claim_bytes_per_day(&self, now: u128) -> Result<Option<f64>> {
        let connection = self.readers.get();
        let (Some(cursor), Some(since)) = (
            meta_integer(&connection, BYTES_CURSOR)?,
            meta_integer(&connection, BYTES_SINCE)?,
        ) else {
            return Ok(None);
        };
        if (cursor as u64) + (BYTES_PAGE as u64) < self.index()? {
            return Ok(None);
        }
        let start = now.saturating_sub(DAY_MS);
        let counted_from = start.max(u128::try_from(since).unwrap_or(0));
        let covered = now.saturating_sub(counted_from);
        if covered < HOUR_MS {
            return Ok(None);
        }
        // The hour that holds the start counts whole: hourly buckets cannot split it.
        let first_hour = start - start % HOUR_MS;
        let bytes: i64 = connection.query_row(
            "SELECT coalesce(sum(bytes), 0) FROM local_condition_claim_bytes WHERE hour_unix_ms >= ?1",
            [i64::try_from(first_hour).unwrap_or(0)],
            |row| row.get(0),
        )?;
        Ok(Some(bytes as f64 * DAY_MS as f64 / covered as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conditions::{Phase, Tracker};
    use crate::graph::parse_test_intent as parse_intent;

    const SOURCE: &str = r#"version 2
condition "fleet/disk" {
  metric "disk.free-percent"
  scope "host"
  below 15
  recover 18
  for "10m"
  owner "agent/ops"
}
condition "fleet/collector-cpu" {
  metric "process.cpu-cores"
  scope "process"
  process "collector"
  host "alder"
  above 1.5
  for "5m"
  owner "person/ada"
}
"#;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
        let intent = parse_intent(SOURCE, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: SOURCE.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "conditions",
                Some("person/ada"),
            )
            .unwrap();
        (directory, store)
    }

    fn decl(store: &Store, subject: &str) -> ConditionDecl {
        store
            .declared_conditions()
            .unwrap()
            .into_iter()
            .find(|condition| condition.subject == subject)
            .unwrap()
            .decl
            .unwrap()
    }

    fn record(
        store: &Store,
        decl: &ConditionDecl,
        instance: &str,
        tracker: &Tracker,
        transition: Option<Transition>,
        now: u128,
    ) -> ClaimRecord {
        store
            .record_condition_state(&ConditionRecord {
                decl,
                host: "alder",
                instance,
                tracker,
                transition,
                now,
            })
            .unwrap()
    }

    #[test]
    fn transitions_survive_a_restart_and_retry_without_duplicate_wakes() {
        let (directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 1_000);
        let enter = tracker.observe(&disk, 10.0, 601_000).unwrap();
        let claim = record(&store, &disk, "alder:/", &tracker, Some(enter), 601_000);
        // The state write committed but the evaluator has not folded or sent anything.
        drop(store);
        let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
        store.fold_condition_heads().unwrap();
        let messages = store.flush_condition_notifications().unwrap();
        assert_eq!(messages.len(), 1);
        let sent = store
            .latest_claim(&messages[0], Some("message.sent"))
            .unwrap()
            .unwrap();
        assert_eq!(sent.body["fields"]["to"], "agent/ops");
        assert_eq!(sent.body["evidence"], json!([claim.id]));
        // Simulate interruption after sending and before removing the durable queue entry.
        let index = store
            .latest_claim(
                &crate::conditions::instance_subject(&disk.subject(), "alder:/"),
                Some("condition.state"),
            )
            .unwrap()
            .unwrap()
            .store_index;
        store
            .connection
            .batched(move |tx| {
                tx.execute(
                    "INSERT INTO local_condition_notifications(store_index) VALUES (?1)",
                    [index],
                )
            })
            .unwrap()
            .unwrap();
        store.flush_condition_notifications().unwrap();
        assert_eq!(
            store
                .claims_for(&messages[0], Some("message.sent"))
                .unwrap()
                .len(),
            1
        );
        assert!(store.flush_condition_notifications().unwrap().is_empty());
    }

    #[test]
    fn condition_publication_enforces_document_and_fleet_caps() {
        let store = Store::open_memory("alder").unwrap();
        let source = |first, end| {
            let mut source = "version 2\n".to_owned();
            for number in first..end {
                source.push_str(&format!(
                    r#"condition "limit/{number}" {{
  metric "memory.available-percent"
  scope "host"
  below 1
  for "1m"
  owner "person/ada"
}}
"#
                ));
            }
            source
        };
        assert!(parse_intent(&source(0, 33), "alder").is_err());
        let first = source(0, 32);
        let intent = parse_intent(&first, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: first,
                    source_name: None,
                },
            )
            .unwrap();
        assert!(plan.blockers.is_empty());
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "condition-cap",
                Some("person/ada"),
            )
            .unwrap();
        let more = source(32, 33);
        let intent = parse_intent(&more, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: more,
                    source_name: None,
                },
            )
            .unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("condition-limit"))
        );
        assert_eq!(store.conditions().unwrap().len(), 32);
    }

    fn replicate(source: &Store, target: &Store) {
        let exchange =
            crate::store::tests::exchange_from(source, &target.replication_inventory().unwrap());
        crate::store::tests::receive_and_project(target, source.origin(), &exchange);
    }

    #[test]
    fn declaration_changes_reseed_preexisting_remote_breaches() {
        let (_directory, source) = store();
        let disk = decl(&source, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        record(&source, &disk, "alder:/", &tracker, transition, 600_000);
        let target = Store::open_memory("birch").unwrap();
        replicate(&source, &target);
        // Projection is delayed after state admission: a fold cannot materialize an
        // undeclared condition, but its cursor must not make the standing breach disappear.
        let body: String = target
            .readers
            .get()
            .query_row(
                "SELECT body FROM desired WHERE subject=?1",
                [&disk.subject()],
                |row| row.get(0),
            )
            .unwrap();
        target
            .connection
            .batched(|tx| {
                tx.execute(
                    "DELETE FROM desired WHERE subject='condition/fleet/disk'",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        target.seed_condition_heads(true).unwrap();
        target.fold_condition_heads().unwrap();
        target.connection.batched(move |tx| tx.execute("INSERT INTO desired(subject,kind,revision,claim_id,body) SELECT subject,'condition',id,id,?1 FROM claims WHERE subject='condition/fleet/disk' AND kind='intent.desired' ORDER BY store_index DESC LIMIT 1", [body])).unwrap().unwrap();
        target.seed_condition_heads(false).unwrap();
        let disk = target
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(disk.instances.len(), 1);
        assert_eq!(disk.instances[0].phase, "breach");
        assert_eq!(disk.instances[0].host, "alder");
        assert!(!disk.instances[0].local_observation);
    }

    #[test]
    fn one_origins_excess_instances_cannot_hide_another_origins_breach() {
        let (_directory, source) = store();
        let disk = decl(&source, "condition/fleet/disk");
        let target = Store::open_memory("birch").unwrap();
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        for index in 0..300 {
            record(
                &source,
                &disk,
                &format!("alder:/volume-{index}"),
                &tracker,
                transition,
                600_000 + index,
            );
        }
        replicate(&source, &target);
        target
            .record_condition_state(&ConditionRecord {
                decl: &disk,
                host: "birch",
                instance: "birch:/important",
                tracker: &tracker,
                transition,
                now: 600_001,
            })
            .unwrap();
        target.fold_condition_heads().unwrap();
        let assert_hosts = || {
            let disk = target
                .conditions()
                .unwrap()
                .into_iter()
                .find(|view| view.subject == disk.subject())
                .unwrap();
            assert_eq!(
                disk.instances
                    .iter()
                    .filter(|view| view.host == "alder")
                    .count(),
                8
            );
            assert!(
                disk.instances
                    .iter()
                    .any(|view| view.instance == "birch:/important" && view.phase == "breach")
            );
        };
        assert_hosts();
        target.seed_condition_heads(true).unwrap();
        assert_hosts();
    }

    #[test]
    fn the_first_authored_byte_seek_uses_the_accepted_order_index() {
        let (_directory, store) = store();
        let connection = store.readers.get();
        for predicate in [
            "length(accepted_at_unix_ms)=length(?1) AND accepted_at_unix_ms>=?1",
            "length(accepted_at_unix_ms)>length(?1)",
        ] {
            let sql = format!(
                "EXPLAIN QUERY PLAN SELECT store_index FROM claims INDEXED BY claims_accepted_order_index WHERE {predicate} ORDER BY length(accepted_at_unix_ms), accepted_at_unix_ms, store_index LIMIT 1"
            );
            let mut statement = connection.prepare(&sql).unwrap();
            let plan = statement
                .query_map(["1700000000000"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter().any(|line| line.contains("SEARCH claims USING")
                    && line.contains("claims_accepted_order_index")),
                "{plan:?}"
            );
            assert!(
                !plan.iter().any(|line| line.starts_with("SCAN claims")),
                "{plan:?}"
            );
        }
    }

    #[test]
    fn changed_host_selectors_prune_inapplicable_local_observations() {
        let (_directory, store) = store();
        let cpu = decl(&store, "condition/fleet/collector-cpu");
        let tracker = Tracker::default();
        store
            .record_condition_observations(&[ConditionRecord {
                decl: &cpu,
                host: "alder",
                instance: "alder",
                tracker: &tracker,
                transition: None,
                now: 1,
            }])
            .unwrap();
        let source = SOURCE.replace("host \"alder\"", "host \"birch\"");
        let intent = parse_intent(&source, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "retarget",
                Some("person/ada"),
            )
            .unwrap();
        store.record_condition_observations(&[]).unwrap();
        let count: i64 = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM local_condition_observations WHERE subject=?1",
                [&cpu.subject()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            store
                .conditions_local("alder")
                .unwrap()
                .into_iter()
                .find(|view| view.subject == cpu.subject())
                .unwrap()
                .instances
                .is_empty()
        );
    }

    #[test]
    fn local_state_rejects_oversized_and_control_character_instances_before_append() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let tracker = Tracker::default();
        let before = store.index().unwrap();
        for instance in [
            format!("alder:/{}", "x".repeat(2048)),
            "alder:/escape\u{1b}[2J".into(),
        ] {
            assert!(
                store
                    .record_condition_state(&ConditionRecord {
                        decl: &disk,
                        host: "alder",
                        instance: &instance,
                        tracker: &tracker,
                        transition: None,
                        now: 1
                    })
                    .is_err()
            );
        }
        assert_eq!(store.index().unwrap(), before);
    }

    #[test]
    fn remote_transitions_and_local_clear_observations_do_not_claim_staleness() {
        let (_directory, store) = store();
        let cpu = decl(&store, "condition/fleet/collector-cpu");
        let mut tracker = Tracker::default();
        tracker.observe(&cpu, 3.0, 0);
        let transition = tracker.observe(&cpu, 3.0, 300_000);
        record(&store, &cpu, "alder", &tracker, transition, 300_000);
        store.fold_condition_heads().unwrap();
        assert!(
            !store.condition_attention_items(Some("person/ada")).unwrap()[0]
                .detail
                .contains("stale")
        );
        let clear = Tracker::default();
        store
            .record_condition_observations(&[ConditionRecord {
                decl: &cpu,
                host: "alder",
                instance: "alder",
                tracker: &clear,
                transition: None,
                now: 1,
            }])
            .unwrap();
        let lines = crate::conditions::doctor_lines_at(
            &store.conditions_local("alder").unwrap(),
            1_000_000,
        );
        assert!(
            lines
                .iter()
                .any(|(name, status, _)| name == &cpu.subject() && *status == "pass")
        );
    }

    #[test]
    fn startup_seeks_latest_instances_without_replaying_their_history() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        for number in 0..520 {
            let now = number * 30_000;
            tracker.observe(&disk, 30.0 + (number % 10) as f64, now);
            record(&store, &disk, "alder:/", &tracker, None, now);
        }
        let top = store.index().unwrap();
        store.seed_condition_heads(true).unwrap();
        assert_eq!(store.fold_condition_heads().unwrap(), 0);
        let connection = store.readers.get();
        assert_eq!(
            meta_integer(&connection, HEADS_CURSOR).unwrap(),
            Some(top as i64)
        );
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM local_condition_heads", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.conditions().unwrap()[1].instances[0].value,
            Some(39.0)
        );
    }

    #[test]
    fn authenticated_members_cannot_forge_another_hosts_instance() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        let mut claim = record(&store, &disk, "alder:/", &tracker, transition, 600_000);
        assert!(matches!(
            classify_replicated_claim_with_registry(&claim, st3_schema::registry()).unwrap(),
            ReplicatedClaimAdmission::Valid
        ));
        claim.origin = "birch".into();
        assert!(classify_replicated_claim_with_registry(&claim, st3_schema::registry()).is_err());
        claim.body["fields"]["host"] = json!("birch");
        assert!(classify_replicated_claim_with_registry(&claim, st3_schema::registry()).is_err());
    }

    #[test]
    fn writer_contention_keeps_notifications_without_consuming_poison_attempts() {
        let (directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        record(&store, &disk, "alder:/", &tracker, transition, 600_000);
        store.fold_condition_heads().unwrap();
        store
            .connection
            .batched(|tx| tx.execute_batch("PRAGMA busy_timeout=0"))
            .unwrap()
            .unwrap();
        let outside = Connection::open(directory.path().join("claims.sqlite3")).unwrap();
        outside
            .busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        outside.execute_batch("BEGIN IMMEDIATE").unwrap();
        for _ in 0..4 {
            if let Ok(messages) = store.flush_condition_notifications() {
                assert!(messages.is_empty());
            }
        }
        let connection = store.readers.get();
        let attempts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key LIKE 'condition_notification_attempt:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let queued: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM local_condition_notifications",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((attempts, queued), (0, 1));
        drop(connection);
        outside.execute_batch("ROLLBACK").unwrap();
        assert_eq!(store.flush_condition_notifications().unwrap().len(), 1);
    }

    #[test]
    fn a_new_breached_instance_replaces_retired_heads_at_the_origin_cap() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let tracker = Tracker::default();
        for volume in 0..8 {
            record(
                &store,
                &disk,
                &format!("alder:/old-{volume}"),
                &tracker,
                None,
                1,
            );
        }
        store.fold_condition_heads().unwrap();
        let mut breached = Tracker::default();
        breached.observe(&disk, 10.0, 0);
        let transition = breached.observe(&disk, 10.0, 600_000);
        record(&store, &disk, "alder:/new", &breached, transition, 600_000);
        store.fold_condition_heads().unwrap();
        store.seed_condition_heads(true).unwrap();
        let view = store
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(view.instances.len(), 8);
        assert!(
            view.instances
                .iter()
                .any(|view| view.instance == "alder:/new" && view.phase == "breach")
        );
        let rebuilt = Store::open_memory("birch").unwrap();
        replicate(&store, &rebuilt);
        rebuilt.seed_condition_heads(true).unwrap();
        let view = rebuilt
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(view.instances.len(), 8);
        assert!(
            view.instances
                .iter()
                .any(|view| view.instance == "alder:/new" && view.phase == "breach")
        );
    }

    #[test]
    fn notification_bursts_are_deferred_by_owner_without_losing_delivery() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        for volume in 0..8 {
            record(
                &store,
                &disk,
                &format!("alder:/volume{volume}"),
                &tracker,
                transition,
                600_000,
            );
        }
        store.fold_condition_heads().unwrap();
        assert_eq!(store.flush_condition_notifications().unwrap().len(), 4);
        assert_eq!(store.flush_condition_notifications().unwrap().len(), 4);
        assert!(store.flush_condition_notifications().unwrap().is_empty());
    }

    #[test]
    fn poison_notification_does_not_block_later_delivery_and_is_bounded() {
        let (_directory, store) = store();
        let mut bad = decl(&store, "condition/fleet/disk");
        bad.owner = "agent/".into();
        let good = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&good, 10.0, 0);
        let transition = tracker.observe(&good, 10.0, 600_000);
        let first = record(&store, &bad, "alder:/bad", &tracker, transition, 600_000);
        let second = record(&store, &good, "alder:/good", &tracker, transition, 600_001);
        store
            .connection
            .batched(move |tx| {
                tx.execute(
                    "INSERT INTO local_condition_notifications(store_index) VALUES (?1),(?2)",
                    params![first.store_index, second.store_index],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(store.flush_condition_notifications().unwrap().len(), 1);
        store.flush_condition_notifications().unwrap();
        store.flush_condition_notifications().unwrap();
        let count: i64 = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM local_condition_notifications",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            store
                .condition_evaluator_status()
                .unwrap()
                .1
                .unwrap()
                .contains("discarded")
        );
        store.flush_condition_notifications().unwrap();
        assert!(store.condition_evaluator_status().unwrap().1.is_none());
    }

    #[test]
    fn doctor_reads_each_condition_without_writing() {
        let (_directory, store) = store();
        let before = store.index().unwrap();
        let lines = crate::conditions::doctor_lines(&store.conditions().unwrap());
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|(_, status, message)| *status == "info"
                    && message.contains("awaiting a sample"))
        );
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 1_000);
        let enter = tracker.observe(&disk, 10.0, 601_000).unwrap();
        assert_eq!(store.index().unwrap(), before);
        record(&store, &disk, "alder:/", &tracker, Some(enter), 601_000);
        store.fold_condition_heads().unwrap();
        let before = store.index().unwrap();
        let lines = crate::conditions::doctor_lines(&store.conditions().unwrap());
        assert!(
            lines
                .iter()
                .any(|(name, status, message)| name == "condition/fleet/disk"
                    && *status == "warn"
                    && message.contains("breach")
                    && message.contains("10"))
        );
        assert_eq!(store.index().unwrap(), before);
    }

    #[test]
    fn a_declared_condition_reads_back_with_no_state_until_one_is_recorded() {
        let (_directory, store) = store();
        let conditions = store.conditions().unwrap();
        assert_eq!(
            conditions
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            ["condition/fleet/collector-cpu", "condition/fleet/disk"]
        );
        assert!(conditions.iter().all(|c| c.instances.is_empty()));
        assert_eq!(
            conditions[1].rule.as_deref(),
            Some("disk.free-percent below 15% (recovers at 18%) for 10m")
        );
        assert_eq!(
            conditions[0].hosts.as_deref(),
            Some(&["alder".to_owned()][..])
        );
    }

    #[test]
    fn the_newest_state_of_each_instance_is_found_after_a_fold_and_a_read_writes_nothing() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut root = Tracker::default();
        root.observe(&disk, 40.0, 1_000);
        record(&store, &disk, "alder:/", &root, None, 1_000);
        let mut data = Tracker::default();
        data.observe(&disk, 10.0, 1_000);
        record(&store, &disk, "alder:/data", &data, None, 1_000);
        data.observe(&disk, 9.0, 700_000);
        let entered = record(
            &store,
            &disk,
            "alder:/data",
            &data,
            Some(Transition::Enter),
            700_000,
        );
        assert_eq!(store.fold_condition_heads().unwrap(), 3);
        assert_eq!(store.fold_condition_heads().unwrap(), 0);

        let before = store.index().unwrap();
        let conditions = store.conditions().unwrap();
        assert_eq!(
            store.index().unwrap(),
            before,
            "a condition read must not write"
        );
        let instances = &conditions[1].instances;
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance, "alder:/");
        assert_eq!(instances[0].phase, "clear");
        assert_eq!(instances[1].instance, "alder:/data");
        assert_eq!(instances[1].claim, entered.id);
        assert_eq!(instances[1].phase, "breach");
        assert_eq!(instances[1].transition.as_deref(), Some("enter"));
        assert_eq!(instances[1].value, Some(9.0));
        assert_eq!(instances[1].breach_since, Some(1_000));
        assert_eq!(instances[1].values.len(), 2);

        let trackers = store.condition_trackers("alder").unwrap();
        let restored = &trackers[&("condition/fleet/disk".to_owned(), "alder:/data".to_owned())];
        assert_eq!(restored.phase, Phase::Breach);
        assert_eq!(restored.breach_since, Some(1_000));
        assert!(store.condition_trackers("birch").unwrap().is_empty());
    }

    #[test]
    fn an_agent_owner_gets_one_message_per_transition_and_a_person_an_alert_while_it_lasts() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let message = |transition, since| {
            store
                .send_condition_notification(&Notification {
                    condition: &disk.subject(),
                    owner: &disk.owner,
                    instance: "alder:/",
                    transition,
                    breach_since: since,
                    title: "Condition breached: fleet/disk on alder:/",
                    body: "body",
                    evidence: None,
                })
                .unwrap()
        };
        let first = message(Transition::Enter, 5).unwrap();
        assert_eq!(
            message(Transition::Enter, 5),
            None,
            "a retry neither appends nor reports a new event"
        );
        let sent = store.claims_for(&first, Some("message.sent")).unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body["fields"]["to"], "agent/ops");
        assert_ne!(message(Transition::Recover, 5).unwrap(), first);
        assert_ne!(
            message(Transition::Enter, 6).unwrap(),
            first,
            "a later breach is a new message"
        );

        // A person's condition never messages anyone; it shows as an alert on their home.
        let cpu = decl(&store, "condition/fleet/collector-cpu");
        assert_eq!(
            store
                .send_condition_notification(&Notification {
                    condition: &cpu.subject(),
                    owner: &cpu.owner,
                    instance: "alder",
                    transition: Transition::Enter,
                    breach_since: 5,
                    title: "t",
                    body: "b",
                    evidence: None,
                })
                .unwrap(),
            None
        );
        let mut tracker = Tracker::default();
        tracker.observe(&cpu, 3.0, 1_000);
        tracker.observe(&cpu, 3.0, 301_000);
        assert_eq!(tracker.phase, Phase::Breach);
        record(
            &store,
            &cpu,
            "alder",
            &tracker,
            Some(Transition::Enter),
            301_000,
        );
        store.fold_condition_heads().unwrap();
        let items = store.condition_attention_items(Some("person/ada")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "condition");
        assert_eq!(items[0].person, "person/ada");
        assert_eq!(
            items[0].title,
            "Condition breached: fleet/collector-cpu on alder"
        );
        assert!(items[0].is_alert());
        assert!(
            store
                .condition_attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
        let snapshot = store
            .attention_snapshot(Some("person/ada"), now_ms())
            .unwrap();
        assert!(
            snapshot.iter().any(|item| item.kind == "condition"),
            "{snapshot:?}"
        );

        tracker.observe(&cpu, 0.5, 302_000);
        tracker.observe(&cpu, 0.5, 602_000);
        assert_eq!(tracker.phase, Phase::Clear);
        record(
            &store,
            &cpu,
            "alder",
            &tracker,
            Some(Transition::Recover),
            602_000,
        );
        store.fold_condition_heads().unwrap();
        assert!(store.condition_attention_items(None).unwrap().is_empty());
    }

    #[test]
    fn claim_bytes_count_only_this_members_claims_in_the_last_day() {
        let (_directory, store) = store();
        let now = now_ms();
        assert_eq!(store.condition_claim_bytes_per_day(now).unwrap(), None);
        store.fold_condition_claim_bytes(now).unwrap();
        // A day back from now covers every claim the test wrote, all from this member.
        let counted = store.condition_claim_bytes_per_day(now).unwrap().unwrap();
        assert!(counted > 0.0);
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 40.0, now);
        record(&store, &disk, "alder:/", &tracker, None, now);
        store.fold_condition_claim_bytes(now).unwrap();
        let more = store.condition_claim_bytes_per_day(now).unwrap().unwrap();
        assert!(more > counted, "{more} > {counted}");
        // An hour later the same bytes are scaled over a longer covered span.
        let later = store
            .condition_claim_bytes_per_day(now + 3_600_000)
            .unwrap()
            .unwrap();
        assert!(later <= more);
    }
}
