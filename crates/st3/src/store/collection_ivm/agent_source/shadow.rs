//! Namespace-only physical source replacements and bounded canonical reverse dependencies.
//! Absence here is provisional until the caller proves the extraction/journal cut complete.
//! This module never consults live source tables, certifies coverage, or writes roots.
use super::canonical::Fact;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use smallclaims::{
    ivm::install::{Mutation, Namespace},
    store::canonical,
};

#[derive(Debug)]
pub enum Pending {
    Batch(String),
    CapturePrefix,
}
impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Batch(id) => write!(f, "canonical batch {id} not yet captured in namespace"),
            Self::CapturePrefix => write!(f, "canonical namespace extraction prefix is incomplete"),
        }
    }
}
impl std::error::Error for Pending {}

const MAX_ROWS: usize = 128;
const MAX_BYTES: usize = 64 * 1024;
const TABLE_NAMES: &[&str] = &[
    "local_agent_source_atoms",
    "local_agent_source_claims",
    "local_agent_source_records",
    "local_agent_source_dirty",
    "local_agent_source_fanout",
];

pub fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_agent_source_atoms(
 namespace TEXT NOT NULL, source_table TEXT NOT NULL, source_key TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(namespace,source_table,source_key));
CREATE TABLE IF NOT EXISTS local_agent_source_claims(
 namespace TEXT NOT NULL,id TEXT NOT NULL,store_index INTEGER NOT NULL,batch_id TEXT NOT NULL,
 subject TEXT NOT NULL,body TEXT NOT NULL, PRIMARY KEY(namespace,id),UNIQUE(namespace,store_index));
CREATE INDEX IF NOT EXISTS local_agent_source_batch_claims
 ON local_agent_source_claims(namespace,batch_id,store_index,id);
CREATE TABLE IF NOT EXISTS local_agent_source_records(
 namespace TEXT NOT NULL,record_ref TEXT NOT NULL,claim_id TEXT,position INTEGER NOT NULL,
 state TEXT NOT NULL,PRIMARY KEY(namespace,record_ref));
CREATE INDEX IF NOT EXISTS local_agent_source_claim_records
 ON local_agent_source_records(namespace,claim_id,position,record_ref);
CREATE TABLE IF NOT EXISTS local_agent_source_dirty(
 namespace TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(namespace,id));
CREATE TABLE IF NOT EXISTS local_agent_source_fanout(
 namespace TEXT NOT NULL,batch_id TEXT NOT NULL,after_index INTEGER NOT NULL,after_id TEXT NOT NULL,
 PRIMARY KEY(namespace,batch_id));
"#,
    )?;
    Ok(())
}

