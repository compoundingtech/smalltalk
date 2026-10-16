//! Conversation routing needs runtime identities, not every agent's harness and claim history.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub subject: String,
    pub incarnation: Option<String>,
    pub runtime: Option<String>,
    pub origin: Option<String>,
}

pub(super) type Owners = BTreeMap<String, Owner>;

#[cfg(test)]
thread_local! {
    static BEFORE_OWNER_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_MESSAGE_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

// Keep the existing physical owner window before selecting the kinds the fallback renders.
// Filtering kinds before LIMIT would incorrectly bring old entries back into the view.
const TIMELINE_OWNER_IDS: &str = "WITH owner_window AS (
    SELECT id, kind, store_index FROM claims
    WHERE subject=?1 AND store_index>0 AND store_index<?2
    ORDER BY store_index DESC LIMIT 10000
) SELECT id FROM owner_window
  WHERE kind IN ('runtime.observed', 'harness.observed', 'harness.diagnostic',
                 'harness.usage', 'message.sent')
  ORDER BY store_index DESC";

// Only indexed identity/position and endpoint metadata: an idle wake must not copy or decode
// unrelated transcript/status/message payloads. Both wrapped and legacy endpoint indexes exist.
const CONVERSATION_CHANGED: &str = "SELECT
    EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_index
      WHERE subject=?1 AND store_index>?2 AND store_index<=?3)
    OR EXISTS(SELECT 1 FROM claims INDEXED BY claims_message_to_order_index
      WHERE kind='message.sent' AND json_extract(body,'$.fields.to')=?1
        AND store_index>?2 AND store_index<=?3)
    OR EXISTS(SELECT 1 FROM claims INDEXED BY claims_message_from_index
      WHERE kind='message.sent' AND json_extract(body,'$.fields.from')=?1
        AND store_index>?2 AND store_index<=?3)
    OR EXISTS(SELECT 1 FROM claims INDEXED BY claims_message_legacy_to_index
      WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL
        AND json_extract(body,'$.to')=?1 AND store_index>?2 AND store_index<=?3)
    OR EXISTS(SELECT 1 FROM claims INDEXED BY claims_message_legacy_from_index
      WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL
        AND json_extract(body,'$.from')=?1 AND store_index>?2 AND store_index<=?3)
    OR EXISTS(SELECT 1 FROM local_observations INDEXED BY local_observations_subject_id_index
      WHERE subject=?1 AND id>?4 AND id<=?5)";

/// SQL text is fixed by the schema registry, not by the conversation or its history.
static IDENTITY_FIELD_READS: std::sync::LazyLock<[String; 2]> = std::sync::LazyLock::new(|| {
    ["runtime_id", "incarnation_id"].map(|field| {
        let predicate = identity_field_predicate(field);
        let path = format!("CASE WHEN json_type(claims.body,'$.fields') IS NULL
            THEN '$.{field}' ELSE '$.fields.{field}' END");
        format!(
            "SELECT CASE WHEN json_type(claims.body,{path})='text'
                    THEN json_extract(claims.body,{path}) END FROM claims
             INDEXED BY claims_identity_{field}_accepted_index
             JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND +claims.store_index<=?2
               AND {ACTUAL_STATE_CLAIM} AND {predicate} AND {UNREPAIRED_CLAIM}
               AND claims.kind NOT LIKE 'harness.%'
             ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
        )
    })
});

/// Both exact-subject and fleet-map routing share these bounded identity seeks.
fn conversation_owner_from_connection(
    connection: &Connection,
    subject: &str,
    through: u64,
) -> Result<Option<Owner>> {
    let exists: bool = connection.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND store_index<=?2)",
    )?.query_row(params![subject, through], |row| row.get(0))?;
    if !exists {
        return Ok(None);
    }
    let read = |query: &str| -> Result<Option<String>> {
        Ok(connection.prepare_cached(query)?
            .query_row(params![subject, through], |row| row.get::<_, Option<String>>(0))
            .optional()?.flatten())
    };
    let runtime = read(&IDENTITY_FIELD_READS[0])?;
    let incarnation = read(&IDENTITY_FIELD_READS[1])?;
    let (_, origin, _) = identity_actual_source_at(connection, subject, through)?;
    Ok(Some(Owner {
        subject: subject.into(),
        incarnation,
        runtime,
        origin,
    }))
}

impl Store {
    pub(crate) fn conversation_local_position(&self) -> Result<u64> {
        Ok(self.readers.get().prepare_cached(
            "SELECT COALESCE(MAX(id),0) FROM local_observations",
        )?.query_row([], |row| row.get(0))?)
    }

    /// Whether a fixed graph/local frontier contains a relevant change. No payload leaves SQL,
    /// and no retained snapshot spans native reads, display preparation, or a long-poll wait.
    pub(crate) fn conversation_changed_at(
        &self, owner: &str, after: u64, through: u64, local_after: u64, local_through: u64,
    ) -> Result<bool> {
        if after == through && local_after == local_through {
            return Ok(false);
        }
        Ok(self.readers.get().prepare_cached(CONVERSATION_CHANGED)?
            .query_row(params![owner, after, through, local_after, local_through], |row| row.get(0))?)
    }

    /// One subject at the captured logical frontier, without building the fleet owner map.
    /// Sparse identity fields use partial accepted-time indexes, not scans over heartbeats.
    pub fn conversation_owner_at(&self, subject: &str, through: u64) -> Result<Option<Owner>> {
        selected_index(self.index()?, Some(through)).map_err(anyhow::Error::new)?;
        let connection = self.readers.get();
        conversation_owner_from_connection(&connection, subject, through)
    }

