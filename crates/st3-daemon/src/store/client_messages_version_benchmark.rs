//! Frozen pre-cutover version design, enabled only by the isolated comparison benchmark.
use super::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_client_message_pending(subject TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_client_message_versions(
 subject TEXT NOT NULL, generation INTEGER NOT NULL, retired INTEGER,
 -- mailbox: 0 = not a recipient candidate, 1 = open candidate, 2 = closed candidate.
 sender TEXT NOT NULL, recipient TEXT NOT NULL, mailbox INTEGER NOT NULL,
 closed INTEGER NOT NULL, reminder TEXT, version TEXT NOT NULL,
 sent_key TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(subject,generation)
) WITHOUT ROWID;
-- Each paging shape walks an ordered, covering metadata index, never the JSON bodies.
CREATE INDEX IF NOT EXISTS local_client_messages_order_page ON local_client_message_versions(
 sent_key DESC,subject,generation,retired,closed,reminder,version,mailbox);
CREATE INDEX IF NOT EXISTS local_client_messages_sender_page ON local_client_message_versions(
 sender,sent_key DESC,subject,generation,retired,closed,reminder,version,mailbox);
CREATE INDEX IF NOT EXISTS local_client_messages_recipient_page ON local_client_message_versions(
 recipient,sent_key DESC,subject,generation,retired,closed,reminder,version,mailbox);
CREATE INDEX IF NOT EXISTS local_client_messages_recipient_sender ON local_client_message_versions(
 recipient,sender,sent_key DESC,subject,generation,retired,closed,reminder,version,mailbox);
CREATE INDEX IF NOT EXISTS local_client_messages_reminder ON local_client_message_versions(
 reminder,version DESC,subject DESC,generation,retired) WHERE closed=0;
CREATE INDEX IF NOT EXISTS local_client_messages_recipient_reminder ON local_client_message_versions(
 recipient,reminder,version DESC,subject DESC,generation,retired) WHERE mailbox=1;
CREATE INDEX IF NOT EXISTS local_client_messages_retired ON local_client_message_versions(retired)
 WHERE retired IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_client_message_retirements(
 generation INTEGER PRIMARY KEY,at_unix_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS local_client_message_retirement_time
 ON local_client_message_retirements(at_unix_ms,generation);
CREATE TABLE IF NOT EXISTS local_client_message_generation(id INTEGER PRIMARY KEY CHECK(id=1),generation INTEGER NOT NULL);
INSERT OR IGNORE INTO local_client_message_generation VALUES(1,0);
CREATE TRIGGER IF NOT EXISTS client_messages_claim_insert AFTER INSERT ON claims WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS client_messages_claim_delete AFTER DELETE ON claims WHEN OLD.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(OLD.subject);
END;
CREATE TRIGGER IF NOT EXISTS client_messages_claim_update AFTER UPDATE ON claims WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(OLD.subject);
 INSERT OR IGNORE INTO local_client_message_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS client_messages_desired_insert AFTER INSERT ON desired WHEN NEW.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS client_messages_desired_delete AFTER DELETE ON desired WHEN OLD.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(OLD.subject);
END;
CREATE TRIGGER IF NOT EXISTS client_messages_desired_update AFTER UPDATE ON desired WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 INSERT OR IGNORE INTO local_client_message_pending VALUES(OLD.subject);
 INSERT OR IGNORE INTO local_client_message_pending VALUES(NEW.subject);
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let filled: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='client_message_versions_v1')", [], |row| row.get(0),
    )?;
    if !filled {
        transaction.execute("INSERT OR IGNORE INTO local_client_message_pending SELECT subject FROM message_index", [])?;
    }
    flush(transaction)?;
    transaction.execute("INSERT OR IGNORE INTO meta VALUES('client_message_versions_v1','1')", [])?;
    Ok(())
}

pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let pending = transaction.prepare_cached("SELECT subject FROM local_client_message_pending")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = u64::try_from(crate::api::client_now_ms())?;
    prune(transaction, now)?;
    if pending.is_empty() { return Ok(()); }
    transaction.execute("UPDATE local_client_message_generation SET generation=generation+1 WHERE id=1", [])?;
    let generation: u64 = transaction.query_row("SELECT generation FROM local_client_message_generation WHERE id=1", [], |row| row.get(0))?;
    transaction.execute(
        "INSERT INTO local_client_message_retirements VALUES(?1,?2)",
        params![generation,now],
    )?;
    for subject in pending {
        transaction.execute("UPDATE local_client_message_versions SET retired=?2 WHERE subject=?1 AND retired IS NULL", params![subject,generation])?;
        let indexed: Option<(u64,bool)> = transaction.query_row(
            "SELECT created_index,closed FROM message_index WHERE subject=?1 AND created_index>0", [&subject], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        if let Some((created,closed)) = indexed {
            let message = message_view_tx(transaction,&subject,created)?;
            let first: ClaimRecord = transaction.query_row(
                &format!("SELECT {CLAIM_COLUMNS} FROM claims INDEXED BY claims_subject_accepted_index JOIN batches ON batches.id=claims.batch_id WHERE claims.subject=?1 ORDER BY {CANONICAL_ORDER} LIMIT 1"), [&subject], claim_from_row,
            )?;
            let last: ClaimRecord = transaction.query_row(
                &format!("SELECT {CLAIM_COLUMNS} FROM claims INDEXED BY claims_subject_accepted_index JOIN batches ON batches.id=claims.batch_id WHERE claims.subject=?1 ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"), [&subject], claim_from_row,
            )?;
            let (reminder, version) = reminder_version(&message.tags);
            let bare = message.to.strip_prefix("agent/").filter(|suffix| !suffix.contains('/')).unwrap_or(&message.to);
            let mailbox: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='message.sent' AND json_extract(body,'$.fields.to') IN (?2,?3)) OR EXISTS(SELECT 1 FROM desired,json_each(desired.body,'$.children') child WHERE desired.subject=?1 AND desired.kind='message' AND json_extract(child.value,'$.name')='to' AND json_extract(child.value,'$.arguments[0]') IN (?2,?3))",
                params![subject,message.to,bare], |row| row.get(0),
            )?;
            let recipient_closed: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='message.closed')",
                [&subject], |row| row.get(0),
            )?;
            let sent_key = crate::api::client_timestamp(first.accepted_at_unix_ms);
            let body = json!({"message":message,"sent_at":first.accepted_at_unix_ms.to_string(),"updated_at":last.accepted_at_unix_ms.to_string(),"revision":last.id,"session_id":first.body.get("fields").unwrap_or(&first.body).get("session_id").cloned().unwrap_or(Value::Null)});
            transaction.execute(
                "INSERT INTO local_client_message_versions VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![subject,generation,message.from,message.to,if !mailbox { 0 } else if recipient_closed || message.status=="closed" { 2 } else { 1 },closed || message.status=="closed",reminder,format!("{version:020}"),sent_key,serde_json::to_string(&body)?],
            )?;
        }
        transaction.execute("DELETE FROM local_client_message_pending WHERE subject=?1", [&subject])?;
    }
    Ok(())
}

/// A cut can be acquired immediately before a retirement. Keep that old version
/// for the entire cursor lifetime after retirement, not after its creation.
fn prune(transaction: &Transaction<'_>, now: u64) -> Result<()> {
    let ttl = u64::try_from(crate::api::CLIENT_PAGE_TTL_MS)?;
    let expired = now.saturating_sub(ttl);
    transaction.execute(
        "DELETE FROM local_client_message_versions WHERE retired IN (
         SELECT generation FROM local_client_message_retirements WHERE at_unix_ms<?1)",
        [expired],
    )?;
    transaction.execute(
        "DELETE FROM local_client_message_retirements WHERE at_unix_ms<?1",
        [expired],
    )?;
    Ok(())
}

fn reminder_version(tags: &[String]) -> (Option<&str>, u64) {
    let reminder = tags.iter().find_map(|tag| tag.strip_prefix("reminder:"));
    let version = tags.iter().find_map(|tag| tag.strip_prefix("version:"))
        .and_then(|value| value.parse::<u64>().ok()).unwrap_or_default();
    (reminder, version)
}