fn text<'a>(row: &'a Value, field: &str) -> Result<&'a str> {
    let value = row[field]
        .as_str()
        .with_context(|| format!("invalid physical {field}"))?;
    ensure!(value.len() <= MAX_BYTES, "oversized physical text");
    Ok(value)
}
fn integer(row: &Value, field: &str) -> Result<u64> {
    let value = row[field]
        .as_u64()
        .with_context(|| format!("invalid physical {field}"))?;
    ensure!(
        value <= i64::MAX as u64,
        "physical integer exceeds SQLite range"
    );
    Ok(value)
}
fn enqueue(tx: &Transaction<'_>, ns: &str, id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 1024,
        "invalid source claim identity"
    );
    tx.execute(
        "INSERT INTO local_agent_source_dirty VALUES(?1,?2) ON CONFLICT(namespace,id) DO NOTHING",
        params![ns, id],
    )?;
    Ok(())
}
fn fanout(tx: &Transaction<'_>, ns: &str, batch: &str, after: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO local_agent_source_fanout VALUES(?1,?2,?3,'')
        ON CONFLICT(namespace,batch_id) DO UPDATE SET
        after_index=min(after_index,excluded.after_index),after_id=''",
        params![ns, batch, after],
    )?;
    Ok(())
}
fn atom(connection: &Connection, ns: &str, table: &str, key: &str) -> Result<Option<Value>> {
    let raw:Option<String>=connection.query_row("SELECT body FROM local_agent_source_atoms WHERE namespace=?1 AND source_table=?2 AND source_key=?3",params![ns,table,key],|r|r.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
/// Exact physical key is the complete `[table,[PK...]]` capture key.
pub fn raw(
    connection: &Connection,
    ns: &Namespace,
    table: &str,
    key: &str,
) -> Result<Option<Value>> {
    ensure!(
        super::TABLES.iter().any(|t| t.name == table),
        "unknown physical source table"
    );
    atom(connection, ns.as_str(), table, key)
}

pub fn apply(tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<()> {
    apply_in(tx, ns.as_str(), rows)
}
fn apply_in(tx: &Transaction<'_>, ns: &str, rows: &[Mutation]) -> Result<()> {
    ensure!(
        rows.len() <= MAX_ROWS,
        "physical namespace page exceeds row budget"
    );
    let mut bytes = 0usize;
    for mutation in rows {
        let key: Value = serde_json::from_str(&mutation.key)?;
        let table_name = key[0]
            .as_str()
            .context("invalid physical source key table")?;
        let table = super::TABLES
            .iter()
            .find(|t| t.name == table_name)
            .context("unknown physical source table")?;
        let keys = key[1]
            .as_array()
            .context("invalid physical source key tuple")?;
        ensure!(
            key.as_array().is_some_and(|v| v.len() == 2) && keys.len() == table.key.len(),
            "invalid physical source key arity"
        );
        for row in mutation.old.iter().chain(mutation.new.iter()) {
            let object = row.as_object().context("invalid physical source row")?;
            ensure!(
                object.len() == table.columns.len()
                    && table.columns.iter().all(|c| object.contains_key(*c)),
                "physical source column mismatch"
            );
            ensure!(
                table.key.iter().zip(keys).all(|(k, v)| row[*k] == *v),
                "physical replacement key mismatch"
            );
            let size = serde_json::to_vec(row)?.len();
            ensure!(
                size <= MAX_BYTES,
                "physical namespace row exceeds byte budget"
            );
            bytes = bytes
                .checked_add(size)
                .context("physical namespace page size overflow")?;
        }
        ensure!(
            bytes <= 1024 * 1024,
            "physical namespace page exceeds byte budget"
        );
        // A raced scan can have installed a newer value. Retract its actual namespace keys,
        // rather than treating journal.old as the namespace's current input.
        let prior = atom(tx, ns, table_name, &mutation.key)?;
        if table_name == "claims" {
            for old in prior.iter().chain(mutation.old.iter()) {
                enqueue(tx, ns, text(old, "id")?)?;
                fanout(
                    tx,
                    ns,
                    text(old, "batch_id")?,
                    integer(old, "store_index")? as i64,
                )?;
            }
            if let Some(old) = &prior {
                tx.execute(
                    "DELETE FROM local_agent_source_claims WHERE namespace=?1 AND store_index=?2",
                    params![ns, integer(old, "store_index")?],
                )?;
            }
            if let Some(new) = &mutation.new {
                let id = text(new, "id")?;
                let index = integer(new, "store_index")?;
                // SQLite source uniqueness corrections can displace a different ID/key.
                let displaced:Vec<(String,String,u64)>=tx.prepare_cached("SELECT id,batch_id,store_index FROM local_agent_source_claims WHERE namespace=?1 AND (id=?2 OR store_index=?3) LIMIT 2")?
                    .query_map(params![ns,id,index],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
                for (id, batch, index) in displaced {
                    enqueue(tx, ns, &id)?;
                    fanout(tx, ns, &batch, index as i64)?;
                }
                tx.execute("DELETE FROM local_agent_source_claims WHERE namespace=?1 AND (id=?2 OR store_index=?3)",params![ns,id,index])?;
                tx.execute(
                    "INSERT INTO local_agent_source_claims VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        ns,
                        id,
                        index,
                        text(new, "batch_id")?,
                        text(new, "subject")?,
                        serde_json::to_string(new)?
                    ],
                )?;
                enqueue(tx, ns, id)?;
                fanout(tx, ns, text(new, "batch_id")?, index as i64)?;
            }
        } else if table_name == "batches" {
            for row in prior
                .iter()
                .chain(mutation.old.iter())
                .chain(mutation.new.iter())
            {
                fanout(tx, ns, text(row, "id")?, -1)?;
            }
        } else if table_name == "replica_records" {
            for row in prior
                .iter()
                .chain(mutation.old.iter())
                .chain(mutation.new.iter())
            {
                if let Some(id) = row["claim_id"].as_str() {
                    enqueue(tx, ns, id)?;
                }
            }
            tx.execute(
                "DELETE FROM local_agent_source_records WHERE namespace=?1 AND record_ref=?2",
                params![ns, keys[0].as_str().context("invalid record key")?],
            )?;
            if let Some(new) = &mutation.new {
                ensure!(
                    new["claim_id"].is_null() || new["claim_id"].is_string(),
                    "invalid record claim identity"
                );
                tx.execute(
                    "INSERT INTO local_agent_source_records VALUES(?1,?2,?3,?4,?5)",
                    params![
                        ns,
                        text(new, "record_ref")?,
                        new["claim_id"].as_str(),
                        integer(new, "position")?,
                        text(new, "state")?
                    ],
                )?;
            }
        } else if table_name == "checkpoint_claims" {
            // Includes foreign parents and known removals; the authority owner retains OLD.
            for row in prior
                .iter()
                .chain(mutation.old.iter())
                .chain(mutation.new.iter())
            {
                enqueue(tx, ns, text(row, "id")?)?;
            }
        }
        if let Some(new) = &mutation.new {
            tx.execute("INSERT INTO local_agent_source_atoms VALUES(?1,?2,?3,?4) ON CONFLICT(namespace,source_table,source_key) DO UPDATE SET body=excluded.body",params![ns,table_name,mutation.key,serde_json::to_string(new)?])?;
        } else {
            tx.execute("DELETE FROM local_agent_source_atoms WHERE namespace=?1 AND source_table=?2 AND source_key=?3",params![ns,table_name,mutation.key])?;
        }
    }
    Ok(())
}

/// Work used includes empty fanout jobs. The caller deducts it from its shared budget.
pub fn expand_page(tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<usize> {
    expand_in(tx, ns.as_str(), limit)
}
fn expand_in(tx: &Transaction<'_>, ns: &str, limit: usize) -> Result<usize> {
    ensure!(
        (1..=MAX_ROWS).contains(&limit),
        "invalid canonical fanout budget"
    );
    let mut used = 0;
    while used < limit {
        let job:Option<(String,i64,String)>=tx.query_row("SELECT batch_id,after_index,after_id FROM local_agent_source_fanout WHERE namespace=?1 ORDER BY batch_id LIMIT 1",[ns],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((batch, index, id)) = job else { break };
        let rows:Vec<(u64,String)>=tx.prepare_cached("SELECT store_index,id FROM local_agent_source_claims WHERE namespace=?1 AND batch_id=?2 AND (store_index,id)>(?3,?4) ORDER BY store_index,id LIMIT ?5")?
            .query_map(params![ns,batch,index,id,limit-used],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (_, id) in &rows {
            enqueue(tx, ns, id)?;
        }
        used += rows.len().max(1);
        if let Some((index, id)) = rows.last() {
            tx.execute("UPDATE local_agent_source_fanout SET after_index=?3,after_id=?4 WHERE namespace=?1 AND batch_id=?2",params![ns,batch,index,id])?;
        } else {
            tx.execute(
                "DELETE FROM local_agent_source_fanout WHERE namespace=?1 AND batch_id=?2",
                params![ns, batch],
            )?;
        }
    }
    Ok(used)
}

pub fn dirty_page(connection: &Connection, ns: &Namespace, limit: usize) -> Result<Vec<String>> {
    ensure!(
        (1..=MAX_ROWS).contains(&limit),
        "invalid canonical dirty page budget"
    );
    Ok(connection
        .prepare_cached(
            "SELECT id FROM local_agent_source_dirty WHERE namespace=?1 ORDER BY id LIMIT ?2",
        )?
        .query_map(params![ns.as_str(), limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
/// Pending IDs consume budget and advance this seek cursor; wrap explicitly after the last page.
/// Unacknowledged IDs remain indexed and keep coverage incomplete throughout retries.
pub fn dirty_page_after(
    connection: &Connection,
    ns: &Namespace,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<String>, bool)> {
    ensure!(
        (1..=MAX_ROWS).contains(&limit),
        "invalid canonical dirty seek budget"
    );
    ensure!(
        after.is_none_or(|id| !id.is_empty() && id.len() <= 1024),
        "invalid canonical dirty continuation"
    );
    let mut ids:Vec<String>=connection.prepare_cached("SELECT id FROM local_agent_source_dirty WHERE namespace=?1 AND id>?2 ORDER BY id LIMIT ?3")?
        .query_map(params![ns.as_str(),after.unwrap_or(""),limit+1],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let more = ids.len() > limit;
    ids.truncate(limit);
    Ok((ids, more))
}

pub fn ack(tx: &Transaction<'_>, ns: &Namespace, ids: &[String]) -> Result<()> {
    ensure!(
        ids.len() <= MAX_ROWS,
        "invalid canonical acknowledgement budget"
    );
    for id in ids {
        tx.execute(
            "DELETE FROM local_agent_source_dirty WHERE namespace=?1 AND id=?2",
            params![ns.as_str(), id],
        )?;
    }
    Ok(())
}
pub fn clean(connection: &Connection, ns: &Namespace) -> Result<bool> {
    Ok(!connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_source_dirty WHERE namespace=?1) OR EXISTS(SELECT 1 FROM local_agent_source_fanout WHERE namespace=?1)",[ns.as_str()],|r|r.get::<_,bool>(0))?)
}

/// Current captured parent identity, including checkpoint-only foreign subjects.
/// `None` is known absence ONLY with the caller's independently proved complete source cut.
pub fn parent_identity(
    connection: &Connection,
    ns: &Namespace,
    id: &str,
) -> Result<Option<String>> {
    let subject: Option<String> = connection
        .query_row(
            "SELECT subject FROM local_agent_source_claims WHERE namespace=?1 AND id=?2",
            params![ns.as_str(), id],
            |r| r.get(0),
        )
        .optional()?;
    if subject.is_some() {
        return Ok(subject);
    }
    let key = serde_json::to_string(&json!(["checkpoint_claims", [id]]))?;
    atom(connection, ns.as_str(), "checkpoint_claims", &key)?
        .map(|row| Ok(text(&row, "subject")?.to_owned()))
        .transpose()
}

pub fn new_fact_in_namespace(
    connection: &Connection,
    ns: &Namespace,
    id: &str,
) -> Result<Option<Fact>> {
    let phase: Option<String> = connection
        .query_row(
            "SELECT phase FROM ivm_install_jobs WHERE id=?1",
            [ns.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if phase.as_deref().is_none_or(|phase| phase == "scan") {
        return Err(Pending::CapturePrefix.into());
    }
    ensure!(
        phase
            .as_deref()
            .is_some_and(|phase| matches!(phase, "catchup" | "published")),
        "canonical namespace job is fenced"
    );
    fact_in(connection, ns.as_str(), id)
}

fn fact_in(connection: &Connection, ns: &str, id: &str) -> Result<Option<Fact>> {
    ensure!(
        !id.is_empty() && id.len() <= 1024,
        "invalid canonical claim ID"
    );
    let raw: Option<String> = connection
        .query_row(
            "SELECT body FROM local_agent_source_claims WHERE namespace=?1 AND id=?2",
            params![ns, id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    ensure!(raw.len() <= MAX_BYTES, "oversized canonical shadow row");
    let physical: Value = serde_json::from_str(&raw)?;
    // Parse original SQL TEXT once, including retained numeric/operation identity.
    let claim = super::canonical::decode_sql(&physical)?;
    let batch_key = serde_json::to_string(&json!(["batches", [claim.batch_id]]))?;
    let batch = atom(connection, ns, "batches", &batch_key)?
        .ok_or_else(|| Pending::Batch(claim.batch_id.clone()))?;
    let records:Vec<(u64,String)>=connection.prepare_cached("SELECT position,state FROM local_agent_source_records WHERE namespace=?1 AND claim_id=?2 ORDER BY position,record_ref LIMIT 129")?
        .query_map(params![ns,id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    ensure!(
        records.len() <= 128,
        "canonical record closure exceeds budget"
    );
    let position = if let Some((position, _)) = records.first() {
        *position
    } else {
        let prefix:Vec<u64>=connection.prepare_cached("SELECT store_index FROM local_agent_source_claims WHERE namespace=?1 AND batch_id=?2 AND store_index<?3 ORDER BY store_index LIMIT 1025")?
            .query_map(params![ns,claim.batch_id,claim.store_index],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ensure!(
            prefix.len() <= 1024,
            "legacy canonical prefix exceeds budget"
        );
        prefix.len() as u64
    };
    let repaired = records.iter().any(|(_, state)| state == "repaired");
    let key = canonical::key_from_record(
        &claim,
        text(&batch, "origin")?.to_owned(),
        integer(&batch, "replica_sequence")?,
        position,
    );
    Ok(Some(Fact {
        claim,
        key,
        repaired,
        physical,
    }))
}

/// Shares `limit` across all shadow tables; never receives a published namespace.
pub fn reclaim(tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<(usize, bool)> {
    ensure!(
        (1..=MAX_ROWS).contains(&limit),
        "invalid shadow reclaim budget"
    );
    let mut used = 0;
    for table in TABLE_NAMES {
        if used == limit {
            break;
        }
        used+=tx.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE namespace=?1 LIMIT ?2)"),params![ns.as_str(),limit-used])?;
    }
    let mut empty = true;
    for table in TABLE_NAMES {
        empty &= !tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE namespace=?1)"),
            [ns.as_str()],
            |r| r.get::<_, bool>(0),
        )?;
    }
    Ok((used, empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn replacement(table: &str, row: Value) -> Mutation {
        let descriptor = super::super::TABLES
            .iter()
            .find(|t| t.name == table)
            .unwrap();
        Mutation {
            key: serde_json::to_string(&json!([
                table,
                descriptor
                    .key
                    .iter()
                    .map(|k| row[*k].clone())
                    .collect::<Vec<_>>()
            ]))
            .unwrap(),
            old: None,
            new: Some(row),
        }
    }
    fn claim(index: u64, id: &str) -> Mutation {
        replacement(
            "claims",
            json!({"store_index":index,"id":id,"batch_id":"batch","subject":"agent/shadow","kind":"usage.recorded","origin":"grove","actor":null,"body":"{\"fields\":{\"cost\":0.12345678901234568}}","predecessors":"[]","accepted_at_unix_ms":"100"}),
        )
    }
    fn batch(sequence: u64) -> Mutation {
        replacement(
            "batches",
            json!({"id":"batch","origin":"grove","replica_sequence":sequence,"previous_hash":null,"hash":"hash","accepted_at_unix_ms":"100"}),
        )
    }
    fn record(id: &str, position: u64, state: &str) -> Mutation {
        replacement(
            "replica_records",
            json!({"record_ref":"record","writer":"grove","sequence":1,"envelope_hash":"hash","position":position,"raw":{"$blob":""},"state":state,"claim_id":id,"subject_hint":null,"kind_hint":null,"error_code":null,"error_message":null,"replacement_claim_id":null,"updated_at_unix_ms":"100"}),
        )
    }
    #[test]
    fn namespace_shadow_isolated_canonical_metadata_replacements_and_float_text() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        apply_in(
            &tx,
            "one",
            &[batch(1), claim(2, "first"), claim(3, "second")],
        )
        .unwrap();
        apply_in(
            &tx,
            "two",
            &[batch(9), claim(2, "first"), record("first", 8, "repaired")],
        )
        .unwrap();
        let before = fact_in(&tx, "one", "first").unwrap().unwrap();
        assert_eq!(before.key.2, 1);
        assert_eq!(before.key.4, 0);
        assert!(!before.repaired);
        let other = fact_in(&tx, "two", "first").unwrap().unwrap();
        assert_eq!(other.key.2, 9);
        assert_eq!(other.key.4, 8);
        assert!(other.repaired);
        assert_eq!(before.claim.body["fields"]["cost"].as_f64().unwrap().to_bits(),serde_json::from_str::<Value>(before.physical["body"].as_str().unwrap()).unwrap()["fields"]["cost"].as_f64().unwrap().to_bits());
        // No live claims/batches tables exist at all: canonical reads remain namespace only.
        apply_in(&tx, "one", &[record("first", 7, "repaired")]).unwrap();
        assert_eq!(fact_in(&tx, "one", "first").unwrap().unwrap().key.4, 7);
        assert_eq!(fact_in(&tx, "two", "first").unwrap().unwrap().key.4, 8);
        tx.rollback().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM local_agent_source_atoms", [], |r| r
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
    }
    #[test]
    fn namespace_shadow_fanout_bounded_and_reassignments_retract_actual_scan_value() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        apply_in(&tx, "one", &[batch(1)]).unwrap();
        for index in 1..=20 {
            apply_in(&tx, "one", &[claim(index, &format!("claim-{index:02}"))]).unwrap();
        }
        tx.execute(
            "DELETE FROM local_agent_source_dirty WHERE namespace='one'",
            [],
        )
        .unwrap();
        apply_in(&tx, "one", &[batch(2)]).unwrap();
        assert_eq!(expand_in(&tx, "one", 3).unwrap(), 3);
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM local_agent_source_dirty WHERE namespace='one'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            3
        );
        let mut raced = claim(1, "replacement");
        raced.old = claim(1, "stale-old").new;
        apply_in(&tx, "one", &[raced]).unwrap();
        assert!(fact_in(&tx, "one", "claim-01").unwrap().is_none());
        assert!(fact_in(&tx, "one", "replacement").unwrap().is_some());
        let dirty: Vec<String> = tx
            .prepare("SELECT id FROM local_agent_source_dirty WHERE namespace='one' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            dirty.contains(&"claim-01".into())
                && dirty.contains(&"stale-old".into())
                && dirty.contains(&"replacement".into())
        );
        while expand_in(&tx, "one", 3).unwrap() > 0 {}
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM local_agent_source_fanout WHERE namespace='one'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(fact_in(&tx, "one", "claim-20").unwrap().unwrap().key.2, 2);
    }
    #[test]
    fn namespace_shadow_repair_removal_legacy_shift_and_unknown_batch_refusal() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        apply_in(&tx, "one", &[claim(1, "first"), claim(2, "second")]).unwrap();
        assert!(fact_in(&tx, "one", "second").is_err());
        apply_in(&tx, "one", &[batch(1), record("second", 99, "repaired")]).unwrap();
        assert!(fact_in(&tx, "one", "second").unwrap().unwrap().repaired);
        let mut delete = record("second", 99, "repaired");
        delete.old = delete.new.take();
        apply_in(&tx, "one", &[delete]).unwrap();
        assert_eq!(fact_in(&tx, "one", "second").unwrap().unwrap().key.4, 1);
        let mut delete = claim(1, "first");
        delete.old = delete.new.take();
        apply_in(&tx, "one", &[delete]).unwrap();
        assert_eq!(fact_in(&tx, "one", "second").unwrap().unwrap().key.4, 0);
        let mut invalid = claim(3, "invalid");
        invalid.new.as_mut().unwrap()["body"] = json!("invalid JSON");
        apply_in(&tx, "one", &[invalid]).unwrap();
        assert!(fact_in(&tx, "one", "invalid").is_err());
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    #[test]
    fn namespace_fact_keeps_real_store_cut_until_same_index_metadata_replacement_is_applied() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        let claim = store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/namespace-native".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([
                    ("runtime_id".into(), json!("namespace-native")),
                    ("status".into(), json!("running")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store.connection.batched(|tx|tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms) SELECT 'namespace-record',origin,replica_sequence,'envelope',0,X'','valid',?1,'1' FROM batches WHERE id=?2",params![claim.id,claim.batch_id])).unwrap().unwrap();
        let physical = |connection: &Connection, table: &str, column: &str, id: &str| -> Value {
            let descriptor = super::super::TABLES
                .iter()
                .find(|t| t.name == table)
                .unwrap();
            let encoded: String = connection
                .query_row(
                    &format!(
                        "SELECT {} FROM {table} AS source WHERE source.{column}=?1",
                        super::super::super::row(descriptor, "source").unwrap()
                    ),
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&encoded).unwrap()
        };
        let replacement = |table: &str, row: Value| {
            let descriptor = super::super::TABLES
                .iter()
                .find(|t| t.name == table)
                .unwrap();
            Mutation {
                key: serde_json::to_string(&json!([
                    table,
                    descriptor
                        .key
                        .iter()
                        .map(|key| row[*key].clone())
                        .collect::<Vec<_>>()
                ]))
                .unwrap(),
                old: None,
                new: Some(row),
            }
        };
        let rows = {
            let connection = store.readers.get();
            vec![
                replacement(
                    "batches",
                    physical(&connection, "batches", "id", &claim.batch_id),
                ),
                replacement("claims", physical(&connection, "claims", "id", &claim.id)),
                replacement(
                    "replica_records",
                    physical(
                        &connection,
                        "replica_records",
                        "record_ref",
                        "namespace-record",
                    ),
                ),
            ]
        };
        let mut shadow = Connection::open_in_memory().unwrap();
        create_schema(&shadow).unwrap();
        let tx = shadow.transaction().unwrap();
        apply_in(&tx, "snapshot", &rows).unwrap();
        let retained = fact_in(&tx, "snapshot", &claim.id)
            .unwrap()
            .unwrap()
            .encoded();
        assert_eq!(
            fact_in(&tx, "snapshot", &claim.id).unwrap().unwrap().key,
            canonical::claim_key(&store.readers.get(), &claim.id).unwrap()
        );
        let index = store.index().unwrap();
        store.connection.batched(|tx|tx.execute("UPDATE replica_records SET position=9,state='repaired' WHERE record_ref='namespace-record'",[])).unwrap().unwrap();
        assert_eq!(store.index().unwrap(), index);
        assert_eq!(
            fact_in(&tx, "snapshot", &claim.id)
                .unwrap()
                .unwrap()
                .encoded(),
            retained
        );
        let changed = {
            let connection = store.readers.get();
            replacement(
                "replica_records",
                physical(
                    &connection,
                    "replica_records",
                    "record_ref",
                    "namespace-record",
                ),
            )
        };
        apply_in(&tx, "snapshot", &[changed]).unwrap();
        let current = fact_in(&tx, "snapshot", &claim.id).unwrap().unwrap();
        assert_eq!(
            current.key,
            canonical::claim_key(&store.readers.get(), &claim.id).unwrap()
        );
        assert!(current.repaired);
    }
}
