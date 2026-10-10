//! Body-free message selectors: ordered eligibility seeks, then page-only claim folds.
use super::*;

#[cfg(test)]
const TABLE: &str = "local_client_message_selectors_v1";
const MARKER: &str = "client_message_selectors_v1_cut";
const BACKFILL_MARKER: &str = "client_message_selectors_v1_backfill";
const FOLD_MARKER: &str = "client_message_selectors_v1_fold";
const MALFORMED_MARKER: &str = "client_message_selectors_v1_malformed_desired";
// Leave ample room for SQLite's durable commit below the 100ms transaction target.
const BACKFILL_SUBJECTS: usize = 16;
const BACKFILL_CLEAR_ROWS: usize = 64;
const FOLD_CLAIMS: usize = 64;
const PRUNE_ROWS: usize = 64;
const BACKFILL_WORK_BUDGET: std::time::Duration = std::time::Duration::from_millis(10);
const SELECTOR_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_client_message_selectors_v1(
 subject TEXT NOT NULL,born_index INTEGER NOT NULL,retired_index INTEGER,
 sender TEXT NOT NULL DEFAULT 'requester',recipient TEXT NOT NULL DEFAULT '',
 mailbox INTEGER NOT NULL DEFAULT 0,closed INTEGER NOT NULL DEFAULT 0,
 reminder TEXT,version TEXT NOT NULL DEFAULT '00000000000000000000',
 sent_key TEXT NOT NULL DEFAULT '',created_index INTEGER NOT NULL DEFAULT 0,
 desired_mask INTEGER NOT NULL DEFAULT 0,native_mailbox INTEGER NOT NULL DEFAULT 0,
 global_current INTEGER NOT NULL DEFAULT 0,recipient_current INTEGER NOT NULL DEFAULT 0,
 retired_at_unix_ms INTEGER,dirty_flags INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(subject,born_index)
) WITHOUT ROWID;
-- born_index=-1 is durable pending bookkeeping, never a page candidate.
CREATE INDEX IF NOT EXISTS client_selector_order ON local_client_message_selectors_v1(
 sent_key DESC,subject,born_index,retired_index,created_index,global_current,recipient_current,mailbox) WHERE born_index>=0;
CREATE INDEX IF NOT EXISTS client_selector_sender ON local_client_message_selectors_v1(
 sender,sent_key DESC,subject,born_index,retired_index,created_index,global_current,recipient_current,mailbox) WHERE born_index>=0;
CREATE INDEX IF NOT EXISTS client_selector_recipient ON local_client_message_selectors_v1(
 recipient,sent_key DESC,subject,born_index,retired_index,created_index,global_current,recipient_current,mailbox) WHERE born_index>=0;
CREATE INDEX IF NOT EXISTS client_selector_recipient_sender ON local_client_message_selectors_v1(
 recipient,sender,sent_key DESC,subject,born_index,retired_index,created_index,global_current,recipient_current,mailbox) WHERE born_index>=0;
CREATE INDEX IF NOT EXISTS client_selector_global_eligible ON local_client_message_selectors_v1(
 sent_key DESC,subject,born_index,created_index) WHERE born_index>=0 AND retired_index IS NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_sender_eligible ON local_client_message_selectors_v1(
 sender,sent_key DESC,subject,born_index,created_index) WHERE born_index>=0 AND retired_index IS NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_global_eligible ON local_client_message_selectors_v1(
 recipient,sent_key DESC,subject,born_index,created_index) WHERE born_index>=0 AND retired_index IS NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_eligible ON local_client_message_selectors_v1(
 recipient,sent_key DESC,subject,born_index,created_index) WHERE born_index>=0 AND retired_index IS NULL AND recipient_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_sender_eligible ON local_client_message_selectors_v1(
 recipient,sender,sent_key DESC,subject,born_index,created_index) WHERE born_index>=0 AND retired_index IS NULL AND recipient_current=1;
DROP INDEX IF EXISTS client_selector_global_cut_eligible;
DROP INDEX IF EXISTS client_selector_sender_cut_eligible;
DROP INDEX IF EXISTS client_selector_recipient_global_cut_eligible;
DROP INDEX IF EXISTS client_selector_recipient_cut_eligible;
DROP INDEX IF EXISTS client_selector_recipient_sender_cut_eligible;
CREATE INDEX IF NOT EXISTS client_selector_global_retired_eligible ON local_client_message_selectors_v1(
 sent_key DESC,subject,born_index,retired_index,created_index) WHERE born_index>=0 AND retired_index IS NOT NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_sender_retired_eligible ON local_client_message_selectors_v1(
 sender,sent_key DESC,subject,born_index,retired_index,created_index) WHERE born_index>=0 AND retired_index IS NOT NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_global_retired_eligible ON local_client_message_selectors_v1(
 recipient,sent_key DESC,subject,born_index,retired_index,created_index) WHERE born_index>=0 AND retired_index IS NOT NULL AND global_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_retired_eligible ON local_client_message_selectors_v1(
 recipient,sent_key DESC,subject,born_index,retired_index,created_index) WHERE born_index>=0 AND retired_index IS NOT NULL AND recipient_current=1;
CREATE INDEX IF NOT EXISTS client_selector_recipient_sender_retired_eligible ON local_client_message_selectors_v1(
 recipient,sender,sent_key DESC,subject,born_index,retired_index,created_index) WHERE born_index>=0 AND retired_index IS NOT NULL AND recipient_current=1;
CREATE INDEX IF NOT EXISTS client_selector_reminder_candidates ON local_client_message_selectors_v1(
 reminder,version DESC,subject DESC) WHERE born_index>=0 AND retired_index IS NULL AND closed=0;
CREATE INDEX IF NOT EXISTS client_selector_recipient_reminder_candidates ON local_client_message_selectors_v1(
 recipient,reminder,version DESC,subject DESC) WHERE born_index>=0 AND retired_index IS NULL AND closed=0 AND mailbox=1;
CREATE INDEX IF NOT EXISTS client_selector_reminder_winner ON local_client_message_selectors_v1(reminder,subject)
 WHERE born_index>=0 AND retired_index IS NULL AND global_current=1 AND reminder IS NOT NULL;
CREATE INDEX IF NOT EXISTS client_selector_recipient_reminder_winner ON local_client_message_selectors_v1(recipient,reminder,subject)
 WHERE born_index>=0 AND retired_index IS NULL AND recipient_current=1 AND reminder IS NOT NULL;
CREATE INDEX IF NOT EXISTS client_selector_retirement ON local_client_message_selectors_v1(retired_at_unix_ms)
 WHERE retired_at_unix_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS client_selector_pending ON local_client_message_selectors_v1(subject)
 WHERE born_index=-1;
CREATE INDEX IF NOT EXISTS client_selector_destructive ON local_client_message_selectors_v1(created_index,subject)
 WHERE born_index=-2;
CREATE INDEX IF NOT EXISTS client_selector_retired_cut ON local_client_message_selectors_v1(retired_index)
 WHERE retired_index IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_client_message_publications(
 through_index INTEGER PRIMARY KEY,published_at_unix_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_client_message_fold_batches(
 batch_id TEXT PRIMARY KEY,after_index INTEGER NOT NULL,claim_count INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS client_selector_claim_insert AFTER INSERT ON claims WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(NEW.subject,-1,1,CASE WHEN NEW.kind IN ('message.sent','message.closed','intent.desired') OR json_type(NEW.body,'$.fields.from') IS NOT NULL OR json_type(NEW.body,'$.fields.to') IS NOT NULL OR json_type(NEW.body,'$.fields.tags') IS NOT NULL OR json_type(NEW.body,'$.fields.status') IS NOT NULL OR (json_type(NEW.body,'$.fields') IS NULL AND (json_type(NEW.body,'$.from') IS NOT NULL OR json_type(NEW.body,'$.to') IS NOT NULL OR json_type(NEW.body,'$.tags') IS NOT NULL OR json_type(NEW.body,'$.status') IS NOT NULL)) THEN 1 ELSE 0 END)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|excluded.dirty_flags,created_index=created_index+1;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_claim_delete AFTER DELETE ON claims WHEN OLD.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(OLD.subject,-1,1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2,created_index=created_index+1;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index) VALUES(OLD.subject,-2,9223372036854775807)
 ON CONFLICT(subject,born_index) DO UPDATE SET created_index=excluded.created_index;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_claim_update AFTER UPDATE ON claims WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(OLD.subject,-1,1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2,created_index=created_index+1;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(NEW.subject,-1,1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2,created_index=created_index+1;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index) VALUES(OLD.subject,-2,9223372036854775807)
 ON CONFLICT(subject,born_index) DO UPDATE SET created_index=excluded.created_index;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index) VALUES(NEW.subject,-2,9223372036854775807)
 ON CONFLICT(subject,born_index) DO UPDATE SET created_index=excluded.created_index;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_repair AFTER UPDATE OF state,replacement_claim_id ON replica_records
WHEN NEW.state='repaired' AND (OLD.state!='repaired' OR OLD.replacement_claim_id IS NOT NEW.replacement_claim_id) BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags)
 SELECT subject,-1,1,2 FROM claims WHERE id=NEW.claim_id AND subject LIKE 'message/%'
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2,created_index=created_index+1;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index)
 SELECT subject,-2,9223372036854775807 FROM claims WHERE id=NEW.claim_id AND subject LIKE 'message/%'
 ON CONFLICT(subject,born_index) DO UPDATE SET created_index=excluded.created_index;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_insert AFTER INSERT ON desired WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(NEW.subject,-1,1,1)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|1,created_index=created_index+1;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_update AFTER UPDATE ON desired WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(NEW.subject,-1,1,1)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|1,created_index=created_index+1;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_delete AFTER DELETE ON desired WHEN OLD.subject LIKE 'message/%' BEGIN
 -- Preserve the affected key before clearing declaration-derived routing fields.
 INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags,reminder,recipient)
 VALUES(OLD.subject,-1,1,4,
  (SELECT reminder FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index>=0 AND retired_index IS NULL),
  COALESCE((SELECT recipient FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index>=0 AND retired_index IS NULL),''))
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|4,reminder=COALESCE(reminder,excluded.reminder),created_index=created_index+1;
 -- Deletion invalidates all old cuts. Updating live headers in-place is safe and
 -- preserves fresh-page parity even for direct projection deletion before flush.
 UPDATE local_client_message_selectors_v1 SET
  sender=CASE WHEN desired_mask&1!=0 THEN 'requester' ELSE sender END,
  recipient=CASE WHEN desired_mask&2!=0 THEN '' ELSE recipient END,
  reminder=CASE WHEN desired_mask&4!=0 THEN NULL ELSE reminder END,
  version=CASE WHEN desired_mask&4!=0 THEN '00000000000000000000' ELSE version END,
  mailbox=native_mailbox,desired_mask=0
 WHERE subject=OLD.subject AND born_index>=0 AND retired_index IS NULL;
 UPDATE local_client_message_selectors_v1 SET
  global_current=(closed=0 AND (reminder IS NULL OR NOT EXISTS(
   SELECT 1 FROM local_client_message_selectors_v1 rival WHERE rival.born_index>=0 AND rival.retired_index IS NULL AND rival.closed=0
    AND rival.reminder=local_client_message_selectors_v1.reminder AND (rival.version,rival.subject)>(local_client_message_selectors_v1.version,local_client_message_selectors_v1.subject)))),
  recipient_current=(closed=0 AND mailbox=1 AND (reminder IS NULL OR NOT EXISTS(
   SELECT 1 FROM local_client_message_selectors_v1 rival WHERE rival.born_index>=0 AND rival.retired_index IS NULL AND rival.closed=0 AND rival.mailbox=1
    AND rival.recipient=local_client_message_selectors_v1.recipient AND rival.reminder=local_client_message_selectors_v1.reminder AND (rival.version,rival.subject)>(local_client_message_selectors_v1.version,local_client_message_selectors_v1.subject))))
 WHERE born_index>=0 AND retired_index IS NULL AND
  subject IN (
   SELECT OLD.subject
   UNION
   SELECT subject FROM local_client_message_selectors_v1 INDEXED BY client_selector_reminder_candidates
    WHERE born_index>=0 AND retired_index IS NULL AND closed=0
     AND reminder=(SELECT reminder FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index=-1));
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    #[cfg(not(test))]
    drop_version_schema(connection)?;
    connection.execute_batch(r#"
CREATE TABLE IF NOT EXISTS local_client_message_cut_epoch(id INTEGER PRIMARY KEY CHECK(id=1),epoch INTEGER NOT NULL);
INSERT OR IGNORE INTO local_client_message_cut_epoch VALUES(1,0);
DROP TRIGGER IF EXISTS client_message_cut_repair;
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_delete AFTER DELETE ON claims WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_update AFTER UPDATE ON claims WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_desired_delete AFTER DELETE ON desired WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_repair AFTER UPDATE OF state,replacement_claim_id ON replica_records
WHEN NEW.state='repaired' AND (OLD.state!='repaired' OR OLD.replacement_claim_id IS NOT NEW.replacement_claim_id) AND EXISTS(SELECT 1 FROM claims WHERE id=NEW.claim_id AND subject LIKE 'message/%') BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
"#)?;
    // Reinstall source triggers so an existing selector layout gains the durable
    // pending-generation counter without changing any header columns.
    connection.execute_batch("DROP TRIGGER IF EXISTS client_selector_claim_insert;DROP TRIGGER IF EXISTS client_selector_claim_delete;DROP TRIGGER IF EXISTS client_selector_claim_update;DROP TRIGGER IF EXISTS client_selector_repair;DROP TRIGGER IF EXISTS client_selector_desired_insert;DROP TRIGGER IF EXISTS client_selector_desired_update;DROP TRIGGER IF EXISTS client_selector_desired_delete;")?;
    connection.execute_batch(SELECTOR_SCHEMA)?;
    // Older completed v1 caches acquire a conservative first-publication cohort.
    connection.execute(
        "INSERT INTO local_client_message_publications SELECT CAST(value AS INTEGER),?2 FROM meta WHERE key=?1 AND NOT EXISTS(SELECT 1 FROM local_client_message_publications)",
        params![MARKER,u64::try_from(crate::api::client_now_ms())?],
    )?;
    Ok(())
}