fn page_sql(person: bool, actor: bool, history: bool, after: bool) -> String {
    let rival_index = if person {
        "local_client_messages_recipient_reminder"
    } else {
        "local_client_messages_reminder"
    };
    let rival_scope = if person { "AND rival.recipient=?2 AND rival.mailbox=1" } else { "AND rival.closed=0" };
    // Actor is deliberately absent here: a winner belonging to another actor
    // must still supersede a message in this actor's page.
    let open = if person { "candidate.mailbox=1" } else { "candidate.closed=0" };
    let actionable = format!(
        "{open} AND (candidate.reminder IS NULL OR NOT EXISTS(
         SELECT 1 FROM local_client_message_versions rival INDEXED BY {rival_index}
         WHERE rival.reminder=candidate.reminder
           AND (rival.version,rival.subject)>(candidate.version,candidate.subject)
           AND rival.generation<=?1 AND (rival.retired IS NULL OR rival.retired>?1)
           {rival_scope}))"
    );
    let recipient_scope = if person { "AND candidate.recipient=?2 AND candidate.mailbox>0" } else { "" };
    let current = if history { String::new() } else { format!("AND ({actionable})") };
    let branch = |index: &str, actor_scope: &str| {
        let range = |keyset: &str| format!(
            "SELECT candidate.subject,candidate.generation,candidate.sent_key,
                    candidate.closed,candidate.reminder,candidate.version,candidate.mailbox
             FROM local_client_message_versions candidate INDEXED BY {index}
             WHERE candidate.generation<=?1 AND (candidate.retired IS NULL OR candidate.retired>?1)
               {recipient_scope} {actor_scope} {keyset} {current}
             ORDER BY candidate.sent_key DESC,candidate.subject LIMIT ?6"
        );
        if after {
            // Separate equality and earlier-time ranges let both timestamp and
            // subject seek into the mixed DESC/ASC index, including timestamp ties.
            let tied = range("AND candidate.sent_key=?4 AND candidate.subject>?5");
            let earlier = range("AND candidate.sent_key<?4");
            format!(
                "SELECT * FROM (SELECT * FROM ({tied}) UNION ALL SELECT * FROM ({earlier}))
                 ORDER BY sent_key DESC,subject LIMIT ?6"
            )
        } else {
            range("")
        }
    };
    let candidates = if actor {
        let sender_index = if person {
            "local_client_messages_recipient_sender"
        } else { "local_client_messages_sender_page" };
        let sender = branch(sender_index, "AND candidate.sender=?3");
        let recipient = branch("local_client_messages_recipient_page", "AND candidate.recipient=?3");
        // Both branches are independently limited before their bounded union.
        // Using two ranges avoids SQLite's unordered OR/multi-index plan.
        format!(
            "sender_page AS MATERIALIZED ({sender}),
             recipient_page AS MATERIALIZED ({recipient}),
             page AS MATERIALIZED (
              SELECT * FROM (SELECT * FROM sender_page UNION SELECT * FROM recipient_page)
              ORDER BY sent_key DESC,subject LIMIT ?6)"
        )
    } else {
        let index = if person { "local_client_messages_recipient_page" } else { "local_client_messages_order_page" };
        format!("page AS MATERIALIZED ({})", branch(index, ""))
    };
    format!(
        "WITH {candidates}
         SELECT versions.body,({actionable}) FROM page candidate
         CROSS JOIN local_client_message_versions versions
         WHERE versions.subject=candidate.subject AND versions.generation=candidate.generation
         ORDER BY candidate.sent_key DESC,candidate.subject"
    )
}

impl Store {
    pub(crate) fn client_messages_version_generation(&self) -> Result<u64> {
        Ok(self.readers.get().query_row("SELECT generation FROM local_client_message_generation WHERE id=1", [], |row| row.get(0))?)
    }

    pub(crate) fn client_messages_version_page(
        &self, person: Option<&str>, actor: Option<&str>, history: bool,
        generation: u64, after: Option<&(u128,String)>, limit: usize,
    ) -> Result<Vec<(MessageView,Value,bool)>> {
        let person = person.map(normalize_message_party);
        let after_key = after.map(|(at,_)| crate::api::client_timestamp(*at));
        let connection = self.readers.get();
        let sql = page_sql(person.is_some(), actor.is_some(), history, after.is_some());
        let mut statement = connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params![generation,person,actor,after_key,after.map(|(_,id)|id),limit.saturating_add(1)], |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(body,current)| {
            let mut body: Value = serde_json::from_str(&body)?;
            let message = body.as_object_mut().and_then(|body| body.remove("message"))
                .ok_or_else(|| anyhow::anyhow!("client message projection has no message"))?;
            let message = serde_json::from_value(message)?;
            Ok((message,body,current))
        }).collect()
    }
}
