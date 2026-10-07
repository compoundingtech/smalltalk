//! Bounded NEW canonical facts inside the caller's source snapshot.
//! OLD canonical/dependency closure belongs to the retained namespace fact. Calling this
//! helper against current SQL to reconstruct OLD input is invalid. No source cut is certified.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimRecord,
    store::{canonical, claim_from_row},
};

const MAX_RECORDS: usize = 128;
const MAX_LEGACY_PREFIX: usize = 1024;
const MAX_BYTES: u64 = 64 * 1024;

pub struct Fact {
    pub claim: ClaimRecord,
    pub key: canonical::ClaimKey,
    /// Family owners choose whether originals survive accepted repair (harness does).
    pub repaired: bool,
    pub physical: Value,
}

impl Fact {
    /// Retained OLD namespace fact; no SQL lookup and original body text parsed exactly once.
    pub fn decode(encoded: &Value) -> Result<Self> {
        let physical = encoded
            .get("sql")
            .context("retained fact is missing physical SQL")?
            .clone();
        let claim = decode_sql(&physical)?;
        let metadata = encoded
            .get("canonical")
            .context("retained fact is missing canonical metadata")?;
        let accepted: u128 = metadata["accepted_at_unix_ms"]
            .as_str()
            .context("invalid retained canonical time")?
            .parse()?;
        let writer = metadata["writer"]
            .as_str()
            .context("invalid retained canonical writer")?
            .to_owned();
        ensure!(
            !writer.is_empty() && writer.len() <= 1024,
            "unbounded retained canonical writer"
        );
        let sequence = metadata["sequence"]
            .as_u64()
            .context("invalid retained canonical sequence")?;
        let position = metadata["position"]
            .as_u64()
            .context("invalid retained canonical position")?;
        ensure!(
            metadata["batch_id"].as_str() == Some(claim.batch_id.as_str())
                && metadata["claim_id"].as_str() == Some(claim.id.as_str())
                && accepted == claim.accepted_at_unix_ms,
            "retained canonical identity mismatch"
        );
        let key = canonical::key_from_record(&claim, writer, sequence, position);
        let rank: Vec<u8> = serde_json::from_value(encoded["rank"].clone())?;
        ensure!(
            rank == canonical::sortable_key(&key),
            "retained canonical rank mismatch"
        );
        let repaired = encoded["repaired"]
            .as_bool()
            .context("invalid retained repair evidence")?;
        Ok(Self {
            claim,
            key,
            repaired,
            physical,
        })
    }
    pub fn encoded(&self) -> Value {
        json!({"sql":self.physical,"canonical":{
            "accepted_at_unix_ms":self.key.0.to_string(),"writer":self.key.1,
            "sequence":self.key.2,"batch_id":self.key.3,"position":self.key.4,"claim_id":self.key.5},
            "rank":canonical::sortable_key(&self.key),"repaired":self.repaired})
    }
}

/// Strict SQL-cell decoder shared by retained facts and namespace NEW input.
pub(super) fn decode_sql(physical: &Value) -> Result<ClaimRecord> {
    let columns = super::TABLES[0].columns;
    ensure!(
        physical
            .as_object()
            .is_some_and(|row| row.len() == columns.len()
                && columns.iter().all(|key| row.contains_key(*key))),
        "retained claim SQL column mismatch"
    );
    ensure!(
        serde_json::to_vec(physical)?.len() <= MAX_BYTES as usize,
        "oversized retained physical claim"
    );
    let text = |field: &str| -> Result<&str> {
        physical[field]
            .as_str()
            .with_context(|| format!("invalid claim {field} cell"))
    };
    let id = text("id")?.to_owned();
    ensure!(
        !id.is_empty() && id.len() <= 1024,
        "invalid retained claim ID"
    );
    let store_index = physical["store_index"]
        .as_u64()
        .context("invalid claim source index")?;
    ensure!(
        store_index <= i64::MAX as u64,
        "claim source index exceeds SQLite range"
    );
    ensure!(
        physical["actor"].is_null() || physical["actor"].is_string(),
        "invalid retained claim actor"
    );
    let body: Value = serde_json::from_str(text("body")?)?;
    let operation = smallclaims::store::operation_parts(&body)
        .map(|(id, digest)| (id.to_owned(), digest.to_owned()));
    Ok(ClaimRecord {
        id,
        store_index,
        batch_id: text("batch_id")?.into(),
        subject: text("subject")?.into(),
        kind: text("kind")?.into(),
        origin: text("origin")?.into(),
        actor: physical["actor"].as_str().map(str::to_owned),
        operation_id: operation.as_ref().map(|value| value.0.clone()),
        request_digest: operation.map(|value| value.1),
        body,
        predecessors: serde_json::from_str(text("predecessors")?)?,
        accepted_at_unix_ms: text("accepted_at_unix_ms")?.parse()?,
    })
}