fn drop_version_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(r#"
DROP TRIGGER IF EXISTS client_messages_claim_insert;
DROP TRIGGER IF EXISTS client_messages_claim_delete;
DROP TRIGGER IF EXISTS client_messages_claim_update;
DROP TRIGGER IF EXISTS client_messages_desired_insert;
DROP TRIGGER IF EXISTS client_messages_desired_delete;
DROP TRIGGER IF EXISTS client_messages_desired_update;
DROP TABLE IF EXISTS local_client_message_pending;
DROP TABLE IF EXISTS local_client_message_versions;
DROP TABLE IF EXISTS local_client_message_retirements;
DROP TABLE IF EXISTS local_client_message_generation;
DELETE FROM meta WHERE key='client_message_versions_v1';
"#)?;
    Ok(())
}

#[derive(Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
struct Header {
    sender: String,recipient: String,mailbox: bool,closed: bool,
    reminder: Option<String>,version: String,sent_key: String,created_index: u64,
    desired_mask: u8,native_mailbox: bool,
}

type FieldSelection = (smallclaims::store::canonical::ClaimKey, Option<String>);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum FoldPhase { Clear, Scan, Cleanup }

/// Only selection scalars and canonical keys survive a transaction. Message
/// content and attachments never enter the resumable accumulator.
#[derive(serde::Serialize, serde::Deserialize)]
struct FoldWork {
    subject: String,
    cut: u64,
    epoch: u64,
    generation: u64,
    after: u64,
    phase: FoldPhase,
    first: Option<String>,
    values: [Option<FieldSelection>; 4],
    base: Option<Header>,
}

#[derive(Default, Debug)]
struct FoldBudget {
    claims: usize,
    subjects: usize,
    scratch_rows: usize,
    #[cfg(test)]
    phase: &'static str,
    #[cfg(test)]
    work: std::time::Duration,
    #[cfg(test)]
    commit: std::time::Duration,
}

enum DesiredSelection { Absent, Parsed(Value), Malformed }

fn desired_selection(connection: &Connection, subject: &str) -> Result<DesiredSelection> {
    let Some(row) = current_desired_row(connection, subject)? else { return Ok(DesiredSelection::Absent); };
    match serde_json::from_str(&row.body) {
        Ok(value) => Ok(DesiredSelection::Parsed(value)),
        Err(error) => {
            connection.execute(
                "INSERT INTO meta(key,value) VALUES(?1,'1') ON CONFLICT(key) DO UPDATE SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT)",
                [MALFORMED_MARKER],
            )?;
            tracing::warn!(subject, error = %error, "skipping malformed message desired projection during selector fold");
            Ok(DesiredSelection::Malformed)
        }
    }
}

fn folded_header(connection: &Connection, work: &mut FoldWork) -> Result<Option<Header>> {
    let Some(sent_key) = work.first.take() else { return Ok(None); };
    if let Some(mut base) = work.base.take() {
        base.sent_key = sent_key;
        return Ok(Some(base));
    }
    let indexed: Option<(u64,bool)> = connection.prepare_cached("SELECT created_index,closed FROM message_index WHERE subject=?1")?
        .query_row([&work.subject],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    let Some((created_index,index_closed)) = indexed else { return Ok(None); };
    let [sender,recipient,tags,status] = std::mem::take(&mut work.values).map(|value| value.and_then(|(_, value)| value));
    let desired = match desired_selection(connection, &work.subject)? {
        DesiredSelection::Absent => None,
        DesiredSelection::Parsed(value) => Some(value),
        DesiredSelection::Malformed => return Ok(None),
    };
    let desired_child=|field|desired.as_ref().and_then(|value|canonical_child_string(value,field));
    let mut desired_mask=0;
    let sender=sender.unwrap_or_else(|| {
        desired_child("from").map(|value|{desired_mask|=1;value}).unwrap_or_else(||"requester".into())
    });
    let recipient=recipient.unwrap_or_else(|| {
        desired_child("to").map(|value|{desired_mask|=2;value}).unwrap_or_default()
    });
    let tags=if let Some(tags)=tags {
        serde_json::from_str::<Vec<Value>>(&tags)?.into_iter().filter_map(|value|match value { Value::String(value)=>Some(value),_=>None }).collect::<Vec<_>>()
    } else {
        let tags=desired.as_ref().map(|value|canonical_child_strings(value,"tag")).unwrap_or_default();
        if desired.is_some() { desired_mask|=4; }
        tags
    };
    let reminder=tags.iter().find_map(|tag|tag.strip_prefix("reminder:")).map(str::to_owned);
    let version=tags.iter().find_map(|tag|tag.strip_prefix("version:")).and_then(|version|version.parse::<u64>().ok()).unwrap_or_default();
    let sender=normalize_message_party(&sender);
    let recipient=normalize_message_party(&recipient);
    let bare=recipient.strip_prefix("agent/").filter(|suffix|!suffix.contains('/')).unwrap_or(&recipient);
    let native_mailbox: bool=connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_message_to_index WHERE kind='message.sent' AND json_extract(body,'$.fields.to') IN (?2,?3) AND subject=?1)")?
        .query_row(params![work.subject,recipient,bare],|row|row.get(0))?;
    let declared_mailbox=desired.as_ref().and_then(|value|canonical_child_string(value,"to")).is_some_and(|to|to==recipient || to==bare);
    Ok(Some(Header { sender,recipient,mailbox:native_mailbox||declared_mailbox,closed:index_closed||status.as_deref()==Some("closed"),
        reminder,version:format!("{version:020}"),sent_key,created_index,desired_mask,native_mailbox }))
}

struct SelectionRow {
    index: u64,
    time: String,
    writer: String,
    sequence: u64,
    batch: String,
    position: Option<u64>,
    id: String,
    values: [Option<Option<String>>; 4],
}

fn next_selection_row(connection: &Connection, work: &FoldWork) -> Result<Option<SelectionRow>> {
    static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let columns = [("from","text"),("to","text"),("tags","array"),("status","text")]
            .into_iter().map(|(field, kind)| {
                let path = format!("CASE WHEN json_type(claims.body,'$.fields') IS NULL THEN '$.{field}' ELSE '$.fields.{field}' END");
                format!("CASE WHEN {ACTUAL_STATE_CLAIM} THEN json_type(claims.body,{path}) IS NOT NULL ELSE 0 END,CASE WHEN {ACTUAL_STATE_CLAIM} AND json_type(claims.body,{path})='{kind}' THEN json_extract(claims.body,{path}) END")
            }).collect::<Vec<_>>().join(",");
        format!("SELECT claims.store_index,claims.accepted_at_unix_ms,batches.origin,batches.replica_sequence,claims.batch_id,
            (SELECT MIN(position) FROM replica_records WHERE claim_id=claims.id),claims.id,{columns}
            FROM claims INDEXED BY claims_subject_index JOIN batches ON batches.id=claims.batch_id
            WHERE claims.subject=?1 AND claims.store_index>?2 AND claims.store_index<=?3
            ORDER BY claims.store_index LIMIT 1")
    });
    connection.prepare_cached(&SQL)?.query_row(params![work.subject,work.after,work.cut], |row| {
        let mut values = std::array::from_fn(|_| None);
        for (index, value) in values.iter_mut().enumerate() {
            if row.get::<_, bool>(7 + index * 2)? { *value = Some(row.get(8 + index * 2)?); }
        }
        Ok(SelectionRow { index:row.get(0)?,time:row.get(1)?,writer:row.get(2)?,sequence:row.get(3)?,
            batch:row.get(4)?,position:row.get(5)?,id:row.get(6)?,values })
    }).optional().map_err(Into::into)
}

/// Legacy position is a prefix count, but never an unbounded COUNT query.
/// Cached batch prefixes also include other subjects' claims, preserving wire order.
fn bounded_position(transaction: &Transaction<'_>, row: &SelectionRow, budget: &mut FoldBudget) -> Result<Option<u64>> {
    if let Some(position) = row.position { return Ok(Some(position)); }
    let (after, count) = transaction.prepare_cached(
        "SELECT after_index,claim_count FROM local_client_message_fold_batches WHERE batch_id=?1",
    )?.query_row([&row.batch], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)))
        .optional()?.unwrap_or((0, 0));
    let available = FOLD_CLAIMS.saturating_sub(budget.claims);
    if available == 0 { return Ok(None); }
    let mut statement = transaction.prepare_cached(
        "SELECT store_index FROM claims INDEXED BY claims_batch_index WHERE batch_id=?1 AND store_index>?2 AND store_index<=?3 ORDER BY store_index LIMIT ?4",
    )?;
    let mut rows = statement.query(params![row.batch,after,row.index,available])?;
    let mut through = after;
    let mut count = count;
    while let Some(row) = rows.next()? {
        through = row.get(0)?;
        count += 1;
        budget.claims += 1;
    }
    drop(rows);
    drop(statement);
    // A completed one-row batch prefix is already the answer. Persist only
    // prefixes that span another row or must resume in a later transaction.
    if through != row.index || count > 1 {
        transaction.execute(
            "INSERT INTO local_client_message_fold_batches(batch_id,after_index,claim_count) VALUES(?1,?2,?3)
             ON CONFLICT(batch_id) DO UPDATE SET after_index=excluded.after_index,claim_count=excluded.claim_count",
            params![row.batch,through,count],
        )?;
    }
    Ok((through == row.index).then(|| count.saturating_sub(1)))
}

fn scan_selection(transaction: &Transaction<'_>, work: &mut FoldWork, budget: &mut FoldBudget, started: std::time::Instant) -> Result<bool> {
    while budget.claims < FOLD_CLAIMS && started.elapsed() < BACKFILL_WORK_BUDGET {
        let Some(row) = next_selection_row(transaction, work)? else { return Ok(true); };
        budget.claims += 1;
        let Some(position) = bounded_position(transaction, &row, budget)? else { return Ok(false); };
        let time = row.time.parse::<u128>()?;
        let sent_key = crate::api::client_timestamp(time);
        if work.first.as_ref().is_none_or(|first| &sent_key < first) { work.first = Some(sent_key); }
        if work.base.is_none() {
            let key = (time,row.writer,row.sequence,row.batch,position,row.id);
            for (selected, value) in work.values.iter_mut().zip(row.values) {
                if let Some(value) = value {
                    if selected.as_ref().is_none_or(|(old, _)| &key > old) {
                        *selected = Some((key.clone(), value));
                    }
                }
            }
        }
        work.after = row.index;
    }
    Ok(false)
}

fn clear_fold_batches(transaction: &Transaction<'_>, budget: &mut FoldBudget) -> Result<bool> {
    let available = BACKFILL_CLEAR_ROWS.saturating_sub(budget.scratch_rows);
    if available == 0 { return Ok(false); }
    let removed = transaction.execute(
        "DELETE FROM local_client_message_fold_batches WHERE batch_id IN (
         SELECT batch_id FROM local_client_message_fold_batches ORDER BY batch_id LIMIT ?1)",
        [available],
    )?;
    budget.scratch_rows += removed;
    Ok(removed < available)
}

