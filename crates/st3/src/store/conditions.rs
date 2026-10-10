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
    origin TEXT NOT NULL,
    instance TEXT NOT NULL,
    store_index INTEGER NOT NULL,
    breached INTEGER NOT NULL,
    accepted_key TEXT NOT NULL,
    PRIMARY KEY (subject, origin, instance)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_condition_heads_origin ON local_condition_heads(subject,origin,breached,accepted_key);
CREATE INDEX IF NOT EXISTS local_condition_heads_member ON local_condition_heads(origin,subject);
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
const HEADS_PAGE: i64 = 50;
/// Claims one byte-count read covers, by store index. A read of this many rows takes a few
/// milliseconds, so no read holds its connection long.
const BYTES_PAGE: i64 = 2_000;
/// Reads one tick makes at most while catching up on byte counts.
const BYTES_PAGES_PER_TICK: usize = 25;
const HOUR_MS: u128 = 3_600_000;
const DAY_MS: u128 = 24 * HOUR_MS;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    // An earlier development cache has no origin key. Rebuild only this disposable cache.
    let old: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('local_condition_heads')) AND NOT EXISTS(SELECT 1 FROM pragma_table_info('local_condition_heads') WHERE name='origin')", [], |row| row.get(0))?;
    if old {
        connection.execute_batch("DROP TABLE local_condition_heads; DELETE FROM meta WHERE key IN ('condition_heads_declared_digest_v3','condition_heads_cursor');")?;
    }
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Default, Debug)]
pub struct ConditionNotificationReport {
    pub messages: Vec<String>,
    pub errors: Vec<String>,
}

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