pub fn new_fact(connection: &Connection, id: &str) -> Result<Option<Fact>> {
    ensure!(
        !id.is_empty() && id.len() <= 1024,
        "invalid canonical claim ID"
    );
    let size: Option<u64> = connection.query_row("SELECT length(CAST(id AS BLOB))+length(CAST(batch_id AS BLOB))+
        length(CAST(subject AS BLOB))+length(CAST(kind AS BLOB))+length(CAST(origin AS BLOB))+
        COALESCE(length(CAST(actor AS BLOB)),0)+length(CAST(body AS BLOB))+length(CAST(predecessors AS BLOB))
        FROM claims WHERE id=?1",[id],|r|r.get(0)).optional()?;
    let Some(size) = size else { return Ok(None) };
    ensure!(
        size <= MAX_BYTES,
        "canonical source claim exceeds byte budget"
    );
    let claim = connection.query_row("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
        FROM claims WHERE id=?1",[id],claim_from_row)?;
    let table = &super::TABLES[0];
    ensure!(table.name == "claims", "claims source descriptor changed");
    let physical: String = connection.query_row(
        &format!(
            "SELECT {} FROM claims AS source WHERE source.id=?1",
            super::super::row(table, "source")?
        ),
        [id],
        |r| r.get(0),
    )?;
    ensure!(
        physical.len() <= MAX_BYTES as usize,
        "encoded canonical source exceeds byte budget"
    );
    let physical: Value = serde_json::from_str(&physical)?;
    // Avoid claim_from_row's historical malformed-JSON/default-time recovery in a certificate.
    let body: Value = serde_json::from_str(
        physical["body"]
            .as_str()
            .context("invalid claim body cell")?,
    )?;
    let predecessors: Vec<String> = serde_json::from_str(
        physical["predecessors"]
            .as_str()
            .context("invalid predecessor cell")?,
    )?;
    let accepted: u128 = physical["accepted_at_unix_ms"]
        .as_str()
        .context("invalid acceptance time cell")?
        .parse()?;
    ensure!(
        body == claim.body
            && predecessors == claim.predecessors
            && accepted == claim.accepted_at_unix_ms,
        "canonical claim recovery is not certified input"
    );
    let (writer, sequence): (String, u64) = connection.query_row(
        "SELECT origin,replica_sequence FROM batches WHERE id=?1",
        [&claim.batch_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let records: Vec<(u64, String)> = connection
        .prepare_cached(
            "SELECT position,state FROM replica_records
        INDEXED BY replica_records_claim WHERE claim_id=?1 ORDER BY position LIMIT ?2",
        )?
        .query_map(params![id, MAX_RECORDS + 1], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        records.len() <= MAX_RECORDS,
        "canonical alias/repair closure exceeds record budget"
    );
    let position = if let Some((position, _)) = records.first() {
        *position
    } else {
        // The legacy order is the number of earlier surviving claims in this batch. Enumerate
        // only a finite indexed prefix; larger legacy batches require a bounded prefix operator.
        let prefix: Vec<u64> = connection
            .prepare_cached(
                "SELECT store_index FROM claims INDEXED BY claims_batch_index
            WHERE batch_id=?1 AND store_index<?2 ORDER BY store_index LIMIT ?3",
            )?
            .query_map(
                params![claim.batch_id, claim.store_index, MAX_LEGACY_PREFIX + 1],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        ensure!(
            prefix.len() <= MAX_LEGACY_PREFIX,
            "legacy canonical position exceeds indexed prefix budget"
        );
        prefix.len() as u64
    };
    let repaired = records.iter().any(|(_, state)| state == "repaired");
    let key = canonical::key_from_record(&claim, writer, sequence, position);
    Ok(Some(Fact {
        claim,
        key,
        repaired,
        physical,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivm_new_canonical_facts_match_native_and_same_index_record_corrections() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        let claim = store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/canonical-source".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([
                    ("runtime_id".into(), json!("canonical-source")),
                    ("status".into(), json!("running")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store.connection.batched(|tx| tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
            SELECT 'fixture-canonical-ref',origin,replica_sequence,'fixture-envelope',0,X'','valid',?1,'1' FROM batches WHERE id=?2",
            params![claim.id,claim.batch_id])).unwrap().unwrap();
        let old = {
            let connection = store.readers.get();
            let fact = new_fact(&connection, &claim.id).unwrap().unwrap();
            assert_eq!(
                fact.key,
                canonical::claim_key(&connection, &claim.id).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&fact.claim).unwrap(),
                serde_json::to_value(&claim).unwrap()
            );
            fact.encoded()
        };
        let index = store.index().unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE replica_records SET position=position+7 WHERE claim_id=?1",
                    [&claim.id],
                )
            })
            .unwrap()
            .unwrap();
        let connection = store.readers.get();
        let changed = new_fact(&connection, &claim.id).unwrap().unwrap();
        assert_eq!(
            changed.key,
            canonical::claim_key(&connection, &claim.id).unwrap()
        );
        assert_eq!(
            changed.key.4,
            old["canonical"]["position"].as_u64().unwrap() + 7
        );
        assert_ne!(changed.encoded(), old);
        assert_eq!(store.index().unwrap(), index);
        assert!(new_fact(&connection, "absent-claim").unwrap().is_none());
    }

    #[test]
    fn ivm_new_legacy_canonical_facts_are_bounded_and_keep_repair_policy_separate() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        let claim = store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/canonical-legacy".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([
                    ("runtime_id".into(), json!("canonical-legacy")),
                    ("status".into(), json!("running")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                    SELECT 'fixture-repaired-ref',origin,replica_sequence,'fixture-envelope',0,X'','repaired',?1,'1' FROM batches WHERE id=?2",
                    params![claim.id,claim.batch_id])
            })
            .unwrap()
            .unwrap();
        {
            let connection = store.readers.get();
            assert!(new_fact(&connection, &claim.id).unwrap().unwrap().repaired);
        }
        store
            .connection
            .batched(|tx| tx.execute("DELETE FROM replica_records WHERE claim_id=?1", [&claim.id]))
            .unwrap()
            .unwrap();
        let connection = store.readers.get();
        let legacy = new_fact(&connection, &claim.id).unwrap().unwrap();
        assert_eq!(
            legacy.key,
            canonical::claim_key(&connection, &claim.id).unwrap()
        );
        assert!(!legacy.repaired);
        assert_eq!(legacy.key.4, 0);
        // A production callback never calls canonical::claim_key's unbounded legacy COUNT.
        drop(connection);
        store.connection.batched(|tx| {
            tx.execute("UPDATE claims SET store_index=?1 WHERE id=?2",params![MAX_LEGACY_PREFIX+2,claim.id])?;
            for n in 0..=MAX_LEGACY_PREFIX {
                tx.execute("INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
                    VALUES(?1,?2,?3,'unrelated','runtime.observed','alder',NULL,'{}','[]','1')",
                    params![n+1,format!("legacy-prefix-{n}"),claim.batch_id])?;
            }
            Ok::<_,rusqlite::Error>(())
        }).unwrap().unwrap();
        let connection = store.readers.get();
        assert!(new_fact(&connection, &claim.id).is_err());
    }
}

