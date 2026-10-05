//! Owner-authored input audit snapshots. Claims remain the only durable authority.
use super::*;
use smallclaims::store::canonical::{self, ClaimKey};
use st3_schema::input_sessions::{InputSessionCloseReason, InputSessionEvent, InputSessionRecord};

pub(super) const KIND: &str = "terminal.input-session";
pub(super) const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

fn decode(claim: &ClaimRecord) -> Result<InputSessionRecord> {
    serde_json::from_value(claim.body["fields"].clone()).map_err(Into::into)
}

pub(super) fn latest(connection: &Connection, subject: &str) -> Result<Option<ClaimRecord>> {
    connection
        .query_row(
            &format!(
        "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.kind='terminal.input-session' AND claims.subject=?1
         ORDER BY json_extract(claims.body,'$.fields.ordinal') DESC, {CANONICAL_ORDER_DESC} LIMIT 1"
    ),
            [subject],
            claim_from_row,
        )
        .optional()
        .map_err(Into::into)
}

/// Checked under the graph writer lock, including for generic system publication.
pub(super) fn check_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    actor: Option<&str>,
    fields: &BTreeMap<String, Value>,
) -> Result<Option<ClaimRecord>, St3Error> {
    st3_schema::input_sessions::validate_origin(subject, origin, actor, fields)
        .map_err(|e| St3Error::new(e.code, e.message))?;
    let record: InputSessionRecord = serde_json::from_value(json!(fields)).map_err(internal)?;
    let existing = transaction
        .query_row(
            &format!(
                "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.kind='terminal.input-session' AND claims.subject=?1
           AND json_extract(claims.body,'$.fields.ordinal')=?2
         ORDER BY {CANONICAL_ORDER} LIMIT 1"
            ),
            params![subject, record.ordinal],
            claim_from_row,
        )
        .optional()
        .map_err(internal)?;
    if let Some(existing) = existing {
        if decode(&existing).map_err(internal)? == record {
            return Ok(Some(existing));
        }
        return Err(St3Error::new(
            "input-session-conflict",
            "session ordinal already has different durable facts",
        ));
    }
    let Some(head) = latest(transaction, subject).map_err(internal)? else {
        let retired: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM checkpoint_claims
             WHERE subject=?1 AND kind='terminal.input-session')",
                [subject],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if retired {
            return Err(St3Error::new(
                "input-session-history-trimmed",
                "a checkpointed session identity cannot be reused",
            ));
        }
        if record.event != InputSessionEvent::Opened || record.ordinal != 0 {
            return Err(St3Error::new(
                "unknown-input-session",
                "persist the opening before progress or closure",
            ));
        }
        return Ok(None);
    };
    let prior = decode(&head).map_err(internal)?;
    if matches!(
        prior.event,
        InputSessionEvent::Closed | InputSessionEvent::Interrupted
    ) {
        return Err(St3Error::new(
            "input-session-ended",
            "an ended input session cannot be reopened",
        ));
    }
    if prior.ordinal.checked_add(1) != Some(record.ordinal)
        || record.event == InputSessionEvent::Opened
    {
        return Err(St3Error::new(
            "stale-input-session",
            "publication must extend the durable session head by one ordinal",
        ));
    }
    if record.observed_at_unix_ms < prior.observed_at_unix_ms
        || record.successful_send_bytes < prior.successful_send_bytes
        || record.successful_batches < prior.successful_batches
        || (prior.uncertain_handoff && !record.uncertain_handoff)
    {
        return Err(St3Error::new(
            "input-session-regression",
            "durable times, counters and handoff uncertainty cannot regress",
        ));
    }
    if record.event == InputSessionEvent::Interrupted
        && (record.successful_send_bytes != prior.successful_send_bytes
            || record.successful_batches != prior.successful_batches)
    {
        return Err(St3Error::new(
            "input-session-interrupted-counts",
            "interruption must retain the last durable counters",
        ));
    }
    let mut identity = record.clone();
    identity.ordinal = prior.ordinal;
    identity.event = prior.event;
    identity.observed_at_unix_ms = prior.observed_at_unix_ms;
    identity.successful_send_bytes = prior.successful_send_bytes;
    identity.successful_batches = prior.successful_batches;
    identity.uncertain_handoff = prior.uncertain_handoff;
    identity.reason = prior.reason;
    if identity != prior {
        return Err(St3Error::new(
            "input-session-identity-changed",
            "session attribution, target and opening identity are immutable",
        ));
    }
    Ok(None)
}