/// Keep eight heads per origin, preferring standing breaches over Clear retired instances.
fn upsert_condition_head(
    tx: &Transaction<'_>,
    condition: &str,
    instance: &str,
    index: i64,
) -> rusqlite::Result<()> {
    let (origin, accepted, id, body): (String, String, String, String) = tx.query_row(
        "SELECT origin,accepted_at_unix_ms,id,body FROM claims WHERE store_index=?1",
        [index],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let phase = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|body| Phase::parse(body["fields"]["phase"].as_str()?));
    let breached = phase.is_some_and(Phase::in_breach);
    let key = format!("{:02}:{accepted}:{id}", accepted.len());
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_condition_heads WHERE subject=?1 AND origin=?2 AND instance=?3)", params![condition,origin,instance], |row| row.get(0))?;
    if !exists {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM local_condition_heads WHERE subject=?1 AND origin=?2",
            params![condition, origin],
            |row| row.get(0),
        )?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM local_condition_heads WHERE subject=?1",
            [condition],
            |row| row.get(0),
        )?;
        if count >= crate::conditions::MAX_INSTANCES as i64
            || total >= crate::conditions::MAX_REMOTE_INSTANCES as i64
        {
            let same_origin = count >= crate::conditions::MAX_INSTANCES as i64;
            let read_victim = |row: &rusqlite::Row<'_>| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                ))
            };
            let victim = if same_origin {
                tx.query_row("SELECT origin,instance,store_index,breached,accepted_key FROM local_condition_heads WHERE subject=?1 AND origin=?2 ORDER BY breached,accepted_key,origin,instance LIMIT 1", params![condition,origin],read_victim).optional()?
            } else {
                tx.query_row("SELECT origin,instance,store_index,breached,accepted_key FROM local_condition_heads WHERE subject=?1 ORDER BY breached,accepted_key,origin,instance LIMIT 1", [condition],read_victim).optional()?
            };
            if let Some((old_origin, old_instance, old_index, old_breach, old_key)) = victim {
                let replace =
                    (!old_breach && breached) || (old_breach == breached && key > old_key);
                if !replace {
                    head_capacity_error(
                        tx,
                        &format!(
                            "{condition}: {} {origin}/{instance} omitted at the instance/head limit; withdraw or narrow the declaration",
                            if breached {
                                "standing breach"
                            } else {
                                "instance"
                            }
                        ),
                    )?;
                    return Ok(());
                }
                if old_breach {
                    head_capacity_error(
                        tx,
                        &format!(
                            "{condition}: standing breach {old_origin}/{old_instance} evicted at the instance/head limit by {origin}/{instance}; withdraw or narrow the declaration"
                        ),
                    )?;
                }
                tx.execute("DELETE FROM local_condition_heads WHERE subject=?1 AND origin=?2 AND instance=?3 AND store_index=?4", params![condition,old_origin,old_instance,old_index])?;
            }
        }
    }
    let newest: i64 = tx.query_row(&canonical_sql("SELECT claims.store_index FROM claims WHERE claims.store_index IN (?1,COALESCE((SELECT store_index FROM local_condition_heads WHERE subject=?2 AND origin=?3 AND instance=?4),?1)) ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![index,condition,origin,instance], |row| row.get(0))?;
    if newest == index {
        tx.execute("INSERT INTO local_condition_heads(subject,origin,instance,store_index,breached,accepted_key) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(subject,origin,instance) DO UPDATE SET store_index=excluded.store_index,breached=excluded.breached,accepted_key=excluded.accepted_key", params![condition,origin,instance,index,breached,key])?;
    }
    Ok(())
}

fn head_capacity_error(tx: &Transaction<'_>, detail: &str) -> rusqlite::Result<()> {
    tx.execute("INSERT INTO meta(key,value) VALUES ('condition_heads_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [detail])?;
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
                    format!("fleet condition limit (32) exceeded; all remaining declarations are not evaluated (at least {} omitted)",connection.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM desired WHERE kind='condition' LIMIT 1024)", [], |row| row.get::<_,i64>(0))?.saturating_sub(32)),
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
        let membership = self.fleet_membership()?;
        let members = membership
            .incarnations()
            .map(|member| member.name.clone())
            .chain(membership.legacy_removed_names())
            .collect::<BTreeSet<_>>();
        let departed = members
            .iter()
            .filter(|name| {
                matches!(
                    membership.state(name),
                    smallclaims::fleet::MemberState::Ended(_)
                        | smallclaims::fleet::MemberState::LegacyRemoved(_)
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let current = members
            .difference(&departed.iter().cloned().collect())
            .cloned()
            .collect::<Vec<_>>();
        let digest = hex::encode(sha2::Sha256::digest(
            format!("{declarations:?}{current:?}{departed:?}").as_bytes(),
        ));
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
        let progress: Option<String> = self
            .readers
            .get()
            .query_row(
                "SELECT value FROM meta WHERE key='condition_heads_seed_progress'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let progress = progress
            .and_then(|value| serde_json::from_str::<Value>(&value).ok())
            .filter(|value| !refresh && value["digest"].as_str() == Some(digest.as_str()));
        let captured = progress
            .as_ref()
            .and_then(|value| value["captured"].as_i64())
            .unwrap_or(i64::try_from(self.index()?)?);
        let resume_condition = progress
            .as_ref()
            .and_then(|value| value["condition"].as_u64())
            .unwrap_or(0) as usize;
        let resume_origin = progress
            .as_ref()
            .and_then(|value| value["origin"].as_u64())
            .unwrap_or(0) as usize;
        let resume_previous = progress
            .as_ref()
            .and_then(|value| value["previous"].as_str())
            .map(str::to_owned);
        let seed_started = std::time::Instant::now();
        let mut origin_work = 0;
        let mut next_progress = None;
        let mut heads = Vec::new();
        let mut diagnostics = Vec::new();
        let now = condition_now();
        let beginning = progress.is_none();
        self.connection
            .batched(move |tx| {
                for origin in departed {
                    tx.execute(
                        "DELETE FROM local_condition_heads WHERE origin=?1",
                        [origin],
                    )?;
                }
                if beginning {
                    tx.execute("DELETE FROM meta WHERE key='condition_heads_error'", [])?;
                }
                Ok::<_, rusqlite::Error>(())
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        'conditions: for (condition_slot, condition) in declarations
            .into_iter()
            .take(crate::conditions::MAX_CONDITIONS)
            .enumerate()
            .skip(resume_condition)
        {
            let prefix = crate::conditions::instance_subject_prefix(&condition.subject);
            let upper = format!("{prefix}~");
            let first_slot = if condition_slot == resume_condition {
                resume_origin
            } else {
                0
            };
            let mut previous_origin = if first_slot > 0 {
                resume_previous.clone().unwrap_or(prefix.clone())
            } else {
                prefix.clone()
            };
            // Keyed fleets seek their current members directly: departed namespaces cannot
            // consume discovery slots. Legacy fleets have a larger bounded origin allowance.
            let known = membership.anchor().is_some();
            if known && current.len() > crate::conditions::MAX_REMOTE_INSTANCES {
                diagnostics.push(format!(
                    "{}: {} members exceed the 256-origin discovery limit",
                    condition.subject,
                    current.len()
                ));
            }
            for slot in first_slot..crate::conditions::MAX_REMOTE_INSTANCES {
                if origin_work >= 128
                    || (origin_work > 0
                        && seed_started.elapsed() >= std::time::Duration::from_secs(1))
                {
                    next_progress = Some(
                        json!({"digest":digest,"captured":captured,"condition":condition_slot,"origin":slot,"previous":previous_origin}),
                    );
                    break 'conditions;
                }
                origin_work += 1;
                let origin_prefix = if known {
                    let Some(origin) = current.get(slot) else {
                        break;
                    };
                    crate::conditions::instance_origin_prefix(&condition.subject, origin)
                } else {
                    let first: Option<String> = self.readers.get().query_row(
                        "SELECT subject FROM claims INDEXED BY claims_subject_kind_accepted_index WHERE subject>?1 AND subject<?2 AND kind='condition.state' ORDER BY subject LIMIT 1",
                        params![previous_origin,upper], |row| row.get(0)).optional()?;
                    let Some(first) = first else { break };
                    let Some((origin_hash, _)) = first
                        .strip_prefix(&prefix)
                        .and_then(|rest| rest.split_once('/'))
                    else {
                        previous_origin = first;
                        continue;
                    };
                    let origin_prefix = format!("{prefix}{origin_hash}/");
                    previous_origin = format!("{origin_prefix}~");
                    origin_prefix
                };
                let origin_upper = format!("{origin_prefix}~");
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
                    if matches!(
                        membership.state(&claim.origin),
                        smallclaims::fleet::MemberState::Ended(_)
                            | smallclaims::fleet::MemberState::LegacyRemoved(_)
                    ) {
                        continue;
                    }
                    candidates.push((
                        Phase::parse(fields["phase"].as_str().unwrap_or(""))
                            .is_some_and(Phase::in_breach),
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
                let more: bool = self.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_accepted_index WHERE subject>?1 AND subject<?2 AND kind='condition.state')", params![previous_instance,origin_upper], |row| row.get(0))?;
                if more {
                    diagnostics.push(format!("{}: instance discovery exceeds 64 namespaces in an origin; additional instances are not reconstructed; narrow or replace the declaration",condition.subject));
                }
                candidates.sort_by(|left, right| {
                    (right.0, right.1, &right.2).cmp(&(left.0, left.1, &left.2))
                });
                if candidates.iter().filter(|candidate| candidate.0).count()
                    > crate::conditions::MAX_INSTANCES
                {
                    diagnostics.push(format!("{}: standing breaches exceed eight instances in an origin; some are omitted; withdraw or narrow the declaration",condition.subject));
                }
                heads.extend(
                    candidates
                        .into_iter()
                        .take(crate::conditions::MAX_INSTANCES)
                        .map(|(_, _, _, head)| head),
                );
            }
            if !known {
                let more: bool = self.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_accepted_index WHERE subject>?1 AND subject<?2 AND kind='condition.state')", params![previous_origin,upper], |row| row.get(0))?;
                if more {
                    diagnostics.push(format!("{}: legacy origin discovery exceeds 256 namespaces; additional origins are not reconstructed",condition.subject));
                }
            }
        }
        self.connection
            .batched(|tx| tx.execute("DELETE FROM local_condition_heads WHERE subject NOT IN (SELECT subject FROM desired WHERE kind='condition')", []))
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        for page in heads.chunks(HEADS_PAGE as usize) {
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
                if let Some(detail) = diagnostics.last() { head_capacity_error(tx,detail)?; }
                if let Some(progress) = next_progress {
                    tx.execute("INSERT INTO meta(key,value) VALUES ('condition_heads_seed_progress',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [progress.to_string()])?;
                    tx.execute("INSERT INTO meta(key,value) VALUES ('condition_heads_discovery','indexed condition discovery continues on the next tick') ON CONFLICT(key) DO UPDATE SET value=excluded.value", [])?;
                } else {
                    let current_cursor: Option<i64> = tx.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1", [HEADS_CURSOR], |row| row.get(0)).optional()?;
                    set_meta_integer(tx, HEADS_CURSOR, captured.max(current_cursor.unwrap_or(0)))?;
                    tx.execute("DELETE FROM meta WHERE key IN ('condition_heads_seed_progress','condition_heads_discovery')", [])?;
                    tx.execute("INSERT INTO meta(key,value) VALUES ('condition_heads_declared_digest_v3',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [digest])?;
                }
                Ok::<_,rusqlite::Error>(())
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    pub fn condition_heads_seed_pending(&self) -> Result<bool> {
        Ok(self.readers.get().query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='condition_heads_seed_progress')",
            [],
            |row| row.get(0),
        )?)
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
            if tx.execute("DELETE FROM local_condition_heads WHERE subject NOT IN (SELECT subject FROM desired WHERE kind='condition')", [])? > 0 {
                tx.execute("DELETE FROM meta WHERE key IN ('condition_heads_declared_digest_v3','condition_heads_seed_progress')", [])?;
            }
            Ok::<_, rusqlite::Error>(())
        }).map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    pub fn condition_evaluator_status(&self) -> Result<(Option<i64>, Option<String>)> {
        let connection = self.readers.get();
        let at = meta_integer(&connection, "condition_evaluator_completed")?;
        let error = connection.query_row("SELECT value FROM meta WHERE key IN ('condition_evaluator_error','condition_notification_last_error','condition_heads_error','condition_heads_discovery','condition_notification_discarded') AND (key!='condition_notification_discarded' OR ?1-CAST((SELECT value FROM meta WHERE key='condition_notification_discarded_at') AS INTEGER)<3600000) ORDER BY key LIMIT 1", [i64::try_from(condition_now()).unwrap_or(i64::MAX)], |row| row.get(0)).optional()?;
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
                "SELECT store_index, CASE WHEN json_valid(body) THEN json_extract(body, '$.fields.condition') END, CASE WHEN json_valid(body) THEN json_extract(body, '$.fields.instance') END, body, origin, accepted_at_unix_ms, subject
                   FROM claims
                  WHERE kind='condition.state' AND store_index > ?1
                  ORDER BY store_index LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![cursor, HEADS_PAGE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
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
        let membership = self.fleet_membership()?;
        let departed = rows
            .iter()
            .filter(|row| {
                matches!(
                    membership.state(&row.4),
                    smallclaims::fleet::MemberState::Ended(_)
                        | smallclaims::fleet::MemberState::LegacyRemoved(_)
                )
            })
            .map(|row| row.4.clone())
            .collect::<BTreeSet<_>>();
        let declared = self
            .declared_conditions()?
            .into_iter()
            .take(crate::conditions::MAX_CONDITIONS)
            .map(|condition| condition.subject)
            .collect::<BTreeSet<_>>();
        self.connection
            .batched(move |transaction| {
                for (index, subject, instance, body, origin, accepted, claim_subject) in &rows {
                    let (Some(subject),Some(instance)) = (subject,instance) else { continue };
                    if !declared.contains(subject) || departed.contains(origin) { continue; }
                    let Ok(body) = serde_json::from_str::<Value>(body) else { continue };
                    let fields = &body["fields"];
                    if !crate::conditions::valid_state_identity(claim_subject, origin, fields) { continue; }
                    if origin == &host && fields["host"] == host
                        && accepted.parse::<u128>().is_ok_and(|at| at <= condition_now() && condition_now().saturating_sub(at) <= 3_600_000) && fields["transition"].is_string()
                        && fields["owner"].as_str().is_some_and(|owner| owner.starts_with("agent/")) {
                        transaction.execute("INSERT OR IGNORE INTO local_condition_notifications(store_index) VALUES (?1)", [index])?;
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
              WHERE heads.subject=?1 AND heads.origin=claims.origin AND claims.origin=json_extract(claims.body,'$.fields.host') AND (heads.instance=claims.origin OR substr(heads.instance,1,length(claims.origin)+1)=claims.origin||':') AND (?2 IS NULL OR heads.origin=?2)
              AND (?3=0 OR heads.breached=1) ORDER BY heads.instance LIMIT 256",
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
            let key = (text("host").unwrap_or_default(), instance.clone());
            let local_observation = claim.is_empty();
            let old_breach = instances
                .get(&key)
                .is_some_and(|view: &ConditionInstanceView| {
                    Phase::parse(&view.phase).is_some_and(Phase::in_breach)
                });
            let stale_observation = fields["measured_at"].as_u64().is_some_and(|at| {
                condition_now().saturating_sub(u128::from(at)) > crate::conditions::STALE_AFTER_MS
            });
            if local_observation
                && old_breach
                && stale_observation
                && fields["phase"].as_str() == Some("pending")
            {
                continue;
            }
            let claim = if claim.is_empty() {
                instances
                    .get(&key)
                    .map(|view: &ConditionInstanceView| view.claim.clone())
                    .unwrap_or_default()
            } else {
                claim
            };
            instances.insert(
                key,
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
        let key = json!([
            "condition",
            condition,
            self.origin(),
            instance,
            transition.as_str(),
            breach_since
        ])
        .to_string();
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
        let report = self.flush_condition_notifications_report()?;
        if report.messages.is_empty() && !report.errors.is_empty() {
            anyhow::bail!("{}", report.errors.join("; "));
        }
        Ok(report.messages)
    }

    pub fn flush_condition_notifications_report(&self) -> Result<ConditionNotificationReport> {
        let rows = {
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(
                "WITH queued AS (SELECT c.store_index,c.id,CASE WHEN json_valid(c.body) THEN json_extract(c.body,'$.fields.condition') END AS condition,c.body,ROW_NUMBER() OVER (PARTITION BY CASE WHEN json_valid(c.body) THEN COALESCE(json_extract(c.body,'$.fields.owner'),c.id) ELSE c.id END ORDER BY c.store_index) AS owner_rank FROM local_condition_notifications n JOIN claims c ON c.store_index=n.store_index) SELECT store_index,id,condition,body FROM queued WHERE owner_rank<=4 ORDER BY store_index LIMIT 16",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut messages = Vec::new();
        let mut errors = Vec::new();
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
                    condition: subject.as_deref().unwrap_or("condition/unknown"),
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
                    malformed |= error.downcast_ref::<St3Error>().is_some_and(|error| {
                        error.code.starts_with("invalid-")
                            || matches!(
                                error.code,
                                "rule-denied"
                                    | "operation-conflict"
                                    | "unknown-subject-family"
                                    | "idempotency-conflict"
                            )
                    });
                    if !malformed {
                        errors.push(format!("condition notification {claim}: {error:#}"));
                        continue;
                    }
                    let attempts = match meta_integer(&self.readers.get(), &attempt_key) {
                        Ok(value) => value.unwrap_or(0) + 1,
                        Err(error) => {
                            errors.push(format!("notification attempt read: {error:#}"));
                            continue;
                        }
                    };
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
                    let discarded = remove;
                    let update = self.connection.batched(move |tx| {
                        set_meta_integer(tx, &key, attempts)?;
                        let error_key = if discarded { "condition_notification_discarded" } else { "condition_notification_last_error" };
                        tx.execute("INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![error_key,detail])?;
                        if discarded { set_meta_integer(tx,"condition_notification_discarded_at",i64::try_from(condition_now()).unwrap_or(i64::MAX))?; }
                        Ok::<_, rusqlite::Error>(())
                    }).map_err(|error| anyhow::anyhow!("{error}")).and_then(|result| result.map_err(Into::into));
                    if let Err(error) = update {
                        errors.push(format!("notification retry update: {error:#}"));
                        continue;
                    }
                }
            }
            if remove {
                let removed = self
                    .connection
                    .batched(move |tx| {
                        tx.execute(
                            "DELETE FROM local_condition_notifications WHERE store_index=?1",
                            [index],
                        )?;
                        tx.execute("DELETE FROM meta WHERE key=?1", [attempt_key])?;
                        Ok::<_, rusqlite::Error>(())
                    })
                    .map_err(|error| anyhow::anyhow!("{error}"))
                    .and_then(|result| result.map_err(Into::into));
                if let Err(error) = removed {
                    errors.push(format!("notification queue cleanup: {error:#}"));
                }
            }
        }
        let has_transient = match self.readers.get().query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='condition_notification_last_error')",
            [],
            |row| row.get::<_, bool>(0),
        ) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("notification diagnostic read: {error:#}"));
                false
            }
        };
        if !had_error && has_transient {
            let cleared = self
                .connection
                .batched(|tx| {
                    tx.execute(
                        "DELETE FROM meta WHERE key='condition_notification_last_error'",
                        [],
                    )
                })
                .map_err(|error| anyhow::anyhow!("{error}"))
                .and_then(|result| result.map_err(Into::into));
            if let Err(error) = cleared {
                errors.push(format!("notification diagnostic cleanup: {error:#}"));
            }
        }
        Ok(ConditionNotificationReport { messages, errors })
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
                    json!([condition.subject, instance.host, instance.instance, since])
                        .to_string()
                        .as_bytes(),
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
                        if stale { "; this instance is absent or its observation is stale; inspect the evaluating host; withdrawing or retargeting the declaration clears it" } else { "" },
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

    fn fold_all(store: &Store) {
        for _ in 0..100 {
            if store.fold_condition_heads().unwrap() < HEADS_PAGE as usize {
                return;
            }
        }
        panic!("test fold did not catch up");
    }

    #[test]
    fn discovery_includes_more_than_thirty_two_origins() {
        let (_directory, target) = store();
        let disk = decl(&target, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        for number in 0..40 {
            let origin = format!("member-{number:02}");
            let remote = Store::open_memory(&origin).unwrap();
            remote
                .record_condition_state(&ConditionRecord {
                    decl: &disk,
                    host: &origin,
                    instance: &origin,
                    tracker: &tracker,
                    transition,
                    now: 600_000,
                })
                .unwrap();
            replicate(&remote, &target);
        }
        target.seed_condition_heads(true).unwrap();
        while target.condition_heads_seed_pending().unwrap() {
            target.seed_condition_heads(false).unwrap();
        }
        let view = target
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(view.instances.len(), 40);
        assert!(
            view.instances
                .iter()
                .all(|instance| instance.phase == "breach")
        );
    }

    #[test]
    fn clear_head_eviction_preserves_a_standing_breach_and_breach_loss_warns() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut breach = Tracker::default();
        breach.observe(&disk, 10.0, 0);
        let enter = breach.observe(&disk, 10.0, 600_000);
        record(&store, &disk, "alder:/standing", &breach, enter, 600_000);
        let clear = Tracker::default();
        for number in 0..8 {
            record(
                &store,
                &disk,
                &format!("alder:/clear-{number}"),
                &clear,
                None,
                700_000 + number,
            );
        }
        fold_all(&store);
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
                .any(|instance| instance.instance == "alder:/standing")
        );
        for number in 0..9 {
            record(
                &store,
                &disk,
                &format!("alder:/breach-{number}"),
                &breach,
                enter,
                800_000 + number,
            );
        }
        fold_all(&store);
        assert!(
            store
                .condition_evaluator_status()
                .unwrap()
                .1
                .unwrap()
                .contains("standing breach")
        );
    }

    #[test]
    fn origin_is_part_of_the_head_and_attention_identity() {
        let (_directory, target) = store();
        let mut disk = decl(&target, "condition/fleet/disk");
        disk.owner = "person/ada".into();
        let source = SOURCE.replace("owner \"agent/ops\"", "owner \"person/ada\"");
        let intent = parse_intent(&source, "alder").unwrap();
        let plan = target
            .mission(
                &intent,
                IntentInput {
                    kdl: source.clone(),
                    source_name: None,
                },
            )
            .unwrap();
        target
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "conditions-origin-identity",
                Some("person/ada"),
            )
            .unwrap();
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let transition = tracker.observe(&disk, 10.0, 600_000);
        for origin in ["a", "a:b"] {
            let remote = Store::open_memory(origin).unwrap();
            remote
                .record_condition_state(&ConditionRecord {
                    decl: &disk,
                    host: origin,
                    instance: "a:b:c",
                    tracker: &tracker,
                    transition,
                    now: 600_000,
                })
                .unwrap();
            replicate(&remote, &target);
        }
        fold_all(&target);
        let view = target
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(view.instances.len(), 2);
        let items = target
            .condition_attention_items(Some("person/ada"))
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_ne!(items[0].episode, items[1].episode);
    }

    #[test]
    fn withdrawal_between_seed_ticks_invalidates_the_digest() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let enter = tracker.observe(&disk, 10.0, 600_000);
        record(&store, &disk, "alder:/", &tracker, enter, 600_000);
        store.seed_condition_heads(true).unwrap();
        let body: String = store
            .readers
            .get()
            .query_row(
                "SELECT body FROM desired WHERE subject=?1",
                [disk.subject()],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "DELETE FROM desired WHERE subject='condition/fleet/disk'",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        store.record_condition_observations(&[]).unwrap();
        store.connection.batched(move |tx|tx.execute("INSERT INTO desired(subject,kind,revision,claim_id,body) SELECT subject,'condition',id,id,?1 FROM claims WHERE subject='condition/fleet/disk' AND kind='intent.desired' ORDER BY store_index DESC LIMIT 1",[body])).unwrap().unwrap();
        store.seed_condition_heads(false).unwrap();
        assert_eq!(
            store
                .conditions()
                .unwrap()
                .into_iter()
                .find(|view| view.subject == disk.subject())
                .unwrap()
                .instances
                .len(),
            1
        );
    }

    #[test]
    fn a_notification_backlog_for_one_owner_does_not_starve_another_owner() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let enter = tracker.observe(&disk, 10.0, 600_000);
        for number in 0..20 {
            record(
                &store,
                &disk,
                &format!("alder:/busy-{number}"),
                &tracker,
                enter,
                600_000 + number,
            );
        }
        let mut other = disk.clone();
        other.owner = "agent/other".into();
        record(&store, &other, "alder:/other", &tracker, enter, 600_100);
        fold_all(&store);
        let messages = store.flush_condition_notifications().unwrap();
        assert_eq!(messages.len(), 5);
        assert!(messages.iter().any(|subject| {
            store
                .latest_claim(subject, Some("message.sent"))
                .unwrap()
                .unwrap()
                .body["fields"]["to"]
                == "agent/other"
        }));
    }

    #[test]
    fn malformed_and_mismatched_head_rows_do_not_stall_the_fold() {
        let (_directory, store) = store();
        let disk = decl(&store, "condition/fleet/disk");
        let mut tracker = Tracker::default();
        tracker.observe(&disk, 10.0, 0);
        let enter = tracker.observe(&disk, 10.0, 600_000);
        let missing = record(&store, &disk, "alder:/missing", &tracker, enter, 600_000);
        let mismatch = record(&store, &disk, "alder:/mismatch", &tracker, enter, 600_001);
        record(&store, &disk, "alder:/good", &tracker, enter, 600_002);
        store
            .connection
            .batched(move |tx| {
                tx.execute(
                    "UPDATE claims SET body='{}' WHERE store_index=?1",
                    [missing.store_index],
                )?;
                tx.execute(
                    "UPDATE claims SET subject='condition-instance/wrong' WHERE store_index=?1",
                    [mismatch.store_index],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(store.fold_condition_heads().unwrap(), 3);
        assert_eq!(store.fold_condition_heads().unwrap(), 0);
        let view = store
            .conditions()
            .unwrap()
            .into_iter()
            .find(|view| view.subject == disk.subject())
            .unwrap();
        assert_eq!(view.instances.len(), 1);
        assert_eq!(view.instances[0].instance, "alder:/good");
    }

    #[test]
    fn condition_seed_fold_and_local_head_queries_use_bounded_indexes() {
        let (_directory, store) = store();
        let connection = store.readers.get();
        for sql in [
            "SELECT subject FROM claims INDEXED BY claims_subject_kind_accepted_index WHERE subject>'condition-instance/a/' AND subject<'condition-instance/a/~' AND kind='condition.state' ORDER BY subject LIMIT 1",
            "SELECT subject FROM claims INDEXED BY claims_subject_kind_accepted_index WHERE subject>'condition-instance/a/b/' AND subject<'condition-instance/a/b/~' AND kind='condition.state' ORDER BY subject LIMIT 1",
            "SELECT store_index FROM claims WHERE kind='condition.state' AND store_index>1 ORDER BY store_index LIMIT 50",
            "SELECT c.id,c.body FROM local_condition_heads h JOIN claims c ON c.store_index=h.store_index WHERE h.subject='condition/a' AND h.origin='alder' LIMIT 256",
            "SELECT COUNT(*) FROM local_condition_heads WHERE subject='condition/a' AND origin='alder'",
            "SELECT origin,instance,store_index,breached,accepted_key FROM local_condition_heads WHERE subject='condition/a' AND origin='alder' ORDER BY breached,accepted_key,origin,instance LIMIT 1",
            "SELECT origin,instance,store_index,breached,accepted_key FROM local_condition_heads WHERE subject='condition/a' ORDER BY breached,accepted_key,origin,instance LIMIT 1",
        ] {
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("; ");
            assert!(
                !plan.contains("SCAN claims") && !plan.contains("SCAN c"),
                "{sql}: {plan}"
            );
            assert!(plan.contains("SEARCH"), "{sql}: {plan}");
        }
    }

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
        fold_all(&store);
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
        fold_all(&target);
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
        fold_all(&target);
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
        fold_all(&store);
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
        fold_all(&store);
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
        fold_all(&store);
        let mut breached = Tracker::default();
        breached.observe(&disk, 10.0, 0);
        let transition = breached.observe(&disk, 10.0, 600_000);
        record(&store, &disk, "alder:/new", &breached, transition, 600_000);
        fold_all(&store);
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
        fold_all(&store);
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
        assert!(
            store
                .condition_evaluator_status()
                .unwrap()
                .1
                .unwrap()
                .contains("discarded")
        );
        store
            .connection
            .batched(|tx| set_meta_integer(tx, "condition_notification_discarded_at", 0))
            .unwrap()
            .unwrap();
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
        fold_all(&store);
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
        fold_all(&store);
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
        fold_all(&store);
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
