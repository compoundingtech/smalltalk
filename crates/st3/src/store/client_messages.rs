//! Body-free message selectors: ordered eligibility seeks, then page-only claim folds.
use super::*;

#[cfg(test)]
const TABLE: &str = "local_client_message_selectors_v1";
const MARKER: &str = "client_message_selectors_v1_cut";
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
-- born_index=-1 is transaction-local pending bookkeeping, not a historical header.
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
CREATE TRIGGER IF NOT EXISTS client_selector_claim_insert AFTER INSERT ON claims WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(NEW.subject,-1,CASE WHEN NEW.kind IN ('message.sent','message.closed','intent.desired') OR json_type(NEW.body,'$.fields.from') IS NOT NULL OR json_type(NEW.body,'$.fields.to') IS NOT NULL OR json_type(NEW.body,'$.fields.tags') IS NOT NULL OR json_type(NEW.body,'$.fields.status') IS NOT NULL OR (json_type(NEW.body,'$.fields') IS NULL AND (json_type(NEW.body,'$.from') IS NOT NULL OR json_type(NEW.body,'$.to') IS NOT NULL OR json_type(NEW.body,'$.tags') IS NOT NULL OR json_type(NEW.body,'$.status') IS NOT NULL)) THEN 1 ELSE 0 END)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|excluded.dirty_flags;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_claim_delete AFTER DELETE ON claims WHEN OLD.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(OLD.subject,-1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_claim_update AFTER UPDATE ON claims WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(OLD.subject,-1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2;
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(NEW.subject,-1,2)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|2;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_insert AFTER INSERT ON desired WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(NEW.subject,-1,1)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|1;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_update AFTER UPDATE ON desired WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags) VALUES(NEW.subject,-1,1)
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|1;
END;
CREATE TRIGGER IF NOT EXISTS client_selector_desired_delete AFTER DELETE ON desired WHEN OLD.subject LIKE 'message/%' BEGIN
 -- Preserve the affected key before clearing declaration-derived routing fields.
 INSERT INTO local_client_message_selectors_v1(subject,born_index,dirty_flags,reminder,recipient)
 VALUES(OLD.subject,-1,4,
  (SELECT reminder FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index>=0 AND retired_index IS NULL),
  COALESCE((SELECT recipient FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index>=0 AND retired_index IS NULL),''))
 ON CONFLICT(subject,born_index) DO UPDATE SET dirty_flags=dirty_flags|4,reminder=COALESCE(reminder,excluded.reminder);
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
  (subject=OLD.subject OR reminder=(SELECT reminder FROM local_client_message_selectors_v1 WHERE subject=OLD.subject AND born_index=-1));
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    #[cfg(not(test))]
    drop_version_schema(connection)?;
    connection.execute_batch(r#"
CREATE TABLE IF NOT EXISTS local_client_message_cut_epoch(id INTEGER PRIMARY KEY CHECK(id=1),epoch INTEGER NOT NULL);
INSERT OR IGNORE INTO local_client_message_cut_epoch VALUES(1,0);
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_delete AFTER DELETE ON claims WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_update AFTER UPDATE ON claims WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_desired_delete AFTER DELETE ON desired WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_repair AFTER UPDATE OF state ON replica_records
WHEN NEW.state='repaired' AND OLD.state!='repaired' AND EXISTS(SELECT 1 FROM claims WHERE id=NEW.claim_id AND subject LIKE 'message/%') BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
"#)?;
    connection.execute_batch(SELECTOR_SCHEMA)?;
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

#[derive(Debug,PartialEq,Eq)]
struct Header {
    sender: String,recipient: String,mailbox: bool,closed: bool,
    reminder: Option<String>,version: String,sent_key: String,created_index: u64,
    desired_mask: u8,native_mailbox: bool,
}

fn first_key(connection: &Connection,subject: &str) -> Result<Option<String>> {
    static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(||canonical_sql(
        "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index WHERE subject=?1 ORDER BY CANONICAL_ASC(claims) LIMIT 1"));
    let at: Option<String> = connection.prepare_cached(&SQL)?.query_row([subject],|row|row.get(0)).optional()?;
    at.map(|at|Ok(crate::api::client_timestamp(at.parse()?))).transpose()
}

/// Decode selection fields only, not complete message bodies. Most metadata
/// writes take the unchanged-first-key shortcut in flush and never call this.
fn header(connection: &Connection,subject: &str,sent_key: String) -> Result<Option<Header>> {
    let indexed: Option<(u64,bool)> = connection.prepare_cached("SELECT created_index,closed FROM message_index WHERE subject=?1")?
        .query_row([subject],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    let Some((created_index,index_closed))=indexed else { return Ok(None); };
    static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let columns=[("from","text"),("to","text"),("tags","array"),("status","text")].into_iter().map(|(field,kind)| {
            let path=format!("CASE WHEN json_type(claims.body,'$.fields') IS NULL THEN '$.{field}' ELSE '$.fields.{field}' END");
            format!("json_type(claims.body,{path}) IS NOT NULL,CASE WHEN json_type(claims.body,{path})='{kind}' THEN json_extract(claims.body,{path}) END")
        }).collect::<Vec<_>>().join(",");
        canonical_sql(&format!("SELECT {columns} FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=?1 AND {ACTUAL_STATE_CLAIM} AND claims.kind NOT LIKE 'harness.%' ORDER BY CANONICAL_DESC(claims)"))
    });
    let mut values: [Option<Option<String>>;4]=std::array::from_fn(|_|None);
    let mut statement=connection.prepare_cached(&SQL)?;
    let mut rows=statement.query([subject])?;
    while let Some(row)=rows.next()? {
        for (index,value) in values.iter_mut().enumerate() {
            if value.is_none() && row.get::<_,bool>(index*2)? { *value=Some(row.get(index*2+1)?); }
        }
        if values.iter().all(Option::is_some) { break; }
    }
    let [sender,recipient,tags,status]=values.map(Option::flatten);
    let desired=current_desired_row(connection,subject)?.map(|row|serde_json::from_str::<Value>(&row.body)).transpose()?;
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
    let native_mailbox: bool=connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='message.sent' AND json_extract(body,'$.fields.to') IN (?2,?3))")?
        .query_row(params![subject,recipient,bare],|row|row.get(0))?;
    let declared_mailbox=desired.as_ref().and_then(|value|canonical_child_string(value,"to")).is_some_and(|to|to==recipient || to==bare);
    Ok(Some(Header { sender,recipient,mailbox:native_mailbox||declared_mailbox,closed:index_closed||status.as_deref()==Some("closed"),
        reminder,version:format!("{version:020}"),sent_key,created_index,desired_mask,native_mailbox }))
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
    transaction.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![MARKER,cut.to_string()])?;
    Ok(())
}

