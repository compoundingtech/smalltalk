//! Rebuildable monotone receipts. A newer sparse/idle observation is never terminal proof.
use super::*;
use smallclaims::store::canonical;
use st_drivers::turn_obligation::{Ledger, Obligation};

// Owned attachment seam; the successor union owner activates its production calls.
#[allow(dead_code)]
pub(crate) mod physical;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_turn_obligation_evidence (
 subject TEXT NOT NULL, receipt TEXT NOT NULL, claim_id TEXT NOT NULL,
 native_key TEXT, terminal INTEGER NOT NULL, canonical_key TEXT NOT NULL,
 visible_index INTEGER NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(subject,receipt,claim_id,terminal)
);
CREATE INDEX IF NOT EXISTS agent_turn_evidence_receipt ON agent_turn_obligation_evidence(subject,receipt,canonical_key DESC);
CREATE INDEX IF NOT EXISTS agent_turn_evidence_native ON agent_turn_obligation_evidence(subject,native_key,terminal,canonical_key);
CREATE INDEX IF NOT EXISTS agent_turn_evidence_claim ON agent_turn_obligation_evidence(claim_id);
CREATE TABLE IF NOT EXISTS agent_turn_obligations (
 subject TEXT NOT NULL, receipt TEXT NOT NULL, native_key TEXT,
 source_claim TEXT NOT NULL, source_key TEXT NOT NULL, body TEXT NOT NULL,
 first_index INTEGER NOT NULL, terminal_index INTEGER, acknowledged_index INTEGER,
 PRIMARY KEY(subject,receipt)
);
CREATE INDEX IF NOT EXISTS agent_turn_obligations_native ON agent_turn_obligations(subject,native_key);
CREATE INDEX IF NOT EXISTS agent_turn_obligations_open ON agent_turn_obligations(subject,first_index,receipt) WHERE terminal_index IS NULL;
CREATE INDEX IF NOT EXISTS agent_turn_obligations_action ON agent_turn_obligations(subject,first_index,receipt) WHERE terminal_index IS NULL AND acknowledged_index IS NULL;
CREATE INDEX IF NOT EXISTS agent_turn_obligations_acknowledged ON agent_turn_obligations(subject,acknowledged_index,receipt) WHERE terminal_index IS NULL AND acknowledged_index IS NOT NULL;
CREATE INDEX IF NOT EXISTS agent_turn_obligations_terminal ON agent_turn_obligations(subject,terminal_index,receipt) WHERE terminal_index IS NOT NULL;
-- Snapshot visibility is local arrival order; these cut versions are not shared digests.
CREATE TABLE IF NOT EXISTS local_turn_obligation_versions (
 subject TEXT NOT NULL, receipt TEXT NOT NULL, visible_index INTEGER NOT NULL,
 source_claim TEXT NOT NULL, body TEXT NOT NULL, terminal INTEGER NOT NULL,
 PRIMARY KEY(subject,receipt,visible_index)
);
CREATE TABLE IF NOT EXISTS local_turn_obligation_pending (claim_id TEXT PRIMARY KEY, subject TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS local_turn_obligation_pending_subject ON local_turn_obligation_pending(subject,claim_id);
CREATE TABLE IF NOT EXISTS local_turn_obligation_dirty (subject TEXT PRIMARY KEY);
CREATE TRIGGER IF NOT EXISTS turn_obligation_insert AFTER INSERT ON claims
WHEN (NEW.kind='harness.observed' AND json_type(NEW.body,'$.fields.turn_obligation')='object') OR NEW.kind='harness.turn-acknowledged' BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_pending VALUES(NEW.id,NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_delete AFTER DELETE ON claims BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT OLD.subject WHERE (OLD.kind='harness.observed' AND json_type(OLD.body,'$.fields.turn_obligation')='object' OR OLD.kind='harness.turn-acknowledged');
 DELETE FROM local_turn_obligation_pending WHERE claim_id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_update AFTER UPDATE ON claims
WHEN OLD.body IS NOT NEW.body OR OLD.subject IS NOT NEW.subject OR OLD.id IS NOT NEW.id
 OR OLD.accepted_at_unix_ms IS NOT NEW.accepted_at_unix_ms OR OLD.batch_id IS NOT NEW.batch_id
 OR OLD.store_index IS NOT NEW.store_index OR OLD.kind IS NOT NEW.kind OR OLD.actor IS NOT NEW.actor BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT OLD.subject WHERE (OLD.kind='harness.observed' AND json_type(OLD.body,'$.fields.turn_obligation')='object' OR OLD.kind='harness.turn-acknowledged');
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT NEW.subject WHERE (NEW.kind='harness.observed' AND json_type(NEW.body,'$.fields.turn_obligation')='object' OR NEW.kind='harness.turn-acknowledged');
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_batch_update AFTER UPDATE OF origin,replica_sequence ON batches BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT DISTINCT subject FROM claims
 WHERE batch_id=NEW.id AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged');
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_record_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT subject FROM claims WHERE id=NEW.claim_id
 AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged') AND NOT EXISTS(SELECT 1 FROM local_turn_obligation_pending WHERE claim_id=NEW.claim_id)
 AND (NEW.state='repaired' OR (SELECT MIN(position) FROM replica_records WHERE claim_id=NEW.claim_id) IS NOT COALESCE(
  (SELECT MIN(position) FROM replica_records WHERE claim_id=NEW.claim_id AND record_ref<>NEW.record_ref),
  (SELECT COUNT(*) FROM claims legacy_position WHERE legacy_position.batch_id=claims.batch_id AND legacy_position.store_index<claims.store_index)));
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_legacy_insert AFTER INSERT ON claims BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT subject FROM claims
 WHERE batch_id=NEW.batch_id AND store_index>NEW.store_index AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged')
 AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id)
 AND NOT EXISTS(SELECT 1 FROM local_turn_obligation_pending WHERE claim_id=claims.id);
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_record_delete AFTER DELETE ON replica_records BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT subject FROM claims WHERE id=OLD.claim_id AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged');
END;
CREATE TRIGGER IF NOT EXISTS turn_obligation_record_update AFTER UPDATE OF position,claim_id,state ON replica_records BEGIN
 INSERT OR IGNORE INTO local_turn_obligation_dirty SELECT subject FROM claims WHERE id IN(OLD.claim_id,NEW.claim_id) AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged');
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(SCHEMA)
        .context("creating turn receipts")
}
pub(super) fn open(tx: &Transaction<'_>) -> Result<()> {
    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key='turn_obligations_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version.as_deref() == Some("1") {
        flush(tx)
    } else {
        rebuild(tx)
    }
}
pub(super) fn rebuild(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DELETE FROM agent_turn_obligation_evidence; DELETE FROM agent_turn_obligations; DELETE FROM local_turn_obligation_versions;
 DELETE FROM local_turn_obligation_dirty; DELETE FROM local_turn_obligation_pending;
 INSERT INTO local_turn_obligation_pending SELECT id,subject FROM claims WHERE (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged');")?;
    flush(tx)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('turn_obligations_version','1')",
        [],
    )?;
    Ok(())
}
pub(super) fn receipt(entry: &Obligation) -> Result<String> {
    canonical_hash(&json!([
        entry.provider_incarnation,
        entry.ownership_sequence,
        entry.source_sequence,
        entry.runtime_incarnation,
        entry.desired_revision,
        entry.native_session_id,
        entry.native_turn_id,
        entry.started_at_ms
    ]))
}
fn native_key(entry: &Obligation) -> Result<Option<String>> {
    match (
        &entry.desired_revision,
        &entry.native_session_id,
        &entry.native_turn_id,
    ) {
        (Some(revision), Some(session), Some(turn)) => {
            Ok(Some(canonical_hash(&json!([revision, session, turn]))?))
        }
        _ => Ok(None),
    }
}
fn unknown_receipt(ledger: &Ledger) -> Result<String> {
    ledger
        .unknown_evidence
        .as_ref()
        .map(|evidence| canonical_hash(&json!(["unknown", evidence])))
        .transpose()
        .map(|receipt| receipt.unwrap_or_else(|| "unknown".into()))
}

