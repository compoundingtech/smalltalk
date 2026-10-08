//! Bounded message pages from immutable claims at a server-retained store-index cut.
use super::*;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    #[cfg(not(test))]
    drop_version_schema(connection)?;
    connection.execute_batch(r#"
CREATE TABLE IF NOT EXISTS local_client_message_cut_epoch(id INTEGER PRIMARY KEY CHECK(id=1),epoch INTEGER NOT NULL);
INSERT OR IGNORE INTO local_client_message_cut_epoch VALUES(1,0);
-- Repair and checkpoint deletion destroy historical folds. Invalidate retained cuts
-- transactionally, including deletions performed by the graph's checkpoint engine.
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_delete AFTER DELETE ON claims
WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_claim_update AFTER UPDATE ON claims
WHEN OLD.subject LIKE 'message/%' OR NEW.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_desired_delete AFTER DELETE ON desired
WHEN OLD.subject LIKE 'message/%' BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS client_message_cut_repair AFTER UPDATE OF state ON replica_records
WHEN NEW.state='repaired' AND OLD.state!='repaired'
 AND EXISTS(SELECT 1 FROM claims WHERE id=NEW.claim_id AND subject LIKE 'message/%') BEGIN
 UPDATE local_client_message_cut_epoch SET epoch=epoch+1 WHERE id=1;
END;
"#)?;
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

#[cfg(test)]
pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    flush(transaction)
}

#[cfg(test)]
pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    // Comparative instrumentation is absent from production writer transactions.
    if transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_client_message_versions')", [], |row| row.get::<_,bool>(0))? {
        super::client_messages_version_benchmark::flush(transaction)?;
    }
    Ok(())
}

fn normalize_sql(expression: &str) -> String {
    format!("CASE WHEN {expression}='' OR {expression}='requester' OR instr({expression},'/')>0 THEN {expression} ELSE 'agent/'||{expression} END")
}

/// Only selection fields are read for candidates. Full actual-state folds are
/// performed after SQL has selected a bounded page, never for the entire mailbox.
fn header_field(field: &str) -> String {
    canonical_sql(&format!(
        "(SELECT json_extract(claims.body,CASE WHEN json_type(claims.body,'$.fields') IS NULL THEN '$.{field}' ELSE '$.fields.{field}' END)
          FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=candidate.subject AND claims.store_index<=?1
            AND {ACTUAL_STATE_CLAIM} AND claims.kind NOT LIKE 'harness.%'
            AND json_type(claims.body,CASE WHEN json_type(claims.body,'$.fields') IS NULL THEN '$.{field}' ELSE '$.fields.{field}' END) IS NOT NULL
          ORDER BY CANONICAL_DESC(claims) LIMIT 1)"
    ))
}

// SQLite's INTEGER is signed; preserve the full u64 tag range with padded
// decimal keys. Malformed/overflowing tags use zero, matching Rust's parser.
fn version_sql(tags: &str) -> String {
    format!(r#"COALESCE((
 SELECT CASE WHEN digits!='' AND digits NOT GLOB '*[^0-9]*'
  AND (length(ltrim(digits,'0'))<20 OR
       (length(ltrim(digits,'0'))=20 AND ltrim(digits,'0')<='18446744073709551615'))
 THEN substr('00000000000000000000'||ltrim(digits,'0'),-20)
 ELSE '00000000000000000000' END
 FROM (
  SELECT CASE WHEN substr(value,9,1)='+' THEN substr(value,10) ELSE substr(value,9) END digits
  FROM json_each({tags}) WHERE substr(value,1,8)='version:' LIMIT 1
 )
),'00000000000000000000')"#)
}

