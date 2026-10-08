//! Conversation routing needs runtime identities, not every agent's harness and claim history.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub subject: String,
    pub incarnation: Option<String>,
    pub runtime: Option<String>,
    pub origin: Option<String>,
}

pub(super) type Owners = BTreeMap<String, Owner>;

#[cfg(test)]
thread_local! {
    static BEFORE_MESSAGE_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

impl Store {
    /// A volatile, rebuildable identity map at a logical claim frontier. Only changed agents
    /// are folded on subsequent reads. No lock or SQLite transaction spans the whole build,
    /// native transcript I/O, display preparation, or a long-poll wait.
    pub(crate) fn conversation_owners_at(&self, through: u64) -> Result<Arc<Owners>> {
        selected_index(self.index()?, Some(through)).map_err(anyhow::Error::new)?;
        let previous = {
            let cache = self
                .smalltalk
                .conversation_owners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((_, owners)) = cache.iter().find(|(at, _)| *at == through) {
                return Ok(Arc::clone(owners));
            }
            cache
                .iter()
                .filter(|(at, _)| *at < through)
                .max_by_key(|(at, _)| *at)
                .map(|(at, owners)| (*at, Arc::clone(owners)))
        };
        // Harness reports do not change a runtime identity. Copy only small metadata while the
        // SELECT is live. Rebuild a large catch-up instead of retaining an unbounded delta.
        let changed = previous
            .as_ref()
            .map(|(after, _)| -> Result<_> {
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
                                && !kind.starts_with("harness.")
                                && !matches!(
                                    kind.as_str(),
                                    "intent.desired"
                                        | "runtime.readiness-deadline-reached"
                                        | "reconcile.fault"
                                )
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
            // The fast path inherits absent runtime fields, retains explicit null, and only
            // applies to one runtime origin. Rival origins and other actual kinds retain the
            // existing canonical fold and desired-host authority selection.
            let (actual, origin) = match runtime_only_authority(
                &connection,
                &subject,
                through,
                &["runtime_id", "incarnation_id"],
            )? {
                Some((actual, origin)) => (Some(actual), Some(origin)),
                None => {
                    let desired = desired_row_at(&connection, &subject, Some(through))?;
                    let member = desired
                        .as_ref()
                        .and_then(|row| row.member.as_deref())
                        .and_then(|value| {
                            serde_json::from_str::<crate::model::MemberSpec>(value).ok()
                        });
                    let actual = latest_actual_at(&connection, &subject, Some(through))?;
                    let (_, origin, _) = selected_actual_source_at(
                        &connection,
                        &subject,
                        Some(through),
                        member.as_ref().map(|member| member.host.as_str()),
                    )?;
                    (actual, origin)
                }
            };
            let fields = actual
                .as_ref()
                .map(|value| value.get("fields").unwrap_or(value));
            let incarnation = fields
                .and_then(|fields| fields["incarnation_id"].as_str())
                .map(str::to_owned);
            let runtime = fields
                .and_then(|fields| fields["runtime_id"].as_str())
                .map(str::to_owned);
            owners.insert(
                subject.clone(),
                Owner {
                    subject,
                    incarnation,
                    runtime,
                    origin,
                },
            );
        }
        let owners = Arc::new(owners);
        let mut cache = self
            .smalltalk
            .conversation_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
            .status_for_subject_prefix_at("agent/", Some(at), true)
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
}
