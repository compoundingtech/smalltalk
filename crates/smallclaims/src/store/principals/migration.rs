//! One finite bootstrap inventory, with cursor and candidate inserts committed together.
use super::*;

const LEGACY_PAGE: usize = 64;
const ENVELOPE_PAGE: usize = 64;
const CLAIM_PAGE: usize = 64;

pub(crate) struct SignatureEnvelopeWork {
    rowid: i64,
    hash: String,
    payload: ReplicaEnvelopePayload,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SignatureMigration {
    pub phase: String,
    pub cursor: i64,
    pub legacy_through: i64,
    pub envelopes_through: i64,
    pub fault: Option<String>,
    pub legacy_fault: Option<String>,
    pub processed: usize,
    pub envelope_row: i64,
    pub claim_offset: usize,
    /// The complete pre-candidate model still serves authorization during this upgrade.
    pub serving_legacy: bool,
    pub started_at_unix_ms: u128,
    pub advanced_at_unix_ms: u128,
    pub completed_work: u64,
}

impl SignatureMigration {
    pub fn progress_description(&self) -> String {
        let elapsed = now_ms().saturating_sub(self.started_at_unix_ms).max(1) as f64 / 1_000.0;
        let rate = self.completed_work as f64 / elapsed;
        let remaining = match self.phase.as_str() {
            "legacy" => Some(self.legacy_through.saturating_sub(self.cursor)),
            _ => None,
        };
        let estimate = remaining.filter(|_| rate > 0.0).map(|remaining|format!("; legacy inventory segment estimate {:.1}s (rowid gaps may overestimate; envelopes and verdict reconciliation excluded)",remaining as f64/rate)).unwrap_or_else(|| "; remaining complete-build duration unknown until actual envelope/candidate work finishes".into());
        format!(
            "{} committed inventory positions/candidate judgments in {:.1}s ({:.1} work units/s), last advance {}ms ago{}",
            self.completed_work,
            elapsed,
            rate,
            now_ms().saturating_sub(self.advanced_at_unix_ms),
            estimate
        )
    }
    pub fn done(&self) -> bool {
        self.phase == "done" && self.fault.is_none()
    }
    pub fn unavailable_reason(&self) -> Option<String> {
        if let Some(fault) = &self.fault {
            Some(format!("signature-state-corrupt: {fault}"))
        } else if !self.done() {
            Some(format!(
                "signature-migration-incomplete: {} cursor {}",
                self.phase, self.cursor
            ))
        } else {
            None
        }
    }
}

/// Preserve the complete old projection without copying historical rows. Renames, the
/// empty candidate projection and its control row commit together. Existing indexes stay
/// attached to legacy tables; no history-sized index is built or dropped at first open.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let upgrading: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='claim_signatures') AND NOT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='claim_signature_migration')", [], |row| row.get(0))?;
    if upgrading {
        for table in [
            "claim_signatures",
            "claim_verdicts",
            "claim_verdict_links",
            "claim_verdict_queue",
            "claim_verdict_fresh",
        ] {
            transaction.execute_batch(&format!("ALTER TABLE {table} RENAME TO legacy_{table};"))?;
        }
    }
    transaction.execute_batch(PRINCIPAL_SCHEMA)?;
    // Compatibility with the earlier unpublished candidate-schema revision.
    let has_model: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('claim_signature_migration') WHERE name='serving_legacy')", [], |row| row.get(0))?;
    if !has_model {
        transaction.execute_batch("ALTER TABLE claim_signature_migration ADD COLUMN serving_legacy INTEGER NOT NULL DEFAULT 0 CHECK(serving_legacy IN (0,1));")?;
    }
    let has_presence: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('claim_signature_migration') WHERE name='legacy_present')", [], |row|row.get(0))?;
    if !has_presence {
        transaction.execute_batch("ALTER TABLE claim_signature_migration ADD COLUMN legacy_present INTEGER NOT NULL DEFAULT 0 CHECK(legacy_present IN (0,1));")?;
    }
    for column in [
        "started_at_unix_ms",
        "advanced_at_unix_ms",
        "completed_work",
    ] {
        let has:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('claim_signature_migration') WHERE name=?1)",[column],|row|row.get(0))?;
        if !has {
            transaction.execute_batch(&format!("ALTER TABLE claim_signature_migration ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"))?;
        }
    }
    let has_legacy_fault: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('claim_signature_migration') WHERE name='legacy_fault')",[],|row|row.get(0))?;
    if !has_legacy_fault {
        transaction
            .execute_batch("ALTER TABLE claim_signature_migration ADD COLUMN legacy_fault TEXT;")?;
    }
    initialize_selection(&transaction)?;
    transaction.execute("UPDATE claim_signature_migration SET started_at_unix_ms=?1,advanced_at_unix_ms=?1 WHERE id=1 AND started_at_unix_ms=0",[i64::try_from(now_ms())?])?;
    // Previously released builds accept only versions through 19. Commit the incompatibility marker
    // with the renamed relations, before any later runtime-schema work can fail.
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != CURRENT_SCHEMA_VERSION {
        transaction.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn initialize_selection(connection: &Connection) -> Result<()> {
    if connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM claim_signature_migration WHERE id=1)",
        [],
        |row| row.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    // MAX(rowid) uses each table's rowid index. Opening never walks payload history.
    let serving_legacy = legacy::table_exists(connection)?;
    let table = if serving_legacy {
        "legacy_claim_signatures"
    } else {
        "claim_signatures"
    };
    connection.execute_batch(&format!("INSERT OR IGNORE INTO claim_signature_migration(id,phase,cursor,legacy_through,envelopes_through,serving_legacy,legacy_present,started_at_unix_ms,advanced_at_unix_ms)
        SELECT 1,CASE WHEN legacy=0 AND envelopes=0 THEN 'done' ELSE 'legacy' END,0,legacy,envelopes,
               CASE WHEN legacy=0 AND envelopes=0 THEN 0 ELSE {presence} END,{presence},{now},{now}
        FROM (SELECT COALESCE((SELECT MAX(rowid) FROM {table}),0) AS legacy,
                     COALESCE((SELECT MAX(rowid) FROM replica_envelopes),0) AS envelopes);", presence=i64::from(serving_legacy),now=i64::try_from(now_ms())?))?;
    Ok(())
}

fn state(connection: &Connection) -> Result<SignatureMigration> {
    let report=connection.prepare_cached("SELECT phase,cursor,legacy_through,envelopes_through,fault,envelope_row,claim_offset,serving_legacy,started_at_unix_ms,advanced_at_unix_ms,completed_work,legacy_fault FROM claim_signature_migration WHERE id=1")?
        .query_row([], |row| Ok(SignatureMigration {
            phase: row.get(0)?, cursor: row.get(1)?, legacy_through: row.get(2)?,
            envelopes_through: row.get(3)?, fault: row.get(4)?, processed: 0, envelope_row:row.get(5)?,claim_offset:row.get(6)?,serving_legacy:row.get(7)?, started_at_unix_ms:row.get::<_,u64>(8)? as u128, advanced_at_unix_ms:row.get::<_,u64>(9)? as u128, completed_work:row.get(10)?,legacy_fault:row.get(11)?,
        }))?;
    anyhow::ensure!(
        matches!(
            report.phase.as_str(),
            "legacy" | "envelopes" | "judging" | "done"
        ),
        "signature-state-corrupt: unknown bootstrap phase {}",
        report.phase
    );
    anyhow::ensure!(
        !matches!(report.phase.as_str(), "judging" | "done")
            || (report.cursor == 0 && report.envelope_row == 0 && report.claim_offset == 0),
        "signature-state-corrupt: completed inventory has pending cursors"
    );
    anyhow::ensure!(
        report.phase != "done" || !report.serving_legacy,
        "signature-state-corrupt: completed bootstrap cannot serve legacy authorization"
    );
    Ok(report)
}

pub(super) fn judging_available(connection: &Connection) -> Result<bool> {
    let state = state(connection)?;
    Ok(state.fault.is_none() && matches!(state.phase.as_str(), "done" | "judging"))
}

pub(super) fn record_fault(connection: &Connection, reason: &str) -> Result<()> {
    connection.execute(
        "UPDATE claim_signature_migration SET fault=?1 WHERE id=1",
        [reason],
    )?;
    Ok(())
}

pub(super) fn finish_judging(connection: &Connection) -> Result<()> {
    connection.execute("UPDATE claim_signature_migration SET phase='done',serving_legacy=0 WHERE id=1 AND phase='judging' AND cursor=0 AND envelope_row=0 AND claim_offset=0 AND fault IS NULL AND NOT EXISTS(SELECT 1 FROM claim_verdict_queue) AND NOT EXISTS(SELECT 1 FROM claim_verdict_fresh)", [])?;
    Ok(())
}

pub(super) fn unavailable(connection: &Connection) -> Result<Option<String>> {
    Ok(state(connection)?.unavailable_reason())
}

impl Store {
    pub fn signature_migration(&self) -> Result<SignatureMigration> {
        state(&self.readers.get())
    }

    /// Advance a finite inventory page. Payload decoding and content hashing precede the
    /// background writer loan. Other reads and writes run between committed pages.
    /// A corrupt local row persists its identity/reason and stops this bootstrap; no retry.
    pub fn advance_signature_migration(&self) -> Result<SignatureMigration> {
        let result = self.advance_signature_migration_inner();
        if let Err(error) = &result {
            record_fault(
                &self.connection.write(),
                &format!("inventory page: {error:#}"),
            )?;
        }
        result
    }
    fn advance_signature_migration_inner(&self) -> Result<SignatureMigration> {
        let captured = self.signature_migration()?;
        if captured.done() || captured.phase == "judging" || captured.fault.is_some() {
            return Ok(captured);
        }
        // This is the continuation of one scan, not a read cache: a large envelope is
        // decoded once per run and its durable claim offset lets a restart reconstruct it.
        let mut work = self
            .signature_migration_work
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut candidates = Vec::new();
        let mut orphan_checks = Vec::new();
        let mut through = captured.cursor;
        let mut envelope_row = captured.envelope_row;
        let mut offset = captured.claim_offset;
        let mut count = 0;
        let mut fault = None;
        if captured.phase == "legacy" {
            let table = if captured.serving_legacy {
                "legacy_claim_signatures"
            } else {
                "claim_signatures"
            };
            let rows=self.readers.get().prepare_cached(&format!("SELECT rowid,claim_id,signature FROM {table} WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3"))?
                .query_map(params![captured.cursor,captured.legacy_through,LEGACY_PAGE as i64],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (rowid, claim_id, text) in rows {
                match serde_json::from_str::<ClaimSignature>(&text) {
                    Ok(signature) => candidates.push((claim_id.clone(), signature, None)),
                    Err(error) => {
                        fault = Some(format!(
                            "claim_signatures rowid {rowid}, claim {claim_id}: {error}"
                        ));
                        break;
                    }
                }
                orphan_checks.push(claim_id);
                through = rowid;
                count += 1;
            }
        } else {
            let mut envelopes = 0;
            let mut claims = 0;
            while envelopes < ENVELOPE_PAGE && claims < CLAIM_PAGE {
                let metadata=self.readers.get().prepare_cached("SELECT rowid,writer,sequence,envelope_hash,previous_hash,accepted_at_unix_ms,receipt_state,batch_id FROM replica_envelopes WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT 1")?
                    .query_row(params![through,captured.envelopes_through],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,String>(5)?,row.get::<_,String>(6)?,row.get::<_,Option<String>>(7)?))).optional()?;
                let Some((rowid, writer, sequence, hash, previous, accepted, receipt, batch)) =
                    metadata
                else {
                    break;
                };
                if rowid != envelope_row {
                    offset = 0;
                    envelope_row = rowid;
                }
                if receipt != "validated" && !(receipt == "degraded" && batch.is_some()) {
                    through = rowid;
                    offset = 0;
                    envelope_row = 0;
                    *work = None;
                    envelopes += 1;
                    count += 1;
                    continue;
                }
                if work
                    .as_ref()
                    .is_none_or(|work| work.rowid != rowid || work.hash != hash)
                {
                    let decoded = (|| -> Result<ReplicaEnvelopePayload> {
                        let bytes: crate::claim::EnvelopePayload = self
                            .readers
                            .get()
                            .prepare_cached("SELECT payload FROM replica_envelopes WHERE rowid=?1")?
                            .query_row([rowid], |row| row.get(0))?;
                        let raw = bytes.bytes()?;
                        let expected = crate::hash::replica_envelope_hash(
                            &writer,
                            sequence,
                            previous.as_deref(),
                            accepted.parse()?,
                            raw,
                        );
                        anyhow::ensure!(
                            expected == hash,
                            "stored envelope hash does not match its payload"
                        );
                        Ok(ciborium::from_reader(raw)?)
                    })();
                    match decoded {
                        Ok(payload) => {
                            *work = Some(SignatureEnvelopeWork {
                                rowid,
                                hash,
                                payload,
                            })
                        }
                        Err(error) => {
                            fault = Some(format!(
                                "replica_envelopes rowid {rowid}, writer {writer}: {error:#}"
                            ));
                            *work = None;
                            break;
                        }
                    }
                }
                let payload = &work.as_ref().unwrap().payload;
                anyhow::ensure!(
                    offset <= payload.batch.claims.len(),
                    "signature-state-corrupt: claim offset exceeds envelope length"
                );
                let end = (offset + CLAIM_PAGE - claims).min(payload.batch.claims.len());
                for claim in &payload.batch.claims[offset..end] {
                    if claim_id_is_content_hash(claim)?
                        && let Some(signature) = payload.claim_signatures.get(&claim.id)
                    {
                        candidates.push((
                            claim.id.clone(),
                            signature.clone(),
                            Some(writer.clone()),
                        ));
                    }
                }
                claims += end - offset;
                count += (end - offset).max(1);
                offset = end;
                if end < payload.batch.claims.len() {
                    break;
                }
                through = rowid;
                offset = 0;
                envelope_row = 0;
                *work = None;
                envelopes += 1;
            }
        }
        let mut connection = self.connection.write_background();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = state(&tx)?;
        if current.phase != captured.phase
            || current.cursor != captured.cursor
            || current.envelope_row != captured.envelope_row
            || current.claim_offset != captured.claim_offset
            || current.fault.is_some()
        {
            return Ok(current);
        }
        // Rejecting a damaged page leaves both its cursor and its inventory unchanged.
        if let Some(fault) = &fault {
            tx.execute(
                "UPDATE claim_signature_migration SET fault=?1,legacy_fault=CASE WHEN serving_legacy=1 AND phase='legacy' THEN ?1 ELSE legacy_fault END WHERE id=1",
                [fault],
            )?;
        } else {
            for claim_id in orphan_checks {
                if claim_row(&tx, &claim_id)?.is_none() {
                    forget_claim_tx(&tx, &claim_id)?;
                }
            }
            for (claim_id, signature, writer) in candidates {
                if claim_row(&tx, &claim_id)?.is_none() {
                    continue;
                }
                store_inventory_signature_tx(&tx, &claim_id, &signature, writer.as_deref())?;
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO claim_verdict_fresh(claim_id) VALUES (?1)",
                )?
                .execute([claim_id])?;
            }
            if count == 0 {
                let next = if captured.phase == "legacy" {
                    "envelopes"
                } else {
                    "judging"
                };
                tx.execute(
                    "UPDATE claim_signature_migration SET phase=?1,cursor=0,envelope_row=0,claim_offset=0 WHERE id=1",
                    [next],
                )?;
            } else {
                tx.execute(
                    "UPDATE claim_signature_migration SET cursor=?1,envelope_row=?2,claim_offset=?3 WHERE id=1",
                    params![through,envelope_row,offset as i64],
                )?;
            }
        }
        if fault.is_none() {
            note_work(&tx, count)?;
        }
        let mut report = state(&tx)?;
        report.processed = count;
        tx.commit()?;
        Ok(report)
    }

    pub fn fail_signature_migration(&self, reason: &str) -> Result<()> {
        record_fault(&self.connection.write(), reason)
    }

    /// After repairing the named local row, explicitly resume the stopped inventory. If the
    /// row is still damaged the next page persists the same fault; it never marks completion.
    pub fn resume_signature_migration(&self) -> Result<()> {
        *self
            .signature_migration_work
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.connection.write().execute(
            "UPDATE claim_signature_migration SET fault=NULL,legacy_fault=NULL WHERE id=1",
            [],
        )?;
        Ok(())
    }
}

