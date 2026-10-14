//! Rebuildable current glass heads. Durable claims remain the authority; queue rows are local.
use super::*;
use smallclaims::store::canonical;
use st3_schema::glasses::MAX_GLASSES;

const VERSION: &str = "1";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS glass_heads (
    subject TEXT PRIMARY KEY,
    person TEXT NOT NULL,
    created_key BLOB,
    head_key BLOB,
    claim_id TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    CHECK((created_key IS NULL) = (head_key IS NULL)),
    CHECK((head_key IS NULL) = (claim_id IS NULL)),
    CHECK(claim_id IS NOT NULL OR deleted=1)
);
CREATE INDEX IF NOT EXISTS glass_heads_live_person_created
ON glass_heads(person, created_key, subject) WHERE deleted=0 AND claim_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_glass_head_pending (claim_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS local_glass_head_dirty (subject TEXT PRIMARY KEY);

CREATE TRIGGER IF NOT EXISTS glass_heads_claim_insert AFTER INSERT ON claims
WHEN NEW.kind IN ('glass.upserted','glass.deleted') BEGIN
    INSERT OR IGNORE INTO local_glass_head_pending VALUES(NEW.id);
END;
-- An explicit earlier store index can change legacy wire positions. An ordinary append
-- finds no later claims through claims_batch_index, regardless of the person's history.
CREATE TRIGGER IF NOT EXISTS glass_heads_legacy_insert AFTER INSERT ON claims BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE batch_id=NEW.batch_id AND store_index>NEW.store_index
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS glass_heads_claim_delete AFTER DELETE ON claims BEGIN
    DELETE FROM local_glass_head_pending WHERE claim_id=OLD.id;
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT OLD.subject WHERE OLD.kind IN ('glass.upserted','glass.deleted');
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE batch_id=OLD.batch_id AND store_index>OLD.store_index
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS glass_heads_claim_update
AFTER UPDATE OF id,batch_id,subject,kind,accepted_at_unix_ms,store_index ON claims
WHEN OLD.id IS NOT NEW.id OR OLD.batch_id IS NOT NEW.batch_id
    OR OLD.subject IS NOT NEW.subject OR OLD.kind IS NOT NEW.kind
    OR OLD.accepted_at_unix_ms IS NOT NEW.accepted_at_unix_ms
    OR OLD.store_index IS NOT NEW.store_index BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT OLD.subject WHERE OLD.kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=OLD.id);
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT NEW.subject WHERE NEW.kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=OLD.id);
    INSERT OR IGNORE INTO local_glass_head_pending
    SELECT NEW.id WHERE NEW.kind IN ('glass.upserted','glass.deleted')
        AND (OLD.kind NOT IN ('glass.upserted','glass.deleted')
            OR EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=OLD.id));
    DELETE FROM local_glass_head_pending
    WHERE claim_id=OLD.id AND (OLD.id IS NOT NEW.id
        OR NEW.kind NOT IN ('glass.upserted','glass.deleted'));
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims
    WHERE (OLD.batch_id IS NOT NEW.batch_id OR OLD.store_index IS NOT NEW.store_index)
        AND ((batch_id=OLD.batch_id AND store_index>OLD.store_index)
            OR (batch_id=NEW.batch_id AND store_index>NEW.store_index))
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id)
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=claims.id);
END;

-- A pending claim has not contributed its key yet: flush observes its final metadata.
-- For an already projected claim, inserting the first receipt only matters when its wire
-- position differs from the legacy fallback. Deferred local envelope seeding usually agrees.
CREATE TRIGGER IF NOT EXISTS glass_heads_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE id=NEW.claim_id
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=NEW.claim_id)
        AND (SELECT MIN(position) FROM replica_records WHERE claim_id=NEW.claim_id)
            IS NOT COALESCE(
                (SELECT MIN(position) FROM replica_records
                    WHERE claim_id=NEW.claim_id AND record_ref<>NEW.record_ref),
                (SELECT COUNT(*) FROM claims legacy_position
                    WHERE legacy_position.batch_id=claims.batch_id
                        AND legacy_position.store_index<claims.store_index));