fn new_fold_work(transaction: &Transaction<'_>, subject: String, flags: u8, generation: u64) -> Result<FoldWork> {
    let base = if flags == 0 { current_header(transaction, &subject)?.map(|(_, header)| header) } else { None };
    let after = if base.is_some() {
        transaction.query_row("SELECT value FROM meta WHERE key=?1", [MARKER], |row| row.get::<_, String>(0))?.parse()?
    } else { 0 };
    let first = base.as_ref().map(|header| header.sent_key.clone());
    Ok(FoldWork {
        subject,cut:current_index_tx(transaction)?,
        epoch:transaction.query_row("SELECT epoch FROM local_client_message_cut_epoch WHERE id=1", [], |row| row.get(0))?,
        generation,after,phase:FoldPhase::Clear,first,values:std::array::from_fn(|_|None),base,
    })
}

fn save_fold_work(transaction: &Transaction<'_>, work: &FoldWork) -> Result<()> {
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![FOLD_MARKER,serde_json::to_string(work)?],
    )?;
    Ok(())
}

fn current_header(connection: &Connection,subject: &str) -> Result<Option<(u64,Header)>> {
    connection.prepare_cached("SELECT born_index,sender,recipient,mailbox,closed,reminder,version,sent_key,created_index,desired_mask,native_mailbox FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index>=0 AND retired_index IS NULL")?
        .query_row([subject],|row|Ok((row.get(0)?,Header { sender:row.get(1)?,recipient:row.get(2)?,mailbox:row.get(3)?,closed:row.get(4)?,reminder:row.get(5)?,version:row.get(6)?,sent_key:row.get(7)?,created_index:row.get(8)?,desired_mask:row.get(9)?,native_mailbox:row.get(10)? }))).optional().map_err(Into::into)
}

fn retire(transaction: &Transaction<'_>,subject: &str,cut: u64,now: u64) -> Result<()> {
    transaction.execute("UPDATE local_client_message_selectors_v1 SET retired_index=?2,retired_at_unix_ms=?3 WHERE subject=?1 AND born_index>=0 AND retired_index IS NULL AND born_index!=?2",params![subject,cut,now])?;
    Ok(())
}

fn put_header(transaction: &Transaction<'_>,subject: &str,value: &Header,cut: u64,now: u64) -> Result<()> {
    retire(transaction,subject,cut,now)?;
    let global_current=!value.closed && value.reminder.is_none();
    let recipient_current=global_current && value.mailbox;
    transaction.execute("INSERT INTO local_client_message_selectors_v1(subject,born_index,sender,recipient,mailbox,closed,reminder,version,sent_key,created_index,desired_mask,native_mailbox,global_current,recipient_current) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT(subject,born_index) DO UPDATE SET sender=excluded.sender,recipient=excluded.recipient,mailbox=excluded.mailbox,closed=excluded.closed,reminder=excluded.reminder,version=excluded.version,sent_key=excluded.sent_key,created_index=excluded.created_index,desired_mask=excluded.desired_mask,native_mailbox=excluded.native_mailbox,global_current=excluded.global_current,recipient_current=excluded.recipient_current,retired_index=NULL,retired_at_unix_ms=NULL",
        params![subject,cut,value.sender,value.recipient,value.mailbox,value.closed,value.reminder,value.version,value.sent_key,value.created_index,value.desired_mask,value.native_mailbox,global_current,recipient_current])?;
    Ok(())
}

fn set_current(transaction: &Transaction<'_>,subject: &str,global: Option<bool>,recipient: Option<bool>,cut: u64,now: u64) -> Result<()> {
    let (born,old_global,old_recipient):(u64,bool,bool)=transaction.prepare_cached("SELECT born_index,global_current,recipient_current FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index>=0 AND retired_index IS NULL")?
        .query_row([subject],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    if global.unwrap_or(old_global)==old_global && recipient.unwrap_or(old_recipient)==old_recipient { return Ok(()); }
    if born==cut {
        transaction.execute("UPDATE local_client_message_selectors_v1 SET global_current=COALESCE(?2,global_current),recipient_current=COALESCE(?3,recipient_current) WHERE subject=?1 AND born_index=?4",params![subject,global,recipient,cut])?;
    } else {
        retire(transaction,subject,cut,now)?;
        transaction.execute("INSERT INTO local_client_message_selectors_v1(subject,born_index,sender,recipient,mailbox,closed,reminder,version,sent_key,created_index,desired_mask,native_mailbox,global_current,recipient_current) SELECT subject,?2,sender,recipient,mailbox,closed,reminder,version,sent_key,created_index,desired_mask,native_mailbox,COALESCE(?3,global_current),COALESCE(?4,recipient_current) FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=?5",params![subject,cut,global,recipient,born])?;
    }
    Ok(())
}

fn reminder_winners(transaction: &Transaction<'_>,reminder: &str,recipient: Option<&str>,cut: u64,now: u64) -> Result<()> {
    let (index,scope,flag,winner_index)=if recipient.is_some() {
        ("client_selector_recipient_reminder_candidates","AND recipient=?2 AND mailbox=1","recipient_current","client_selector_recipient_reminder_winner")
    } else { ("client_selector_reminder_candidates","","global_current","client_selector_reminder_winner") };
    let winner: Option<String>=transaction.prepare_cached(&format!("SELECT subject FROM local_client_message_selectors_v1 INDEXED BY {index} WHERE born_index>=0 AND retired_index IS NULL AND closed=0 AND reminder=?1 {scope} AND (?2 IS NULL OR ?2 IS NOT NULL) ORDER BY version DESC,subject DESC LIMIT 1"))?
        .query_row(params![reminder,recipient],|row|row.get(0)).optional()?;
    let old=transaction.prepare_cached(&format!("SELECT subject FROM local_client_message_selectors_v1 INDEXED BY {winner_index} WHERE born_index>=0 AND retired_index IS NULL AND {flag}=1 AND reminder IS NOT NULL AND reminder=?1 {scope} AND (?2 IS NULL OR ?2 IS NOT NULL)"))?
        .query_map(params![reminder,recipient],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in old {
        if winner.as_deref()!=Some(&subject) {
            set_current(transaction,&subject,recipient.is_none().then_some(false),recipient.is_some().then_some(false),cut,now)?;
        }
    }
    if let Some(subject)=winner {
        set_current(transaction,&subject,recipient.is_none().then_some(true),recipient.is_some().then_some(true),cut,now)?;
    }
    Ok(())
}

fn record_cut(transaction: &Transaction<'_>,cut: u64) -> Result<()> {
    transaction.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value!=excluded.value",params![MARKER,cut.to_string()])?;
    transaction.execute(
        "INSERT OR IGNORE INTO local_client_message_publications(through_index,published_at_unix_ms)
         SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM local_client_message_publications) OR EXISTS(
          SELECT 1 FROM local_client_message_selectors_v1 INDEXED BY client_selector_retired_cut
          WHERE retired_index IS NOT NULL AND retired_index<=?1 AND retired_index>COALESCE((
           SELECT through_index FROM local_client_message_publications ORDER BY through_index DESC LIMIT 1
          ),-1))",
        params![cut,u64::try_from(crate::api::client_now_ms())?],
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BackfillStats {
    pub(crate) transactions: usize,
    pub(crate) subjects: usize,
    pub(crate) max_subjects_per_transaction: usize,
    pub(crate) max_claims_per_transaction: usize,
    pub(crate) malformed_desired: u64,
    pub(crate) longest_transaction: std::time::Duration,
    #[cfg(test)]
    longest_transaction_phase: &'static str,
    #[cfg(test)]
    longest_transaction_work: std::time::Duration,
    #[cfg(test)]
    longest_transaction_commit: std::time::Duration,
    pub(crate) checkpoint: std::time::Duration,
    pub(crate) total: std::time::Duration,
}

#[cfg(test)]
impl BackfillStats {
    pub(crate) fn benchmark_measurement(self) -> Value {
        json!({
            "transactions": self.transactions,
            "subjects": self.subjects,
            "max_subjects_per_transaction": self.max_subjects_per_transaction,
            "max_claims_per_transaction": self.max_claims_per_transaction,
            "malformed_desired": self.malformed_desired,
            "longest_transaction_including_commit_ms": self.longest_transaction.as_secs_f64() * 1_000.0,
            "longest_transaction_phase": self.longest_transaction_phase,
            "longest_transaction_work_ms": self.longest_transaction_work.as_secs_f64() * 1_000.0,
            "longest_transaction_commit_ms": self.longest_transaction_commit.as_secs_f64() * 1_000.0,
            "checkpoint_outside_transactions_ms": self.checkpoint.as_secs_f64() * 1_000.0,
            "total_ms": self.total.as_secs_f64() * 1_000.0,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackfillPhase {
    Clear,
    Headers,
    Pending,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BackfillProgress {
    cut: u64,
    phase: BackfillPhase,
    after: String,
}

fn save_progress(transaction: &Transaction<'_>, progress: &BackfillProgress) -> Result<()> {
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![BACKFILL_MARKER, serde_json::to_string(progress)?],
    )?;
    Ok(())
}

fn begin_backfill(transaction: &Transaction<'_>) -> Result<BackfillProgress> {
    // Invalidate old receipts once, atomically with making the projection unavailable.
    // Clearing itself is bounded, including a rebuild with many retired headers.
    transaction.execute("UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1", [])?;
    transaction.execute("DELETE FROM meta WHERE key=?1", [MARKER])?;
    transaction.execute("DELETE FROM meta WHERE key=?1", [FOLD_MARKER])?;
    let progress = BackfillProgress {
        cut: current_index_tx(transaction)?,
        phase: BackfillPhase::Clear,
        after: String::new(),
    };
    save_progress(transaction, &progress)?;
    Ok(progress)
}

fn has_pending(connection: &Connection) -> Result<bool> {
    Ok(connection.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM local_client_message_selectors_v1 WHERE born_index=-1)",
    )?.query_row([], |row| row.get(0))?)
}

fn fold_incomplete(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1) OR EXISTS(SELECT 1 FROM local_client_message_fold_batches)",
        [FOLD_MARKER], |row| row.get(0),
    )?)
}

/// One restart-safe transaction. `None` means a populated reopen did no work.
/// Progress, headers, reminder winners and pending removal always commit together.
fn backfill_chunk(connection: &mut Connection, reset: bool) -> Result<Option<(usize, bool, FoldBudget)>> {
    #[cfg(test)]
    let started = std::time::Instant::now();
    let transaction = connection.transaction()?;
    let saved: Option<String> = transaction.prepare_cached("SELECT value FROM meta WHERE key=?1")?
        .query_row([BACKFILL_MARKER], |row| row.get(0)).optional()?;
    let mut progress = if reset {
        begin_backfill(&transaction)?
    } else if let Some(saved) = saved {
        serde_json::from_str(&saved)?
    } else {
        let seeded: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [MARKER], |row| row.get(0),
        )?;
        if !seeded {
            begin_backfill(&transaction)?
        } else if has_pending(&transaction)? {
            // A populated store can also resume interrupted projection maintenance.
            // Preserve its historical headers; only reconcile the trigger queue.
            BackfillProgress { cut: current_index_tx(&transaction)?, phase: BackfillPhase::Pending, after: String::new() }
        } else {
            return Ok(None);
        }
    };
    let mut processed = 0;
    let mut complete = false;
    let mut budget = FoldBudget::default();
    #[cfg(test)]
    { budget.phase = match progress.phase {
        BackfillPhase::Clear => "clear",
        BackfillPhase::Headers => "headers",
        BackfillPhase::Pending => "pending",
    }; }
    match progress.phase {
        BackfillPhase::Clear => {
            let removed = transaction.execute(
                "DELETE FROM local_client_message_selectors_v1 WHERE (subject,born_index) IN (
                 SELECT subject,born_index FROM local_client_message_selectors_v1 ORDER BY subject,born_index LIMIT ?1)",
                [BACKFILL_CLEAR_ROWS],
            )?;
            if removed < BACKFILL_CLEAR_ROWS {
                progress.phase = BackfillPhase::Headers;
            }
        }
        BackfillPhase::Headers => {
            let subjects = transaction.prepare_cached(
                "SELECT subject FROM message_index WHERE subject>?1 AND created_index>0 ORDER BY subject LIMIT ?2",
            )?.query_map(params![progress.after, BACKFILL_SUBJECTS], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for subject in &subjects {
                transaction.execute(
                    "INSERT INTO local_client_message_selectors_v1(subject,born_index,created_index,dirty_flags) VALUES(?1,-1,1,2)
                     ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2,created_index=created_index+1",
                    [subject],
                )?;
                progress.after.clone_from(subject);
                processed += 1;
            }
            if processed == subjects.len() && subjects.len() < BACKFILL_SUBJECTS {
                progress.phase = BackfillPhase::Pending;
            }
        }
        BackfillPhase::Pending => {
            #[cfg(test)]
            let phase = budget.phase;
            budget = flush_pending(&transaction, false)?;
            #[cfg(test)]
            { budget.phase = phase; }
            processed = budget.subjects;
            if !has_pending(&transaction)? && !fold_incomplete(&transaction)? {
                // Publication and removal of the resume marker are one atomic commit.
                record_cut(&transaction, current_index_tx(&transaction)?)?;
                transaction.execute("DELETE FROM meta WHERE key=?1", [BACKFILL_MARKER])?;
                complete = true;
            }
        }
    }
    if !complete { save_progress(&transaction, &progress)?; }
    #[cfg(test)]
    { budget.work = started.elapsed(); }
    #[cfg(test)]
    let commit_started = std::time::Instant::now();
    transaction.commit()?;
    #[cfg(test)]
    { budget.commit = commit_started.elapsed(); }
    Ok(Some((processed, complete, budget)))
}

fn open_chunks(connection: &mut Connection, reset: bool) -> Result<BackfillStats> {
    // SQLite's automatic checkpoint runs inside COMMIT and can make an otherwise
    // bounded transaction rewrite the preceding WAL. Both startup and explicit
    // rebuild share this driver: checkpoint after the chunks, retain FULL
    // synchronous commits, and restore the caller's policy even on failure.
    let automatic: u32 = connection.pragma_query_value(None,"wal_autocheckpoint",|row|row.get(0))?;
    if automatic != 0 { connection.pragma_update(None,"wal_autocheckpoint",0)?; }
    let mut result = run_open_chunks(connection,reset);
    let checkpoint = if result.as_ref().is_ok_and(|stats|stats.transactions>0) {
        let checkpoint_started = std::time::Instant::now();
        let checkpoint = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        if let Ok(stats) = &mut result { stats.checkpoint = checkpoint_started.elapsed(); }
        checkpoint
    } else { Ok(()) };
    // Attempt restoration before propagating either the run or checkpoint error.
    let restore = connection.pragma_update(None,"wal_autocheckpoint",automatic);
    checkpoint?;
    restore?;
    result
}

fn run_open_chunks(connection: &mut Connection, mut reset: bool) -> Result<BackfillStats> {
    #[cfg(test)]
    if !connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)", [TABLE], |row| row.get::<_, bool>(0))? {
        return Ok(BackfillStats::default());
    }
    let started = std::time::Instant::now();
    let mut stats = BackfillStats::default();
    let malformed_before = malformed_count(connection)?;
    loop {
        let transaction_started = std::time::Instant::now();
        let chunk = backfill_chunk(connection, reset)?;
        let elapsed = transaction_started.elapsed(); // includes BEGIN and durable COMMIT
        reset = false;
        let Some((subjects, complete, budget)) = chunk else { break; };
        stats.transactions += 1;
        stats.subjects += subjects;
        stats.max_subjects_per_transaction = stats.max_subjects_per_transaction.max(subjects);
        stats.max_claims_per_transaction = stats.max_claims_per_transaction.max(budget.claims);
        #[cfg(test)]
        if elapsed > stats.longest_transaction {
            stats.longest_transaction_phase = budget.phase;
            stats.longest_transaction_work = budget.work;
            stats.longest_transaction_commit = budget.commit;
        }
        stats.longest_transaction = stats.longest_transaction.max(elapsed);
        if complete { break; }
    }
    stats.total = started.elapsed();
    stats.malformed_desired = malformed_count(connection)?.saturating_sub(malformed_before);
    if stats.transactions > 0 {
        tracing::info!(
            transactions = stats.transactions,
            subjects = stats.subjects,
            max_subjects_per_transaction = stats.max_subjects_per_transaction,
            max_claims_per_transaction = stats.max_claims_per_transaction,
            malformed_desired = stats.malformed_desired,
            longest_transaction_ms = stats.longest_transaction.as_secs_f64() * 1_000.0,
            total_ms = stats.total.as_secs_f64() * 1_000.0,
            "message selector backfill complete",
        );
    }
    Ok(stats)
}