pub(super) fn visible(record: &InputSessionRecord, retained_from: u64) -> bool {
    !matches!(
        record.event,
        InputSessionEvent::Closed | InputSessionEvent::Interrupted
    ) || record.observed_at_unix_ms >= retained_from
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    terminal: String,
    person: Option<String>,
    retained_from: u64,
    before: ClaimKey,
}

impl Store {
    /// Returns only after the graph writer has durably committed the snapshot.
    pub fn append_input_session(
        &self,
        record: &InputSessionRecord,
    ) -> Result<ClaimRecord, St3Error> {
        record
            .validate()
            .map_err(|e| St3Error::new(e.code, e.message))?;
        let fields = serde_json::from_value(serde_json::to_value(record).map_err(internal)?)
            .map_err(internal)?;
        self.append_claim(&ClaimInput {
            subject: record.subject(),
            kind: KIND.into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
    }

    /// Keyset pagination over immutable opening source keys, never replica arrival indexes.
    /// Fleet completeness is deliberately not inferred from an empty or fully paged replica.
    pub fn input_session_history(
        &self,
        terminal: &str,
        person: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        now: u64,
    ) -> Result<Value> {
        self.read_snapshot(|_| {
            self.input_session_history_at_snapshot(terminal, person, cursor, limit, now)
        })
    }

    fn input_session_history_at_snapshot(
        &self,
        terminal: &str,
        person: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        now: u64,
    ) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(St3Error::new(
                "validation-failed",
                "input audit limit must be between 1 and 200",
            )
            .into());
        }
        anyhow::ensure!(
            now <= st3_schema::input_sessions::MAX_AUDIT_INTEGER,
            "input audit time is out of range"
        );
        let cursor: Option<Cursor> = cursor
            .map(|cursor| -> Result<Cursor> {
                anyhow::ensure!(cursor.len() <= 8192, "input audit cursor is too large");
                let cursor: Cursor = serde_json::from_slice(&hex::decode(cursor)?)?;
                anyhow::ensure!(
                    cursor.version == 1
                        && cursor.terminal == terminal
                        && cursor.person.as_deref() == person,
                    "input audit cursor belongs to another history scope"
                );
                anyhow::ensure!(
                    cursor.retained_from <= now,
                    "input audit cursor has a future retention bound"
                );
                Ok(cursor)
            })
            .transpose()
            .map_err(|error| St3Error::new("validation-failed", error.to_string()))?;
        let retained_from = cursor
            .as_ref()
            .map_or(now.saturating_sub(RETENTION_MS), |cursor| {
                cursor.retained_from.max(now.saturating_sub(RETENTION_MS))
            });
        // The existing snapshot mechanism also reuses an endpoint's already pinned reader.
        let transaction = self.readers.get();
        let before = cursor.as_ref().map(|cursor| &cursor.before);
        let order = canonical::components("claims").join(", ");
        let earlier_key = canonical::after_sql("claims", "earlier");
        let mut statement = transaction.prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.kind='terminal.input-session'
               AND json_extract(claims.body,'$.fields.terminal')=?1
               AND (?2 IS NULL OR json_extract(claims.body,'$.fields.person')=?2)
               AND NOT EXISTS (SELECT 1 FROM claims earlier
                 WHERE earlier.kind='terminal.input-session' AND earlier.subject=claims.subject
                   AND (json_extract(earlier.body,'$.fields.ordinal') < json_extract(claims.body,'$.fields.ordinal')
                     OR (json_extract(earlier.body,'$.fields.ordinal') = json_extract(claims.body,'$.fields.ordinal')
                       AND {earlier_key})))
               AND NOT EXISTS (SELECT 1 FROM claims ended
                 WHERE ended.kind='terminal.input-session' AND ended.subject=claims.subject
                   AND json_extract(ended.body,'$.fields.event') IN ('closed','interrupted')
                   AND json_extract(ended.body,'$.fields.observed_at_unix_ms') < ?3)
               AND (?4 IS NULL OR ({order}) < (length(?4),?4,?5,?6,?7,?8,?9))
             ORDER BY {CANONICAL_ORDER_DESC} LIMIT ?10"
        ))?;
        let time = before.map(|key| key.0.to_string());
        let anchors = statement
            .query_map(
                params![
                    terminal,
                    person,
                    retained_from,
                    time,
                    before.map(|key| &key.1),
                    before.map(|key| key.2),
                    before.map(|key| &key.3),
                    before.map(|key| key.4),
                    before.map(|key| &key.5),
                    limit + 1
                ],
                claim_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = anchors.len() > limit;
        let anchors = &anchors[..anchors.len().min(limit)];
        let mut items = Vec::with_capacity(anchors.len());
        for anchor in anchors {
            let head =
                latest(&transaction, &anchor.subject)?.context("input session head disappeared")?;
            let record = decode(&head)?;
            anyhow::ensure!(
                record.terminal == terminal
                    && person.is_none_or(|person| Some(person) == record.person.as_deref()),
                "input session attribution conflicts with its opening"
            );
            items.push(record);
        }
        let next_cursor = if has_more {
            let anchor = anchors.last().context("input session page has no anchor")?;
            Some(hex::encode(serde_json::to_vec(&Cursor {
                version: 1,
                terminal: terminal.into(),
                person: person.map(str::to_owned),
                retained_from,
                before: canonical::claim_key(&transaction, &anchor.id)?,
            })?))
        } else {
            None
        };
        let owner_coverage = transaction.prepare_cached(
            "SELECT DISTINCT claims.origin FROM claims WHERE claims.kind='terminal.input-session'
             AND json_extract(claims.body,'$.fields.terminal')=?1
             AND (?2 IS NULL OR json_extract(claims.body,'$.fields.person')=?2)
             AND NOT EXISTS (SELECT 1 FROM claims ended
               WHERE ended.kind='terminal.input-session' AND ended.subject=claims.subject
                 AND json_extract(ended.body,'$.fields.event') IN ('closed','interrupted')
                 AND json_extract(ended.body,'$.fields.observed_at_unix_ms') < ?3)
             ORDER BY claims.origin"
        )?.query_map(params![terminal, person, retained_from], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({ "api_version": "st3.client.v0", "kind": "terminal-input-audit", "terminal": terminal,
            "items": items, "retained_from": retained_from, "complete": false,
            "next_cursor": next_cursor, "owner_coverage": owner_coverage }),
        )
    }

    /// A prior local process cannot close precisely: keep its last proven counts and epoch.
    pub fn recover_input_sessions(&self, owner_epoch: &str, now: u64) -> Result<usize, St3Error> {
        Uuid::parse_str(owner_epoch)
            .map_err(|_| St3Error::new("invalid-input-session", "recovery epoch must be a UUID"))?;
        if now > st3_schema::input_sessions::MAX_AUDIT_INTEGER {
            return Err(St3Error::new(
                "invalid-input-session",
                "recovery time is out of range",
            ));
        }
        self.connection
            .batched(|transaction| -> Result<usize, St3Error> {
                let subjects = transaction
                    .prepare_cached(
                        "SELECT DISTINCT claims.subject FROM claims
                 WHERE claims.kind='terminal.input-session' AND claims.origin=?1
                   AND json_extract(claims.body,'$.fields.owner_epoch')<>?2
                   AND json_extract(claims.body,'$.fields.event') IN ('opened','checkpoint')
                   AND NOT EXISTS (SELECT 1 FROM claims ended
                     WHERE ended.kind='terminal.input-session' AND ended.subject=claims.subject
                       AND json_extract(ended.body,'$.fields.event') IN ('closed','interrupted'))
                 ORDER BY claims.subject",
                    )
                    .map_err(internal)?
                    .query_map(params![&self.origin, owner_epoch], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(internal)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(internal)?;
                let mut recovered = 0;
                for subject in subjects {
                    let head = latest(transaction, &subject)
                        .map_err(internal)?
                        .ok_or_else(|| St3Error::new("internal", "input session has no head"))?;
                    let mut record = decode(&head).map_err(internal)?;
                    if record.owner != self.origin
                        || record.owner_epoch == owner_epoch
                        || matches!(
                            record.event,
                            InputSessionEvent::Closed | InputSessionEvent::Interrupted
                        )
                    {
                        continue;
                    }
                    record.ordinal = record.ordinal.checked_add(1).ok_or_else(|| {
                        St3Error::new("input-session-overflow", "session ordinal exhausted")
                    })?;
                    record.event = InputSessionEvent::Interrupted;
                    record.reason = Some(InputSessionCloseReason::OwnerRestarted);
                    record.uncertain_handoff = true;
                    record.observed_at_unix_ms = now.max(record.observed_at_unix_ms);
                    let body = json!({"fields": record, "evidence": []});
                    append_claim_tx(
                        transaction,
                        &self.origin,
                        &subject,
                        KIND,
                        None,
                        &body,
                        &[head.id],
                        None,
                    )
                    .map_err(claim_append_error)?;
                    recovered += 1;
                }
                Ok(recovered)
            })
            .map_err(|error| St3Error::new("internal", error))?
    }
}