fn hydrate(transaction: &Transaction<'_>) -> Result<()> {
    // All historical receipts are unavailable after a reset, never silently short.
    transaction.execute("UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1",[])?;
    transaction.execute("DELETE FROM local_client_message_selectors_v1",[])?;
    let cut=current_index_tx(transaction)?;
    let now=u64::try_from(crate::api::client_now_ms())?;
    let subjects=transaction.prepare_cached("SELECT subject FROM message_index WHERE created_index>0 ORDER BY subject")?
        .query_map([],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut reminders=BTreeSet::new();
    for subject in subjects {
        if let Some(key)=first_key(transaction,&subject)? {
            if let Some(value)=header(transaction,&subject,key)? {
                if let Some(reminder)=&value.reminder { reminders.insert((reminder.clone(),value.recipient.clone())); }
                put_header(transaction,&subject,&value,cut,now)?;
            }
        }
    }
    let mut global=BTreeSet::new();
    for (reminder,recipient) in reminders {
        if global.insert(reminder.clone()) { reminder_winners(transaction,&reminder,None,cut,now)?; }
        reminder_winners(transaction,&reminder,Some(&recipient),cut,now)?;
    }
    record_cut(transaction,cut)
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    #[cfg(test)]
    if !transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",[TABLE],|row|row.get::<_,bool>(0))? { return Ok(()); }
    let seeded: bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",[MARKER],|row|row.get(0))?;
    if !seeded { hydrate(transaction)?; }
    flush(transaction)
}

pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    #[cfg(test)]
    if !transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",[TABLE],|row|row.get::<_,bool>(0))? { return Ok(()); }
    let pending=transaction.prepare_cached("SELECT subject,dirty_flags,reminder,recipient FROM local_client_message_selectors_v1 WHERE born_index=-1")?
        .query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u8>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if !pending.is_empty() {
        let cut=current_index_tx(transaction)?;
        let now=u64::try_from(crate::api::client_now_ms())?;
        let mut changed=false;
        let mut reminders=BTreeSet::new();
        for (subject,flags,pending_reminder,pending_recipient) in pending {
            let existing=current_header(transaction,&subject)?;
            let key=first_key(transaction,&subject)?;
            if flags==0 && existing.as_ref().is_some_and(|(_,value)|Some(&value.sent_key)==key.as_ref()) {
                transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=-1",[&subject])?;
                continue;
            }
            let value=key.map(|key|header(transaction,&subject,key)).transpose()?.flatten();
            if existing.as_ref().map(|(_,header)|header)!=value.as_ref() {
                changed=true;
                if let Some((_,old))=&existing {
                    if let Some(reminder)=&old.reminder { reminders.insert((reminder.clone(),old.recipient.clone())); }
                }
                if let Some(value)=&value {
                    if let Some(reminder)=&value.reminder { reminders.insert((reminder.clone(),value.recipient.clone())); }
                    put_header(transaction,&subject,value,cut,now)?;
                } else {
                    retire(transaction,&subject,cut,now)?;
                    transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=?2",params![subject,cut])?;
                }
            }
            if let Some(reminder)=pending_reminder { reminders.insert((reminder,pending_recipient)); }
            transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=-1",[&subject])?;
        }
        let mut global=BTreeSet::new();
        for (reminder,recipient) in reminders {
            if global.insert(reminder.clone()) { reminder_winners(transaction,&reminder,None,cut,now)?; }
            reminder_winners(transaction,&reminder,Some(&recipient),cut,now)?;
        }
        if changed { record_cut(transaction,cut)?; }
        let expired=now.saturating_sub(u64::try_from(crate::api::CLIENT_PAGE_TTL_MS)?);
        transaction.execute("DELETE FROM local_client_message_selectors_v1 WHERE retired_at_unix_ms IS NOT NULL AND retired_at_unix_ms<?1",[expired])?;
    }
    #[cfg(test)]
    if transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_client_message_versions')",[],|row|row.get::<_,bool>(0))? {
        super::client_messages_version_benchmark::flush(transaction)?;
    }
    Ok(())
}