pub(super) fn open(connection: &mut Connection) -> Result<BackfillStats> {
    open_chunks(connection, false)
}

pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    #[cfg(test)]
    if !transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",[TABLE],|row|row.get::<_,bool>(0))? { return Ok(()); }
    // Startup/rebuild owns this queue until every header is available. Ordinary
    // replay hooks must not publish an incomplete selector projection.
    let ready: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [MARKER], |row| row.get(0))?;
    if !ready { return Ok(()); }
    flush_pending(transaction, true)?;
    #[cfg(test)]
    if transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_client_message_versions')",[],|row|row.get::<_,bool>(0))? {
        super::client_messages_version_benchmark::flush(transaction)?;
    }
    Ok(())
}

fn malformed_count(connection: &Connection) -> Result<u64> {
    Ok(connection.query_row("SELECT value FROM meta WHERE key=?1", [MALFORMED_MARKER], |row| row.get::<_, String>(0))
        .optional()?.map(|value| value.parse()).transpose()?.unwrap_or(0))
}

/// Every caller, including replay and replication projection, shares these hard
/// row/subject bounds. Unfinished work survives the commit for the next worker loan.
fn flush_pending(transaction: &Transaction<'_>, publish: bool) -> Result<FoldBudget> {
    let started = std::time::Instant::now();
    let mut budget = FoldBudget::default();
    let saved: Option<String> = transaction.query_row("SELECT value FROM meta WHERE key=?1", [FOLD_MARKER], |row| row.get(0)).optional()?;
    let mut active = saved.map(|saved| serde_json::from_str::<FoldWork>(&saved)).transpose()?;
    while budget.subjects < BACKFILL_SUBJECTS && budget.claims < FOLD_CLAIMS && started.elapsed() < BACKFILL_WORK_BUDGET {
        let pending: Option<(String,u8,u64,Option<String>,String)> = if let Some(work) = &active {
            transaction.prepare_cached("SELECT subject,dirty_flags,created_index,reminder,recipient FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=-1")?
                .query_row([&work.subject], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?
        } else {
            transaction.prepare_cached("SELECT subject,dirty_flags,created_index,reminder,recipient FROM local_client_message_selectors_v1 INDEXED BY client_selector_pending WHERE born_index=-1 ORDER BY subject LIMIT 1")?
                .query_row([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?
        };
        let Some((subject,flags,generation,pending_reminder,pending_recipient)) = pending else {
            if active.take().is_some() {
                transaction.execute("DELETE FROM meta WHERE key=?1", [FOLD_MARKER])?;
                continue;
            }
            clear_fold_batches(transaction, &mut budget)?;
            break;
        };
        let epoch: u64 = transaction.query_row("SELECT epoch FROM local_client_message_cut_epoch WHERE id=1", [], |row| row.get(0))?;
        let mut work = match active.take() {
            Some(work) if work.epoch == epoch => work,
            _ => new_fold_work(transaction, subject, flags, generation)?,
        };
        if matches!(work.phase, FoldPhase::Clear) {
            if !clear_fold_batches(transaction, &mut budget)? { save_fold_work(transaction, &work)?; break; }
            work.phase = FoldPhase::Scan;
        }
        if matches!(work.phase, FoldPhase::Scan) {
            if !scan_selection(transaction, &mut work, &mut budget, started)? {
                save_fold_work(transaction, &work)?;
                break;
            }
            work.phase = FoldPhase::Cleanup;
        }
        // Finish the captured input before admitting later append-only touches.
        // A full fold keeps its canonical scalar winners and scans only the new
        // tail; a metadata-only base must expand once if selection became dirty.
        if work.generation != generation {
            if work.base.is_some() && flags != 0 {
                work = new_fold_work(transaction, work.subject, flags, generation)?;
            } else {
                work.cut = current_index_tx(transaction)?;
                work.generation = generation;
                work.phase = FoldPhase::Scan;
            }
            save_fold_work(transaction, &work)?;
            active = Some(work);
            continue;
        }
        if !clear_fold_batches(transaction, &mut budget)? { save_fold_work(transaction, &work)?; break; }
        let now = u64::try_from(crate::api::client_now_ms())?;
        let existing = current_header(transaction, &work.subject)?;
        let value = folded_header(transaction, &mut work)?;
        let mut reminders = BTreeSet::new();
        if existing.as_ref().map(|(_,header)| header) != value.as_ref() {
            if let Some((_,old)) = &existing {
                if let Some(reminder) = &old.reminder { reminders.insert((reminder.clone(),old.recipient.clone())); }
            }
            if let Some(value) = &value {
                if let Some(reminder) = &value.reminder { reminders.insert((reminder.clone(),value.recipient.clone())); }
                put_header(transaction, &work.subject, value, work.cut, now)?;
            } else {
                retire(transaction, &work.subject, work.cut, now)?;
                transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=?2", params![work.subject,work.cut])?;
            }
        }
        if let Some(reminder) = pending_reminder { reminders.insert((reminder,pending_recipient)); }
        let mut global = BTreeSet::new();
        for (reminder,recipient) in reminders {
            if global.insert(reminder.clone()) { reminder_winners(transaction,&reminder,None,work.cut,now)?; }
            reminder_winners(transaction,&reminder,Some(&recipient),work.cut,now)?;
        }
        transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=-1", [&work.subject])?;
        transaction.execute(
            "UPDATE local_client_message_selectors_v1 SET created_index=?2 WHERE subject=?1 AND born_index=-2",
            params![work.subject,work.cut],
        )?;
        transaction.execute("DELETE FROM meta WHERE key=?1", [FOLD_MARKER])?;
        budget.subjects += 1;
    }
    if publish {
        if !has_pending(transaction)? && !fold_incomplete(transaction)? { record_cut(transaction, current_index_tx(transaction)?)?; }
        prune_retired(transaction, u64::try_from(crate::api::client_now_ms())?)?;
    }
    Ok(budget)
}

const EXPIRED_RETIREMENTS: &str = r#"
SELECT subject,born_index FROM (
 SELECT subject,born_index,retired_index FROM local_client_message_selectors_v1 INDEXED BY client_selector_retirement
 WHERE retired_at_unix_ms IS NOT NULL AND retired_at_unix_ms<?1 ORDER BY retired_at_unix_ms LIMIT ?2
) candidates WHERE (
 SELECT published_at_unix_ms FROM local_client_message_publications
 WHERE through_index>=candidates.retired_index ORDER BY through_index LIMIT 1
)<?1
"#;

const UNREFERENCED_PUBLICATIONS: &str = r#"
SELECT through_index FROM (
 SELECT through_index FROM local_client_message_publications
 WHERE through_index<(SELECT through_index FROM local_client_message_publications ORDER BY through_index DESC LIMIT 1)
 AND through_index<=?3 ORDER BY through_index LIMIT ?2
) candidates WHERE NOT EXISTS(
 SELECT 1 FROM local_client_message_selectors_v1 INDEXED BY client_selector_retired_cut
 WHERE retired_index IS NOT NULL AND retired_index<=candidates.through_index AND retired_index>COALESCE((
  SELECT through_index FROM local_client_message_publications WHERE through_index<candidates.through_index ORDER BY through_index DESC LIMIT 1
 ),-1)
)
"#;

fn prune_retired(transaction: &Transaction<'_>, now: u64) -> Result<usize> {
    let expired = now.saturating_sub(u64::try_from(crate::api::CLIENT_PAGE_TTL_MS)?);
    let published: u64 = transaction.query_row("SELECT value FROM meta WHERE key=?1", [MARKER], |row| row.get::<_, String>(0))?.parse()?;
    static RETIRED: std::sync::LazyLock<String> = std::sync::LazyLock::new(||format!(
        "DELETE FROM local_client_message_selectors_v1 WHERE (subject,born_index) IN ({EXPIRED_RETIREMENTS})"
    ));
    let deleted = transaction.execute(&RETIRED,params![expired,PRUNE_ROWS])?;
    transaction.execute(
        "DELETE FROM local_client_message_selectors_v1 WHERE born_index=-2 AND subject IN (
         SELECT subject FROM local_client_message_selectors_v1 INDEXED BY client_selector_destructive
         WHERE born_index=-2 AND created_index<=?1 ORDER BY created_index,subject LIMIT ?2)",
        params![published,PRUNE_ROWS],
    )?;
    static PUBLICATIONS: std::sync::LazyLock<String> = std::sync::LazyLock::new(||format!(
        "DELETE FROM local_client_message_publications WHERE through_index IN ({UNREFERENCED_PUBLICATIONS}) AND (?1 IS NULL OR ?1 IS NOT NULL)"
    ));
    transaction.execute(&PUBLICATIONS,params![expired,PRUNE_ROWS,published])?;
    Ok(deleted)
}

fn has_selector_cleanup(connection: &Connection) -> Result<bool> {
    let expired = u64::try_from(crate::api::client_now_ms())?.saturating_sub(u64::try_from(crate::api::CLIENT_PAGE_TTL_MS)?);
    let published: u64 = connection.query_row("SELECT value FROM meta WHERE key=?1", [MARKER], |row| row.get::<_, String>(0))?.parse()?;
    static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(||format!(
        "SELECT EXISTS({EXPIRED_RETIREMENTS}) OR EXISTS({UNREFERENCED_PUBLICATIONS}) OR EXISTS(
         SELECT 1 FROM local_client_message_selectors_v1 INDEXED BY client_selector_destructive
         WHERE born_index=-2 AND created_index<=?3)"
    ));
    Ok(connection.prepare_cached(&SQL)?.query_row(params![expired,PRUNE_ROWS,published], |row| row.get(0))?)
}

fn page_sql(person: bool,actor: bool,history: bool,current: bool,after: bool) -> String {
    let flag=if person { "recipient_current" } else { "global_current" };
    let alive=if current { "retired_index IS NULL" } else { "born_index<=?1 AND (retired_index IS NULL OR retired_index>?1)" };
    let eligible=if history { String::new() } else { format!("AND {flag}=1") };
    let recipient_scope=if person { "AND recipient=?2 AND mailbox=1" } else { "" };
    let branch=|index: &str,scope: &str,alive: &str| {
        let range=|keyset: &str|format!("SELECT subject,born_index,sent_key,created_index,{flag} current FROM local_client_message_selectors_v1 INDEXED BY {index} WHERE born_index>=0 AND {alive} {recipient_scope} {scope} {eligible} {keyset} AND NOT EXISTS(SELECT 1 FROM local_client_message_selectors_v1 quarantine WHERE quarantine.subject=local_client_message_selectors_v1.subject AND quarantine.born_index=-2 AND quarantine.created_index>?1) AND EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_index WHERE claims.subject=local_client_message_selectors_v1.subject AND claims.store_index<=?1) ORDER BY sent_key DESC,subject LIMIT ?6");
        if after {
            let tied=range("AND sent_key=?4 AND subject>?5");
            let earlier=range("AND sent_key<?4");
            format!("SELECT * FROM (SELECT * FROM ({tied}) UNION ALL SELECT * FROM ({earlier})) ORDER BY sent_key DESC,subject LIMIT ?6")
        } else { range("") }
    };
    let eligible_branch = |all: &str,live: &str,retired: &str,scope: &str| {
        if history { return branch(all,scope,alive); }
        if current { return branch(live,scope,alive); }
        let live = branch(live,scope,"retired_index IS NULL AND born_index<=?1");
        let retired = branch(retired,scope,"retired_index IS NOT NULL AND born_index<=?1 AND retired_index>?1");
        format!("SELECT * FROM (SELECT * FROM ({live}) UNION ALL SELECT * FROM ({retired})) ORDER BY sent_key DESC,subject LIMIT ?6")
    };
    let candidates=if actor && !person {
        let sender=eligible_branch("client_selector_sender","client_selector_sender_eligible","client_selector_sender_retired_eligible","AND sender=?3");
        let recipient=eligible_branch("client_selector_recipient","client_selector_recipient_global_eligible","client_selector_recipient_global_retired_eligible","AND recipient=?3");
        format!("SELECT * FROM (SELECT * FROM ({sender}) UNION SELECT * FROM ({recipient})) ORDER BY sent_key DESC,subject LIMIT ?6")
    } else if actor {
        eligible_branch("client_selector_recipient_sender","client_selector_recipient_sender_eligible","client_selector_recipient_sender_retired_eligible","AND sender=?3")
    } else {
        if person {
            eligible_branch("client_selector_recipient","client_selector_recipient_eligible","client_selector_recipient_retired_eligible","")
        } else {
            eligible_branch("client_selector_order","client_selector_global_eligible","client_selector_global_retired_eligible","")
        }
    };
    format!("SELECT subject,created_index,current FROM ({candidates}) WHERE (?1 IS NULL OR ?1 IS NOT NULL) AND (?2 IS NULL OR ?2 IS NOT NULL) AND (?3 IS NULL OR ?3 IS NOT NULL) AND (?4 IS NULL OR ?4 IS NOT NULL) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY sent_key DESC,subject")
}

impl Store {
    pub(crate) fn client_messages_cut_epoch(&self) -> Result<u64> {
        Ok(self.readers.get().query_row("SELECT epoch FROM local_client_message_cut_epoch WHERE id=1",[],|row|row.get(0))?)
    }

    /// Pin the last fully published selector frontier while source projection lags.
    /// Pending rows and partially folded replacement headers are never candidates.
    pub(crate) fn client_messages_page_cut(&self, through: u64) -> Result<u64> {
        let connection = self.readers.get();
        let published: String = connection.query_row("SELECT value FROM meta WHERE key=?1", [MARKER], |row| row.get(0))?;
        Ok(if has_pending(&connection)? { through.min(published.parse()?) } else { through })
    }

    pub fn client_message_selectors_pending(&self) -> Result<bool> {
        has_pending(&self.readers.get())
    }

    /// One bounded writer transaction; true requests another independent loan.
    pub fn maintain_client_message_selectors(&self) -> Result<bool> {
        {
            let connection = self.readers.get();
            if !has_pending(&connection)? && !fold_incomplete(&connection)? && !has_selector_cleanup(&connection)? { return Ok(false); }
        }
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let ready: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [MARKER], |row| row.get(0))?;
        if !ready { return Ok(false); }
        flush_pending(&transaction, true)?;
        let more = has_pending(&transaction)? || fold_incomplete(&transaction)? || has_selector_cleanup(&transaction)?;
        transaction.commit()?;
        Ok(more)
    }

    pub(crate) fn client_messages_page(&self,person: Option<&str>,actor: Option<&str>,history: bool,through: u64,after: Option<&(u128,String)>,limit: usize) -> Result<Vec<(MessageView,Value,bool)>> {
        let connection=self.readers.get();
        let last_change: Option<String>=connection.prepare_cached("SELECT value FROM meta WHERE key=?1")?.query_row([MARKER],|row|row.get(0)).optional()?;
        let last_change=last_change.ok_or_else(||anyhow::anyhow!("message selector backfill is incomplete"))?;
        let published = last_change.parse::<u64>()?;
        let pending = has_pending(&connection)?;
        let through = if pending { through.min(published) } else { through };
        let current = !pending && through>=published;
        let person=person.map(normalize_message_party);
        let actor=actor.filter(|actor|Some(*actor)!=person.as_deref());
        static SQL: std::sync::LazyLock<[String;32]>=std::sync::LazyLock::new(||std::array::from_fn(|flags|page_sql(flags&1!=0,flags&2!=0,flags&4!=0,flags&8!=0,flags&16!=0)));
        let flags=usize::from(person.is_some())|(usize::from(actor.is_some())<<1)|(usize::from(history)<<2)|(usize::from(current)<<3)|(usize::from(after.is_some())<<4);
        let key=after.map(|(at,_)|crate::api::client_timestamp(*at));
        let rows=connection.prepare_cached(&SQL[flags])?.query_map(params![through,person,actor,key,after.map(|(_,subject)|subject),limit.saturating_add(1)],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,bool>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let subjects=rows.iter().map(|(subject,_,_)|subject).collect::<Vec<_>>();
        static METADATA_SQL: std::sync::LazyLock<String>=std::sync::LazyLock::new(||canonical_sql(r#"
WITH subjects AS (SELECT value subject FROM json_each(?1)),ends AS (
 SELECT subject,
  (SELECT id FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=subjects.subject AND store_index<=?2 ORDER BY CANONICAL_ASC(claims) LIMIT 1) first_id,
  (SELECT id FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=subjects.subject AND store_index<=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1) last_id
 FROM subjects
)
SELECT ends.subject,first.accepted_at_unix_ms,last.accepted_at_unix_ms,last.id,
 json_extract(first.body,CASE WHEN json_type(first.body,'$.fields') IS NULL THEN '$.session_id' ELSE '$.fields.session_id' END)
FROM ends JOIN claims first ON first.id=ends.first_id JOIN claims last ON last.id=ends.last_id
"#));
        let mut metadata=connection.prepare_cached(&METADATA_SQL)?.query_map(params![serde_json::to_string(&subjects)?,through],|row|Ok((row.get::<_,String>(0)?,json!({"sent_at":row.get::<_,String>(1)?,"updated_at":row.get::<_,String>(2)?,"revision":row.get::<_,String>(3)?,"session_id":row.get::<_,Option<String>>(4)?}))))?.collect::<rusqlite::Result<BTreeMap<_,_>>>()?;
        rows.into_iter().map(|(subject,created,current)| {
            let message=message_view_at(&connection,&subject,created,Some(through))?;
            let metadata=metadata.remove(&subject).ok_or_else(||anyhow::anyhow!("message cut metadata is unavailable"))?;
            Ok((message,metadata,current))
        }).collect()
    }

    #[cfg(test)]
    pub(crate) fn client_messages_selector_benchmark_set_enabled(&self,enabled: bool) -> Result<()> {
        let connection=self.connection.write();
        if enabled { connection.execute_batch(SELECTOR_SCHEMA)?; } else {
            connection.execute_batch("DROP TRIGGER IF EXISTS client_selector_claim_insert;DROP TRIGGER IF EXISTS client_selector_claim_delete;DROP TRIGGER IF EXISTS client_selector_claim_update;DROP TRIGGER IF EXISTS client_selector_repair;DROP TRIGGER IF EXISTS client_selector_desired_insert;DROP TRIGGER IF EXISTS client_selector_desired_update;DROP TRIGGER IF EXISTS client_selector_desired_delete;DROP TABLE IF EXISTS local_client_message_selectors_v1;")?;
            connection.execute("DELETE FROM meta WHERE key=?1",[MARKER])?;
            connection.execute("DELETE FROM meta WHERE key=?1",[BACKFILL_MARKER])?;
            connection.execute("DELETE FROM meta WHERE key=?1",[FOLD_MARKER])?;
            connection.execute_batch("DROP TABLE IF EXISTS local_client_message_fold_batches;DROP TABLE IF EXISTS local_client_message_publications;")?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn client_messages_selector_benchmark_rebuild(&self) -> Result<BackfillStats> {
        open_chunks(&mut self.connection.write(), true)
    }

    #[cfg(test)]
    pub(crate) fn client_messages_selector_benchmark_open_stats(&self) -> BackfillStats {
        *self.smalltalk.client_message_backfill_stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn client_messages_selector_benchmark_plan(&self,person: Option<&str>,actor: Option<&str>,history: bool,after: bool) -> Result<Vec<String>> {
        let person=person.map(normalize_message_party);
        let actor=actor.filter(|actor|Some(*actor)!=person.as_deref());
        let sql=format!("EXPLAIN QUERY PLAN {}",page_sql(person.is_some(),actor.is_some(),history,true,after));
        let connection=self.readers.get();
        let rows=connection.prepare(&sql)?.query_map(params![current_index(&connection)?,person,actor,after.then_some("2027-01-15T08:00:00.000Z"),after.then_some("message/selector-000001"),21],|row|row.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[cfg(test)]
    pub(crate) fn client_messages_benchmark_prepare_io(&self) -> Result<()> {
        self.connection.write().execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")?;Ok(())
    }
    #[cfg(test)]
    pub(crate) fn client_messages_benchmark_checkpoint(&self) -> Result<()> {
        self.connection.write().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;Ok(())
    }
    #[cfg(test)]
    pub(crate) fn client_messages_enable_version_benchmark(&self) -> Result<()> {
        let mut connection=self.connection.write();super::client_messages_version_benchmark::create_schema(&connection)?;
        let transaction=connection.transaction()?;super::client_messages_version_benchmark::open(&transaction)?;transaction.commit()?;Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn send(store: &Store,subject: &str,sender: &str,version: &str) {
        store.append_claim(&ClaimInput {subject:subject.into(),kind:"message.sent".into(),actor:Some(sender.into()),fields:BTreeMap::from([
            ("from".into(),json!(sender)),("to".into(),json!("person/recipient")),("content".into(),json!("message body")),("status".into(),json!("sent")),
            ("tags".into(),json!(["reminder:cut-test",format!("version:{version}")]))]),evidence:Vec::new(),expected_subject:None,idempotency_key:None}).unwrap();
    }
    #[test]
    fn version_schema_removal_preserves_claims_and_cut_pages() {
        let store=Store::open_memory("cut-schema-test").unwrap();send(&store,"message/old","agent/filtered","1");let through=store.index().unwrap();
        store.client_messages_enable_version_benchmark().unwrap();{
            let connection=store.connection.write();drop_version_schema(&connection).unwrap();
            let remaining:u64=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('local_client_message_pending','local_client_message_versions','local_client_message_retirements','local_client_message_generation') OR (type='trigger' AND name LIKE 'client_messages_%')",[],|row|row.get(0)).unwrap();
            assert_eq!(remaining,0);assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='client_message_versions_v1')",[],|row|row.get::<_,bool>(0)).unwrap());
        }
        assert_eq!(store.index().unwrap(),through);let rows=store.read_snapshot(|_|store.client_messages_page(None,None,true,through,None,20)).unwrap();
        assert_eq!(rows.len(),1);assert_eq!(rows[0].0.content,"message body");assert_eq!(store.claims_for("message/old",None).unwrap().len(),1);
    }
    #[test]
    fn unsigned_reminder_winner_precedes_actor_filter() {
        let store=Store::open_memory("cut-reminder-test").unwrap();send(&store,"message/old","agent/filtered","18446744073709551614");
        send(&store,"message/new","agent/rival","+18446744073709551615");send(&store,"message/z-overflow","agent/rival","18446744073709551616");
        store.read_snapshot(|through| {let current=store.client_messages_page(Some("person/recipient"),Some("agent/filtered"),false,through,None,1)?;assert!(current.is_empty());
            let history=store.client_messages_page(Some("person/recipient"),Some("agent/filtered"),true,through,None,1)?;assert_eq!(history.len(),1);assert!(!history[0].2);Ok(())}).unwrap();
    }

    fn seed_backfill(store: &Store, count: usize) {
        store.set_write_clock_at(1_800_000_000_000).unwrap();
        for index in 0..count {
            let from = if index.is_multiple_of(3) { "agent/filtered" } else { "agent/rival" };
            let to = if index.is_multiple_of(2) { "person/recipient" } else { "person/other" };
            let tags = if index.is_multiple_of(4) {
                Vec::new()
            } else {
                vec![format!("reminder:group-{}", index / 8), format!("version:{index}")]
            };
            store.append_claim(&ClaimInput {
                subject: format!("message/backfill-{index:04}"), kind: "message.sent".into(),
                actor: Some(from.into()), fields: BTreeMap::from([
                    ("from".into(), json!(from)), ("to".into(), json!(to)),
                    ("content".into(), json!(format!("body {index}"))),
                    ("status".into(), json!("sent")), ("tags".into(), json!(tags)),
                ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
    }

    fn paged_backfill_rows(store: &Store, person: Option<&str>, actor: Option<&str>, history: bool) -> Vec<Value> {
        store.read_snapshot(|through| {
            let mut after = None;
            let mut result = Vec::new();
            loop {
                let mut rows = store.client_messages_page(person, actor, history, through, after.as_ref(), 5)?;
                let more = rows.len() > 5;
                rows.truncate(5);
                for (message, metadata, current) in rows {
                    after = Some((metadata["sent_at"].as_str().unwrap().parse::<u128>()?, message.subject.clone()));
                    result.push(json!({"message": message, "metadata": metadata, "current": current}));
                }
                if !more { return Ok(result); }
            }
        }).unwrap()
    }

    fn backfill_progress(connection: &Connection) -> BackfillProgress {
        let value: String = connection.query_row(
            "SELECT value FROM meta WHERE key=?1", [BACKFILL_MARKER], |row| row.get(0),
        ).unwrap();
        serde_json::from_str(&value).unwrap()
    }

    #[test]
    fn selector_backfill_resumes_each_phase_before_serving_complete_ordered_pages() {
        let count = BACKFILL_CLEAR_ROWS + BACKFILL_SUBJECTS * 2 + 3;
        for phase in ["clear", "headers", "pending"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("resume.sqlite");
            let store = Store::open(&path, "backfill-resume").unwrap();
            seed_backfill(&store, count);
            let cases = [
                (None, None, true), (None, None, false),
                (Some("person/recipient"), None, true), (Some("person/recipient"), None, false),
                (None, Some("agent/filtered"), true), (None, Some("agent/filtered"), false),
                (Some("person/recipient"), Some("agent/filtered"), false),
            ];
            let expected = cases.map(|(person, actor, history)| paged_backfill_rows(&store, person, actor, history));
            let epoch = store.client_messages_cut_epoch().unwrap();
            let cut = store.index().unwrap();
            {
                let mut connection = store.connection.write();
                assert!(!backfill_chunk(&mut connection, true).unwrap().unwrap().1);
                loop {
                    let progress = backfill_progress(&connection);
                    assert_eq!(progress.cut, cut);
                    let reached = match (&progress.phase, phase) {
                        (BackfillPhase::Clear, "clear") | (BackfillPhase::Pending, "pending") => true,
                        (BackfillPhase::Headers, "headers") => !progress.after.is_empty(),
                        _ => false,
                    };
                    if reached { break; }
                    let (processed, complete, budget) = backfill_chunk(&mut connection, false).unwrap().unwrap();
                    assert!(budget.claims <= FOLD_CLAIMS);
                    assert!(processed <= BACKFILL_SUBJECTS);
                    assert!(!complete, "must stop before final publication");
                }
                assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [MARKER], |row| row.get::<_, bool>(0)).unwrap());
            }
            assert_eq!(store.client_messages_cut_epoch().unwrap(), epoch + 1);
            assert!(store.read_snapshot(|through| store.client_messages_page(None, None, true, through, None, 5)).is_err(),
                "a committed partial backfill cannot serve pages");
            drop(store); // interruption: no in-memory progress survives
            let reopened = Store::open(&path, "backfill-resume").unwrap();
            let stats = reopened.client_messages_selector_benchmark_open_stats();
            assert!(stats.transactions > 0);
            assert!(stats.max_subjects_per_transaction <= BACKFILL_SUBJECTS);
            assert_eq!(reopened.client_messages_cut_epoch().unwrap(), epoch + 1, "resume must not reset epoch");
            assert_eq!(reopened.index().unwrap(), cut);
            for ((person, actor, history), expected) in cases.into_iter().zip(expected) {
                assert_eq!(paged_backfill_rows(&reopened, person, actor, history), expected, "{phase}");
            }
            assert!(!has_pending(&reopened.readers.get()).unwrap());
            assert!(!reopened.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [BACKFILL_MARKER], |row| row.get::<_, bool>(0)).unwrap());
            drop(reopened);
            let populated = Store::open(&path, "backfill-resume").unwrap();
            assert_eq!(populated.client_messages_selector_benchmark_open_stats().transactions, 0);
            assert_eq!(populated.client_messages_cut_epoch().unwrap(), epoch + 1);
        }
    }

    #[test]
    fn selector_backfill_reconciles_edits_deletes_and_earlier_inserts_after_interruption() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changes.sqlite");
        let store = Store::open(&path, "backfill-changes").unwrap();
        seed_backfill(&store, BACKFILL_SUBJECTS * 3);
        {
            let mut connection = store.connection.write();
            backfill_chunk(&mut connection, true).unwrap();
            while backfill_progress(&connection).after.as_str() < "message/backfill-0001" {
                backfill_chunk(&mut connection, false).unwrap();
            }
            // Both subjects were already queued. Their trigger generations must survive resume.
            connection.execute(
                "UPDATE claims SET body=json_set(body,'$.fields.from','agent/edited','$.fields.to','person/blair','$.fields.content','edited body','$.fields.tags',json('[\"reminder:edited\",\"version:18446744073709551615\"]')) WHERE subject='message/backfill-0000'",
                [],
            ).unwrap();
            connection.execute("DELETE FROM claims WHERE subject='message/backfill-0001'", []).unwrap();
        }
        store.append_claim(&ClaimInput {
            subject: "message/aaa-before-scan".into(), kind: "message.sent".into(),
            actor: Some("agent/edited".into()), fields: BTreeMap::from([
                ("from".into(), json!("agent/edited")), ("to".into(), json!("person/blair")),
                ("content".into(), json!("inserted behind keyset")), ("status".into(), json!("sent")),
                ("tags".into(), json!(["reminder:edited", "version:1"])),
            ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let cut = store.index().unwrap();
        drop(store);
        let reopened = Store::open(&path, "backfill-changes").unwrap();
        assert_eq!(reopened.index().unwrap(), cut);
        let rows = paged_backfill_rows(&reopened, None, None, true);
        assert_eq!(rows.len(), BACKFILL_SUBJECTS * 3);
        assert!(!rows.iter().any(|row| row["message"]["subject"] == "message/backfill-0001"));
        let edited = rows.iter().find(|row| row["message"]["subject"] == "message/backfill-0000").unwrap();
        assert_eq!(edited["message"]["content"], "edited body");
        assert_eq!(edited["message"]["from"], "agent/edited");
        assert_eq!(edited["message"]["to"], "person/blair");
        assert_eq!(edited["current"], true);
        let current = paged_backfill_rows(&reopened, Some("person/blair"), Some("agent/edited"), false);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0]["message"]["subject"], "message/backfill-0000");
        let indexed: usize = reopened.readers.get().query_row(
            "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index>=0 AND retired_index IS NULL",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(indexed, BACKFILL_SUBJECTS * 3);
        let expected = rows;
        reopened.client_messages_selector_benchmark_rebuild().unwrap();
        assert_eq!(paged_backfill_rows(&reopened, None, None, true), expected,
            "resumed reconciliation must match a complete rebuild");
    }

    #[test]
    fn graph_runtime_finishes_selector_backfill_before_exposing_readers() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("direct-runtime.sqlite");
        let store = Store::open(&path, "direct-runtime").unwrap();
        seed_backfill(&store, BACKFILL_SUBJECTS * 2);
        backfill_chunk(&mut store.connection.write(), true).unwrap();
        drop(store);
        let graph = GraphStore::open(&path, "direct-runtime", runtime()).unwrap();
        let reader = graph.readers.get();
        assert!(reader.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [MARKER], |row| row.get::<_, bool>(0)).unwrap());
        assert!(!reader.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [BACKFILL_MARKER], |row| row.get::<_, bool>(0)).unwrap());
        let headers: usize = reader.query_row(
            "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index>=0 AND retired_index IS NULL",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(headers, BACKFILL_SUBJECTS * 2);
    }

    fn deferred_claim(transaction: &Transaction<'_>, subject: &str, kind: &str, fields: Value, batch: Option<&str>) -> smallclaims::ClaimRecord {
        smallclaims::store::append_claim_record_tx(
            transaction, "node", subject, kind, Some("agent/filtered"), &json!({"fields":fields}), &[], batch,
        ).unwrap()
    }

    #[test]
    fn pending_selectors_serve_committed_headers_while_large_queue_drains_in_bounded_steps() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("pending.sqlite"), "node").unwrap();
        let count = BACKFILL_SUBJECTS * 4 + 3;
        seed_backfill(&store, count);
        let published = store.index().unwrap();
        let baseline = paged_backfill_rows(&store, None, None, true);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            for index in 0..count {
                deferred_claim(&transaction, &format!("message/backfill-{index:04}"), "message.closed", json!({"status":"closed"}), None);
            }
            deferred_claim(&transaction, "message/new-pending", "message.sent",
                json!({"from":"agent/filtered","to":"person/recipient","status":"sent","content":"not published yet","tags":[]}), None);
            transaction.commit().unwrap();
        }
        assert!(store.client_message_selectors_pending().unwrap());
        assert_eq!(store.client_messages_page_cut(store.index().unwrap()).unwrap(), published);
        assert_eq!(paged_backfill_rows(&store, None, None, true), baseline,
            "committed pending rows must neither appear nor make a page fail");
        let mut chunks = 0;
        while store.client_message_selectors_pending().unwrap() {
            let budget = {
                let mut connection = store.connection.write();
                let transaction = connection.transaction().unwrap();
                let budget = flush_pending(&transaction, true).unwrap();
                transaction.commit().unwrap();
                budget
            };
            assert!(budget.claims <= FOLD_CLAIMS);
            assert!(budget.subjects <= BACKFILL_SUBJECTS);
            assert!(budget.scratch_rows <= BACKFILL_CLEAR_ROWS);
            chunks += 1;
            if store.client_message_selectors_pending().unwrap() {
                assert_eq!(paged_backfill_rows(&store, None, None, true), baseline,
                    "partially folded versions cannot leak before publication");
            }
        }
        assert!(chunks >= (count + 1).div_ceil(BACKFILL_SUBJECTS));
        let history = paged_backfill_rows(&store, None, None, true);
        assert_eq!(history.len(), count + 1);
        let open = paged_backfill_rows(&store, None, None, false);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["message"]["subject"], "message/new-pending");
    }

    #[test]
    fn historical_open_continuations_seek_eligible_version_indexes() {
        let store = Store::open_memory("node").unwrap();
        seed_backfill(&store, 64);
        let through = store.index().unwrap();
        let expected = paged_backfill_rows(&store, None, None, false);
        store.append_claim(&ClaimInput {
            subject:"message/backfill-0000".into(),kind:"message.closed".into(),actor:Some("daemon/runtime".into()),
            fields:BTreeMap::from([("status".into(),json!("closed"))]),
            evidence:Vec::new(),expected_subject:None,idempotency_key:None,
        }).unwrap();
        store.read_snapshot(|_| {
            let rows = store.client_messages_page(None,None,false,through,None,100)?;
            assert_eq!(rows.len(), expected.len());
            assert!(rows.iter().any(|(message,_,_)| message.subject=="message/backfill-0000" && message.status!="closed"));
            Ok(())
        }).unwrap();
        let connection = store.readers.get();
        for (person,actor,indexes) in [
            (None,None,vec!["client_selector_global_eligible","client_selector_global_retired_eligible"]),
            (None,Some("agent/filtered"),vec!["client_selector_sender_eligible","client_selector_sender_retired_eligible","client_selector_recipient_global_eligible","client_selector_recipient_global_retired_eligible"]),
            (Some("person/recipient"),None,vec!["client_selector_recipient_eligible","client_selector_recipient_retired_eligible"]),
            (Some("person/recipient"),Some("agent/filtered"),vec!["client_selector_recipient_sender_eligible","client_selector_recipient_sender_retired_eligible"]),
        ] {
            let sql = format!("EXPLAIN QUERY PLAN {}", page_sql(person.is_some(),actor.is_some(),false,false,true));
            let plan = connection.prepare(&sql).unwrap().query_map(
                params![through,person,actor,crate::api::client_timestamp(1_800_000_000_000),"message/backfill-0000",6],
                |row| row.get::<_,String>(3),
            ).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            for index in indexes {
                assert!(plan.iter().any(|step|step.contains("SEARCH") && step.contains(index)),
                    "historical open continuation must seek {index}: {plan:?}");
            }
            assert!(!plan.iter().any(|step|step.contains("client_selector_order") ||
                (step.contains("client_selector_sender ") || step.contains("client_selector_recipient "))),
                "closed/superseded versions cannot be the continuation's scan path: {plan:?}");
        }
    }

    fn declare_message(store: &Store) {
        let source = "version 2\nagent \"keyed\" { workspace \"/tmp\"; command \"true\" }\nmessage \"keyed\" { from \"requester\"; to \"keyed\"; content \"keyed lookup\" }\n";
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let mission = store.mission(&intent, crate::model::IntentInput { kdl:source.into(),source_name:None }).unwrap();
        store.apply(&intent,&mission.subject_tokens,"keyed-message").unwrap();
        while store.maintain_client_message_selectors().unwrap() {}
    }

    #[test]
    fn message_body_fold_uses_keyed_desired_lookup_not_owned_set_inventory() {
        thread_local! {
            static QUERIES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        fn capture(sql: &str) { QUERIES.with(|queries| queries.borrow_mut().push(sql.into())); }
        let store = Store::open_memory("node").unwrap();
        declare_message(&store);
        let through = store.index().unwrap();
        let mut connection = store.connection.write();
        let created: u64 = connection.query_row("SELECT created_index FROM message_index WHERE subject='message/keyed'", [], |row| row.get(0)).unwrap();
        connection.trace(Some(capture));
        let message = message_view_at(&connection,"message/keyed",created,Some(through)).unwrap();
        connection.trace(None);
        assert_eq!(message.content,"keyed lookup");
        let queries = QUERIES.with(|queries| std::mem::take(&mut *queries.borrow_mut()));
        assert!(!queries.iter().any(|sql|sql.contains("owned-set.revised")),
            "message page body folding cannot scan owned-set receipts: {queries:?}");
        let desired = queries.iter().find(|sql|sql.contains("SELECT kind, revision, claim_id, body") && sql.contains("FROM desired WHERE subject="))
            .expect("the actual body fold must issue the desired primary-key lookup");
        let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {desired}")).unwrap()
            .query_map([], |row|row.get::<_,String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(plan.iter().any(|step|step.contains("SEARCH desired") && step.contains("subject=")),
            "desired lookup must be a subject-key seek: {plan:?}");
    }

    #[test]
    fn long_subject_history_and_legacy_positions_resume_with_hard_claim_row_bound() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("long-history.sqlite");
        let store = Store::open(&path,"node").unwrap();
        seed_backfill(&store,1);
        store.client_messages_selector_benchmark_set_enabled(false).unwrap();
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let mut batch = None;
            for index in 0..(FOLD_CLAIMS * 8 + 17) {
                let subject = if index.is_multiple_of(5) { "custom/test/batch-neighbor" } else { "message/backfill-0000" };
                let claim = deferred_claim(&transaction,subject,"custom.test.recorded",json!({"note":index}),batch.as_deref());
                if batch.is_none() { batch = Some(claim.batch_id); }
            }
            deferred_claim(&transaction,"message/backfill-0000","message.sent",
                json!({"from":"agent/final","to":"person/robin","content":"last canonical body","status":"sent","tags":["reminder:long","version:99"]}),batch.as_deref());
            transaction.commit().unwrap();
        }
        let source_count: u64 = store.readers.get().query_row("SELECT COUNT(*) FROM claims", [], |row|row.get(0)).unwrap();
        store.client_messages_selector_benchmark_set_enabled(true).unwrap();
        {
            let mut connection = store.connection.write();
            backfill_chunk(&mut connection,false).unwrap();
            backfill_chunk(&mut connection,false).unwrap();
            for _ in 0..3 {
                let (_,complete,budget) = backfill_chunk(&mut connection,false).unwrap().unwrap();
                assert!(!complete, "long subject must span more than one transaction");
                assert!(budget.claims <= FOLD_CLAIMS);
            }
            assert!(connection.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [FOLD_MARKER], |row|row.get::<_,bool>(0)).unwrap());
        }
        drop(store);
        let reopened = Store::open(&path,"node").unwrap();
        let stats = reopened.client_messages_selector_benchmark_open_stats();
        assert!(stats.transactions>1);
        assert!(stats.max_claims_per_transaction<=FOLD_CLAIMS);
        assert_eq!(reopened.readers.get().query_row("SELECT COUNT(*) FROM claims", [], |row|row.get::<_,u64>(0)).unwrap(),source_count);
        let header: (String,String,String) = reopened.readers.get().query_row(
            "SELECT sender,recipient,version FROM local_client_message_selectors_v1 WHERE subject='message/backfill-0000' AND born_index>=0 AND retired_index IS NULL",
            [], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!(header,("agent/final".into(),"person/robin".into(),"00000000000000000099".into()));
        assert_eq!(reopened.readers.get().query_row("SELECT COUNT(*) FROM local_client_message_fold_batches", [], |row|row.get::<_,usize>(0)).unwrap(),0);
    }

    #[test]
    fn malformed_desired_projection_is_counted_and_skipped_without_blocking_startup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("malformed-desired.sqlite");
        let store = Store::open(&path,"node").unwrap();
        seed_backfill(&store,1);
        declare_message(&store);
        {
            let connection = store.connection.write();
            connection.execute("UPDATE desired SET body='{broken' WHERE subject='message/keyed'", []).unwrap();
            connection.execute("DELETE FROM meta WHERE key=?1", [MARKER]).unwrap();
        }
        let claims = store.index().unwrap();
        drop(store);
        let reopened = Store::open(&path,"node").unwrap();
        assert_eq!(reopened.index().unwrap(),claims);
        assert_eq!(reopened.client_messages_selector_benchmark_open_stats().malformed_desired,1);
        assert_eq!(malformed_count(&reopened.readers.get()).unwrap(),1);
        assert!(!reopened.client_message_selectors_pending().unwrap());
        let rows = paged_backfill_rows(&reopened,None,None,true);
        assert_eq!(rows.len(),1, "only the malformed desired subject is skipped");
        assert_eq!(rows[0]["message"]["subject"],"message/backfill-0000");
    }

    #[test]
    fn retired_header_pruning_deletes_at_most_one_bounded_batch_per_transaction() {
        let store = Store::open_memory("node").unwrap();
        seed_backfill(&store,PRUNE_ROWS*2+7);
        for index in 0..(PRUNE_ROWS*2+7) {
            store.append_claim(&ClaimInput {
                subject:format!("message/backfill-{index:04}"),kind:"message.closed".into(),actor:Some("daemon/runtime".into()),
                fields:BTreeMap::from([("status".into(),json!("closed"))]),
                evidence:Vec::new(),expected_subject:None,idempotency_key:None,
            }).unwrap();
        }
        while store.maintain_client_message_selectors().unwrap() {}
        let mut connection = store.connection.write();
        connection.execute("UPDATE local_client_message_selectors_v1 SET retired_at_unix_ms=1 WHERE retired_index IS NOT NULL", []).unwrap();
        connection.execute("UPDATE local_client_message_publications SET published_at_unix_ms=1", []).unwrap();
        let mut remaining: usize = connection.query_row(
            "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE retired_index IS NOT NULL", [], |row|row.get(0),
        ).unwrap();
        assert!(remaining>PRUNE_ROWS*2);
        let mut publications: usize = connection.query_row("SELECT COUNT(*) FROM local_client_message_publications",[],|row|row.get(0)).unwrap();
        let mut chunks = 0;
        while remaining>0 {
            let transaction = connection.transaction().unwrap();
            let deleted = prune_retired(&transaction,u64::try_from(crate::api::CLIENT_PAGE_TTL_MS).unwrap()+1000).unwrap();
            assert!(deleted>0 && deleted<=PRUNE_ROWS);
            transaction.commit().unwrap();
            let next: usize = connection.query_row(
                "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE retired_index IS NOT NULL", [], |row|row.get(0),
            ).unwrap();
            assert_eq!(remaining-next,deleted);
            let next_publications: usize = connection.query_row("SELECT COUNT(*) FROM local_client_message_publications",[],|row|row.get(0)).unwrap();
            assert!(publications-next_publications<=PRUNE_ROWS,"publication cohort cleanup must also be bounded");
            assert!(next_publications>0,"the latest publication cohort must remain");
            publications=next_publications;
            remaining=next;
            chunks+=1;
        }
        assert!(chunks>=3);
    }

    #[test]
    fn destructive_recipient_edit_omits_unsafe_subject_until_bounded_reconciliation() {
        let store = Store::open_memory("node").unwrap();
        seed_backfill(&store,2);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            for index in 0..FOLD_CLAIMS*4 {
                deferred_claim(&transaction,"message/backfill-0000","custom.test.recorded",json!({"note":index}),None);
            }
            transaction.commit().unwrap();
        }
        while store.maintain_client_message_selectors().unwrap() {}
        let epoch = store.client_messages_cut_epoch().unwrap();
        store.connection.write().execute(
            "UPDATE claims SET body=json_set(body,'$.fields.to','person/blair') WHERE subject='message/backfill-0000' AND kind='message.sent'",
            [],
        ).unwrap();
        assert!(store.client_messages_cut_epoch().unwrap()>epoch);
        let mut chunks = 0;
        while store.client_message_selectors_pending().unwrap() {
            for person in [None,Some("person/recipient"),Some("person/blair")] {
                let rows = paged_backfill_rows(&store,person,None,true);
                assert!(!rows.iter().any(|row|row["message"]["subject"]=="message/backfill-0000"),
                    "a mutated body cannot be served under its old routing header");
                if person.is_none() { assert_eq!(rows.len(),1,"unaffected pages remain usable"); }
            }
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let budget = flush_pending(&transaction,true).unwrap();
            assert!(budget.claims<=FOLD_CLAIMS);
            transaction.commit().unwrap();
            chunks+=1;
        }
        assert!(chunks>1,"destructive reconciliation must span bounded loans");
        assert!(paged_backfill_rows(&store,Some("person/recipient"),None,true).is_empty());
        let rows = paged_backfill_rows(&store,Some("person/blair"),None,true);
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0]["message"]["subject"],"message/backfill-0000");
        assert_eq!(rows[0]["message"]["to"],"person/blair");
    }

    #[test]
    fn historical_desired_selection_rejects_current_leaf_that_won_only_after_the_cut() {
        let store = Store::open_memory("node").unwrap();
        declare_message(&store);
        let mut connection = store.connection.write();
        let original: (String,String) = connection.query_row(
            "SELECT id,body FROM claims WHERE subject='message/keyed' AND kind='intent.desired' LIMIT 1",
            [], |row|Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        let template: crate::model::DesiredSubject = serde_json::from_str(&original.1).unwrap();
        let mut choices = (0..3).map(|index| {
            let mut desired = template.clone();
            desired.desired.as_object_mut().unwrap().insert("test_revision".into(),json!(index));
            (super::super::desired_revision(&desired),desired)
        }).collect::<Vec<_>>();
        choices.sort_by(|left,right|left.0.cmp(&right.0));
        let [(revision_c,c),(revision_b,b),(revision_a,a)]: [(String,crate::model::DesiredSubject);3] = choices.try_into().unwrap();
        let transaction = connection.transaction().unwrap();
        let claim_a = smallclaims::store::append_claim_record_tx(
            &transaction,"node","message/keyed","intent.desired",None,
            &serde_json::to_value(&a).unwrap(),std::slice::from_ref(&original.0),None,
        ).unwrap();
        let claim_b = smallclaims::store::append_claim_record_tx(
            &transaction,"node","message/keyed","intent.desired",None,
            &serde_json::to_value(&b).unwrap(),std::slice::from_ref(&original.0),None,
        ).unwrap();
        let through = claim_b.store_index;
        smallclaims::store::append_claim_record_tx(
            &transaction,"node","message/keyed","intent.desired",None,
            &serde_json::to_value(&c).unwrap(),std::slice::from_ref(&claim_a.id),None,
        ).unwrap();
        assert!(revision_a>revision_b && revision_b>revision_c);
        // Canonical replay now chooses B, even though B was already present at
        // the older cut where A was the higher-revision competing leaf.
        transaction.execute(
            "UPDATE desired SET revision=?1,claim_id=?2,body=?3 WHERE subject='message/keyed'",
            params![revision_b,claim_b.id,serde_json::to_string(&b.desired).unwrap()],
        ).unwrap();
        let current = super::super::desired_row_at(&transaction,"message/keyed",None).unwrap().unwrap();
        assert_eq!(current.claim_id,claim_b.id);
        let historical = super::super::desired_row_at(&transaction,"message/keyed",Some(through)).unwrap().unwrap();
        assert_eq!(historical.claim_id,claim_a.id);
        assert_eq!(historical.revision,revision_a);
        assert_eq!(serde_json::from_str::<Value>(&historical.body).unwrap(),a.desired);
        transaction.commit().unwrap();
    }

    #[test]
    fn append_touches_extend_a_suspended_full_fold_without_restarting_its_prefix() {
        let store = Store::open_memory("node").unwrap();
        seed_backfill(&store,1);
        let captured_tail = {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            for index in 0..FOLD_CLAIMS*4 {
                deferred_claim(&transaction,"message/backfill-0000","custom.test.recorded",json!({"note":index}),None);
            }
            let tail = deferred_claim(&transaction,"message/backfill-0000","message.sent",
                json!({"from":"agent/filtered","to":"person/robin","content":"new tail","status":"sent","tags":[]}),None).store_index;
            let budget = flush_pending(&transaction,true).unwrap();
            assert!(budget.claims<=FOLD_CLAIMS);
            transaction.commit().unwrap();
            tail
        };
        let mut previous_after = 0;
        let mut reached_captured_tail = false;
        for index in 0..FOLD_CLAIMS*4 {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            deferred_claim(&transaction,"message/backfill-0000","custom.test.recorded",json!({"touch":index}),None);
            let budget = flush_pending(&transaction,true).unwrap();
            assert!(budget.claims<=FOLD_CLAIMS);
            let saved: Option<String> = transaction.query_row("SELECT value FROM meta WHERE key=?1",[FOLD_MARKER],|row|row.get(0)).optional().unwrap();
            if let Some(saved) = saved {
                let work: FoldWork = serde_json::from_str(&saved).unwrap();
                assert!(work.after>=previous_after,"append-only touches must never reset the scanned prefix");
                previous_after = work.after;
                reached_captured_tail |= work.after>=captured_tail;
            } else {
                reached_captured_tail = true;
            }
            transaction.commit().unwrap();
            if reached_captured_tail { break; }
        }
        assert!(reached_captured_tail,"the captured tail must finish despite an append between every loan");
        while store.maintain_client_message_selectors().unwrap() {}
        let rows = paged_backfill_rows(&store,Some("person/robin"),None,true);
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0]["message"]["content"],"new tail");
    }

    #[test]
    fn retired_headers_outlive_cursors_issued_late_in_publication_lag() {
        let store = Store::open_memory("node").unwrap();
        seed_backfill(&store,2);
        let through = store.client_messages_page_cut(store.index().unwrap()).unwrap();
        let baseline = paged_backfill_rows(&store,None,None,true);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            deferred_claim(&transaction,"message/backfill-0000","message.closed",json!({"status":"closed"}),None);
            for index in 0..FOLD_CLAIMS*4 {
                deferred_claim(&transaction,"message/backfill-0001","custom.test.recorded",json!({"note":index}),None);
            }
            flush_pending(&transaction,true).unwrap();
            transaction.commit().unwrap();
            assert!(has_pending(&connection).unwrap());
            connection.execute("UPDATE local_client_message_selectors_v1 SET retired_at_unix_ms=1 WHERE retired_index IS NOT NULL",[]).unwrap();
            connection.execute("UPDATE local_client_message_publications SET published_at_unix_ms=1",[]).unwrap();
        }
        // This old cut is still offered to newly issued full-TTL cursors even
        // though the first subject's old header has already been retired.
        assert_eq!(store.client_messages_page_cut(store.index().unwrap()).unwrap(),through);
        assert_eq!(paged_backfill_rows(&store,None,None,true),baseline);
        while store.maintain_client_message_selectors().unwrap() {}
        let published = store.index().unwrap();
        let publication_time = u64::try_from(crate::api::CLIENT_PAGE_TTL_MS).unwrap()*2+1000;
        let mut connection = store.connection.write();
        connection.execute(
            "UPDATE local_client_message_publications SET published_at_unix_ms=?1 WHERE through_index=?2",
            params![publication_time,published],
        ).unwrap();
        let transaction = connection.transaction().unwrap();
        let ttl = u64::try_from(crate::api::CLIENT_PAGE_TTL_MS).unwrap();
        assert_eq!(prune_retired(&transaction,publication_time+ttl-1).unwrap(),0);
        transaction.commit().unwrap();
        drop(connection);
        let rows = store.client_messages_page(None,None,true,through,None,10).unwrap();
        assert_eq!(rows.len(),baseline.len(),"a still-valid lag cursor cannot silently lose a retired header");
        assert!(rows.iter().any(|(message,_,_)|message.subject=="message/backfill-0000"));
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        assert!(prune_retired(&transaction,publication_time+ttl+1).unwrap()>0);
        transaction.commit().unwrap();
    }

    #[test]
    fn repaired_declaration_reselection_quarantines_old_routing_during_bounded_fold() {
        fn apply_route(store: &Store, recipient: &str, key: &str) -> String {
            let source = format!(
                "version 2\nagent \"old\" {{ workspace \"/tmp\"; command \"true\" }}\nagent \"new\" {{ workspace \"/tmp\"; command \"true\" }}\nmessage \"repair-route\" {{ from \"requester\"; to {recipient:?}; content \"repair routing\" }}\n"
            );
            let intent = crate::graph::parse_test_intent(&source,"source").unwrap();
            let mission = store.mission(&intent,crate::model::IntentInput { kdl:source,source_name:None }).unwrap();
            store.apply(&intent,&mission.subject_tokens,key).unwrap();
            store.selected_desired_token("message/repair-route").unwrap().unwrap()
        }
        let source = Store::open_memory("source").unwrap();
        let target = Store::open_memory("target").unwrap();
        let replacement = apply_route(&source,"old","initial-route");
        let repaired = apply_route(&source,"new","changed-route");
        assert_ne!(replacement,repaired);
        let exchange = super::super::tests::exchange_from(&source,&target.replication_inventory().unwrap());
        super::super::tests::receive_and_project(&target,"source",&exchange);
        seed_backfill(&target,1);
        {
            let mut connection = target.connection.write();
            let transaction = connection.transaction().unwrap();
            for index in 0..FOLD_CLAIMS*12 {
                deferred_claim(&transaction,"message/repair-route","custom.test.recorded",json!({"note":index}),None);
            }
            transaction.commit().unwrap();
        }
        while target.maintain_client_message_selectors().unwrap() {}
        assert_eq!(paged_backfill_rows(&target,Some("agent/new"),None,true).len(),1);
        let epoch = target.client_messages_cut_epoch().unwrap();
        let record = {
            let connection = target.connection.write();
            let record: String = connection.query_row(
                "SELECT record_ref FROM replica_records WHERE claim_id=?1",
                [&repaired], |row|row.get(0),
            ).unwrap();
            connection.execute("UPDATE replica_records SET state='invalid' WHERE record_ref=?1",[&record]).unwrap();
            record
        };
        target.repair_replica_record(
            &record,&replacement,"restore the earlier declaration","person/alex","repair-routing",
        ).unwrap();
        assert_eq!(target.selected_desired_token("message/repair-route").unwrap(),Some(replacement));
        assert!(target.client_messages_cut_epoch().unwrap()>epoch);
        assert!(target.client_message_selectors_pending().unwrap(),"long repair history must require later bounded loans");
        for person in [None,Some("agent/new"),Some("agent/old")] {
            let rows = paged_backfill_rows(&target,person,None,true);
            assert!(!rows.iter().any(|row|row["message"]["subject"]=="message/repair-route"),
                "a repaired desired body cannot appear under the old routing header");
            if person.is_none() { assert_eq!(rows.len(),1,"unaffected subjects remain readable"); }
        }
        while target.maintain_client_message_selectors().unwrap() {}
        assert!(paged_backfill_rows(&target,Some("agent/new"),None,true).is_empty());
        let rows = paged_backfill_rows(&target,Some("agent/old"),None,true);
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0]["message"]["subject"],"message/repair-route");
        assert_eq!(rows[0]["message"]["to"],"agent/old");
    }

    #[test]
    fn plain_sends_avoid_unneeded_prefix_and_publication_rows() {
        let store = Store::open_memory("node").unwrap();
        for index in 0..64 {
            store.append_claim(&ClaimInput {
                subject:format!("message/plain-{index}"),kind:"message.sent".into(),actor:Some("agent/sender".into()),
                fields:BTreeMap::from([
                    ("from".into(),json!("agent/sender")),("to".into(),json!("person/recipient")),
                    ("content".into(),json!("plain body")),("status".into(),json!("sent")),
                ]),
                evidence:Vec::new(),expected_subject:None,idempotency_key:None,
            }).unwrap();
        }
        let connection = store.readers.get();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM local_client_message_fold_batches",[],|row|row.get::<_,u64>(0)).unwrap(),0);
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM local_client_message_publications",[],|row|row.get::<_,u64>(0)).unwrap(),1);
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE retired_index IS NOT NULL",[],|row|row.get::<_,u64>(0)).unwrap(),0);
    }

    #[test]
    fn chunk_driver_restores_checkpoint_policy_after_completion_and_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("checkpoint-policy.sqlite"),"node").unwrap();
        seed_backfill(&store,64);
        let mut connection = store.connection.write();
        for automatic in [0,7] {
            connection.pragma_update(None,"wal_autocheckpoint",automatic).unwrap();
            let stats = open_chunks(&mut connection,true).unwrap();
            assert!(stats.transactions>1);
            assert!(!stats.checkpoint.is_zero(),"explicit rebuild checkpoints outside transactions even when automatic checkpoints were already off");
            assert_eq!(connection.pragma_query_value(None,"wal_autocheckpoint",|row|row.get::<_,u32>(0)).unwrap(),automatic);
            let checkpoint: (i64,i64,i64) = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            assert_eq!(checkpoint.0,0);
            assert_eq!(checkpoint.1,checkpoint.2);
        }
        let transaction = connection.transaction().unwrap();
        deferred_claim(&transaction,"message/backfill-0000","custom.test.recorded",json!({"note":"pending"}),None);
        transaction.execute("INSERT INTO meta(key,value) VALUES(?1,'{broken') ON CONFLICT(key) DO UPDATE SET value=excluded.value",[FOLD_MARKER]).unwrap();
        transaction.commit().unwrap();
        assert!(open_chunks(&mut connection,false).is_err());
        assert_eq!(connection.pragma_query_value(None,"wal_autocheckpoint",|row|row.get::<_,u32>(0)).unwrap(),7);
    }
}