/// Normalize a producer snapshot into changed complete receipts. Absence never closes debt.
/// This writer-side operation is separate from bounded captured-source reads.
pub(super) fn publication_delta(
    connection: &Connection,
    subject: &str,
    raw: &Value,
) -> Result<Option<Value>> {
    let mut ledger: Ledger = serde_json::from_value(raw.clone())?;
    let mut changed_open = Vec::new();
    for entry in ledger.open {
        let key = receipt(&entry)?;
        let previous: Option<String> = connection
            .query_row(
                "SELECT body FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
                params![subject, key],
                |row| row.get(0),
            )
            .optional()?;
        let previous = previous
            .map(|body| serde_json::from_str::<Value>(&body))
            .transpose()?
            .map(|mut body| {
                if let Some(object) = body.as_object_mut() {
                    object.remove("_acknowledgement");
                }
                body
            });
        if previous.as_ref() != Some(&serde_json::to_value(&entry)?) {
            changed_open.push(entry);
        }
    }
    ledger.open = changed_open;
    let mut changed_terminal = Vec::new();
    for terminal in ledger.terminal {
        let key = receipt(&terminal.obligation)?;
        let body = canonical_json_text(&serde_json::to_value(&terminal)?)?;
        let exists = connection.query_row(
            "SELECT 1 FROM agent_turn_obligation_evidence WHERE subject=?1 AND receipt=?2 AND terminal=1 AND body=?3 LIMIT 1",
            params![subject, key, body], |_| Ok(()),
        ).optional()?.is_some();
        if !exists {
            changed_terminal.push(terminal);
        }
    }
    ledger.terminal = changed_terminal;
    let mut changed_results = Vec::new();
    for result in ledger.tool_results {
        let key = format!("tools:{}", receipt(&result.terminal.obligation)?);
        let mut value = serde_json::to_value(&result)?;
        value["tool_receipt"] = json!(key);
        value["unknown_tool_outcome"] = json!(result.terminal.obligation.tool_outcome_unknown);
        let body = canonical_json_text(&value)?;
        let exists = connection.query_row(
            "SELECT 1 FROM agent_turn_obligation_evidence WHERE subject=?1 AND receipt=?2 AND body=?3 LIMIT 1",
            params![subject, key, body], |_| Ok(()),
        ).optional()?.is_some();
        if !exists {
            changed_results.push(result);
        }
    }
    ledger.tool_results = changed_results;
    // Represented native invocation uncertainty is scoped to its original turn receipt.
    // The legacy sentinel is reserved for unrepresented, older producer evidence.
    if raw
        .get("terminal")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["obligation"]["tool_outcome_unknown"] == true)
        })
        || raw
            .get("tool_results")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        || raw
            .get("open")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["tool_outcome_unknown"] == true)
            })
    {
        ledger.unknown_tool_outcome = false;
    }
    let unknown_key = unknown_receipt(&ledger)?;
    for (flag, key) in [
        (&mut ledger.unknown, unknown_key.as_str()),
        (&mut ledger.unknown_tool_outcome, "unknown-tool-outcome"),
    ] {
        if *flag
            && connection
                .query_row(
                    "SELECT 1 FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
                    params![subject, key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
        {
            *flag = false;
        }
    }
    if ledger.open.is_empty()
        && ledger.terminal.is_empty()
        && ledger.tool_results.is_empty()
        && !ledger.unknown
        && !ledger.unknown_tool_outcome
    {
        return Ok(None);
    }
    Ok(Some(serde_json::to_value(ledger)?))
}

/// Tool invocation/result bookkeeping does not itself require a reconcile pass.
pub(crate) fn actionable_receipt_keys(evidence: &Value) -> Vec<String> {
    let mut keys = evidence["selected_receipts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["acknowledgement"].is_null())
        .filter_map(|row| row["receipt"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    keys
}

pub(super) fn wakes_reconciler(
    connection: &Connection,
    subject: &str,
    raw: &Value,
) -> Result<bool> {
    let ledger: Ledger = serde_json::from_value(raw.clone())?;
    if ledger.unknown
        || ledger.unknown_tool_outcome
        || !ledger.terminal.is_empty()
        || ledger
            .tool_results
            .iter()
            .any(|result| !result.terminal.obligation.tool_outcome_unknown)
    {
        return Ok(true);
    }
    for entry in ledger.open {
        let old: Option<String> = connection
            .query_row(
                "SELECT body FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
                params![subject, receipt(&entry)?],
                |row| row.get(0),
            )
            .optional()?;
        let Some(old) = old else {
            return Ok(true);
        };
        let previous: Obligation = serde_json::from_str(&old)?;
        if previous.pending_human != entry.pending_human {
            return Ok(true);
        }
    }
    Ok(false)
}

fn project(tx: &Transaction<'_>, id: &str) -> Result<()> {
    let row: Option<(String, String, u64)> = tx.query_row(
        "SELECT subject,body,store_index FROM claims WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional()?;
    let Some((subject, body, index)) = row else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(&body)?;
    if value.pointer("/fields/receipts").is_some() {
        return project_acknowledgement(tx, id, &subject, index, &value);
    }
    let Some(raw) = value.pointer("/fields/turn_obligation") else {
        return Ok(());
    };
    let key = hex::encode(canonical::sortable_key(&canonical::claim_key(tx, id)?));
    let ledger = serde_json::from_value::<Ledger>(raw.clone())
        .ok()
        .filter(|l| l.open.len() <= 32 && l.terminal.len() <= 64 && l.tool_results.len() <= 64);
    let mut affected = BTreeSet::new();
    let mut put = |entry: Option<&Obligation>, terminal: bool, body: Value| -> Result<()> {
        let receipt = entry.map(receipt).transpose()?.unwrap_or_else(|| {
            if let Some(key) = body["unknown_receipt"]
                .as_str()
                .or_else(|| body["tool_receipt"].as_str())
            {
                key.into()
            } else if body["unknown_tool_outcome"] == true {
                "unknown-tool-outcome".into()
            } else {
                "unknown".into()
            }
        });
        let native = entry.map(native_key).transpose()?.flatten();
        tx.execute(
            "INSERT OR REPLACE INTO agent_turn_obligation_evidence VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                subject,
                receipt,
                id,
                native,
                terminal,
                key,
                index,
                canonical_json_text(&body)?
            ],
        )?;
        affected.insert(receipt);
        if terminal && let Some(native) = native {
            let rows = tx
                .prepare(
                    "SELECT receipt FROM agent_turn_obligations WHERE subject=?1 AND native_key=?2",
                )?
                .query_map(params![subject, native], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            affected.extend(rows);
        }
        Ok(())
    };
    if let Some(ledger) = ledger {
        for entry in &ledger.open {
            put(Some(entry), false, serde_json::to_value(entry)?)?;
        }
        for terminal in &ledger.terminal {
            if matches!(
                terminal.outcome.as_str(),
                "completed" | "cancelled" | "failed"
            ) {
                put(
                    Some(&terminal.obligation),
                    true,
                    serde_json::to_value(terminal)?,
                )?;
            }
        }
        for proof in &ledger.terminal {
            if proof.obligation.tool_outcome_unknown {
                put(
                    None,
                    false,
                    json!({"tool_receipt":format!("tools:{}", receipt(&proof.obligation)?),
                    "original_receipt":receipt(&proof.obligation)?,"unknown_tool_outcome":true,
                    "pending_tool_ids":proof.obligation.pending_tool_ids}),
                )?;
            }
        }
        for result in &ledger.tool_results {
            // This carries a real original turn terminal plus a positive result for its
            // named invocation. It never infers tool success from a clean later turn.
            put(
                Some(&result.terminal.obligation),
                true,
                serde_json::to_value(&result.terminal)?,
            )?;
            let mut body = serde_json::to_value(result)?;
            body["tool_receipt"] =
                json!(format!("tools:{}", receipt(&result.terminal.obligation)?));
            body["unknown_tool_outcome"] = json!(result.terminal.obligation.tool_outcome_unknown);
            put(None, !result.terminal.obligation.tool_outcome_unknown, body)?;
        }
        if ledger.unknown {
            put(
                None,
                false,
                json!({"unknown_receipt":unknown_receipt(&ledger)?,
                "unknown_evidence":ledger.unknown_evidence}),
            )?;
        }
        if ledger.unknown_tool_outcome
            && !ledger
                .terminal
                .iter()
                .any(|proof| proof.obligation.tool_outcome_unknown)
            && !ledger.open.iter().any(|entry| entry.tool_outcome_unknown)
            && ledger.tool_results.is_empty()
        {
            put(None, false, json!({"unknown_tool_outcome":true}))?;
        }
    } else {
        put(None, false, json!(null))?;
    }
    for receipt in affected {
        refresh(tx, &subject, &receipt)?;
        tx.execute(
            "INSERT OR REPLACE INTO local_turn_obligation_versions
            SELECT subject,receipt,?3,source_claim,body,terminal_index IS NOT NULL
            FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
            params![subject, receipt, index],
        )?;
    }
    Ok(())
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct ReceiptEvidence {
    pub receipt: String,
    pub source_claim: String,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub(crate) struct AcknowledgeRequest {
    pub subject: String,
    pub actor: String,
    pub receipts: Vec<ReceiptEvidence>,
    pub source_revision: String,
    pub captured_cut: u64,
    pub reason: String,
    pub idempotency_key: String,
}

pub(super) fn validate_acknowledgement_tx(
    tx: &Transaction<'_>,
    input: &ClaimInput,
) -> Result<(), St3Error> {
    let actor = input.actor.as_deref().unwrap_or_default();
    if actor != input.subject && !actor.starts_with("person/") {
        return Err(St3Error::new(
            "wrong-turn-acknowledgement-actor",
            "only this seat or a person can acknowledge its unknown execution evidence",
        ));
    }
    owned_sets::guard_member(tx, &input.subject)?;
    let current = current_desired_row_tx(tx, &input.subject)
        .map_err(internal)?
        .ok_or_else(|| St3Error::new("missing-agent", "the seat has no selected declaration"))?;
    if input.fields["source_revision"].as_str() != Some(current.claim_id.as_str()) {
        return Err(St3Error::new(
            "stale-turn-acknowledgement",
            "the captured declaration changed",
        ));
    }
    for target in input.fields["receipts"].as_array().into_iter().flatten() {
        let present = tx.query_row(
            "SELECT 1 FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2 AND source_claim=?3 AND first_index<=?4 AND terminal_index IS NULL",
            params![input.subject, target["receipt"].as_str(), target["source_claim"].as_str(), input.fields["captured_cut"].as_u64().unwrap_or(0).min(i64::MAX as u64)], |_| Ok(()),
        ).optional().map_err(internal)?.is_some();
        if !present
            || !input
                .evidence
                .iter()
                .any(|id| Some(id.as_str()) == target["source_claim"].as_str())
        {
            return Err(St3Error::new(
                "stale-turn-acknowledgement",
                "a receipt/evidence pair changed or has settled",
            ));
        }
    }
    Ok(())
}

fn project_acknowledgement(
    tx: &Transaction<'_>,
    id: &str,
    subject: &str,
    index: u64,
    value: &Value,
) -> Result<()> {
    let Some(claim) = claim_by_id_tx(tx, id)? else {
        return Ok(());
    };
    if claim.kind != "harness.turn-acknowledged"
        || claim
            .actor
            .as_deref()
            .is_none_or(|actor| actor != subject && !actor.starts_with("person/"))
    {
        return Ok(());
    }
    let Some(receipts) = value["fields"]["receipts"]
        .as_array()
        .filter(|items| items.len() <= 32)
    else {
        return Ok(());
    };
    let key = hex::encode(canonical::sortable_key(&canonical::claim_key(tx, id)?));
    for target in receipts {
        let Some(receipt) = target["receipt"].as_str() else {
            continue;
        };
        let Some(source) = target["source_claim"].as_str() else {
            continue;
        };
        // Keep the admitted attributed reference independently of source arrival. The
        // head selector below grants it effect only through an admitted cited receipt;
        // removal/repair invalidates that join without inventing native completion.
        let cited = claim.body["evidence"]
            .as_array()
            .is_some_and(|evidence| evidence.iter().any(|id| id == source));
        if !cited {
            continue;
        }
        let audit = json!({"claim":id,"actor":claim.actor,"accepted_at_unix_ms":claim.accepted_at_unix_ms.to_string(),
            "receipt":receipt,"source_claim":source,"source_revision":value["fields"]["source_revision"],
            "captured_cut":value["fields"]["captured_cut"],"reason":value["fields"]["reason"],
            "execution_outcome":"unknown"});
        tx.execute("INSERT OR REPLACE INTO agent_turn_obligation_evidence VALUES(?1,?2,?3,NULL,0,?4,?5,?6)",
            params![subject, format!("ack:{receipt}"), id, key, index, canonical_json_text(&audit)?])?;
        refresh(tx, subject, receipt)?;
        tx.execute("INSERT OR REPLACE INTO local_turn_obligation_versions SELECT subject,receipt,?3,source_claim,body,terminal_index IS NOT NULL FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
            params![subject, receipt, index])?;
    }
    Ok(())
}

fn refresh(tx: &Transaction<'_>, subject: &str, receipt: &str) -> Result<()> {
    let row: Option<(String,String,String,Option<String>,u64)>=tx.query_row(
  "SELECT claim_id,canonical_key,body,native_key,(SELECT MIN(visible_index) FROM agent_turn_obligation_evidence WHERE subject=?1 AND receipt=?2)
   FROM agent_turn_obligation_evidence WHERE subject=?1 AND receipt=?2 AND terminal=0 ORDER BY canonical_key DESC LIMIT 1",
  params![subject,receipt], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((claim, key, body, native, first)) = row else {
        return Ok(());
    };
    let terminal: Option<u64>=tx.query_row(
  "SELECT MIN(first_index) FROM (SELECT MIN(visible_index) AS first_index FROM agent_turn_obligation_evidence
   INDEXED BY agent_turn_evidence_receipt WHERE subject=?1 AND receipt=?2 AND terminal=1
   UNION ALL SELECT MIN(visible_index) FROM agent_turn_obligation_evidence INDEXED BY agent_turn_evidence_native
   WHERE subject=?1 AND native_key=?3 AND terminal=1)",params![subject,receipt,native], |r| r.get(0))?;
    let acknowledgement: Option<(String, u64)> = tx.query_row(
        "SELECT ack.body,ack.visible_index FROM agent_turn_obligation_evidence AS ack
         JOIN agent_turn_obligation_evidence AS original ON original.subject=ack.subject
           AND original.receipt=?3 AND original.claim_id=json_extract(ack.body,'$.source_claim') AND original.terminal=0
         WHERE ack.subject=?1 AND ack.receipt=?2 AND ack.terminal=0 ORDER BY ack.canonical_key DESC LIMIT 1",
        params![subject, format!("ack:{receipt}"), receipt], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let mut value: Value = serde_json::from_str(&body)?;
    if let Some((ack, _)) = &acknowledgement {
        if let Some(object) = value.as_object_mut() {
            object.insert("_acknowledgement".into(), serde_json::from_str(ack)?);
        } else {
            value = json!({"unknown_receipt":receipt,"_acknowledgement":serde_json::from_str::<Value>(ack)?});
        }
    }
    let body = canonical_json_text(&value)?;
    tx.execute("INSERT INTO agent_turn_obligations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
  ON CONFLICT(subject,receipt) DO UPDATE SET native_key=excluded.native_key,source_claim=excluded.source_claim,
  source_key=excluded.source_key,body=excluded.body,first_index=excluded.first_index,terminal_index=excluded.terminal_index,acknowledged_index=excluded.acknowledged_index",
  params![subject,receipt,native,claim,key,body,first,terminal,acknowledgement.map(|(_, index)| index)])?;
    Ok(())
}
pub(super) fn flush(tx: &Transaction<'_>) -> Result<()> {
    let dirty = tx
        .prepare("SELECT subject FROM local_turn_obligation_dirty")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in dirty {
        tx.execute(
            "DELETE FROM agent_turn_obligations WHERE subject=?1",
            [&subject],
        )?;
        tx.execute(
            "DELETE FROM local_turn_obligation_versions WHERE subject=?1",
            [&subject],
        )?;
        tx.execute(
            "DELETE FROM agent_turn_obligation_evidence WHERE subject=?1",
            [&subject],
        )?;
        let mut after = 0_u64;
        loop {
            let rows = tx
                .prepare(
                    "SELECT id,store_index FROM claims WHERE subject=?1 AND store_index>?2 AND (kind='harness.observed' AND json_type(body,'$.fields.turn_obligation')='object' OR kind='harness.turn-acknowledged')
                ORDER BY store_index LIMIT 128",
                )?
                .query_map(params![subject, after], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.is_empty() {
                break;
            }
            for (id, index) in rows {
                project(tx, &id)?;
                tx.execute(
                    "DELETE FROM local_turn_obligation_pending WHERE claim_id=?1",
                    [&id],
                )?;
                after = index;
            }
        }
    }
    loop {
        let ids = tx
            // Keep the receipt queue outermost. Otherwise SQLite walks the entire
            // claim log in arrival order even when there is no pending receipt.
            .prepare("SELECT claim_id FROM local_turn_obligation_pending CROSS JOIN claims ON claims.id=claim_id ORDER BY claims.store_index LIMIT 128")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            project(tx, &id)?;
            tx.execute(
                "DELETE FROM local_turn_obligation_pending WHERE claim_id=?1",
                [id],
            )?;
        }
    }
    tx.execute_batch(
        "DELETE FROM local_turn_obligation_pending; DELETE FROM local_turn_obligation_dirty;",
    )?;
    Ok(())
}

/// At most 33 candidates: overflow is explicit unknown, never silently dropped debt.
/// Current reads seek the partial open index. Older cuts also seek terminal visibility.
pub(super) fn source(
    connection: &Connection,
    subject: &str,
    index: u64,
) -> Result<Option<(String, Value)>> {
    let index = index.min(i64::MAX as u64);
    let unsettled: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_turn_obligation_dirty WHERE subject=?1)
        OR EXISTS(SELECT 1 FROM local_turn_obligation_pending WHERE subject=?1)",
        [subject],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        !unsettled,
        "turn obligation projection is unavailable until its queued source repair commits"
    );
    // Actionable evidence wins the bounded window. A long acknowledged archive cannot
    // hide a fresh interruption. Older cuts still select acknowledgements made after the cut.
    let query="SELECT receipt FROM agent_turn_obligations INDEXED BY agent_turn_obligations_action WHERE subject=?1 AND terminal_index IS NULL AND acknowledged_index IS NULL AND first_index<=?2
 UNION ALL SELECT receipt FROM agent_turn_obligations INDEXED BY agent_turn_obligations_acknowledged WHERE subject=?1 AND terminal_index IS NULL AND acknowledged_index>?2 AND first_index<=?2
 UNION ALL SELECT receipt FROM agent_turn_obligations INDEXED BY agent_turn_obligations_terminal WHERE subject=?1 AND terminal_index>?2 LIMIT 33";
    let mut receipts = connection
        .prepare_cached(query)?
        .query_map(params![subject, index], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let action_overflow = receipts.len() == 33;
    // The overflow witness itself needs no selected version: completeness is already
    // refused. Retain 32 named sources for bounded inspection/acknowledgement. Together
    // with empty range probes and two health slots this keeps source work <=68 points.
    if action_overflow {
        receipts.truncate(32);
    }
    if receipts.len() < 32 {
        let archived = connection.prepare_cached(
            "SELECT receipt FROM agent_turn_obligations INDEXED BY agent_turn_obligations_acknowledged WHERE subject=?1 AND terminal_index IS NULL AND acknowledged_index<=?2 AND first_index<=?2 ORDER BY acknowledged_index DESC LIMIT ?3"
        )?.query_map(params![subject, index, 32 - receipts.len()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        receipts.extend(archived);
    }
    if receipts.is_empty() {
        return Ok(None);
    }
    let mut rows = Vec::new();
    for receipt in receipts {
        let version: Option<(String,String,bool)> = connection.query_row(
            "SELECT source_claim,body,terminal FROM local_turn_obligation_versions WHERE subject=?1 AND receipt=?2 AND visible_index<=?3 ORDER BY visible_index DESC LIMIT 1",
            params![subject,receipt,index], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        rows.push(CapturedReceipt { receipt, version });
    }
    aggregate_receipts(rows, action_overflow)
}

pub(crate) const REDUCER_CONTRACT: &str = "turn-recovery.v3:bounded-canonical-images,aggregate-receipts,from-evidence,apply-capture-before-since;source-incomplete-unavailable;ack-retained-as-unknown-reference-quiets-owner-only;exact-native-fences;stop-suspended-default-false";

pub(crate) fn reducer_fingerprint() -> Result<String> {
    canonical_hash(&json!({"contract":REDUCER_CONTRACT,"physical":physical::schema_fingerprint()?}))
}

/// Complete captured head/version correspondence at the caller's immutable graph cut.
#[derive(Clone, Debug)]
pub(crate) struct CapturedReceipt {
    pub receipt: String,
    pub version: Option<(String, String, bool)>,
}

/// Pure aggregation shared by native source and the qualified namespace mirror. Unknown
/// execution is admitted evidence; missing/malformed/oversized capture is unavailable.
pub(crate) fn aggregate_receipts(
    rows: Vec<CapturedReceipt>,
    action_overflow: bool,
) -> Result<Option<(String, Value)>> {
    let mut ledger = Ledger {
        unknown: action_overflow,
        ..Ledger::default()
    };
    let mut actionable = Ledger {
        unknown: action_overflow,
        ..Ledger::default()
    };
    let mut source_complete = !action_overflow && rows.len() <= 33;
    let mut claims = BTreeSet::new();
    let mut body_bytes = 0_usize;
    let mut captured_bytes = 0_usize;
    let mut unresolved = 0_usize;
    let mut owner_action_required = action_overflow;
    let mut selected_receipts = Vec::new();
    for row in rows {
        let receipt = row.receipt;
        let Some((claim, body, terminal)) = row.version else {
            source_complete = false;
            ledger.unknown = true;
            owner_action_required = true;
            continue;
        };
        if terminal {
            continue;
        }
        unresolved += 1;
        captured_bytes = captured_bytes.saturating_add(body.len());
        if unresolved > 32
            || body.len() > 64 * 1024
            || captured_bytes > 32 * 64 * 1024
            || receipt.len() > 128
            || claim.is_empty()
            || claim.len() > 512
        {
            ledger.unknown = true;
            source_complete = false;
            owner_action_required = true;
            continue;
        }
        claims.insert(claim.clone());
        let value: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(_) => {
                ledger.unknown = true;
                source_complete = false;
                owner_action_required = true;
                continue;
            }
        };
        let acknowledgement = value.get("_acknowledgement");
        if acknowledgement.is_some_and(|ack| {
            !ack.is_object()
                || ack["receipt"] != receipt
                || ack["execution_outcome"] != "unknown"
                || ack["claim"].as_str().is_none_or(str::is_empty)
                || ack["actor"].as_str().is_none_or(str::is_empty)
                || ack["accepted_at_unix_ms"]
                    .as_str()
                    .is_none_or(|stamp| stamp.parse::<u128>().is_err())
                || ack["source_claim"].as_str().is_none_or(str::is_empty)
        }) {
            ledger.unknown = true;
            source_complete = false;
            owner_action_required = true;
            continue;
        }
        owner_action_required |= acknowledgement.is_none();
        if let Some(ack) = acknowledgement.and_then(|ack| ack["claim"].as_str()) {
            claims.insert(ack.into());
        }
        selected_receipts.push(json!({"receipt":receipt,"source_claim":claim,
                "acknowledgement":acknowledgement.map(|ack| json!({"claim":ack["claim"],"actor":ack["actor"],"accepted_at_unix_ms":ack["accepted_at_unix_ms"],"source_claim":ack["source_claim"],"execution_outcome":"unknown"}))}));
        if receipt == "unknown" || value.get("unknown_receipt").is_some() {
            if !value.is_null() && value["unknown_receipt"] != receipt {
                source_complete = false;
                owner_action_required = true;
            }
            ledger.unknown = true;
            if acknowledgement.is_none() {
                actionable.unknown = true;
            }
            ledger.unknown_evidence = value
                .get("unknown_evidence")
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            if value.get("unknown_evidence").is_some_and(|v| !v.is_null())
                && ledger.unknown_evidence.as_ref().is_none_or(|scope| {
                    scope.ownership_sequence == 0
                        || scope.reason.is_empty()
                        || scope.reason.len() > 4096
                        || scope
                            .provider_incarnation
                            .as_ref()
                            .is_some_and(|provider| provider.is_empty() || provider.len() > 4096)
                })
            {
                source_complete = false;
                owner_action_required = true;
            }
        } else if receipt == "unknown-tool-outcome" || receipt.starts_with("tools:") {
            if value["unknown_tool_outcome"] != true
                || (receipt.starts_with("tools:") && value["tool_receipt"] != receipt)
            {
                source_complete = false;
                owner_action_required = true;
            }
            ledger.unknown_tool_outcome = true;
            if acknowledgement.is_none() {
                actionable.unknown_tool_outcome = true;
            }
        } else if ledger.open.len() < 32 {
            match serde_json::from_str::<Obligation>(&body) {
                Ok(entry) => {
                    if self::receipt(&entry)?.as_str() != receipt
                        || entry.ownership_sequence == 0
                        || entry.source_sequence == 0
                    {
                        ledger.unknown = true;
                        source_complete = false;
                        owner_action_required = true;
                        continue;
                    }
                    if acknowledgement.is_some() {
                        // Exact receipt/source plus the attributed audit above retains unknown
                        // execution. Archived arguments/tool IDs do not suppress a normal turn.
                        ledger.unknown = true;
                        ledger.unknown_tool_outcome |= entry.tool_outcome_unknown;
                    } else {
                        body_bytes = body_bytes.saturating_add(body.len());
                        if body_bytes > 24 * 1024 {
                            ledger.unknown = true;
                            source_complete = false;
                            owner_action_required = true;
                            continue;
                        }
                        actionable.open.push(entry.clone());
                        ledger.open.push(entry);
                    }
                }
                Err(_) => {
                    ledger.unknown = true;
                    source_complete = false;
                }
            }
        } else {
            ledger.unknown = true;
            source_complete = false;
        }
    }
    if ledger.open.is_empty() && !ledger.unknown && !ledger.unknown_tool_outcome {
        return Ok(None);
    }
    let claims = claims.into_iter().collect::<Vec<_>>();
    let mut evidence = serde_json::to_value(ledger)?;
    evidence["source_claims"] = json!(claims);
    evidence["selected_receipts"] = json!(selected_receipts);
    evidence["owner_action_required"] = json!(owner_action_required);
    evidence["source_complete"] = json!(source_complete);
    evidence["actionable_evidence"] = serde_json::to_value(actionable)?;
    if serde_json::to_vec(&evidence)?.len() > 64 * 1024 {
        evidence = json!({"unknown":true,"source_complete":false,"owner_action_required":true,
            "capture_bound":"output-byte-bound-exhausted","source_claims":claims,
            "selected_receipts":evidence["selected_receipts"].as_array().into_iter().flatten()
                .map(|row| json!({"receipt":row["receipt"],"source_claim":row["source_claim"]})).collect::<Vec<_>>()});
    }
    Ok(Some((
        claims.first().cloned().unwrap_or_default(),
        evidence,
    )))
}

/// Immutable eligibility captured by the card/native source at the same SQLite snapshot.
/// Absence of any authority fence refuses a current execution/human-wait classification.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnRecoveryInput {
    pub desired_revision: Option<String>,
    pub desired_token: Option<String>,
    pub runtime_incarnation: Option<String>,
    pub provider_incarnation: Option<String>,
    pub ownership_sequence: Option<u64>,
    pub host_eligible: bool,
    pub native_state: Option<String>,
    pub native_blocked_on: Option<String>,
    pub intentional_stop: bool,
    pub suspended: bool,
}

/// No live Store/producer reads belong to formatting this captured output. `None` says
/// only that this graph cut has no admitted owed receipt; it does not certify provider
/// tracking capability or a baseline agent-card namespace's completeness.
#[derive(Clone, Debug)]
pub(crate) struct CapturedTurnRecovery {
    pub graph_index: u64,
    pub input: TurnRecoveryInput,
    pub value: Option<Value>,
}

pub(crate) fn capture(
    connection: &Connection,
    subject: &str,
    index: u64,
    input: TurnRecoveryInput,
) -> Result<CapturedTurnRecovery> {
    Ok(from_evidence(
        index,
        input,
        source(connection, subject, index)?,
    ))
}

/// Pure owner seam for a Source's already captured evidence and authority at one cut.
pub(crate) fn from_evidence(
    graph_index: u64,
    input: TurnRecoveryInput,
    evidence: Option<(String, Value)>,
) -> CapturedTurnRecovery {
    let value = evidence.map(|(claim, raw)| reduce(&claim, raw, &input));
    CapturedTurnRecovery {
        graph_index,
        input,
        value,
    }
}

/// Apply before the native since/transition enrichment; no Store or producer reads.
pub(crate) fn apply_capture(
    view: &mut crate::model::CurrentHarnessView,
    captured: &CapturedTurnRecovery,
) {
    view.turn_recovery = captured.value.clone();
    if view.turn_recovery.as_ref().is_some_and(|value| {
        value["owner_action_required"] != false
            && !matches!(value["state"].as_str(), Some("in-flight" | "pending-human"))
    }) && matches!(view.state.as_str(), "idle" | "ready")
    {
        view.state = "blocked".into();
        view.reason = Some("interrupted-turn-obligation".into());
        view.blocked_on = Some("native-recovery".into());
    }
}

fn reduce(claim: &str, raw: Value, input: &TurnRecoveryInput) -> Value {
    if raw["source_complete"] == false {
        return json!({"state":"unavailable","source_claim":claim,"owner_action_required":true,
            "reason":"turn-recovery source capture is incomplete","evidence":raw});
    }
    let ledger =
        serde_json::from_value::<Ledger>(raw.get("actionable_evidence").unwrap_or(&raw).clone())
            .ok();
    let Some(ledger) = ledger else {
        return json!({"state":"unknown","source_claim":claim,"reason":"malformed-obligation-evidence"});
    };
    let is_current = |entry: &Obligation| {
        input.host_eligible
            && input.desired_token.is_some()
            && input.desired_revision.is_some()
            && entry.desired_revision == input.desired_revision
            && entry.runtime_incarnation.is_some()
            && entry.runtime_incarnation == input.runtime_incarnation
            && entry.provider_incarnation == input.provider_incarnation.as_deref().unwrap_or("")
            && Some(entry.ownership_sequence) == input.ownership_sequence
    };
    let own = ledger.open.iter().all(|entry| {
        is_current(entry)
            || (entry.native_session_id.is_some()
                && entry.native_turn_id.is_some()
                && entry.desired_revision.is_some()
                && ledger.open.iter().any(|current| {
                    is_current(current)
                        && current.desired_revision == entry.desired_revision
                        && current.native_session_id == entry.native_session_id
                        && current.native_turn_id == entry.native_turn_id
                }))
    });
    let state = if raw["owner_action_required"] == false {
        "unknown"
    } else if input.intentional_stop {
        "intentional-stop"
    } else if input.suspended {
        "suspended"
    } else if ledger.unknown_tool_outcome {
        "unknown-tool-outcome"
    } else if ledger.unknown {
        "unknown"
    } else if own && input.native_blocked_on.as_deref() == Some("human") {
        "pending-human"
    } else if own && input.native_state.as_deref() == Some("working") {
        "in-flight"
    } else if ledger.open.iter().any(|entry| entry.tool_outcome_unknown) {
        "unknown-tool-outcome"
    } else if ledger.open.iter().all(|entry| entry.pending_human) {
        "pending-human-unavailable"
    } else {
        "recovery-blocked"
    };
    json!({"state":state,"source_claim":claim,"current_incarnation":input.runtime_incarnation,
        "owner_action_required":raw["owner_action_required"] != false,
        "native_state":input.native_state,"native_continuation":"unsupported",
        "reason":"No supported native checkpoint continuation is available; inspect the saved native session and resolve with the owner. No input or tool has been replayed.",
        "evidence":raw})
}

pub(super) fn enrich(
    connection: &Connection,
    subject: &str,
    index: u64,
    view: &mut crate::model::CurrentHarnessView,
) -> Result<()> {
    // The materialized declaration and the selected source's PK are bounded seeks.
    // Old cuts without that declaration fence cannot certify a current episode.
    let declaration: Option<(String, String, Option<String>)> = connection
        .query_row(
            "SELECT desired.revision,desired.claim_id,json_extract(desired.member,'$.host')
         FROM desired JOIN claims ON claims.id=desired.claim_id
         WHERE desired.subject=?1 AND claims.store_index<=?2",
            params![subject, index.min(i64::MAX as u64)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let claim = claim_by_id_tx(connection, &view.claim)?;
    let fields = claim.as_ref().and_then(|claim| claim.body.get("fields"));
    let input = TurnRecoveryInput {
        desired_revision: declaration.as_ref().map(|d| d.0.clone()),
        desired_token: declaration.as_ref().map(|d| d.1.clone()),
        runtime_incarnation: Some(view.incarnation_id.clone()),
        provider_incarnation: fields
            .and_then(|fields| fields["evidence_incarnation"].as_str())
            .map(str::to_owned),
        ownership_sequence: fields.and_then(|fields| fields["ownership_sequence"].as_u64()),
        host_eligible: claim.as_ref().is_some_and(|claim| {
            claim.kind == "harness.observed" && claim.actor.as_deref() == Some(subject)
                && claim.body["fields"]["incarnation_id"].as_str() == Some(view.incarnation_id.as_str())
                && claim.store_index <= index && declaration.as_ref().and_then(|d| d.2.as_deref()) == Some(claim.origin.as_str())
        }) && claim.as_ref().map(|claim| connection.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=?1 AND state='repaired')",
            [&claim.id],|row| row.get::<_,bool>(0))).transpose()?.unwrap_or(false),
        native_state: Some(view.state.clone()),
        native_blocked_on: view.blocked_on.clone(),
        ..TurnRecoveryInput::default()
    };
    let captured = capture(connection, subject, index, input)?;
    debug_assert_eq!(captured.graph_index, index);
    debug_assert_eq!(
        captured.input.runtime_incarnation.as_deref(),
        Some(view.incarnation_id.as_str())
    );
    apply_capture(view, &captured);
    Ok(())
}

impl Store {
    pub(crate) fn turn_obligation_failures(
        &self,
        subject: &str,
        revision: &str,
        incarnation: &str,
    ) -> Result<Vec<ClaimRecord>> {
        let connection = self.readers.get();
        let mut query = connection.prepare(
            "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='operational.failure' AND json_extract(body,'$.fields.condition')='turn-obligation'
             AND json_extract(body,'$.fields.source_revision')=?2 AND json_extract(body,'$.fields.incarnation')=?3
             ORDER BY store_index DESC LIMIT 33")?;
        Ok(query
            .query_map(params![subject, revision, incarnation], claim_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn turn_obligation_snapshot(&self, subject: &str) -> Result<Value> {
        let connection = self.readers.get();
        let tx = connection.unchecked_transaction()?;
        let cut = current_index(&tx)?;
        let revision = tx
            .query_row(
                "SELECT claim_id FROM desired WHERE subject=?1",
                [subject],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let evidence = source(&tx, subject, cut)?.map(|(_, evidence)| evidence);
        Ok(
            json!({"subject":subject,"captured_cut":cut,"source_revision":revision,"evidence":evidence,
            "reducer_contract":reducer_fingerprint()?,"native_continuation":"unsupported","tracking_capability":"not-certified-by-receipt-absence"}),
        )
    }

    pub(crate) fn acknowledge_turn_obligation(
        &self,
        request: AcknowledgeRequest,
    ) -> Result<ClaimRecord, St3Error> {
        let mut evidence = request
            .receipts
            .iter()
            .map(|receipt| receipt.source_claim.clone())
            .collect::<Vec<_>>();
        evidence.push(request.source_revision.clone());
        evidence.sort();
        evidence.dedup();
        let input = ClaimInput {
            subject: request.subject,
            kind: "harness.turn-acknowledged".into(),
            actor: Some(request.actor),
            fields: BTreeMap::from([
                ("receipts".into(), json!(request.receipts)),
                ("source_revision".into(), json!(request.source_revision)),
                ("captured_cut".into(), json!(request.captured_cut)),
                ("reason".into(), json!(request.reason)),
            ]),
            evidence,
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key),
        };
        append_claim_with_fences(&self.graph, &input, None, None).map(|(claim, _)| claim)
    }

    /// Publish owner action against captured authority, checked inside the writer transaction.
    pub(crate) fn record_turn_obligation_failure(
        &self,
        episode: &str,
        input: &AttentionRequest,
        desired: &DesiredSubject,
        desired_token: &str,
        incarnation: &str,
    ) -> Result<(), St3Error> {
        let previous = self.operational_failure(episode).map_err(internal)?;
        if previous.is_some() {
            return Ok(());
        }
        let receipts = self
            .turn_obligation_source_at(&desired.subject, i64::MAX as u64)
            .map_err(internal)?
            .map(|source| actionable_receipt_keys(&source["evidence"]))
            .unwrap_or_default();
        let claim = ClaimInput {
            subject: desired.subject.clone(),
            kind: "operational.failure".into(),
            actor: Some(input.actor.clone()),
            fields: BTreeMap::from([
                ("episode".into(), json!(episode)),
                ("condition".into(), json!("turn-obligation")),
                ("turn_receipts".into(), json!(receipts)),
                ("reviewer".into(), json!(input.reviewer)),
                ("title".into(), json!(input.title)),
                ("reason".into(), json!(input.reason)),
                ("severity".into(), json!(input.severity)),
                ("targets".into(), json!(input.targets)),
                ("source_revision".into(), json!(desired_token)),
                ("incarnation".into(), json!(incarnation)),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("{}:first", input.idempotency_key)),
        };
        append_claim_with_subject_fences(
            &self.graph,
            &claim,
            None,
            Some(incarnation),
            None,
            Some((&desired.subject, &desired_revision(desired), &|| true)),
        )?;
        Ok(())
    }

    pub(crate) fn turn_obligation_source_at(
        &self,
        subject: &str,
        index: u64,
    ) -> Result<Option<Value>> {
        Ok(source(&self.readers.get(), subject, index)?
            .map(|(claim, evidence)| json!({"source_claim":claim,"evidence":evidence})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st_drivers::turn_obligation::{Obligation, Terminal};

    fn entry(provider: &str, sequence: u64, turn: Option<&str>) -> Obligation {
        Obligation {
            source_sequence: sequence,
            provider_incarnation: provider.into(),
            ownership_sequence: 1,
            runtime_incarnation: Some(format!("runtime-{provider}")),
            desired_revision: Some("desired-one".into()),
            native_session_id: Some("session-one".into()),
            native_turn_id: turn.map(str::to_owned),
            started_at_ms: 1,
            pending_human: false,
            tool_outcome_unknown: false,
            pending_tool_ids: vec![],
        }
    }
    fn observe(store: &Store, ledger: Option<&Ledger>) -> ClaimRecord {
        let mut fields = BTreeMap::from([
            ("state".into(), json!("idle")),
            ("incarnation_id".into(), json!("successor")),
        ]);
        if let Some(ledger) = ledger {
            fields.insert(
                "turn_obligation".into(),
                serde_json::to_value(ledger).unwrap(),
            );
        }
        store
            .append_claim(&ClaimInput {
                subject: "agent/receipt/fixture".into(),
                kind: "harness.observed".into(),
                actor: None,
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }
    fn debt(store: &Store, index: u64) -> Option<Value> {
        source(&store.readers.get(), "agent/receipt/fixture", index)
            .unwrap()
            .map(|(_, body)| body)
    }
    fn declare_ack_fixture(store: &Store) -> String {
        let intent = crate::graph::parse_test_intent(
            "version 2\nagent \"receipt/fixture\" { command \"true\" }",
            "ack-fixture",
        )
        .unwrap();
        let preview = store
            .mission(
                &intent,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &preview.subject_tokens, "declaration")
            .unwrap();
        store
            .selected_desired_token("agent/receipt/fixture")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn turn_acknowledgement_quiets_only_named_evidence_and_retains_unknown_execution() {
        let store = Store::open_memory("ack-fixture").unwrap();
        let revision = declare_ack_fixture(&store);
        let first = entry("old", 1, None);
        let claim = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![first.clone()],
                ..Ledger::default()
            }),
        );
        let request = AcknowledgeRequest {
            subject: "agent/receipt/fixture".into(),
            actor: "person/operator".into(),
            receipts: vec![ReceiptEvidence {
                receipt: receipt(&first).unwrap(),
                source_claim: claim.id.clone(),
            }],
            source_revision: revision,
            captured_cut: claim.store_index,
            reason: "Inspected the unknown outcome; quiet owner action only".into(),
            idempotency_key: "ack-first".into(),
        };
        let mut foreign = request.clone();
        foreign.actor = "agent/foreign".into();
        foreign.idempotency_key = "foreign".into();
        assert_eq!(
            store.acknowledge_turn_obligation(foreign).unwrap_err().code,
            "wrong-turn-acknowledgement-actor"
        );
        let mut stale = request.clone();
        stale.source_revision = "missing".into();
        stale.idempotency_key = "stale".into();
        assert_eq!(
            store.acknowledge_turn_obligation(stale).unwrap_err().code,
            "stale-turn-acknowledgement"
        );
        let ack = store.acknowledge_turn_obligation(request.clone()).unwrap();
        assert_eq!(ack.actor.as_deref(), Some("person/operator"));
        let raw = debt(&store, ack.store_index).unwrap();
        assert!(raw["open"].as_array().unwrap().is_empty());
        assert_eq!(
            raw["unknown"], true,
            "acknowledged execution remains unknown by exact retained source reference"
        );
        assert_eq!(raw["owner_action_required"], false);
        assert_eq!(
            raw["selected_receipts"][0]["acknowledgement"]["claim"],
            ack.id
        );
        assert_eq!(
            raw["selected_receipts"][0]["acknowledgement"]["execution_outcome"],
            "unknown"
        );
        let captured = from_evidence(
            ack.store_index,
            TurnRecoveryInput::default(),
            Some((claim.id.clone(), raw)),
        );
        assert_eq!(captured.value.as_ref().unwrap()["state"], "unknown");
        assert_eq!(
            captured.value.as_ref().unwrap()["owner_action_required"],
            false
        );
        assert_eq!(
            debt(&store, claim.store_index).unwrap()["owner_action_required"],
            true,
            "acknowledgement is cut-qualified"
        );
        let heartbeat = observe(&store, None);
        assert_eq!(
            debt(&store, heartbeat.store_index).unwrap()["owner_action_required"],
            false
        );
        let mut current = entry("current", 2, Some("new-normal-turn"));
        current.runtime_incarnation = Some("runtime-current".into());
        let normal = observe(
            &store,
            Some(&Ledger {
                sequence: 2,
                open: vec![current.clone()],
                ..Ledger::default()
            }),
        );
        let input = TurnRecoveryInput {
            desired_revision: current.desired_revision.clone(),
            desired_token: Some(request.source_revision.clone()),
            runtime_incarnation: current.runtime_incarnation.clone(),
            provider_incarnation: Some(current.provider_incarnation.clone()),
            ownership_sequence: Some(current.ownership_sequence),
            host_eligible: true,
            native_state: Some("working".into()),
            ..TurnRecoveryInput::default()
        };
        let native = from_evidence(
            normal.store_index,
            input,
            source(&store.readers.get(), &request.subject, normal.store_index).unwrap(),
        );
        assert_eq!(
            native.value.as_ref().unwrap()["state"],
            "in-flight",
            "acknowledged unknown does not block a normal native turn"
        );
        assert!(
            native.value.as_ref().unwrap()["evidence"]["selected_receipts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["acknowledgement"]["claim"] == ack.id),
            "unknown audit stays visible during normal work"
        );
        let second = entry("new", 2, None);
        let interrupted = observe(
            &store,
            Some(&Ledger {
                sequence: 2,
                open: vec![second],
                ..Ledger::default()
            }),
        );
        assert_eq!(
            debt(&store, interrupted.store_index).unwrap()["owner_action_required"],
            true,
            "new interruption is not quieted by an old acknowledgement"
        );
        assert_eq!(
            store.acknowledge_turn_obligation(request).unwrap().id,
            ack.id,
            "a retry returns its original scope after new interruption"
        );
        let writer = store.connection.write();
        let still_open: bool = writer.query_row("SELECT terminal_index IS NULL FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2", params!["agent/receipt/fixture", receipt(&first).unwrap()], |row| row.get(0)).unwrap();
        assert!(still_open, "acknowledgement never writes a native terminal");
    }

    #[test]
    fn acknowledgement_projection_is_order_independent_and_cited_source_repair_retires_its_effect()
    {
        let store = Store::open_memory("ack-order-fixture").unwrap();
        let revision = declare_ack_fixture(&store);
        let first = entry("old", 1, None);
        let start = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![first.clone()],
                ..Ledger::default()
            }),
        );
        let ack = store
            .acknowledge_turn_obligation(AcknowledgeRequest {
                subject: "agent/receipt/fixture".into(),
                actor: "person/operator".into(),
                receipts: vec![ReceiptEvidence {
                    receipt: receipt(&first).unwrap(),
                    source_claim: start.id.clone(),
                }],
                source_revision: revision,
                captured_cut: start.store_index,
                reason: "Fixture inspected unknown execution".into(),
                idempotency_key: "ack-order".into(),
            })
            .unwrap();
        // Hold both admitted claims, then reconstruct their physical rows in reverse order.
        // This is a projection/authority control, not a new replica admission certificate.
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute_batch("DELETE FROM agent_turn_obligation_evidence; DELETE FROM agent_turn_obligations; DELETE FROM local_turn_obligation_versions;").unwrap();
            project(&tx, &ack.id).unwrap();
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM agent_turn_obligations", [], |row| row
                    .get::<_, u64>(0))
                    .unwrap(),
                0,
                "an audit cannot fabricate a native start"
            );
            project(&tx, &start.id).unwrap();
            tx.commit().unwrap();
        }
        let captured = debt(&store, u64::MAX).unwrap();
        assert_eq!(captured["unknown"], true);
        assert_eq!(captured["owner_action_required"], false);
        assert_eq!(
            captured["selected_receipts"][0]["acknowledgement"]["claim"],
            ack.id
        );
        // Another admitted carrier cannot confer the original cited claim's authority.
        let mut changed_carrier = first;
        changed_carrier.pending_tool_ids.push("another-tool".into());
        changed_carrier.tool_outcome_unknown = true;
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![changed_carrier],
                ..Ledger::default()
            }),
        );
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                VALUES('fixture-repaired-ack-source','fixture',1,'fixture-hash',0,X'00','repaired',?1,'1')", [&start.id]).unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["owner_action_required"],
            true,
            "repaired cited source cannot quiet another admitted carrier"
        );
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            // Remove this fixture's idempotency reference before mutating its claim.
            tx.execute(
                "DELETE FROM operations WHERE canonical_claim_id=?1",
                [&ack.id],
            )
            .unwrap();
            tx.execute("DELETE FROM claims WHERE id=?1", [&ack.id])
                .unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["owner_action_required"],
            true,
            "removing acknowledgement never settles execution"
        );
    }

    #[test]
    fn checkpoint_compacts_tool_carriers_and_preserves_receipt_authority() {
        use super::super::checkpoint::*;
        let store = Store::open_memory("receipt-checkpoint-fixture").unwrap();
        let revision = declare_ack_fixture(&store);
        let mut owed = entry("old", 1, None);
        let mut carriers = Vec::new();
        for n in 0..12 {
            owed.pending_tool_ids = vec![format!("tool-{n}")];
            owed.tool_outcome_unknown = true;
            carriers.push(observe(&store, Some(&Ledger {
                sequence: n + 1, open: vec![owed.clone()], ..Ledger::default()
            })));
        }
        let ack = store.acknowledge_turn_obligation(AcknowledgeRequest {
            subject: "agent/receipt/fixture".into(), actor: "person/operator".into(),
            receipts: vec![ReceiptEvidence { receipt: receipt(&owed).unwrap(), source_claim: carriers[0].id.clone() }],
            source_revision: revision, captured_cut: carriers.last().unwrap().store_index,
            reason: "Inspected original receipt".into(), idempotency_key: "checkpoint-ack".into(),
        });
        // An acknowledgement must cite the currently selected image, not an old carrier.
        assert!(ack.is_err());
        let before = debt(&store, u64::MAX).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let cut = now_ms() + 1_000;
        let (plan, proof) = store.plan_checkpoint(cut, scratch.path()).unwrap();
        assert!(proof.passed, "{:?}", proof.mismatches);
        let dropped = plan.claims.iter().map(|row| row.id.as_str()).collect::<BTreeSet<_>>();
        assert!(carriers[..11].iter().filter(|row| dropped.contains(row.id.as_str())).count() >= 10);
        assert!(!dropped.contains(carriers[11].id.as_str()));
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            record_checkpoint_tombstones_tx(&tx, &checkpoint_name(cut), &plan.envelopes, &plan.claims).unwrap();
            delete_dropped_rows_tx(&tx, &plan.envelopes, &plan.claims).unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(debt(&store, u64::MAX).unwrap(), before);
    }

    #[test]
    fn aggregation_distinguishes_admitted_unknown_from_missing_or_malformed_capture() {
        let known = CapturedReceipt {
            receipt: "unknown".into(),
            version: Some(("claim/unknown".into(), "null".into(), false)),
        };
        let evidence = aggregate_receipts(vec![known], false).unwrap().unwrap();
        assert_eq!(evidence.1["source_complete"], true);
        assert_eq!(
            from_evidence(1, TurnRecoveryInput::default(), Some(evidence))
                .value
                .unwrap()["state"],
            "unknown"
        );
        for row in [
            CapturedReceipt {
                receipt: "missing".into(),
                version: None,
            },
            CapturedReceipt {
                receipt: "bad".into(),
                version: Some(("claim/bad".into(), "{bad".into(), false)),
            },
        ] {
            let evidence = aggregate_receipts(vec![row], false).unwrap().unwrap();
            assert_eq!(evidence.1["source_complete"], false);
            assert_eq!(
                from_evidence(1, TurnRecoveryInput::default(), Some(evidence))
                    .value
                    .unwrap()["state"],
                "unavailable"
            );
        }
    }

    #[test]
    fn captured_overflow_and_malformed_ack_never_become_clean_or_native_identity() {
        let rows = (0..33)
            .map(|n| CapturedReceipt {
                receipt: format!("unknown-{n}"),
                version: Some((
                    format!("claim/{n}"),
                    json!({"unknown_receipt":format!("unknown-{n}"),"unknown_evidence":null})
                        .to_string(),
                    false,
                )),
            })
            .collect();
        let evidence = aggregate_receipts(rows, true).unwrap().unwrap();
        assert_eq!(evidence.1["source_complete"], false);
        assert_eq!(
            from_evidence(1, TurnRecoveryInput::default(), Some(evidence))
                .value
                .unwrap()["state"],
            "unavailable"
        );
        let malformed = CapturedReceipt { receipt:"unknown".into(), version:Some(("claim/original".into(), json!({"unknown_receipt":"unknown","_acknowledgement":{"claim":"ack","receipt":"different","execution_outcome":"completed"}}).to_string(), false)) };
        let evidence = aggregate_receipts(vec![malformed], false).unwrap().unwrap();
        assert_eq!(evidence.1["source_complete"], false);
        assert_eq!(evidence.1["owner_action_required"], true);
        let mismatched = CapturedReceipt {
            receipt: "wrong-native-receipt".into(),
            version: Some((
                "claim/native".into(),
                serde_json::to_string(&entry("old", 1, Some("turn"))).unwrap(),
                false,
            )),
        };
        let evidence = aggregate_receipts(vec![mismatched], false)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.1["source_complete"], false);
        assert!(evidence.1["open"].as_array().unwrap().is_empty());
    }

    #[test]
    fn publication_omits_heartbeat_history_and_bounds_tool_delta_to_one_receipt() {
        let store = Store::open_memory("delta-fixture").unwrap();
        let mut ledger = Ledger {
            sequence: 32,
            open: (1..=32)
                .map(|n| entry("a", n, Some(&format!("turn-{n}"))))
                .collect(),
            ..Ledger::default()
        };
        observe(&store, Some(&ledger));
        let full = serde_json::to_value(&ledger).unwrap();
        assert!(
            publication_delta(&store.readers.get(), "agent/receipt/fixture", &full)
                .unwrap()
                .is_none()
        );
        ledger.open[0].pending_tool_ids.push("tool-one".into());
        ledger.open[0].tool_outcome_unknown = true;
        let delta = publication_delta(
            &store.readers.get(),
            "agent/receipt/fixture",
            &serde_json::to_value(&ledger).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(delta["open"].as_array().unwrap().len(), 1);
        assert!(delta["terminal"].as_array().unwrap().is_empty());
        assert!(!wakes_reconciler(&store.readers.get(), "agent/receipt/fixture", &delta).unwrap());
        let delta_bytes = serde_json::to_vec(&delta).unwrap().len();
        assert!(delta_bytes < 1024);
        assert!(serde_json::to_vec(&full).unwrap().len() > 10 * delta_bytes);
        let before = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM agent_turn_obligation_evidence",
                [],
                |r| r.get::<_, u64>(0),
            )
            .unwrap();
        let changed = observe(&store, Some(&serde_json::from_value(delta).unwrap()));
        let after = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM agent_turn_obligation_evidence",
                [],
                |r| r.get::<_, u64>(0),
            )
            .unwrap();
        assert_eq!(
            after - before,
            1,
            "a tool update writes only its changed receipt"
        );
        // Measure this receipt's projection separately from unrelated graph admission work.
        thread_local! {
            static RECEIPT_SQL: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        fn trace_receipt_sql(sql: &str) {
            RECEIPT_SQL.with(|rows| rows.borrow_mut().push(sql.to_owned()));
        }
        let statements = {
            let mut writer = store.connection.write();
            writer.trace(Some(trace_receipt_sql));
            let tx = writer.transaction().unwrap();
            project(&tx, &changed.id).unwrap();
            tx.commit().unwrap();
            writer.trace(None);
            RECEIPT_SQL.with(|rows| std::mem::take(&mut *rows.borrow_mut()))
        };
        for prefix in [
            "INSERT OR REPLACE INTO agent_turn_obligation_evidence",
            "INSERT INTO agent_turn_obligations",
            "INSERT OR REPLACE INTO local_turn_obligation_versions",
        ] {
            assert_eq!(statements.iter().filter(|sql| sql.trim_start().starts_with(prefix)).count(), 1);
        }
        println!("tool delta: {delta_bytes} bytes; projection trace={} statements including digest triggers; one evidence/head/version write; heartbeat receipt writes=0", statements.len());
        assert!(
            checkpoint_rules::slot_of(&changed).is_some(),
            "a complete single-receipt carrier is compactable"
        );
        assert!(
            publication_delta(
                &store.readers.get(),
                "agent/receipt/fixture",
                &serde_json::to_value(&ledger).unwrap()
            )
            .unwrap()
            .is_none()
        );
        ledger.open[0].pending_tool_ids.clear();
        ledger.open[0].tool_outcome_unknown = false;
        let result = publication_delta(
            &store.readers.get(),
            "agent/receipt/fixture",
            &serde_json::to_value(&ledger).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(!wakes_reconciler(&store.readers.get(), "agent/receipt/fixture", &result).unwrap());
        println!(
            "receipt tool delta bytes={delta_bytes}; evidence rows added=1; receipt head/version writes=1 each; unchanged heartbeat receipt writes=0"
        );
    }

    #[test]
    fn physical_row_json_encoding_fits_source_cap_and_pending_repair_refuses_capture() {
        let store = Store::open_memory("padded-physical-fixture").unwrap();
        let mut owed = entry(&"\"".repeat(4000), 1, None);
        owed.native_session_id = Some("\"".repeat(4000));
        let claim = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![owed],
                ..Ledger::default()
            }),
        );
        let connection = store.readers.get();
        for table in [
            "agent_turn_obligation_evidence",
            "agent_turn_obligations",
            "local_turn_obligation_versions",
        ] {
            let mut stmt = connection
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap();
            let count = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    (0..count)
                        .map(|column| {
                            Ok(match row.get_ref(column)? {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(value) => json!(value),
                                rusqlite::types::ValueRef::Text(value) => {
                                    json!(std::str::from_utf8(value).unwrap())
                                }
                                _ => panic!("receipt source has unexpected encoding"),
                            })
                        })
                        .collect::<rusqlite::Result<Vec<Value>>>()
                })
                .unwrap();
            for row in rows {
                assert!(
                    serde_json::to_vec(&row.unwrap()).unwrap().len() < 64 * 1024,
                    "{table} physical encoding exceeds Source cap"
                );
            }
        }
        drop(connection);
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute(
                "UPDATE claims SET accepted_at_unix_ms='99999999999999' WHERE id=?1",
                [&claim.id],
            )
            .unwrap();
            assert!(
                capture(
                    &tx,
                    "agent/receipt/fixture",
                    u64::MAX,
                    TurnRecoveryInput::default()
                )
                .is_err(),
                "uncommitted repair cannot certify a current source"
            );
            flush(&tx).unwrap();
            assert!(
                capture(
                    &tx,
                    "agent/receipt/fixture",
                    u64::MAX,
                    TurnRecoveryInput::default()
                )
                .unwrap()
                .value
                .is_some()
            );
            tx.commit().unwrap();
        }
    }

    #[test]
    fn current_classification_requires_every_captured_fence_and_exact_native_join() {
        let old = entry("old", 1, Some("native-turn"));
        let mut current = entry("new", 2, Some("native-turn"));
        let input = TurnRecoveryInput {
            desired_revision: Some("desired-one".into()),
            desired_token: Some("claim/current-declaration".into()),
            runtime_incarnation: Some("runtime-new".into()),
            provider_incarnation: Some("new".into()),
            ownership_sequence: Some(1),
            host_eligible: true,
            native_state: Some("working".into()),
            ..TurnRecoveryInput::default()
        };
        let evidence = |old: Obligation, current: Obligation| {
            serde_json::to_value(Ledger {
                sequence: 2,
                open: vec![old, current],
                ..Ledger::default()
            })
            .unwrap()
        };
        let full = evidence(old.clone(), current.clone());
        assert_eq!(
            reduce("fixture", full.clone(), &input)["state"],
            "in-flight"
        );
        for fence in 0..6 {
            let mut missing = input.clone();
            match fence {
                0 => missing.desired_revision = None,
                1 => missing.desired_token = None,
                2 => missing.runtime_incarnation = None,
                3 => missing.provider_incarnation = None,
                4 => missing.ownership_sequence = None,
                _ => missing.host_eligible = false,
            }
            assert_eq!(
                reduce("fixture", full.clone(), &missing)["state"],
                "recovery-blocked",
                "missing fence {fence}"
            );
        }
        let mut human = input.clone();
        human.native_blocked_on = Some("human".into());
        assert_eq!(
            reduce("fixture", full.clone(), &human)["state"],
            "pending-human"
        );
        current.native_turn_id = None;
        assert_eq!(
            reduce("fixture", evidence(old.clone(), current), &input)["state"],
            "recovery-blocked",
            "OMP partial native join cannot renew predecessor"
        );
        let mut stop = input.clone();
        stop.intentional_stop = true;
        assert_eq!(reduce("fixture", full, &stop)["state"], "intentional-stop");
    }

    #[test]
    fn only_correlated_late_tool_results_settle_the_original_invocation_scope() {
        let store = Store::open_memory("late-result-fixture").unwrap();
        let mut owed = entry("old", 1, Some("original-turn"));
        owed.tool_outcome_unknown = true;
        owed.pending_tool_ids = vec!["first".into(), "second".into()];
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![owed.clone()],
                ..Ledger::default()
            }),
        );
        let terminal = Terminal {
            obligation: owed.clone(),
            outcome: "cancelled".into(),
            evidence_provider_incarnation: "old".into(),
            observed_at_ms: 2,
        };
        let interrupted = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                terminal: vec![terminal.clone()],
                unknown_tool_outcome: true,
                ..Ledger::default()
            }),
        );
        let before = debt(&store, interrupted.store_index).unwrap();
        assert_eq!(before["unknown_tool_outcome"], true);
        assert!(
            before["selected_receipts"][0]["receipt"]
                .as_str()
                .unwrap()
                .starts_with("tools:")
        );
        observe(
            &store,
            Some(&Ledger {
                sequence: 2,
                terminal: vec![Terminal {
                    obligation: entry("new", 2, Some("clean-later-turn")),
                    outcome: "completed".into(),
                    evidence_provider_incarnation: "new".into(),
                    observed_at_ms: 3,
                }],
                ..Ledger::default()
            }),
        );
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["unknown_tool_outcome"],
            true,
            "clean native turn is no tool result"
        );
        let mut result = st_drivers::turn_obligation::ToolResult {
            terminal,
            tool_id: "first".into(),
            evidence_provider_incarnation: "new".into(),
            observed_at_ms: 4,
        };
        result.terminal.obligation.pending_tool_ids = vec!["second".into()];
        let partial = Ledger {
            sequence: 2,
            tool_results: vec![result.clone()],
            ..Ledger::default()
        };
        observe(&store, Some(&partial));
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["unknown_tool_outcome"],
            true
        );
        assert!(
            publication_delta(
                &store.readers.get(),
                "agent/receipt/fixture",
                &serde_json::to_value(&partial).unwrap()
            )
            .unwrap()
            .is_none()
        );
        result.tool_id = "second".into();
        result.observed_at_ms = 5;
        result.terminal.obligation.pending_tool_ids.clear();
        result.terminal.obligation.tool_outcome_unknown = false;
        let complete = Ledger {
            sequence: 2,
            tool_results: vec![result],
            ..Ledger::default()
        };
        assert!(
            wakes_reconciler(
                &store.readers.get(),
                "agent/receipt/fixture",
                &serde_json::to_value(&complete).unwrap()
            )
            .unwrap()
        );
        let proof = observe(&store, Some(&complete));
        assert!(debt(&store, u64::MAX).is_none());
        assert_eq!(
            debt(&store, interrupted.store_index).unwrap()["unknown_tool_outcome"],
            true,
            "older cut retains uncertainty"
        );
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("DELETE FROM claims WHERE id=?1", [&proof.id])
                .unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["unknown_tool_outcome"],
            true,
            "removing the exact result reopens only its original scope"
        );
    }

    #[test]
    fn exact_turn_terminal_does_not_erase_a_missing_tool_result() {
        let store = Store::open_memory("receipt-tool-outcome-fixture").unwrap();
        let mut owed = entry("old", 1, Some("native-turn"));
        owed.tool_outcome_unknown = true;
        owed.pending_tool_ids = vec!["fixture-side-effect".into()];
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![owed.clone()],
                ..Ledger::default()
            }),
        );
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                terminal: vec![Terminal {
                    obligation: owed,
                    outcome: "completed".into(),
                    evidence_provider_incarnation: "new".into(),
                    observed_at_ms: 2,
                }],
                ..Ledger::default()
            }),
        );
        let evidence = debt(&store, u64::MAX).unwrap();
        assert!(
            evidence["open"].as_array().unwrap().is_empty(),
            "turn itself has exact terminal proof"
        );
        assert_eq!(evidence["unknown_tool_outcome"], true);
        assert_eq!(
            reduce("claim/fixture", evidence, &TurnRecoveryInput::default())["state"],
            "unknown-tool-outcome"
        );
    }

    #[test]
    fn removal_and_repaired_original_terminal_reopen_only_the_admitted_debt() {
        let store = Store::open_memory("receipt-repair-fixture").unwrap();
        let owed = entry("old", 1, Some("native-turn"));
        let start = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![owed.clone()],
                ..Ledger::default()
            }),
        );
        let proof = Ledger {
            sequence: 1,
            terminal: vec![Terminal {
                obligation: owed,
                outcome: "completed".into(),
                evidence_provider_incarnation: "new".into(),
                observed_at_ms: 2,
            }],
            ..Ledger::default()
        };
        let terminal = observe(&store, Some(&proof));
        assert!(debt(&store, u64::MAX).is_none());
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                VALUES('invented-repaired-proof','fixture',1,'fixture-hash',0,X'00','valid',?1,'1')",[&terminal.id]).unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert!(debt(&store, u64::MAX).is_none());
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute(
                "UPDATE replica_records SET state='repaired' WHERE claim_id=?1",
                [&terminal.id],
            )
            .unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("DELETE FROM claims WHERE id=?1", [&start.id])
                .unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert!(
            debt(&store, u64::MAX).is_none(),
            "removed start and repaired terminal confer no current debt"
        );
        store.rebuild_claim_projections().unwrap();
        assert!(debt(&store, u64::MAX).is_none());
    }

    #[test]
    fn canonical_metadata_reselection_and_capture_byte_overflow_are_explicit() {
        let store = Store::open_memory("receipt-canonical-fixture").unwrap();
        let mut ledger = Ledger {
            sequence: 1,
            open: vec![entry("old", 1, Some("native-turn"))],
            ..Ledger::default()
        };
        let first = observe(&store, Some(&ledger));
        ledger.open[0].pending_human = true;
        let second = observe(&store, Some(&ledger));
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"][0]["pending_human"],
            true
        );
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            // Metadata mutation is a projection qualification fixture, not a replica authorization claim.
            tx.execute(
                "UPDATE claims SET accepted_at_unix_ms='99999999999999' WHERE id=?1",
                [&first.id],
            )
            .unwrap();
            flush(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"][0]["pending_human"],
            false
        );
        assert_ne!(first.id, second.id);
        for n in 0..12 {
            let mut large = entry(&format!("source-{n}"), 1, None);
            large.native_session_id = Some("s".repeat(4000));
            observe(
                &store,
                Some(&Ledger {
                    sequence: 1,
                    open: vec![large],
                    ..Ledger::default()
                }),
            );
        }
        let captured = debt(&store, u64::MAX).unwrap();
        assert_eq!(
            captured["unknown"], true,
            "union exceeding wire/source bound is not a clean result"
        );
        assert!(serde_json::to_vec(&captured).unwrap().len() <= 64 * 1024);
        let connection = store.readers.get();
        for table in [
            "agent_turn_obligation_evidence",
            "agent_turn_obligations",
            "local_turn_obligation_versions",
        ] {
            let maximum: i64 = connection
                .query_row(
                    &format!("SELECT MAX(length(CAST(body AS BLOB))) FROM {table}"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                maximum < 64 * 1024,
                "{table} source body exceeds physical cap"
            );
        }
    }

    #[test]
    fn signed_replica_admission_and_reordered_sources_preserve_terminal_proof() {
        const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let anchor = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        let later_key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        let reader_key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        let node = |name: &str, key: &Arc<crate::fleet::MemberKey>| {
            let store = Store::open_memory(name).unwrap();
            store.bind_fleet(FLEET).unwrap();
            store.pin_fleet_anchor(anchor.public()).unwrap();
            store.set_member_key(Some(key.clone())).unwrap();
            store
        };
        let original = node("original", &anchor);
        let later = node("later", &later_key);
        let reader = node("reader", &reader_key);
        for (name, key, via) in [
            ("original", &anchor, "anchor"),
            ("later", &later_key, "invite"),
            ("reader", &reader_key, "invite"),
        ] {
            original.append_claim(&ClaimInput {
                subject: format!("host/{name}"), kind: "fleet.member-admitted".into(), actor: None,
                fields: serde_json::from_value(json!({"fleet_id":FLEET,"member_key":key.public(),"via":via,"mode":"listening"})).unwrap(),
                evidence: vec![], expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
        let sync = |from: &Store, to: &Store| {
            let exchange = from
                .export_replication_exchange_answering(
                    FLEET,
                    &to.replication_inventory().unwrap(),
                    &to.replication_signature_requests().unwrap(),
                )
                .unwrap();
            to.receive_replication_exchange(&from.origin, FLEET, &exchange)
                .unwrap();
            to.validate_replication_backlog().unwrap();
            to.project_replication_backlog().unwrap();
        };
        sync(&original, &later);
        sync(&original, &reader);
        let receipt = entry("old", 1, Some("native-turn"));
        let start = observe(
            &original,
            Some(&Ledger {
                sequence: 1,
                open: vec![receipt.clone()],
                ..Ledger::default()
            }),
        );
        let terminal = observe(
            &later,
            Some(&Ledger {
                sequence: 1,
                terminal: vec![Terminal {
                    obligation: receipt,
                    outcome: "completed".into(),
                    evidence_provider_incarnation: "new".into(),
                    observed_at_ms: 2,
                }],
                ..Ledger::default()
            }),
        );
        sync(&later, &reader);
        assert!(reader.claim_by_id(&terminal.id).unwrap().is_some());
        sync(&original, &reader);
        assert!(reader.claim_by_id(&start.id).unwrap().is_some());
        assert!(debt(&reader, u64::MAX).is_none());
        sync(&original, &later);
        sync(&later, &original);
        let a = projection_digest::oracle(&original.readers.get()).unwrap();
        let b = projection_digest::oracle(&reader.readers.get()).unwrap();
        assert_eq!(a["agent_turn_obligations"], b["agent_turn_obligations"],
            "arrival order must not alter shared receipt heads");
        let candidate_images = |store: &Store| {
            store.readers.get().prepare(
                "SELECT json_array(subject,receipt,claim_id,native_key,terminal,canonical_key,body)
                 FROM agent_turn_obligation_evidence ORDER BY subject,receipt,claim_id,terminal",
            ).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(candidate_images(&original), candidate_images(&reader),
            "the local candidate index still normalizes admitted evidence identically");
        let mut older = st3_schema::registry().clone();
        older
            .claims
            .get_mut("harness.observed")
            .unwrap()
            .fields
            .remove("turn_obligation");
        assert!(matches!(
            classify_replicated_claim_with_registry(&terminal, &older).unwrap(),
            ReplicatedClaimAdmission::UnknownField
        ));
        assert!(matches!(
            classify_replicated_claim_with_registry(&terminal, st3_schema::registry()).unwrap(),
            ReplicatedClaimAdmission::Valid
        ));
        let mut invalid = terminal;
        invalid.body["fields"]["turn_obligation"]["terminal"][0]["obligation"]["native_turn_id"] =
            Value::Null;
        assert!(classify_replicated_claim_with_registry(&invalid, st3_schema::registry()).is_err());

        let revision = declare_ack_fixture(&original);
        let acknowledged = entry("acknowledged-provider", 2, None);
        let start = observe(&original, Some(&Ledger {
            sequence: 2, open: vec![acknowledged.clone()], ..Ledger::default()
        }));
        let ack = original.acknowledge_turn_obligation(AcknowledgeRequest {
            subject: "agent/receipt/fixture".into(), actor: "person/operator".into(),
            receipts: vec![ReceiptEvidence { receipt: super::receipt(&acknowledged).unwrap(), source_claim: start.id }],
            source_revision: revision, captured_cut: start.store_index,
            reason: "Inspected signed receipt fixture".into(), idempotency_key: "signed-receipt-ack".into(),
        }).unwrap();
        sync(&original, &reader);
        let admitted_ack = reader.claim_by_id(&ack.id).unwrap().unwrap();
        assert_eq!(admitted_ack.actor.as_deref(), Some("person/operator"));
        let received = debt(&reader, u64::MAX).unwrap();
        assert_eq!(received["unknown"], true);
        assert_eq!(received["owner_action_required"], false);
        assert_eq!(received["selected_receipts"][0]["acknowledgement"]["claim"], ack.id);
        assert_eq!(received, debt(&original, u64::MAX).unwrap());
        older.claims.remove("harness.turn-acknowledged");
        assert!(matches!(classify_replicated_claim_with_registry(&ack, &older).unwrap(),
            ReplicatedClaimAdmission::UnknownKind));
        assert!(matches!(classify_replicated_claim_with_registry(&ack, st3_schema::registry()).unwrap(),
            ReplicatedClaimAdmission::Valid));
    }

    #[test]
    fn sparse_idle_cannot_erase_debt_and_historical_cut_keeps_tool_state() {
        let store = Store::open_memory("receipt-fixture").unwrap();
        let mut ledger = Ledger {
            sequence: 1,
            open: vec![entry("old", 1, Some("turn-one"))],
            ..Ledger::default()
        };
        let first = observe(&store, Some(&ledger));
        ledger.open[0].tool_outcome_unknown = true;
        ledger.open[0]
            .pending_tool_ids
            .push("side-effecting-tool".into());
        observe(&store, Some(&ledger));
        observe(&store, None);
        observe(&store, Some(&Ledger::default()));
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"][0]["tool_outcome_unknown"],
            true
        );
        assert_eq!(
            debt(&store, first.store_index).unwrap()["open"][0]["tool_outcome_unknown"],
            false
        );
        store.rebuild_claim_projections().unwrap();
        assert_eq!(
            debt(&store, first.store_index).unwrap()["open"][0]["tool_outcome_unknown"],
            false
        );
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn terminal_before_late_start_wins_but_other_revision_and_turn_remain_owed() {
        let store = Store::open_memory("terminal-fixture").unwrap();
        let old = entry("old", 1, Some("turn-one"));
        let proof = Terminal {
            obligation: old.clone(),
            outcome: "completed".into(),
            evidence_provider_incarnation: "new".into(),
            observed_at_ms: 2,
        };
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                terminal: vec![proof],
                ..Ledger::default()
            }),
        );
        observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![old.clone()],
                ..Ledger::default()
            }),
        );
        assert!(debt(&store, u64::MAX).is_none());
        let mut new_source = entry("new", 2, Some("turn-one"));
        observe(
            &store,
            Some(&Ledger {
                sequence: 2,
                open: vec![new_source.clone()],
                ..Ledger::default()
            }),
        );
        assert!(
            debt(&store, u64::MAX).is_none(),
            "exact native proof closes a delayed receipt too"
        );
        new_source.desired_revision = Some("desired-two".into());
        new_source.source_sequence = 3;
        let mut other = entry("new", 4, Some("turn-two"));
        other.native_session_id = Some("session-two".into());
        observe(
            &store,
            Some(&Ledger {
                sequence: 4,
                open: vec![new_source, other],
                ..Ledger::default()
            }),
        );
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        store.rebuild_claim_projections().unwrap();
        assert_eq!(
            debt(&store, u64::MAX).unwrap()["open"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn unidentified_successor_terminal_does_not_close_predecessor_and_reopen_is_durable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite");
        let store = Store::open(&path, "receipt-fixture").unwrap();
        let old = entry("old", 1, None);
        let first = observe(
            &store,
            Some(&Ledger {
                sequence: 1,
                open: vec![old],
                ..Ledger::default()
            }),
        );
        let new = entry("new", 2, None);
        observe(
            &store,
            Some(&Ledger {
                sequence: 2,
                terminal: vec![Terminal {
                    obligation: new,
                    outcome: "cancelled".into(),
                    evidence_provider_incarnation: "new".into(),
                    observed_at_ms: 3,
                }],
                ..Ledger::default()
            }),
        );
        drop(store);
        let reopened = Store::open(&path, "receipt-fixture").unwrap();
        assert_eq!(
            debt(&reopened, u64::MAX).unwrap()["open"][0]["provider_incarnation"],
            "old"
        );
        assert_eq!(
            debt(&reopened, first.store_index).unwrap()["open"][0]["provider_incarnation"],
            "old"
        );
    }
}
