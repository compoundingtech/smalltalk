//! Current person context, never authorization evidence. Claims remain authoritative.
use super::*;
use smallclaims::store::canonical;
use st3_schema::directive_notes::KIND;

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectiveNote {
    pub person: String,
    pub author: String,
    pub time: String,
    pub text: String,
    pub expires_at: Option<String>,
    pub revision: String,
}

const CURRENT_QUERY: &str = "SELECT c.id,c.store_index,c.batch_id,c.subject,c.kind,c.origin,c.actor,c.body,c.predecessors,c.accepted_at_unix_ms
    FROM person_directive_notes n JOIN claims c ON c.id=n.claim_id WHERE n.person=?1";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS person_directive_notes (
    person TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL,
    head_key BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS local_directive_note_pending (claim_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS local_directive_note_dirty (subject TEXT PRIMARY KEY);
CREATE TRIGGER IF NOT EXISTS directive_note_claim_insert AFTER INSERT ON claims
WHEN NEW.kind='person.directive-note-set' BEGIN
    INSERT OR IGNORE INTO local_directive_note_pending VALUES(NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS directive_note_legacy_insert AFTER INSERT ON claims BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE batch_id=NEW.batch_id AND store_index>NEW.store_index
        AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS directive_note_claim_delete AFTER DELETE ON claims BEGIN
    DELETE FROM local_directive_note_pending WHERE claim_id=OLD.id;
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT OLD.subject WHERE OLD.kind='person.directive-note-set';
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE batch_id=OLD.batch_id AND store_index>OLD.store_index
        AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS directive_note_claim_update
AFTER UPDATE OF id,batch_id,subject,kind,accepted_at_unix_ms,store_index ON claims
WHEN OLD.id IS NOT NEW.id OR OLD.batch_id IS NOT NEW.batch_id
    OR OLD.subject IS NOT NEW.subject OR OLD.kind IS NOT NEW.kind
    OR OLD.accepted_at_unix_ms IS NOT NEW.accepted_at_unix_ms
    OR OLD.store_index IS NOT NEW.store_index BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT OLD.subject WHERE OLD.kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=OLD.id);
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT NEW.subject WHERE NEW.kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=OLD.id);
    INSERT OR IGNORE INTO local_directive_note_pending
    SELECT NEW.id WHERE NEW.kind='person.directive-note-set'
        AND (OLD.kind<>'person.directive-note-set'
            OR EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=OLD.id));
    DELETE FROM local_directive_note_pending
    WHERE claim_id=OLD.id AND (OLD.id IS NOT NEW.id OR NEW.kind<>'person.directive-note-set');
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims
    WHERE (OLD.batch_id IS NOT NEW.batch_id OR OLD.store_index IS NOT NEW.store_index)
        AND ((batch_id=OLD.batch_id AND store_index>OLD.store_index)
            OR (batch_id=NEW.batch_id AND store_index>NEW.store_index))
        AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS directive_note_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE id=NEW.claim_id AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=NEW.claim_id)
        AND (SELECT MIN(position) FROM replica_records WHERE claim_id=NEW.claim_id)
            IS NOT COALESCE(
                (SELECT MIN(position) FROM replica_records
                    WHERE claim_id=NEW.claim_id AND record_ref<>NEW.record_ref),
                (SELECT COUNT(*) FROM claims legacy_position
                    WHERE legacy_position.batch_id=claims.batch_id
                        AND legacy_position.store_index<claims.store_index));
END;
CREATE TRIGGER IF NOT EXISTS directive_note_record_update
AFTER UPDATE OF position,claim_id ON replica_records
WHEN OLD.position IS NOT NEW.position OR OLD.claim_id IS NOT NEW.claim_id BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE id IN (OLD.claim_id,NEW.claim_id)
        AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS directive_note_record_delete AFTER DELETE ON replica_records
WHEN OLD.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE id=OLD.claim_id AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=OLD.claim_id)
        AND ((SELECT MIN(position) FROM replica_records WHERE claim_id=OLD.claim_id)>OLD.position
            OR (NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=OLD.claim_id)
                AND OLD.position IS NOT (SELECT COUNT(*) FROM claims legacy_position
                    WHERE legacy_position.batch_id=claims.batch_id
                        AND legacy_position.store_index<claims.store_index)));