fn page_sql(person: bool,actor: bool,history: bool,current: bool,after: bool) -> String {
    let flag=if person { "recipient_current" } else { "global_current" };
    let alive=if current { "retired_index IS NULL" } else { "born_index<=?1 AND (retired_index IS NULL OR retired_index>?1)" };
    let eligible=if history { String::new() } else { format!("AND {flag}=1") };
    let recipient_scope=if person { "AND recipient=?2 AND mailbox=1" } else { "" };
    let branch=|index: &str,scope: &str| {
        let range=|keyset: &str|format!("SELECT subject,born_index,sent_key,created_index,{flag} current FROM local_client_message_selectors_v1 INDEXED BY {index} WHERE born_index>=0 AND {alive} {recipient_scope} {scope} {eligible} {keyset} ORDER BY sent_key DESC,subject LIMIT ?6");
        if after {
            let tied=range("AND sent_key=?4 AND subject>?5");
            let earlier=range("AND sent_key<?4");
            format!("SELECT * FROM (SELECT * FROM ({tied}) UNION ALL SELECT * FROM ({earlier})) ORDER BY sent_key DESC,subject LIMIT ?6")
        } else { range("") }
    };
    let fast=current && !history;
    let candidates=if actor && !person {
        let sender=branch(if fast { "client_selector_sender_eligible" } else { "client_selector_sender" },"AND sender=?3");
        let recipient=branch(if fast { "client_selector_recipient_global_eligible" } else { "client_selector_recipient" },"AND recipient=?3");
        format!("SELECT * FROM (SELECT * FROM ({sender}) UNION SELECT * FROM ({recipient})) ORDER BY sent_key DESC,subject LIMIT ?6")
    } else if actor {
        branch(if fast { "client_selector_recipient_sender_eligible" } else { "client_selector_recipient_sender" },"AND sender=?3")
    } else {
        let index=match (person,fast) {
            (true,true)=>"client_selector_recipient_eligible",(true,false)=>"client_selector_recipient",
            (false,true)=>"client_selector_global_eligible",(false,false)=>"client_selector_order",
        };
        branch(index,"")
    };
    format!("SELECT subject,created_index,current FROM ({candidates}) WHERE (?1 IS NULL OR ?1 IS NOT NULL) AND (?2 IS NULL OR ?2 IS NOT NULL) AND (?3 IS NULL OR ?3 IS NOT NULL) AND (?4 IS NULL OR ?4 IS NOT NULL) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY sent_key DESC,subject")
}

impl Store {
    pub(crate) fn client_messages_cut_epoch(&self) -> Result<u64> {
        Ok(self.readers.get().query_row("SELECT epoch FROM local_client_message_cut_epoch WHERE id=1",[],|row|row.get(0))?)
    }

    pub(crate) fn client_messages_page(&self,person: Option<&str>,actor: Option<&str>,history: bool,through: u64,after: Option<&(u128,String)>,limit: usize) -> Result<Vec<(MessageView,Value,bool)>> {
        let connection=self.readers.get();
        let pending: bool=connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM local_client_message_selectors_v1 WHERE born_index=-1 AND dirty_flags!=4)")?.query_row([],|row|row.get(0))?;
        anyhow::ensure!(!pending,"message selector projection is not current");
        let last_change: String=connection.prepare_cached("SELECT value FROM meta WHERE key=?1")?.query_row([MARKER],|row|row.get(0))?;
        let current=through>=last_change.parse::<u64>()?;
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
            connection.execute_batch("DROP TRIGGER IF EXISTS client_selector_claim_insert;DROP TRIGGER IF EXISTS client_selector_claim_delete;DROP TRIGGER IF EXISTS client_selector_claim_update;DROP TRIGGER IF EXISTS client_selector_desired_insert;DROP TRIGGER IF EXISTS client_selector_desired_update;DROP TRIGGER IF EXISTS client_selector_desired_delete;DROP TABLE IF EXISTS local_client_message_selectors_v1;")?;
            connection.execute("DELETE FROM meta WHERE key=?1",[MARKER])?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn client_messages_selector_benchmark_rebuild(&self) -> Result<()> {
        let mut connection=self.connection.write();
        let transaction=connection.transaction()?;
        hydrate(&transaction)?;
        transaction.commit()?;
        Ok(())
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
}