END;
CREATE TRIGGER IF NOT EXISTS glass_heads_record_update
AFTER UPDATE OF position,claim_id ON replica_records
WHEN OLD.position IS NOT NEW.position OR OLD.claim_id IS NOT NEW.claim_id BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE id IN (OLD.claim_id,NEW.claim_id)
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS glass_heads_record_delete AFTER DELETE ON replica_records
WHEN OLD.claim_id IS NOT NULL BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE id=OLD.claim_id
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=OLD.claim_id)
        AND ((SELECT MIN(position) FROM replica_records WHERE claim_id=OLD.claim_id)>OLD.position
            OR (NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=OLD.claim_id)
                AND OLD.position IS NOT (SELECT COUNT(*) FROM claims legacy_position
                    WHERE legacy_position.batch_id=claims.batch_id
                        AND legacy_position.store_index<claims.store_index)));
END;
CREATE TRIGGER IF NOT EXISTS glass_heads_batch_update
AFTER UPDATE OF origin,replica_sequence ON batches
WHEN OLD.origin IS NOT NEW.origin OR OLD.replica_sequence IS NOT NEW.replica_sequence BEGIN
    INSERT OR IGNORE INTO local_glass_head_dirty
    SELECT subject FROM claims WHERE batch_id=NEW.id
        AND kind IN ('glass.upserted','glass.deleted')
        AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending WHERE claim_id=claims.id);
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(SCHEMA)
        .context("creating glass head projection")
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let version: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key='glass_heads_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version.as_deref() == Some(VERSION) {
        flush(transaction)
    } else {
        rebuild(transaction)
    }
}

pub(super) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "DELETE FROM glass_heads;
         DELETE FROM local_glass_head_pending;
         DELETE FROM local_glass_head_dirty;
         INSERT INTO local_glass_head_dirty
             SELECT DISTINCT subject FROM claims WHERE kind IN ('glass.upserted','glass.deleted');",
    )?;
    flush(transaction)?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('glass_heads_version',?1)",
        [VERSION],
    )?;
    Ok(())
}

/// Call inside the writer transaction before preparing a glass write and before committing
/// newly admitted glass claims. Metadata corrections take the exceptional subject replay path.
pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let subjects = transaction
        .prepare("SELECT subject FROM local_glass_head_dirty ORDER BY subject")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in subjects {
        refresh(transaction, &subject)?;
    }
    let pending = transaction
        .prepare(
            "SELECT claims.id,claims.subject,claims.kind FROM local_glass_head_pending pending
             JOIN claims ON claims.id=pending.claim_id ORDER BY pending.claim_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, subject, kind) in pending {
        let person = st3_schema::glasses::owner(&subject).map_err(anyhow::Error::new)?;
        if kind == "glass.deleted" {
            transaction.execute(
                "INSERT INTO glass_heads(subject,person,deleted) VALUES(?1,?2,1)
                 ON CONFLICT(subject) DO UPDATE SET deleted=1",
                params![subject, person],
            )?;
        } else if kind == "glass.upserted" {
            let key = canonical::sortable_key(&canonical::claim_key(transaction, &id)?);
            transaction.execute(
                "INSERT INTO glass_heads(subject,person,created_key,head_key,claim_id,deleted)
                 VALUES(?1,?2,?3,?3,?4,0) ON CONFLICT(subject) DO UPDATE SET
                 created_key=CASE WHEN created_key IS NULL OR excluded.created_key<created_key
                     THEN excluded.created_key ELSE created_key END,
                 claim_id=CASE WHEN head_key IS NULL OR excluded.head_key>head_key
                     THEN excluded.claim_id ELSE claim_id END,
                 head_key=CASE WHEN head_key IS NULL OR excluded.head_key>head_key
                     THEN excluded.head_key ELSE head_key END",
                params![subject, person, key, id],
            )?;
        }
    }
    transaction.execute_batch(
        "DELETE FROM local_glass_head_pending;
         DELETE FROM local_glass_head_dirty;",
    )?;
    Ok(())
}