END;
CREATE TRIGGER IF NOT EXISTS directive_note_batch_update
AFTER UPDATE OF origin,replica_sequence ON batches
WHEN OLD.origin IS NOT NEW.origin OR OLD.replica_sequence IS NOT NEW.replica_sequence BEGIN
    INSERT OR IGNORE INTO local_directive_note_dirty
    SELECT subject FROM claims WHERE batch_id=NEW.id AND kind='person.directive-note-set'
        AND NOT EXISTS (SELECT 1 FROM local_directive_note_pending WHERE claim_id=claims.id);
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    // Earlier builds cannot admit this kind. Install an empty projection before any new
    // admission, without scanning or rewriting the existing log during schema migration.
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn rebuild(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DELETE FROM person_directive_notes;
        DELETE FROM local_directive_note_pending;
        DELETE FROM local_directive_note_dirty;
        INSERT INTO local_directive_note_pending SELECT id FROM claims WHERE kind='person.directive-note-set';")?;
    flush(tx)
}

pub(super) fn flush(tx: &Transaction<'_>) -> Result<()> {
    let dirty = tx.prepare("SELECT subject FROM local_directive_note_dirty ORDER BY subject")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for person in dirty {
        tx.execute("DELETE FROM person_directive_notes WHERE person=?1", [&person])?;
        let latest = tx.query_row(&canonical_sql("SELECT id FROM claims WHERE subject=?1 AND kind='person.directive-note-set'
            ORDER BY CANONICAL_DESC(claims) LIMIT 1"), [&person], |row| row.get::<_, String>(0)).optional()?;
        if let Some(id) = latest { project(tx, &person, &id)?; }
    }
    let pending = tx.prepare("SELECT c.subject,c.id FROM local_directive_note_pending p JOIN claims c ON c.id=p.claim_id ORDER BY p.claim_id")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (person, id) in pending { project(tx, &person, &id)?; }
    tx.execute_batch("DELETE FROM local_directive_note_dirty; DELETE FROM local_directive_note_pending;")?;
    Ok(())
}

fn project(tx: &Transaction<'_>, person: &str, id: &str) -> Result<()> {
    let key = canonical::sortable_key(&canonical::claim_key(tx, id)?);
    tx.execute("INSERT INTO person_directive_notes(person,claim_id,head_key) VALUES(?1,?2,?3)
        ON CONFLICT(person) DO UPDATE SET claim_id=excluded.claim_id,head_key=excluded.head_key
        WHERE excluded.head_key>person_directive_notes.head_key", params![person, id, key])?;
    Ok(())
}

/// A pinned membership is necessary, not proof of undiscovered legacy peers. Operators must
/// fence legacy replication before enabling notes; reject every observed unfenced writer too.
pub(super) fn validate_publication(tx: &Transaction<'_>, origin: &str) -> Result<(), St3Error> {
    fn refused(message: impl Into<String>) -> St3Error {
        St3Error::new("directive-note-feature-barrier", message)
    }
    // A newly staged local admission/removal is not in the signed membership fold yet.
    // Do not publish against yesterday's members while the replication writer seals it.
    let pending: bool = tx.query_row(&format!(
        "SELECT EXISTS(SELECT 1 FROM claims c JOIN batches b ON b.id=c.batch_id
         LEFT JOIN replica_envelopes e ON e.batch_id=b.id
         WHERE c.origin=?1 AND c.kind IN ({})
           AND NOT EXISTS(SELECT 1 FROM replica_envelope_signatures s
             WHERE s.writer=b.origin AND s.sequence=b.replica_sequence
               AND s.envelope_hash=e.envelope_hash))",
        smallclaims::store::FLEET_CLAIM_KINDS,
    ), [origin], |row| row.get(0)).map_err(internal)?;
    if pending {
        return Err(refused("pending local fleet membership must be signed before publishing directive notes"));
    }
    let membership = fleet_membership_tx(tx).map_err(internal)?;
    if membership.anchor().is_none()
        || !matches!(membership.state(origin), crate::fleet::MemberState::Current(_))
    {
        return Err(refused("directive notes require anchored membership and an active local member; fence legacy peers from replication before enabling notes"));
    }
    for member in membership.incarnations().filter(|member| member.end.is_none()) {
        if !matches!(membership.state(&member.name), crate::fleet::MemberState::Current(_)) {
            return Err(refused("conflicted fleet membership cannot enable directive notes"));
        }
        let supported: Option<bool> = tx.query_row(&canonical_sql(
            "SELECT json_extract(claims.body,'$.fields.features.person_directive_note')=1
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='daemon.started' AND claims.origin=?2
                 AND batches.replica_sequence>=?3 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            params![format!("daemon/{}", member.name), member.name, member.start], |row| row.get(0))
            .optional().map_err(internal)?.flatten();
        if supported != Some(true) {
            return Err(refused(format!("host/{} has not advertised features.person_directive_note=1; upgrade or fence it before enabling notes", member.name)));
        }
    }
    let fenced = membership.legacy_removed_names().collect::<BTreeSet<_>>();
    // Seek past each writer prefix instead of scanning all retained batches/envelopes.
    // Checkpoint tombstones still witness legacy writers after their payloads are trimmed.
    for (table, column) in [
        ("replica_envelopes", "writer"),
        ("batches", "origin"),
        ("checkpoint_envelopes", "writer"),
    ] {
        let mut statement = tx.prepare(&format!(
            "SELECT {column} FROM {table} WHERE {column}>?1 ORDER BY {column} LIMIT 1"
        )).map_err(internal)?;
        let mut previous = String::new();
        while let Some(writer) = statement.query_row([&previous], |row| row.get::<_, String>(0))
            .optional().map_err(internal)?
        {
            if !membership.is_keyed_writer(&writer) && !fenced.contains(&writer) {
                return Err(refused(format!("legacy writer `{writer}` remains unfenced; feature advertisements do not establish complete legacy discovery")));
            }
            previous = writer;
        }
    }
    Ok(())
}

fn current(connection: &Connection, person: &str, now: u128) -> Result<Option<DirectiveNote>> {
    smallclaims::touched::note_read(|| person.to_owned());
    let claim = connection.query_row(CURRENT_QUERY, [person], claim_from_row).optional()?;
    let Some(mut claim) = claim else { return Ok(None); };
    let fields = claim.body.get_mut("fields").and_then(Value::as_object_mut)
        .context("admitted directive note has no fields")?;
    let Some(Value::String(text)) = fields.remove("text") else { return Ok(None); };
    let expires_at = match fields.remove("expires_at") {
        Some(Value::String(at)) => Some(at),
        None => None,
        Some(_) => anyhow::bail!("admitted directive note has an invalid expiry"),
    };
    if let Some(at) = expires_at.as_deref() {
        let expires = st3_schema::directive_notes::expiry(at).map_err(anyhow::Error::new)?;
        let expires_nanos = i128::from(expires.timestamp()) * 1_000_000_000
            + i128::from(expires.timestamp_subsec_nanos());
        let now_nanos = i128::try_from(now).unwrap_or(i128::MAX).saturating_mul(1_000_000);
        if expires_nanos <= now_nanos { return Ok(None); }
    }
    Ok(Some(DirectiveNote {
        person: claim.subject,
        author: claim.actor.context("admitted directive note has no person author")?,
        time: chrono::DateTime::from_timestamp_millis(i64::try_from(claim.accepted_at_unix_ms).unwrap_or(i64::MAX))
            .context("directive note creation time is outside UTC timestamp range")?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        text,
        expires_at,
        revision: claim.id,
    }))
}

impl Store {
    pub fn set_directive_note(&self, person: &str, actor: &str, text: Option<&str>, expires_at: Option<&str>) -> Result<Option<DirectiveNote>, St3Error> {
        let mut fields = BTreeMap::from([("text".into(), json!(text))]);
        if let Some(at) = expires_at { fields.insert("expires_at".into(), json!(at)); }
        self.append_claim(&ClaimInput {
            subject: person.into(), kind: KIND.into(), actor: Some(actor.into()), fields,
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        })?;
        current(&self.readers.get(), person, now_ms()).map_err(internal)
    }

    pub fn directive_notes(&self, actor: &str) -> Result<Vec<DirectiveNote>, St3Error> {
        validate_actor(actor)?;
        self.read_snapshot(|_| {
            let people = self.directive_note_people(actor)?;
            let connection = self.readers.get();
            let now = now_ms();
            let mut notes = Vec::with_capacity(people.len());
            for person in people {
                if let Some(note) = current(&connection, &person, now)? { notes.push(note); }
            }
            Ok(notes)
        }).map_err(internal)
    }

    fn directive_note_people(&self, actor: &str) -> Result<BTreeSet<String>> {
        const MAX_HOPS: usize = 16;
        const MAX_RELATIONS: usize = 64;
        let mut people = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut work_relations = BTreeSet::new();
        let mut pending = VecDeque::from([(actor.to_owned(), 0)]);
        while let Some((actor, hops)) = pending.pop_front() {
            if actor.starts_with("person/") {
                if st3_schema::directive_notes::validate_actor(&actor, KIND, Some(&actor)).is_ok() {
                    people.insert(actor);
                }
                continue;
            }
            if !actor.starts_with("agent/") || hops >= MAX_HOPS || !seen.insert(actor.clone()) { continue; }
            anyhow::ensure!(seen.len() <= MAX_RELATIONS, "directive note ownership exceeds the bounded relation set");
            let Some((desired, author)) = self.desired_subject_with_writer(&actor)? else { continue; };
            let owner = match crate::accounts::harness_binding(&desired.desired).map(|binding| binding.binding) {
                Some(crate::accounts::Binding::Pool(person)) => Some(person),
                Some(crate::accounts::Binding::Account(account)) => self.desired_subject_with_writer(&format!("account/{account}"))?
                    .and_then(|(account, _)| crate::accounts::parse_account(&account.subject, &account.desired))
                    .and_then(|account| account.owner),
                None => None,
            };
            let requester = desired.owner_run.as_deref().map(|run| {
                smallclaims::touched::note_read(|| run.to_owned());
                self.readers.get().query_row(
                    "SELECT requester FROM mission_runs WHERE id=?1",
                    [run.trim_start_matches("mission-run/")],
                    |row| row.get::<_, String>(0),
                ).optional()
            }).transpose()?.flatten();
            // Union all relations. An account owner must not hide the person requesting work,
            // and requester precedence must not hide a different declaration author.
            for next in [owner, author, requester].into_iter().flatten() {
                pending.push_back((next, hops + 1));
            }
            // A top-level seat can also work for a run without being owned by that run.
            // Bound the indexed source rows before joining, including stale rows in the
            // budget: an oversized relation set fails closed rather than scanning history.
            smallclaims::touched::note_read(|| format!("actor:{actor}"));
            for column in ["assignee", "lease_owner"] {
                let connection = self.readers.get();
                let mut statement = connection.prepare(&format!(
                    "SELECT s.subject,r.requester FROM (
                         SELECT subject,run_id,generation_id FROM step_runs WHERE {column}=?1
                         AND status IN ('pending','ready','claimed','working','submitted','verifying','blocked','waiting-person')
                         LIMIT ?2
                     ) s LEFT JOIN mission_runs r ON r.id=s.run_id
                         AND r.current_generation_id=s.generation_id
                         AND r.status IN ('running','standing','blocked')
                         AND r.phase NOT LIKE 'cleanup-%' AND r.phase<>'terminal'"
                ))?;
                let rows = statement.query_map(params![actor, MAX_RELATIONS + 1], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                for row in rows {
                    let (subject, requester) = row?;
                    work_relations.insert(subject);
                    anyhow::ensure!(work_relations.len() <= MAX_RELATIONS, "directive note work exceeds the bounded relation set");
                    if let Some(requester) = requester {
                        pending.push_back((requester, hops + 1));
                    }
                }
            }
        }
        Ok(people)
    }
}

#[cfg(test)]
mod tests;