impl Store {
    /// Retire a page of the obsolete authorization projection after permanent activation.
    /// Each loan removes at most 64 rows. Tables and indexes are dropped only when empty.
    pub fn retire_legacy_signature_page(&self) -> Result<bool> {
        let mut connection = self.connection.write_background();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if legacy::active(&tx)? || !legacy::present(&tx)? {
            return Ok(false);
        }
        for table in legacy::TABLES {
            let removed = if table == "legacy_claim_verdict_links" {
                tx.execute("DELETE FROM legacy_claim_verdict_links WHERE (link,claim_id) IN (SELECT link,claim_id FROM legacy_claim_verdict_links LIMIT 64)", [])?
            } else {
                tx.execute(&format!("DELETE FROM {table} WHERE claim_id IN (SELECT claim_id FROM {table} LIMIT 64)"), [])?
            };
            if removed != 0 {
                tx.commit()?;
                return Ok(true);
            }
        }
        for table in legacy::TABLES {
            tx.execute_batch(&format!("DROP TABLE {table};"))?;
        }
        tx.execute(
            "UPDATE claim_signature_migration SET legacy_present=0 WHERE id=1",
            [],
        )?;
        tx.execute("DELETE FROM meta WHERE key='claim_verdict_roots'", [])?;
        tx.commit()?;
        Ok(false)
    }
}

pub(super) fn note_work(connection: &Connection, work: usize) -> Result<()> {
    connection.execute("UPDATE claim_signature_migration SET completed_work=completed_work+?1,advanced_at_unix_ms=?2 WHERE id=1 AND phase<>'done'",params![i64::try_from(work)?,i64::try_from(now_ms())?])?;
    Ok(())
}