fn page_sql(person: bool, actor: bool, history: bool) -> String {
    let scope = if person {
        "SELECT subject FROM claims INDEXED BY claims_message_to_index
         WHERE kind='message.sent' AND json_extract(body,'$.fields.to') IN (?2,?3) AND store_index<=?1
         UNION SELECT subject FROM claims INDEXED BY claims_message_legacy_to_index
         WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL AND json_extract(body,'$.to') IN (?2,?3) AND store_index<=?1
         UNION SELECT json_extract(value,'$.subject') FROM json_each(?7) WHERE json_extract(value,'$.to')=?2"
    } else {
        "SELECT subject FROM message_index WHERE created_index<=?1"
    };
    let from = normalize_sql(&format!("COALESCE({},json_extract(legacy.value,'$.from'),'requester')",header_field("from")));
    let to = normalize_sql(&format!("COALESCE({},json_extract(legacy.value,'$.to'),'')",header_field("to")));
    let status = header_field("status");
    let tags = header_field("tags");
    let actor_candidates = if actor {
        "SELECT subject FROM claims INDEXED BY claims_message_from_index WHERE kind='message.sent' AND json_extract(body,'$.fields.from') IN (?4,?9) AND store_index<=?1
         UNION SELECT subject FROM claims INDEXED BY claims_message_to_index WHERE kind='message.sent' AND json_extract(body,'$.fields.to') IN (?4,?9) AND store_index<=?1
         UNION SELECT subject FROM claims INDEXED BY claims_message_legacy_from_index WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL AND json_extract(body,'$.from') IN (?4,?9) AND store_index<=?1
         UNION SELECT subject FROM claims INDEXED BY claims_message_legacy_to_index WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL AND json_extract(body,'$.to') IN (?4,?9) AND store_index<=?1
         UNION SELECT json_extract(value,'$.subject') FROM json_each(?7) WHERE json_extract(value,'$.from')=?4 OR json_extract(value,'$.to')=?4
         UNION SELECT subject FROM candidates WHERE ?4='requester'"
    } else { "SELECT subject FROM candidates" };
    let rival_recipient = if person { format!("AND {to}=?2") } else { String::new() };
    let actor_scope = if actor { "AND (sender=?4 OR recipient=?4)" } else { "" };
    let recipient_scope = if person { "AND recipient=?2" } else { "" };
    let current_scope = if history { "" } else { "AND current" };
    let version = version_sql("tags");
    let rival_version = version_sql("rival.tags");
    // Resolve reminder winners in recipient scope, deliberately before actor filtering.
    canonical_sql(&format!(r#"
WITH candidates AS MATERIALIZED ({scope}),
actor_candidates AS MATERIALIZED ({actor_candidates}),
candidate_keys AS MATERIALIZED (
 SELECT candidate.subject,
  (SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=candidate.subject AND store_index<=?1 ORDER BY CANONICAL_ASC(claims) LIMIT 1) sent_at
 FROM candidates candidate WHERE candidate.subject IN (SELECT subject FROM actor_candidates)
),
ordered AS MATERIALIZED (
 SELECT * FROM candidate_keys
 WHERE (?5 IS NULL OR CAST(sent_at AS INTEGER)<CAST(?5 AS INTEGER) OR (sent_at=?5 AND subject>?6))
 ORDER BY CAST(sent_at AS INTEGER) DESC,subject LIMIT ?8
),
headers AS MATERIALIZED (
 SELECT candidate.subject,{from} sender,{to} recipient,COALESCE({status},'sent') status,
        COALESCE({tags},json_extract(legacy.value,'$.tags'),'[]') tags,
        candidate.sent_at,
        EXISTS(SELECT 1 FROM claims WHERE claims.subject=candidate.subject AND kind='message.closed' AND store_index<=?1) closed
 FROM ordered candidate LEFT JOIN json_each(?7) legacy ON json_extract(legacy.value,'$.subject')=candidate.subject
),
page_headers AS MATERIALIZED (
 SELECT *,
  (SELECT substr(value,10) FROM json_each(tags) WHERE substr(value,1,9)='reminder:' LIMIT 1) reminder,
  {version} version
 FROM headers
),
rival_scope AS MATERIALIZED (
 SELECT candidate.subject,COALESCE({status},'sent') status,
  COALESCE({tags},json_extract(legacy.value,'$.tags'),'[]') tags
 FROM candidates candidate LEFT JOIN json_each(?7) legacy ON json_extract(legacy.value,'$.subject')=candidate.subject
 WHERE NOT EXISTS(SELECT 1 FROM claims WHERE claims.subject=candidate.subject AND kind='message.closed' AND store_index<=?1) {rival_recipient}
),
selected AS (
 SELECT candidate.*, CASE WHEN candidate.status='closed' OR candidate.closed THEN 0
  WHEN candidate.reminder IS NULL THEN 1 ELSE NOT EXISTS(
    SELECT 1 FROM rival_scope rival
     WHERE (SELECT substr(value,10) FROM json_each(rival.tags) WHERE substr(value,1,9)='reminder:' LIMIT 1)=candidate.reminder
     AND rival.status!='closed'
     AND ({rival_version},rival.subject)>(candidate.version,candidate.subject)) END current
 FROM page_headers candidate
)
SELECT subject,sent_at,current,(1 {recipient_scope} {actor_scope} {current_scope}) eligible FROM selected
 WHERE (?9 IS NULL OR ?9 IS NOT NULL)
 ORDER BY CAST(sent_at AS INTEGER) DESC,subject
"#))
}

impl Store {
    pub(crate) fn client_messages_cut_epoch(&self) -> Result<u64> {
        Ok(self.readers.get().query_row("SELECT epoch FROM local_client_message_cut_epoch WHERE id=1", [], |row| row.get(0))?)
    }

    #[cfg(test)]
    pub(crate) fn client_messages_benchmark_prepare_io(&self) -> Result<()> {
        self.connection.write()
            .execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn client_messages_benchmark_checkpoint(&self) -> Result<()> {
        self.connection.write()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn client_messages_enable_version_benchmark(&self) -> Result<()> {
        let mut connection = self.connection.write();
        super::client_messages_version_benchmark::create_schema(&connection)?;
        let transaction = connection.transaction()?;
        super::client_messages_version_benchmark::open(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn client_messages_page(
        &self, person: Option<&str>, actor: Option<&str>, history: bool,
        through: u64, after: Option<&(u128,String)>, limit: usize,
    ) -> Result<Vec<(MessageView,Value,bool)>> {
        let connection = self.readers.get();
        let person = person.map(normalize_message_party);
        let bare = person.as_deref().map(|person| person.strip_prefix("agent/").filter(|suffix| !suffix.contains('/')).unwrap_or(person));
        // Desired-only messages have no endpoint claim index. Resolve their small
        // declaration headers at the cut, without folding any actual-state body.
        let subjects = connection.prepare_cached("SELECT subject FROM desired WHERE kind='message' AND subject LIKE 'message/%' AND EXISTS(SELECT 1 FROM claims WHERE claims.subject=desired.subject AND kind='intent.desired' AND store_index<=?1)")?
            .query_map([through], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut legacy = Vec::new();
        for subject in subjects {
            if let Some(desired) = desired_row_at(&connection,&subject,Some(through))? {
                let desired: Value = serde_json::from_str(&desired.body)?;
                legacy.push(json!({"subject":subject,
                    "from":normalize_message_party(&canonical_child_string(&desired,"from").unwrap_or_else(||"requester".into())),
                    "to":normalize_message_party(&canonical_child_string(&desired,"to").unwrap_or_default()),
                    "tags":canonical_child_strings(&desired,"tag")}));
            }
        }
        static SQL: std::sync::LazyLock<[String; 8]> = std::sync::LazyLock::new(|| {
            std::array::from_fn(|flags| page_sql(flags & 1 != 0, flags & 2 != 0, flags & 4 != 0))
        });
        let flags = usize::from(person.is_some()) | (usize::from(actor.is_some()) << 1) | (usize::from(history) << 2);
        let legacy = serde_json::to_string(&legacy)?;
        let actor_bare = actor.map(|actor|actor.strip_prefix("agent/").filter(|suffix| !suffix.contains('/')).unwrap_or(actor));
        let mut scan_after = after.cloned();
        let wanted = limit.saturating_add(1);
        let mut rows = Vec::with_capacity(wanted);
        loop {
            // Seek and limit candidates before reading even their selection fields.
            // Sparse filters refill by the last scanned key, not the last accepted row.
            let batch = connection.prepare_cached(&SQL[flags])?.query_map(
                params![through,person,bare,actor,scan_after.as_ref().map(|(at,_)|at.to_string()),scan_after.as_ref().map(|(_,id)|id),legacy,wanted,actor_bare],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,bool>(2)?,row.get::<_,bool>(3)?)),
            )?.collect::<rusqlite::Result<Vec<_>>>()?;
            let exhausted = batch.len() < wanted;
            if let Some((subject, sent_at, _, _)) = batch.last() {
                scan_after = Some((sent_at.parse()?,subject.clone()));
            }
            for (subject,sent_at,current,eligible) in batch {
                if eligible {
                    rows.push((subject,sent_at,current));
                    if rows.len() == wanted { break; }
                }
            }
            if rows.len() == wanted || exhausted { break; }
        }
        // Canonical first/last metadata is selected in one batch; payload decoding
        // remains bounded by page size even with deep unrelated subject history.
        let subjects = rows.iter().map(|(subject,_,_)|subject).collect::<Vec<_>>();
        static METADATA_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| canonical_sql(r#"
WITH subjects AS (SELECT value subject FROM json_each(?1)),
ends AS (
 SELECT subject,
  (SELECT id FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=subjects.subject AND store_index<=?2 ORDER BY CANONICAL_ASC(claims) LIMIT 1) first_id,
  (SELECT id FROM claims INDEXED BY claims_subject_accepted_index WHERE claims.subject=subjects.subject AND store_index<=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1) last_id
 FROM subjects
)
SELECT ends.subject,first.accepted_at_unix_ms,last.accepted_at_unix_ms,last.id,
 json_extract(first.body,CASE WHEN json_type(first.body,'$.fields') IS NULL THEN '$.session_id' ELSE '$.fields.session_id' END)
FROM ends JOIN claims first ON first.id=ends.first_id JOIN claims last ON last.id=ends.last_id
"#));
        let mut metadata = connection.prepare_cached(&METADATA_SQL)?.query_map(params![serde_json::to_string(&subjects)?,through], |row| {
            Ok((row.get::<_,String>(0)?,json!({"sent_at":row.get::<_,String>(1)?,"updated_at":row.get::<_,String>(2)?,"revision":row.get::<_,String>(3)?,"session_id":row.get::<_,Option<String>>(4)?})))
        })?.collect::<rusqlite::Result<BTreeMap<_,_>>>()?;
        rows.into_iter().map(|(subject,_,current)| {
            let created = connection.query_row("SELECT MIN(store_index) FROM claims WHERE subject=?1 AND store_index<=?2",params![subject,through], |row|row.get::<_,u64>(0))?;
            let message = message_view_at(&connection,&subject,created,Some(through))?;
            let metadata = metadata.remove(&subject).ok_or_else(||anyhow::anyhow!("message cut metadata is unavailable"))?;
            Ok((message,metadata,current))
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(store: &Store, subject: &str, sender: &str, version: &str) {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: "message.sent".into(), actor: Some(sender.into()),
            fields: BTreeMap::from([
                ("from".into(),json!(sender)), ("to".into(),json!("person/recipient")),
                ("content".into(),json!("message body")), ("status".into(),json!("sent")),
                ("tags".into(),json!(["reminder:cut-test",format!("version:{version}")])),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    }

    #[test]
    fn version_schema_removal_preserves_claims_and_cut_pages() {
        let store = Store::open_memory("cut-schema-test").unwrap();
        send(&store,"message/old","agent/filtered","1");
        let through = store.index().unwrap();
        store.client_messages_enable_version_benchmark().unwrap();
        {
            let connection = store.connection.write();
            drop_version_schema(&connection).unwrap();
            let remaining: u64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('local_client_message_pending','local_client_message_versions','local_client_message_retirements','local_client_message_generation') OR (type='trigger' AND name LIKE 'client_messages_%')",
                [], |row|row.get(0),
            ).unwrap();
            assert_eq!(remaining,0);
            assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='client_message_versions_v1')", [], |row|row.get::<_,bool>(0)).unwrap());
        }
        assert_eq!(store.index().unwrap(),through);
        let rows = store.read_snapshot(|_|store.client_messages_page(None,None,true,through,None,20)).unwrap();
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0].0.content,"message body");
        assert_eq!(store.claims_for("message/old",None).unwrap().len(),1);
    }

    #[test]
    fn unsigned_reminder_winner_precedes_actor_filter() {
        let store = Store::open_memory("cut-reminder-test").unwrap();
        send(&store,"message/old","agent/filtered","18446744073709551614");
        send(&store,"message/new","agent/rival","+18446744073709551615");
        send(&store,"message/z-overflow","agent/rival","18446744073709551616");
        store.read_snapshot(|through| {
            let current = store.client_messages_page(Some("person/recipient"),Some("agent/filtered"),false,through,None,1)?;
            assert!(current.is_empty());
            let history = store.client_messages_page(Some("person/recipient"),Some("agent/filtered"),true,through,None,1)?;
            assert_eq!(history.len(),1);
            assert!(!history[0].2);
            Ok(())
        }).unwrap();
    }
}