#[cfg(test)]
mod retained_tests {
    use super::*;
    #[test]
    fn retained_fact_decode_preserves_raw_float_and_rejects_rank_identity_and_json_recovery() {
        let physical = json!({"id":"retained","store_index":1,"batch_id":"batch","subject":"agent/retained","kind":"usage.recorded","origin":"grove","actor":null,"body":"{\"fields\":{\"cost\":0.12345678901234568}}","predecessors":"[]","accepted_at_unix_ms":"123"});
        let claim = decode_sql(&physical).unwrap();
        let key = canonical::key_from_record(&claim, "grove".into(), 7, 2);
        let fact = Fact {
            claim,
            key,
            repaired: true,
            physical,
        };
        let encoded = fact.encoded();
        let decoded = Fact::decode(&encoded).unwrap();
        assert_eq!(decoded.key, fact.key);
        assert!(decoded.repaired);
        assert_eq!(
            decoded.claim.body["fields"]["cost"]
                .as_f64()
                .unwrap()
                .to_bits(),
            fact.claim.body["fields"]["cost"]
                .as_f64()
                .unwrap()
                .to_bits()
        );
        let mut invalid = encoded.clone();
        invalid["rank"] = json!([0]);
        assert!(Fact::decode(&invalid).is_err());
        let mut invalid = encoded.clone();
        invalid["canonical"]["claim_id"] = json!("other");
        assert!(Fact::decode(&invalid).is_err());
        let mut invalid = encoded;
        invalid["sql"]["body"] = json!("malformed");
        assert!(Fact::decode(&invalid).is_err());
    }
}