fn refresh(transaction: &Transaction<'_>, subject: &str) -> Result<()> {
    let deleted: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='glass.deleted')",
        [subject],
        |row| row.get(0),
    )?;
    let created: Option<String> = transaction
        .query_row(
            &canonical_sql(
                "SELECT id FROM claims WHERE subject=?1 AND kind='glass.upserted'
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1",
            ),
            [subject],
            |row| row.get(0),
        )
        .optional()?;
    let latest: Option<String> = transaction
        .query_row(
            &canonical_sql(
                "SELECT id FROM claims WHERE subject=?1 AND kind='glass.upserted'
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ),
            [subject],
            |row| row.get(0),
        )
        .optional()?;
    if created.is_none() && !deleted {
        transaction.execute("DELETE FROM glass_heads WHERE subject=?1", [subject])?;
        return Ok(());
    }
    let person = st3_schema::glasses::owner(subject).map_err(anyhow::Error::new)?;
    let created_key = created
        .as_deref()
        .map(|id| canonical::claim_key(transaction, id).map(|key| canonical::sortable_key(&key)))
        .transpose()?;
    let head_key = latest
        .as_deref()
        .map(|id| canonical::claim_key(transaction, id).map(|key| canonical::sortable_key(&key)))
        .transpose()?;
    transaction.execute(
        "INSERT INTO glass_heads(subject,person,created_key,head_key,claim_id,deleted)
         VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(subject) DO UPDATE SET
         person=excluded.person,created_key=excluded.created_key,head_key=excluded.head_key,
         claim_id=excluded.claim_id,deleted=excluded.deleted",
        params![subject, person, created_key, head_key, latest, deleted],
    )?;
    Ok(())
}

/// Eligibility and the pinned-head read share one SQLite snapshot. No result rows means
/// historical/dirty fallback; the left-join sentinel means an eligible empty collection.
pub(super) fn current_claims(
    connection: &Connection,
    person: &str,
    through: u64,
) -> Result<Option<Vec<ClaimRecord>>> {
    let mut statement = connection.prepare(
        "WITH eligible AS (
             SELECT 1 WHERE MAX(
                 COALESCE((SELECT MAX(store_index) FROM claims),0),
                 COALESCE((SELECT seq FROM sqlite_sequence WHERE name='claims'),0))<=?2
                 AND NOT EXISTS (SELECT 1 FROM local_glass_head_pending)
                 AND NOT EXISTS (SELECT 1 FROM local_glass_head_dirty)
                 AND EXISTS (SELECT 1 FROM meta WHERE key='glass_heads_version' AND value=?4)
         ), selected AS MATERIALIZED (
             SELECT claim_id,created_key,subject FROM glass_heads
             WHERE person=?1 AND deleted=0 AND claim_id IS NOT NULL AND EXISTS (SELECT 1 FROM eligible)
             ORDER BY created_key,subject LIMIT ?3
         )
         SELECT claims.id,claims.store_index,claims.batch_id,claims.subject,claims.kind,
             claims.origin,claims.actor,claims.body,claims.predecessors,claims.accepted_at_unix_ms
         FROM eligible LEFT JOIN selected ON 1 LEFT JOIN claims ON claims.id=selected.claim_id
         ORDER BY selected.created_key,selected.subject",
    )?;
    let mut rows = statement.query(params![
        person,
        through.min(i64::MAX as u64),
        i64::try_from(MAX_GLASSES)?,
        VERSION,
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    if matches!(row.get_ref(0)?, rusqlite::types::ValueRef::Null) {
        return Ok(Some(Vec::new()));
    }
    let mut claims = Vec::with_capacity(MAX_GLASSES);
    claims.push(claim_from_row(row)?);
    while let Some(row) = rows.next()? {
        claims.push(claim_from_row(row)?);
    }
    Ok(Some(claims))
}

pub(super) struct SubjectHead {
    pub(super) claim_id: Option<String>,
    pub(super) deleted: bool,
}

/// Writer callers must flush first; this lookup never reads a subject's durable history.
pub(super) fn head(connection: &Connection, subject: &str) -> Result<Option<SubjectHead>> {
    connection
        .query_row(
            "SELECT claim_id,deleted FROM glass_heads WHERE subject=?1",
            [subject],
            |row| {
                Ok(SubjectHead {
                    claim_id: row.get(0)?,
                    deleted: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

/// Only quota occupancy matters; retained overflow never makes this read exceed 100 rows.
/// Writer callers must flush first.
pub(super) fn live_count(connection: &Connection, person: &str) -> Result<usize> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM glass_heads WHERE person=?1 AND deleted=0 AND claim_id IS NOT NULL
                 LIMIT ?2)",
            params![person, i64::try_from(MAX_GLASSES)?],
            |row| row.get(0),
        )
        .map_err(Into::into)
}