    /// A volatile, rebuildable identity map at a logical claim frontier. Only changed agents
    /// are folded on subsequent reads. No lock or SQLite transaction spans the whole build,
    /// native transcript I/O, display preparation, or a long-poll wait.
    pub(crate) fn conversation_owners_at(&self, through: u64) -> Result<Arc<Owners>> {
        selected_index(self.index()?, Some(through)).map_err(anyhow::Error::new)?;
        let (generation, previous) = {
            let cache = self
                .smalltalk
                .conversation_owners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((_, owners)) = cache.iter().find(|(at, _)| *at == through) {
                return Ok(Arc::clone(owners));
            }
            let previous = cache
                .iter()
                .filter(|(at, _)| *at < through)
                .max_by_key(|(at, _)| *at)
                .map(|(at, owners)| (*at, Arc::clone(owners)));
            (
                self.smalltalk
                    .conversation_owner_generation
                    .load(std::sync::atomic::Ordering::Relaxed),
                previous,
            )
        };
        // Harness reports do not change a runtime identity. Copy only small metadata while the
        // SELECT is live. Rebuild a large catch-up instead of retaining an unbounded delta.
        let changed = previous
            .as_ref()
            .map(|(after, owners)| -> Result<_> {
                let connection = self.readers.get();
                let rows = connection
                    .prepare_cached(
                        "SELECT subject, kind FROM claims WHERE store_index>?1 AND store_index<=?2
                 ORDER BY store_index LIMIT 4097",
                    )?
                    .query_map(params![after, through], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((rows.len() <= 4096).then(|| {
                    rows.into_iter()
                        .filter(|(name, kind)| {
                            name.starts_with("agent/")
                                && (!owners.contains_key(name)
                                    || (!kind.starts_with("harness.")
                                        && !matches!(
                                            kind.as_str(),
                                            "intent.desired"
                                                | "runtime.readiness-deadline-reached"
                                                | "reconcile.fault"
                                        )))
                        })
                        .map(|(name, _)| name)
                        .collect::<BTreeSet<_>>()
                }))
            })
            .transpose()?
            .flatten();
        let (mut owners, names) = match (previous, changed) {
            (Some((_, previous)), Some(names)) => ((*previous).clone(), names),
            _ => {
                let connection = self.readers.get();
                let names = connection
                    .prepare_cached(RANGE_SUBJECTS)?
                    .query_map(params![through, "agent/", "agent0"], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()?;
                (Owners::new(), names)
            }
        };
        for subject in names {
            let connection = self.readers.get();
            if let Some(owner) = conversation_owner_from_connection(&connection, &subject, through)? {
                owners.insert(subject, owner);
            }
        }
        let owners = Arc::new(owners);
        #[cfg(test)]
        BEFORE_OWNER_PUBLISH.with(|pause| {
            if let Some(pause) = pause.borrow_mut().take() {
                pause();
            }
        });
        let mut cache = self
            .smalltalk
            .conversation_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A repair may have cleared the cache while this build released its SQL readers.
        // The generation is changed under this same lock, so an older build cannot republish.
        if self
            .smalltalk
            .conversation_owner_generation
            .load(std::sync::atomic::Ordering::Relaxed)
            != generation
        {
            return Ok(owners);
        }
        if let Some((_, published)) = cache.iter().find(|(at, _)| *at == through) {
            return Ok(Arc::clone(published));
        }
        cache.push_back((through, Arc::clone(&owners)));
        if cache.len() > 8 {
            cache.pop_front();
        }
        Ok(owners)
    }

    /// Preserve the existing newest-10,000 fleet-send window, but read payloads only for the
    /// selected endpoint and (for replay) after its cursor. Wrapped sends use existing endpoint
    /// indexes; partial indexes cover legacy root bodies.
    pub(crate) fn conversation_messages_at(
        &self,
        owner: &str,
        before: Option<u64>,
        after: u64,
    ) -> Result<Vec<ClaimRecord>> {
        self.conversation_messages_window(owner, before, after, None)
    }

    /// Bounded indexed post-snapshot sends for native/graph read reconciliation.
    pub(crate) fn conversation_message_delta_at(
        &self,
        owner: &str,
        before: Option<u64>,
        after: u64,
        limit: usize,
    ) -> Result<Vec<ClaimRecord>> {
        self.conversation_messages_window(owner, before, after, Some(limit))
    }

    fn conversation_messages_window(
        &self,
        owner: &str,
        before: Option<u64>,
        after: u64,
        limit: Option<usize>,
    ) -> Result<Vec<ClaimRecord>> {
        let before = before.unwrap_or(i64::MAX as u64);
        let ids = {
            let connection = self.readers.get();
            let floor = if limit.is_some() {
                after.saturating_add(1)
            } else {
                connection
                    .prepare_cached(
                        "SELECT store_index FROM claims WHERE kind='message.sent' AND store_index<?1
                     ORDER BY store_index DESC LIMIT 1 OFFSET 9999",
                    )?
                    .query_row([before], |row| row.get::<_, u64>(0))
                    .optional()?
                    .unwrap_or(0)
                    .max(after.saturating_add(1))
            };
            connection
                .prepare_cached(
                    "WITH selected AS (
                    SELECT store_index FROM claims INDEXED BY claims_message_to_order_index
                    WHERE kind='message.sent' AND json_extract(body,'$.fields.to')=?1
                      AND store_index>=?2 AND store_index<?3
                    UNION
                    SELECT store_index FROM claims INDEXED BY claims_message_from_index
                    WHERE kind='message.sent' AND json_extract(body,'$.fields.from')=?1
                      AND store_index>=?2 AND store_index<?3
                    UNION
                    SELECT store_index FROM claims INDEXED BY claims_message_legacy_to_index
                    WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL
                      AND json_extract(body,'$.to')=?1 AND store_index>=?2 AND store_index<?3
                    UNION
                    SELECT store_index FROM claims INDEXED BY claims_message_legacy_from_index
                    WHERE kind='message.sent' AND json_type(body,'$.fields') IS NULL
                      AND json_extract(body,'$.from')=?1 AND store_index>=?2 AND store_index<?3
                 ) SELECT claims.id FROM claims JOIN selected USING(store_index)
                   ORDER BY claims.store_index DESC LIMIT ?4",
                )?
                .query_map(params![owner, floor, before, limit.map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX))], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        self.conversation_claims_by_ids(&ids)
    }

    /// The owner records the stored-history fallback actually renders, selected inside the
    /// same latest-10,000 physical-row window as the previous claims_page read. Timeline
    /// operations keep their separate incarnation window and missing-history checks.
    pub(crate) fn conversation_timeline_owner_claims_at(
        &self,
        owner: &str,
        before: Option<u64>,
    ) -> Result<Vec<ClaimRecord>> {
        let ids = {
            let connection = self.readers.get();
            connection
                .prepare_cached(TIMELINE_OWNER_IDS)?
                .query_map(params![owner, before.unwrap_or(i64::MAX as u64)], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        self.conversation_claims_by_ids(&ids)
    }

    fn conversation_claims_by_ids(&self, ids: &[String]) -> Result<Vec<ClaimRecord>> {
        // Copy at most 64 rows, stopping after 1 MiB plus one complete row, as raw strings.
        // Release the statement and connection
        // before JSON decoding. This avoids both a fleet-wide live cursor and one query per
        // message; no read transaction spans the full result or native preparation.
        let mut claims = Vec::new();
        let mut offset = 0;
        while offset < ids.len() {
            let end = (offset + 64).min(ids.len());
            let batch = &ids[offset..end];
            let raw = {
                let connection = self.readers.get();
                let mut statement = connection.prepare_cached(&format!(
                    "SELECT {CLAIM_COLUMNS} FROM claims
                     WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY store_index DESC",
                ))?;
                let mut rows = statement.query([serde_json::to_string(batch)?])?;
                let mut copied = Vec::new();
                let mut bytes = 0;
                while let Some(row) = rows.next()? {
                    let entry = (
                        ClaimRecord {
                            id: row.get(0)?,
                            store_index: row.get(1)?,
                            batch_id: row.get(2)?,
                            subject: row.get(3)?,
                            kind: row.get(4)?,
                            origin: row.get(5)?,
                            actor: row.get(6)?,
                            body: Value::Null,
                            predecessors: Vec::new(),
                            operation_id: None,
                            request_digest: None,
                            accepted_at_unix_ms: row
                                .get::<_, String>(9)?
                                .parse()
                                .unwrap_or_default(),
                        },
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    );
                    bytes += entry.1.len() + entry.2.len();
                    copied.push(entry);
                    if bytes >= 1024 * 1024 {
                        break;
                    }
                }
                copied
            };
            let Some((last, _, _)) = raw.last() else {
                offset = end;
                continue;
            };
            offset += batch
                .iter()
                .position(|id| id == &last.id)
                .expect("selected batch ID")
                + 1;
            #[cfg(test)]
            BEFORE_MESSAGE_DECODE.with(|pause| {
                if let Some(pause) = pause.borrow_mut().take() {
                    pause();
                }
            });
            for (mut claim, body, predecessors) in raw {
                claim.body = serde_json::from_str(&body).unwrap_or(Value::Null);
                claim.predecessors = serde_json::from_str(&predecessors).unwrap_or_default();
                if let Some((id, digest)) = operation_parts(&claim.body) {
                    claim.operation_id = Some(id.to_owned());
                    claim.request_digest = Some(digest.to_owned());
                }
                claims.push(claim);
            }
        }
        Ok(claims)
    }

    /// Session membership reads indexed acceptance times without copying runtime bodies into Rust.
    pub(crate) fn conversation_runtime_span(
        &self,
        owner: &str,
        incarnation: &str,
    ) -> Result<(Option<u128>, Option<u128>)> {
        let connection = self.readers.get();
        let started: Option<String> = connection
            .prepare_cached(&format!(
            "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_incarnation_accepted_index
             WHERE subject=?1 AND {INCARNATION_OF_CLAIM}=?2 AND kind='runtime.observed'
               AND json_type(body,'$.fields.incarnation_id')='text'
             ORDER BY length(accepted_at_unix_ms), accepted_at_unix_ms LIMIT 1",
        ))?
            .query_row(params![owner, incarnation], |row| row.get(0))
            .optional()?;
        let ended = started.as_ref().map(|started| -> Result<Option<String>> {
            Ok(connection.prepare_cached(&format!(
                "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_kind_accepted_index
                 WHERE subject=?1 AND kind='runtime.observed' AND json_type(body,'$.fields.incarnation_id')='text'
                   AND {INCARNATION_OF_CLAIM}!=?2
                   AND (length(accepted_at_unix_ms),accepted_at_unix_ms)>(length(?3),?3)
                 ORDER BY length(accepted_at_unix_ms),accepted_at_unix_ms LIMIT 1",
            ))?.query_row(params![owner, incarnation, started], |row| row.get(0)).optional()?)
        }).transpose()?.flatten();
        Ok((
            started.and_then(|value| value.parse().ok()),
            ended.and_then(|value| value.parse().ok()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic historical SQL rows exercise legacy kinds/shapes that current admission
    /// correctly rejects. The admitted scaffold supplies real batch/replica ordering metadata;
    /// rewriting this fixture is not evidence that these rows can be admitted or replicated.
    fn synthetic_owner_history_row(
        tx: &Transaction<'_>,
        origin: &str,
        subject: &str,
        kind: &str,
        body: &Value,
        predecessors: &[String],
    ) -> Result<ClaimRecord> {
        let mut claim = append_claim_tx(tx, origin, subject, "runtime.observed", None,
            &json!({"fields":{"status":"running","runtime_id":"pty/ada",
                "incarnation_id":"ada-fixture"}}), predecessors, None)?;
        tx.execute("UPDATE claims SET kind=?1,body=?2 WHERE id=?3",
            params![kind, body.to_string(), claim.id])?;
        claim.kind = kind.into();
        claim.body = body.clone();
        Ok(claim)
    }

    #[test]
    fn conversation_subject_owner_resource_attribution_does_not_replace_identity() {
        let store = Store::open_memory("tester").unwrap();
        append(&store, "agent/ada", "runtime.observed",
            json!({"runtime_id":"pty/ada","incarnation_id":"ada-1","status":"running"}));
        for opener_kind in RESOURCE_OPENER_KINDS {
            for attribution in [json!(true), json!(false), json!(1), Value::Null] {
                for wrapped in [false, true] {
                    store.connection.batched(|tx| -> Result<()> {
                        let fields = json!({"kind":opener_kind,"attribution_only":attribution,
                            "runtime_id":"pty/reviewer","incarnation_id":null,
                            "facts":{"opened_by":"person/alex","number":1}});
                        let body = if wrapped { json!({"fields":fields}) } else { fields };
                        synthetic_owner_history_row(tx, "reviewer", "agent/ada",
                            "resource.observed", &body, &[])?;
                        Ok(())
                    }).unwrap().unwrap();
                    let through = store.index().unwrap();
                    let connection = store.readers.get();
                    let actual = latest_actual_at(&connection, "agent/ada", Some(through)).unwrap().unwrap();
                    let expected_runtime = actual["runtime_id"].as_str().map(str::to_owned);
                    let expected_incarnation = actual["incarnation_id"].as_str().map(str::to_owned);
                    drop(connection);
                    let owner = store.conversation_owner_at("agent/ada", through).unwrap().unwrap();
                    assert_eq!(owner.runtime, expected_runtime);
                    assert_eq!(owner.incarnation, expected_incarnation);
                    assert_eq!(owner.origin.as_deref(), Some("tester"));
                }
            }
        }
    }

    #[test]
    fn conversation_subject_owner_rival_ancestry_crosses_checkpoint_tombstone() {
        let store = Store::open_memory("tester").unwrap();
        store.connection.batched(|tx| -> Result<()> {
            let rival = append_claim_tx(tx, "reviewer", "agent/ada", "runtime.observed",
                None, &json!({"fields":{"status":"running","runtime_id":"pty/reviewer"}}), &[], None)?;
            let bridge = append_claim_tx(tx, "tester", "agent/ada", "harness.observed",
                None, &json!({"fields":{"state":"working"}}), &[rival.id], None)?;
            append_claim_tx(tx, "tester", "agent/ada", "runtime.observed",
                None, &json!({"fields":{"status":"running","runtime_id":"pty/ada",
                    "incarnation_id":"ada-1"}}), std::slice::from_ref(&bridge.id), None)?;
            tx.execute("INSERT INTO checkpoint_claims
                (id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,
                 operation_id,request_digest,accepted_at_unix_ms,checkpoint)
                SELECT id,'tester',1,'fixture',subject,kind,actor,predecessors,
                       NULL,NULL,accepted_at_unix_ms,'checkpoint/tester'
                FROM claims WHERE id=?1", [&bridge.id])?;
            tx.execute("DELETE FROM claims WHERE id=?1", [&bridge.id])?;
            Ok(())
        }).unwrap().unwrap();
        let through = store.index().unwrap();
        let connection = store.readers.get();
        let source = identity_actual_source_at(&connection, "agent/ada", through).unwrap();
        assert!(!source.2, "the checkpoint tombstone preserves rival descent");
        assert_eq!(source, selected_actual_source_at(&connection, "agent/ada", Some(through), None).unwrap());
        drop(connection);
        assert_eq!(store.conversation_owner_at("agent/ada", through).unwrap().unwrap().runtime.as_deref(),
            Some("pty/ada"));
    }

    #[test]
    fn conversation_transport_pair_seeks_match_full_window_ranking() {
        let store = Store::open_memory("tester").unwrap();
        store.connection.batched(|tx| -> Result<()> {
            for number in 0..120_u64 {
                let origin = ["tester", "reviewer", "operator"][(number % 3) as usize];
                let subject = ["host/alex", "host/ada"][(number % 2) as usize];
                let claim = append_claim_tx(tx, origin, subject, "transport.observed", None,
                    &json!({"fields":{"status":if number % 5 == 0 { "unknown" } else { "up" },
                        "last_success_at":900 + number}}), &[], None)?;
                tx.execute("UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                    params![(900 + (number * 17) % 100).to_string(), claim.id])?;
            }
            Ok(())
        }).unwrap().unwrap();
        let connection = store.readers.get();
        let ranked = connection.prepare_cached(&smallclaims::store::canonical_sql(
            "SELECT origin, subject, json_extract(body,'$.fields.status'),
                    json_extract(body,'$.fields.last_success_at') FROM (
                SELECT origin,subject,body,
                    ROW_NUMBER() OVER (PARTITION BY origin,subject
                        ORDER BY CANONICAL_DESC(claims)) AS rank
                FROM claims WHERE kind='transport.observed'
             ) WHERE rank=1 ORDER BY origin,subject",
        )).unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?,
            row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, u64>(3)?)))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        drop(connection);
        for now in [0, 1_000, 1_020, 1_000 + TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS] {
            let expected = ranked.iter().filter(|(_, _, status, at)| status == "up"
                && u128::from(*at) <= now.saturating_add(TRANSPORT_LINK_CLOCK_SKEW_MS)
                && now.saturating_sub(u128::from(*at))
                    < TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS)
                .map(|(origin, subject, _, _)| (origin.clone(), subject.strip_prefix("host/").unwrap().into()))
                .collect::<Vec<_>>();
            assert_eq!(store.transport_links_at(now).unwrap(), expected, "now={now}");
        }
    }

    #[test]
    fn conversation_subject_owner_generated_histories_match_full_fold_and_sources() {
        for seed in 0_u64..16 {
            let store = Store::open_memory("tester").unwrap();
            let subject = "agent/ada";
            let mut random = seed + 1;
            let mut frontiers = Vec::new();
            let mut ids = Vec::new();
            store.connection.batched(|tx| -> Result<()> {
                for number in 0..40 {
                    random = random.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let kind = match random % 8 {
                        0 | 1 => "runtime.observed",
                        2 => "runtime.reconcile-decision",
                        3 => "custom.tester.identity",
                        4 => "harness.observed",
                        5 => "runtime.readiness-deadline-reached",
                        6 => "reconcile.fault",
                        _ if seed % 2 == 1 => "resource.observed",
                        _ => "custom.tester.identity",
                    };
                    let mut fields = match (random >> 8) % 6 {
                        0 => json!({"status":"running","host":"tester","runtime_id":"pty/ada",
                            "incarnation_id":format!("ada-{number}")}),
                        1 => json!({"status":"idle"}),
                        2 => json!({"runtime_id":null,"incarnation_id":null}),
                        3 => json!({"runtime_id":7,"incarnation_id":false}),
                        4 => json!({"incarnation_id":format!("ada-{number}")}),
                        _ => json!({"status":"stopped","runtime_id":"pty/reviewer"}),
                    };
                    if kind == "resource.observed" {
                        fields["kind"] = json!(match (random >> 24) % 3 {
                            0 => "vcs.issue", 1 => "vcs.pull-request", _ => "custom.tester",
                        });
                        fields["attribution_only"] = match (random >> 28) % 4 {
                            0 => json!(true), 1 => json!(false), 2 => json!(1), _ => Value::Null,
                        };
                        fields["facts"] = json!({"opened_by":"person/alex","number":number});
                    }
                    let body = if random & 16 == 0 { json!({"fields":fields}) } else { fields };
                    let origin = if (seed % 4 == 0 && kind == "runtime.observed")
                        || random & 32 == 0 { "tester" } else { "reviewer" };
                    let predecessors = if random & 64 == 0 { ids.clone() } else { Vec::new() };
                    let claim = synthetic_owner_history_row(tx, origin, subject, kind,
                        &body, &predecessors)?;
                    // Numeric boundaries, equal-time canonical ties, and arrival/canonical
                    // disagreement are deliberate; none may change a pinned answer.
                    let accepted = [9, 10, 99, 100][((random >> 16) % 4) as usize];
                    tx.execute("UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                        params![accepted.to_string(), claim.id])?;
                    if number % 11 == 0 {
                        tx.execute("INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                            [&claim.id])?;
                    }
                    ids.push(claim.id);
                    frontiers.push(claim.store_index);
                }
                Ok(())
            }).unwrap().unwrap();
            for through in frontiers {
                let connection = store.readers.get();
                let actual = latest_actual_at(&connection, subject, Some(through)).unwrap();
                let source = selected_actual_source_at(&connection, subject, Some(through), None).unwrap();
                assert_eq!(identity_actual_source_at(&connection, subject, through).unwrap(), source,
                    "source seed={seed}, through={through}");
                let field = |name| actual.as_ref().and_then(|v| v.get(name))
                    .and_then(Value::as_str).map(str::to_owned);
                let expected = Owner { subject: subject.into(), incarnation: field("incarnation_id"),
                    runtime: field("runtime_id"), origin: source.1 };
                drop(connection);
                assert_eq!(store.conversation_owners_at(through).unwrap().get(subject), Some(&expected),
                    "fleet identity seed={seed}, through={through}");
                assert_eq!(store.conversation_owner_at(subject, through).unwrap(), Some(expected),
                    "identity seed={seed}, through={through}");
            }
            assert!(store.conversation_owner_at("agent/alex", store.index().unwrap()).unwrap().is_none());
        }
    }

    fn append(store: &Store, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: fields
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }

    fn oracle(store: &Store, at: u64) -> Owners {
        store
            .status_for_subject_prefix_at("agent/", Some(at), true, crate::store::ClaimsReduction::Full)
            .unwrap()
            .subjects
            .into_iter()
            .map(|subject| {
                let fields = subject
                    .actual
                    .as_ref()
                    .map(|v| v.get("fields").unwrap_or(v));
                let incarnation = fields
                    .and_then(|v| v["incarnation_id"].as_str())
                    .or(subject.projection.runtime_incarnation.as_deref())
                    .map(str::to_owned);
                let runtime = fields
                    .and_then(|v| v["runtime_id"].as_str())
                    .map(str::to_owned);
                (
                    subject.subject.clone(),
                    Owner {
                        subject: subject.subject,
                        incarnation,
                        runtime,
                        origin: subject.actual_origin,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn conversation_change_budget_seeks_metadata_through_large_unrelated_history() {
        let store = Store::open_memory("node").unwrap();
        let owner = "agent/change-budget";
        append(&store, owner, "runtime.observed", json!({"status":"running"}));
        let after = store.index().unwrap();
        let mut costs = Vec::new();
        // Insert retained, unrelated sends in one isolated transaction. Their bodies must not
        // be decoded by a wake, and absent endpoints must stay indexed as history grows.
        store.connection.batched(|tx| -> Result<()> {
            for index in 0..10_000 {
                append_claim_tx(tx, "node", &format!("message/unrelated-{index}"), "message.sent", None,
                    &json!({"fields":{"from":"person/other", "to":"agent/other", "content":"x".repeat(512), "status":"sent"}}),
                    &[], None)?;
                if index == 99 || index == 9999 {
                    let mut query = tx.prepare_cached(CONVERSATION_CHANGED)?;
                    query.reset_status(rusqlite::StatementStatus::VmStep);
                    assert!(!query.query_row(params![owner, after, i64::MAX, 0, i64::MAX], |row| row.get::<_, bool>(0))?);
                    costs.push(query.get_status(rusqlite::StatementStatus::VmStep));
                }
            }
            let plan = tx.prepare(&format!("EXPLAIN QUERY PLAN {CONVERSATION_CHANGED}"))?
                .query_map(params![owner, after, i64::MAX, 0, i64::MAX], |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?.join("\n");
            for index in ["claims_subject_index", "claims_message_to_order_index", "claims_message_from_index",
                "claims_message_legacy_to_index", "claims_message_legacy_from_index", "local_observations_subject_id_index"] {
                assert!(plan.contains(index), "{plan}");
            }
            assert!(!plan.contains("SCAN claims"), "{plan}");
            Ok(())
        }).unwrap().unwrap();
        assert!(costs[0] > 0 && costs[1] <= costs[0] * 2 && costs[1] < 250,
            "wake work must not scale with unrelated payloads: {costs:?}");
        let through = store.index().unwrap();
        assert!(!store.conversation_changed_at(owner, after, through, 0, 0).unwrap());
        // Boundaries exclude changes beyond the captured frontier, including legacy endpoints.
        store.connection.batched(|tx| -> Result<()> {
            append_claim_tx(tx, "node", "message/legacy", "message.sent", None,
                &json!({"from":owner, "to":"person/other", "content":"original legacy body", "status":"sent"}), &[], None)?;
            Ok(())
        }).unwrap().unwrap();
        assert!(!store.conversation_changed_at(owner, after, through, 0, 0).unwrap());
        assert!(store.conversation_changed_at(owner, through, store.index().unwrap(), 0, 0).unwrap());
        for side in ["from", "to"] {
            let before = store.index().unwrap();
            let mut fields = json!({"from":"person/other", "to":"agent/other", "content":"hello", "status":"sent"});
            fields[side] = json!(owner);
            append(&store, &format!("message/wrapped-{side}"), "message.sent", fields);
            assert!(store.conversation_changed_at(owner, before, store.index().unwrap(), 0, 0).unwrap());
        }
        let before = store.index().unwrap();
        append(&store, owner, "runtime.observed", json!({"status":"idle"}));
        assert!(store.conversation_changed_at(owner, before, store.index().unwrap(), 0, 0).unwrap());
        assert_eq!(store.conversation_local_position().unwrap(), 0);
        let index = store.index().unwrap();
        store.connection.batched(|tx| -> Result<()> {
            tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                VALUES(?1,'agent/other','harness.timeline',?2,1)",
                params![index, json!({"fields":{"body":{"text":"x".repeat(2 * 1024 * 1024)}}}).to_string()])?;
            tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                VALUES(?1,?2,'harness.timeline','{}',2)", params![index, owner])?;
            Ok(())
        }).unwrap().unwrap();
        assert_eq!(store.conversation_local_position().unwrap(), 2);
        assert!(!store.conversation_changed_at(owner, index, index, 0, 1).unwrap());
        assert!(store.conversation_changed_at(owner, index, index, 1, 2).unwrap());
        assert!(!store.conversation_changed_at(owner, index, index, 2, 2).unwrap());
    }

    #[test]
    fn conversation_owner_frontiers_match_full_status_and_refresh_only_changed_agents() {
        let store = Store::open_memory("node").unwrap();
        for agent in ["agent/alder", "agent/birch"] {
            append(
                &store,
                agent,
                "runtime.observed",
                json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
            );
        }
        let first = store.index().unwrap();
        let owners = store.conversation_owners_at(first).unwrap();
        assert_eq!(*owners, oracle(&store, first));
        append(
            &store,
            "message/unrelated",
            "message.sent",
            json!({"from":"person/receiver","to":"agent/other","content":"Unrelated", "status":"sent"}),
        );
        let unrelated = store.index().unwrap();
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        let unchanged = store.conversation_owners_at(unrelated).unwrap();
        let queries = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
        assert_eq!(*unchanged, *owners);
        assert!(
            queries <= 3,
            "an unrelated write must not fold agents: {queries} queries"
        );
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"idle"}),
        );
        let inherited = store.index().unwrap();
        assert_eq!(
            *store.conversation_owners_at(inherited).unwrap(),
            oracle(&store, inherited)
        );
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"running","incarnation_id":null,"runtime_id":"second"}),
        );
        let nulled = store.index().unwrap();
        assert_eq!(
            *store.conversation_owners_at(nulled).unwrap(),
            oracle(&store, nulled)
        );
        assert_eq!(
            *store.conversation_owners_at(first).unwrap(),
            oracle(&store, first)
        );
        // A historical rebuild must exclude later fields even after eviction.
        store.forget_current_views();
        assert_eq!(
            *store.conversation_owners_at(first).unwrap(),
            oracle(&store, first)
        );
        assert_eq!(
            *store.conversation_owners_at(nulled).unwrap(),
            oracle(&store, nulled)
        );
    }

    #[test]
    fn conversation_owner_new_declarations_and_harness_only_agents_match_cold_reads() {
        let store = Store::open_memory("node").unwrap();
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
        );
        store
            .conversation_owners_at(store.index().unwrap())
            .unwrap();
        store
            .connection
            .batched(|tx| -> Result<()> {
                append_claim_tx(
                    tx,
                    "node",
                    "agent/birch",
                    "intent.desired",
                    None,
                    &json!({"subject":"agent/birch","kind":"agent","desired":{}}),
                    &[],
                    None,
                )?;
                append_claim_tx(
                    tx,
                    "node",
                    "agent/cedar",
                    "harness.observed",
                    None,
                    &json!({"fields":{"state":"idle"}}),
                    &[],
                    None,
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let frontier = store.index().unwrap();
        let warm = store.conversation_owners_at(frontier).unwrap();
        assert!(warm.contains_key("agent/birch"));
        assert!(warm.contains_key("agent/cedar"));
        assert_eq!(*warm, oracle(&store, frontier));
        store.forget_current_views();
        let cold = store.conversation_owners_at(frontier).unwrap();
        assert_eq!(*warm, *cold);
    }

    #[test]
    fn conversation_owner_build_before_repair_cannot_repopulate_the_cleared_cache() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
        );
        let frontier = store.index().unwrap();
        let (ready_send, ready) = std::sync::mpsc::channel();
        let (resume_send, resume) = std::sync::mpsc::channel();
        let reader = store.clone();
        let worker = std::thread::spawn(move || {
            BEFORE_OWNER_PUBLISH.with(|pause| {
                *pause.borrow_mut() = Some(Box::new(move || {
                    ready_send.send(()).unwrap();
                    resume
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }));
            });
            reader.conversation_owners_at(frontier).unwrap()
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        store.forget_current_views();
        resume_send.send(()).unwrap();
        let result = worker.join().unwrap();
        assert!(
            store
                .smalltalk
                .conversation_owners
                .lock()
                .unwrap()
                .is_empty(),
            "an in-flight build republished a pre-repair cache entry"
        );
        assert_eq!(*result, oracle(&store, frontier));
        assert_eq!(*store.conversation_owners_at(frontier).unwrap(), *result);
        assert_eq!(store.smalltalk.conversation_owners.lock().unwrap().len(), 1);
    }

    #[test]
    fn conversation_owner_rivals_and_other_actual_kinds_keep_canonical_authority() {
        let older = Store::open_memory("older").unwrap();
        append(
            &older,
            "agent/alder",
            "runtime.observed",
            json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
        );
        older.connection.batched(|tx| append_claim_tx(
            tx, "newer", "agent/alder", "runtime.observed", None,
            &json!({"fields":{"status":"running","incarnation_id":"rival","runtime_id":"other"}}), &[], None,
        )).unwrap().unwrap();
        let rival = older.index().unwrap();
        assert_eq!(
            *older.conversation_owners_at(rival).unwrap(),
            oracle(&older, rival)
        );
        append(
            &older,
            "agent/alder",
            "runtime.reconcile-decision",
            json!({"decision":"hold","reachability":"unreachable"}),
        );
        let other = older.index().unwrap();
        assert_eq!(
            *older.conversation_owners_at(other).unwrap(),
            oracle(&older, other)
        );
    }

    #[test]
    fn conversation_endpoint_reads_match_the_existing_window_and_replay_floor() {
        let store = Store::open_memory("node").unwrap();
        let mut cursor = 0;
        for (number, from, to) in [
            (1, "person/receiver", "agent/alder"),
            (2, "agent/other", "agent/elsewhere"),
            (3, "agent/alder", "person/receiver"),
            (4, "agent/alder", "agent/alder"),
        ] {
            let claim = append(
                &store,
                &format!("message/{number}"),
                "message.sent",
                json!({"from":from,"to":to,"content":format!("Message {number}"), "status":"sent"}),
            );
            if number == 2 {
                cursor = claim.store_index;
            }
        }
        let before = store.index().unwrap() + 1;
        append(
            &store,
            "message/later",
            "message.sent",
            json!({"from":"agent/alder","to":"person/receiver","content":"Outside snapshot", "status":"sent"}),
        );
        for after in [0, cursor] {
            let old = store
                .claims_for_kind_at("message.sent", Some(before), true, 10_000)
                .unwrap()
                .claims
                .into_iter()
                .filter(|c| {
                    c.store_index > after
                        && ["from", "to"]
                            .iter()
                            .any(|k| c.body["fields"][k] == "agent/alder")
                })
                .map(|c| c.id)
                .collect::<Vec<_>>();
            let new = store
                .conversation_messages_at("agent/alder", Some(before), after)
                .unwrap();
            assert_eq!(new.into_iter().map(|c| c.id).collect::<Vec<_>>(), old);
        }
    }

    #[test]
    fn conversation_runtime_membership_matches_old_observation_fold() {
        let store = Store::open_memory("node").unwrap();
        for (number, incarnation) in [(1, "first"), (2, "first"), (3, "next"), (4, "next")] {
            let claim = append(
                &store,
                "agent/alder",
                "runtime.observed",
                json!({"status":"running","incarnation_id":incarnation,"runtime_id":"pty"}),
            );
            store
                .connection
                .batched(|tx| {
                    tx.execute(
                        "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                        params![(number * 100).to_string(), claim.id],
                    )
                })
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            store
                .conversation_runtime_span("agent/alder", "first")
                .unwrap(),
            (Some(100), Some(300))
        );
        assert_eq!(
            store
                .conversation_runtime_span("agent/alder", "next")
                .unwrap(),
            (Some(300), None)
        );
        assert_eq!(
            store
                .conversation_runtime_span("agent/alder", "absent")
                .unwrap(),
            (None, None)
        );
    }
    #[test]
    fn conversation_message_decode_does_not_pin_the_wal() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
        );
        append(
            &store,
            "message/large",
            "message.sent",
            json!({"from":"person/receiver","to":"agent/alder","content":"x".repeat(2 * 1024 * 1024),"status":"sent"}),
        );
        let (ready_send, ready) = std::sync::mpsc::channel();
        let (resume_send, resume) = std::sync::mpsc::channel();
        let reader = store.clone();
        let worker = std::thread::spawn(move || {
            BEFORE_MESSAGE_DECODE.with(|pause| {
                *pause.borrow_mut() = Some(Box::new(move || {
                    ready_send.send(()).unwrap();
                    resume
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }))
            });
            reader
                .conversation_messages_at("agent/alder", None, 0)
                .unwrap()
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        append(
            &store,
            "message/while-decoding",
            "message.sent",
            json!({"from":"person/receiver","to":"agent/alder","content":"Concurrent message","status":"sent"}),
        );
        let checkpoint: (u64, i64, i64) = store
            .connection
            .write()
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        resume_send.send(()).unwrap();
        let claims = worker.join().unwrap();
        assert_eq!(checkpoint.0, 0);
        assert_eq!(
            checkpoint.1, checkpoint.2,
            "message decoding retained a SQLite snapshot: {checkpoint:?}"
        );
        assert_eq!(claims.len(), 1, "the new write belongs to the next read");
        assert_eq!(
            claims[0].body["fields"]["content"].as_str().unwrap().len(),
            2 * 1024 * 1024
        );
    }
    #[test]
    fn conversation_message_window_keeps_legacy_bodies_and_excludes_older_fleet_sends() {
        let store = Store::open_memory("node").unwrap();
        append(
            &store,
            "message/old",
            "message.sent",
            json!({"from":"person/receiver","to":"agent/alder","content":"Outside the newest fleet window","status":"sent"}),
        );
        let before = store.index().unwrap() + 1;
        store.connection.batched(|tx| -> Result<()> {
            for number in 0..10_000 {
                append_claim_tx(tx, "node", &format!("message/other-{number}"), "message.sent", None,
                    &json!({"fields":{"from":"agent/elsewhere","to":"agent/other","content":"Unrelated","status":"sent"}}), &[], None)?;
            }
            append_claim_tx(tx, "node", "message/legacy", "message.sent", None,
                &json!({"from":"person/receiver","to":"agent/alder","content":"Legacy root body","status":"sent"}), &[], None)?;
            Ok(())
        }).unwrap().unwrap();
        let claims = store
            .conversation_messages_at("agent/alder", None, 0)
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].body["content"], "Legacy root body");
        assert_eq!(
            store
                .conversation_messages_at("agent/alder", Some(before), 0)
                .unwrap()[0]
                .subject,
            "message/old"
        );
    }
    #[test]
    fn conversation_message_byte_batches_preserve_every_ordered_payload() {
        let store = Store::open_memory("node").unwrap();
        store.connection.batched(|tx| -> Result<()> {
            for number in 0..70 {
                append_claim_tx(tx, "node", &format!("message/batch-{number}"), "message.sent", None,
                    &json!({"fields":{"from":"person/receiver","to":"agent/alder","content":"x".repeat(32 * 1024),"status":"sent"}}), &[], None)?;
            }
            Ok(())
        }).unwrap().unwrap();
        let old = store
            .claims_for_kind_at("message.sent", None, true, 10_000)
            .unwrap()
            .claims;
        let new = store
            .conversation_messages_at("agent/alder", None, 0)
            .unwrap();
        assert_eq!(new.len(), 70);
        for (old, new) in old.iter().zip(&new) {
            assert_eq!(old.id, new.id);
            assert_eq!(old.body, new.body);
            assert_eq!(old.operation_id, new.operation_id);
            assert_eq!(old.predecessors, new.predecessors);
        }
    }
    #[test]
    fn conversation_timeline_owner_window_matches_the_old_physical_window() {
        let store = Store::open_memory("node").unwrap();
        let owner = "agent/alder";
        append(
            &store,
            owner,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"first","runtime_id":"pty"}),
        );
        let old_frontier = store.index().unwrap() + 1;
        store.connection.batched(|tx| -> Result<()> {
            for number in 0..10_005 {
                append_claim_tx(tx, "node", owner, "harness.timeline", None,
                    &json!({"fields":{"operation":"append",
                        "entry_id":format!("timeline-entry/alder-{number}"),
                        "sequence":number + 1,"revision":1,"role":"assistant",
                        "entry_type":"content","final":true,"body":{"text":"Native text"},
                        "driver":"codex","incarnation_id":"first"}}), &[], None)?;
            }
            for (kind, fields) in [
                ("runtime.observed", json!({"status":"idle","incarnation_id":"first"})),
                ("harness.observed", json!({"state":"idle","incarnation_id":"first"})),
                ("harness.diagnostic", json!({"code":"provider-error","reason":"Provider stopped","severity":"warning"})),
                ("harness.usage", json!({"input_tokens":12,"output_tokens":3,"driver":"codex","incarnation_id":"first","semantics":"response"})),
            ] {
                append_claim_tx(tx, "node", owner, kind, None,
                    &json!({"fields":fields}), &[], None)?;
            }
            // Large bodies that this fallback never renders must not be copied or parsed.
            append_claim_tx(tx, "node", owner, "harness.timeline", None,
                &json!({"fields":{"operation":"append","entry_id":"timeline-entry/alder-large",
                    "sequence":10_006,"revision":1,"role":"assistant","entry_type":"content",
                    "final":true,"body":{"text":"x".repeat(2 * 1024 * 1024)},
                    "driver":"codex","incarnation_id":"first"}}), &[], None)?;
            append_claim_tx(tx, "node", "agent/birch", "runtime.observed", None,
                &json!({"fields":{"status":"running"}}), &[], None)?;
            Ok(())
        }).unwrap().unwrap();
        for before in [Some(old_frontier), None] {
            let expected = store
                .claims_page(Some(owner), None, 0, before, true, 10_000)
                .unwrap()
                .claims
                .into_iter()
                .filter(|c| {
                    matches!(
                        c.kind.as_str(),
                        "runtime.observed"
                            | "harness.observed"
                            | "harness.diagnostic"
                            | "harness.usage"
                            | "message.sent"
                    )
                })
                .collect::<Vec<_>>();
            let actual = store
                .conversation_timeline_owner_claims_at(owner, before)
                .unwrap();
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(&expected) {
                assert_eq!(actual.id, expected.id);
                assert_eq!(actual.body, expected.body);
                assert_eq!(actual.predecessors, expected.predecessors);
            }
        }
        assert_eq!(
            store
                .conversation_timeline_owner_claims_at(owner, None)
                .unwrap()
                .len(),
            4,
            "filtering kinds before the physical window would resurrect the old runtime row"
        );
        let connection = store.readers.get();
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {TIMELINE_OWNER_IDS}"))
            .unwrap()
            .query_map(params![owner, i64::MAX], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("claims_subject_index"), "{plan}");
        assert!(!plan.contains("SCAN claims"), "{plan}");
    }

    #[test]
    fn conversation_timeline_owner_decode_does_not_pin_the_wal() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
        append(
            &store,
            "agent/alder",
            "harness.diagnostic",
            json!({"code":"provider-error","reason":"x".repeat(2 * 1024 * 1024),"severity":"warning"}),
        );
        let (ready_send, ready) = std::sync::mpsc::channel();
        let (resume_send, resume) = std::sync::mpsc::channel();
        let reader = store.clone();
        let worker = std::thread::spawn(move || {
            BEFORE_MESSAGE_DECODE.with(|pause| {
                *pause.borrow_mut() = Some(Box::new(move || {
                    ready_send.send(()).unwrap();
                    resume
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }));
            });
            reader
                .conversation_timeline_owner_claims_at("agent/alder", None)
                .unwrap()
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        append(
            &store,
            "agent/alder",
            "runtime.observed",
            json!({"status":"idle"}),
        );
        let checkpoint: (u64, i64, i64) = store
            .connection
            .write()
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        resume_send.send(()).unwrap();
        let claims = worker.join().unwrap();
        assert_eq!(checkpoint.0, 0);
        assert_eq!(
            checkpoint.1, checkpoint.2,
            "owner decoding retained a SQLite snapshot: {checkpoint:?}"
        );
        assert_eq!(claims.len(), 1, "the new write belongs to the next read");
        assert_eq!(
            claims[0].body["fields"]["reason"].as_str().unwrap().len(),
            2 * 1024 * 1024
        );
    }
}
