//! Bounded source candidates, not authorized views. Call all reads inside the caller's
//! `read_snapshot`; fences do not replace the pinned reader or audience checks.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeSourceFence {
    pub graph_index: u64,
    pub local_position: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeSourceRecord {
    pub record: ClaimRecord,
    /// Actual local_observations.id; durable claims have no local identity.
    pub local_position: Option<u64>,
}

pub(crate) fn native_source_log_order(source: &NativeSourceRecord) -> (u64, u64) {
    (source.record.store_index, source.local_position.unwrap_or(0))
}

pub(crate) fn normalize_native_claim_fields(
    kind: &str,
    body: &Value,
) -> Result<BTreeMap<String, Value>> {
    schema_fields_for_body(kind, body)
}

const CLAIM_COLUMNS: &str = "SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind, claims.origin, claims.actor, claims.body, claims.predecessors, claims.accepted_at_unix_ms FROM claims";
const ADMITTED: &str = "NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired WHERE repaired.id=claims.id)";

fn sql_limit(limit: usize) -> i64 {
    limit.min(i64::MAX as usize) as i64
}

fn prefix_upper_bound(prefix: &str) -> Option<String> {
    for (offset, character) in prefix.char_indices().rev() {
        let next = (u32::from(character) + 1..=0x10ffff).find_map(char::from_u32);
        if let Some(next) = next {
            let mut upper = prefix[..offset].to_owned();
            upper.push(next);
            return Some(upper);
        }
    }
    None
}

#[derive(Clone, Debug)]
pub(super) struct NativeRetentionLimits {
    local_window_ms: u64,
    local_max_per_subject_kind: usize,
    checkpoint_enabled: bool,
}

impl Default for NativeRetentionLimits {
    fn default() -> Self {
        let observations = crate::config::ObservationsConfig::default();
        Self {
            local_window_ms: observations.retention_ms().expect("valid default retention"),
            local_max_per_subject_kind: observations.max_per_subject_kind,
            checkpoint_enabled: crate::config::CheckpointConfig::default().enabled,
        }
    }
}

impl Store {
    /// Bind the advertised limits to the same validated configuration used by
    /// the daemon's observation trimmer and checkpoint scheduler.
    pub fn configure_native_retention(
        &self, observations: &crate::config::ObservationsConfig, checkpoint_enabled: bool,
    ) -> Result<()> {
        *self.native_retention.lock().unwrap_or_else(PoisonError::into_inner) = NativeRetentionLimits {
            local_window_ms: observations.retention_ms()?,
            local_max_per_subject_kind: observations.max_per_subject_kind,
            checkpoint_enabled,
        };
        Ok(())
    }

    pub(crate) fn native_retention_coverage(&self) -> Value {
        let limits = self.native_retention.lock().unwrap_or_else(PoisonError::into_inner);
        json!({
            "local_retention": {
                "window_ms": limits.local_window_ms,
                "max_per_subject_kind": limits.local_max_per_subject_kind,
                "keeps_latest_per_subject_kind": true,
            },
            "replicated_retention": {
                "mode": "checkpoint-prunable",
                "checkpoint_enabled": limits.checkpoint_enabled,
            },
        })
    }

    pub(crate) fn native_source_fence(&self) -> Result<NativeSourceFence> {
        self.read_snapshot(|graph_index| {
            let local_position = self.readers.get().query_row(
                "SELECT COALESCE(MAX(id),0) FROM local_observations", [], |row| row.get(0),
            )?;
            Ok(NativeSourceFence { graph_index, local_position })
        })
    }

    /// Literal binary prefix ranges (never LIKE patterns), exclusive subject seek.
    /// The slash-terminated family and optional prefix ranges intersect.
    pub(crate) fn native_subject_refs(
        &self, family: &str, ref_prefix: Option<&str>, after_ref: Option<&str>,
        fence: &NativeSourceFence, recorded_actor: Option<&str>, limit: usize,
    ) -> Result<Vec<String>> {
        let lower = format!("{family}/");
        let upper = format!("{family}0");
        let prefix = ref_prefix.unwrap_or("");
        let prefix_upper = prefix_upper_bound(prefix);
        let connection = self.readers.get();
        let mut statement = connection.prepare(&format!(
            "SELECT subject FROM (
               SELECT subject FROM (SELECT DISTINCT subject FROM claims
               WHERE subject>=?1 AND subject<?2 AND subject>=?3
                 AND (?9 IS NULL OR subject<?9)
                 AND (?4 IS NULL OR subject>?4) AND store_index<=?5
                 AND (?7 IS NULL OR actor=?7) AND {ADMITTED}
               ORDER BY subject LIMIT ?8)
               UNION
               SELECT subject FROM (SELECT DISTINCT subject FROM local_observations
               WHERE subject>=?1 AND subject<?2 AND subject>=?3
                 AND (?9 IS NULL OR subject<?9)
                 AND (?4 IS NULL OR subject>?4) AND after_store_index<=?5 AND id<=?6
                 AND (?7 IS NULL OR actor=?7)
               ORDER BY subject LIMIT ?8)
             ) ORDER BY subject LIMIT ?8"
        ))?;
        Ok(statement.query_map(params![lower,upper,prefix,after_ref,fence.graph_index,
            fence.local_position,recorded_actor,sql_limit(limit),prefix_upper], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Source-log order DESC: durable `(store_index,0)`, local `(after_store_index,id)`.
    /// Each source query is independently limited before a bounded merge. Exclusive
    /// tuple cursors preserve all local rows sharing the same graph index.
    pub(crate) fn native_subject_history(
        &self, subject: &str, kind: Option<&str>, before: Option<(u64,u64)>,
        fence: &NativeSourceFence, limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let (before_graph,before_local) = before.map_or((None,None), |(g,l)| (Some(g),Some(l)));
        let mut durable = connection.prepare(&format!(
            "{CLAIM_COLUMNS} WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
             AND store_index<=?3 AND {ADMITTED}
             AND (?4 IS NULL OR (store_index,0)<(?4,?5))
             ORDER BY store_index DESC LIMIT ?6"
        ))?;
        let mut records = durable.query_map(params![subject,kind,fence.graph_index,
            before_graph,before_local,sql_limit(limit)], |row| {
                Ok(NativeSourceRecord { record: claim_from_row(row)?, local_position: None })
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut local = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
             AND after_store_index<=?3 AND id<=?4
             AND (?5 IS NULL OR (after_store_index,id)<(?5,?6))
             ORDER BY after_store_index DESC,id DESC LIMIT ?7"
        ))?;
        records.extend(local.query_map(params![subject,kind,fence.graph_index,fence.local_position,
            before_graph,before_local,sql_limit(limit)], |row| {
                Ok(NativeSourceRecord { record: local_observation_from_row(&self.origin,row)?,
                    local_position: Some(row.get(0)?) })
            })?.collect::<rusqlite::Result<Vec<_>>>()?);
        records.sort_unstable_by_key(|record| std::cmp::Reverse(native_source_log_order(record)));
        records.truncate(limit);
        Ok(records)
    }

    /// Local head candidates, newest actual local position first. Invalid or
    /// cross-subject cursors yield no candidates, never silently restart a page.
    pub(crate) fn native_subject_local_kind_heads(
        &self, subject: &str, kind: &str, before_id: Option<&str>,
        fence: &NativeSourceFence, limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let prefix = format!("{LOCAL_OBSERVATION_ID_PREFIX}{}/", self.origin);
        let local_before = before_id.map(|id| {
            id.strip_prefix(&prefix).and_then(|position| position.parse::<u64>().ok())
                .context("invalid local head cursor")
        }).transpose()?;
        let mut local = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND kind=?2
             AND after_store_index<=?3 AND id<=?4
             AND (?5 IS NULL OR (id<?5 AND EXISTS(SELECT 1 FROM local_observations boundary
                 WHERE boundary.id=?5 AND boundary.subject=?1 AND boundary.kind=?2)))
             ORDER BY id DESC LIMIT ?6"
        ))?;
        Ok(local.query_map(params![subject,kind,fence.graph_index,fence.local_position,
            local_before,sql_limit(limit)], |row| Ok(NativeSourceRecord {
                record: local_observation_from_row(&self.origin,row)?,
                local_position: Some(row.get(0)?),
            }))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replicated candidates in canonical DESC order, NOT arrival order.
    /// Exclusive ids resolve their canonical tuple in SQL; authorize each
    /// candidate before choosing the head and continue past hidden candidates.
    pub(crate) fn native_subject_replicated_kind_heads(
        &self, subject: &str, kind: &str, before_id: Option<&str>,
        fence: &NativeSourceFence, limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let seek = canonical::after_sql("boundary", "claims");
        let mut durable = connection.prepare(&canonical_sql(&format!(
            "{CLAIM_COLUMNS} INDEXED BY claims_subject_kind_accepted_index
             WHERE subject=?1 AND kind=?2 AND store_index<=?3 AND {ADMITTED}
             AND (?4 IS NULL OR EXISTS(SELECT 1 FROM claims boundary
                 WHERE boundary.id=?4 AND boundary.subject=?1 AND boundary.kind=?2 AND {seek}))
             ORDER BY CANONICAL_DESC(claims) LIMIT ?5"
        )))?;
        Ok(durable.query_map(params![subject,kind,fence.graph_index,before_id,
            sql_limit(limit)], |row| Ok(NativeSourceRecord {
                record: claim_from_row(row)?, local_position: None,
            }))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Resolve an audience's earlier admitted durable episode in canonical order.
    /// The boundary may be another kind/subject (an answer or reply), and its local
    /// arrival index must not exclude a request admitted later by this host.
    pub(crate) fn native_preceding_kind_heads(
        &self, subject: &str, kind: &str, before_id: &str,
        fence: &NativeSourceFence, limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let seek = canonical::after_sql("boundary", "claims");
        let mut query = connection.prepare(&canonical_sql(&format!(
            "{CLAIM_COLUMNS} INDEXED BY claims_subject_kind_accepted_index
             WHERE subject=?1 AND kind=?2 AND store_index<=?3 AND {ADMITTED}
             AND EXISTS(SELECT 1 FROM claims boundary WHERE boundary.id=?4 AND {seek})
             ORDER BY CANONICAL_DESC(claims) LIMIT ?5"
        )))?;
        Ok(query.query_map(params![subject,kind,fence.graph_index,before_id,sql_limit(limit)],
            |row| Ok(NativeSourceRecord { record: claim_from_row(row)?, local_position: None }))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Internal continuation fingerprint of retained source membership under the
    /// original upper fences. Source identities are immutable and append-only;
    /// deletes/repairs therefore reduce a count even when the cursor boundary
    /// survives. New observations above either fence do not invalidate a page.
    /// Never expose the underlying counts or source identities to clients.
    pub(crate) fn native_subject_retention_version(
        &self, subject: &str, kind: Option<&str>, fence: &NativeSourceFence,
    ) -> Result<String> {
        let connection = self.readers.get();
        let durable: (u64, Option<u64>) = connection.query_row(
            &format!("SELECT COUNT(*), MIN(store_index) FROM claims
                WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
                  AND store_index<=?3 AND {ADMITTED}"),
            params![subject,kind,fence.graph_index],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let local: (u64, Option<u64>, Option<u64>) = connection.query_row(
            "SELECT COUNT(*), MIN(id), MAX(id) FROM local_observations
             WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
               AND after_store_index<=?3 AND id<=?4",
            params![subject,kind,fence.graph_index,fence.local_position],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        canonical_hash(&("st3.native-subject-retention.v1", subject, kind,
            fence.graph_index, fence.local_position, durable, local))
    }

    /// Family continuations pin retained membership as well as their upper
    /// fences. Pruning or repairing any admitted source invalidates the page,
    /// including sources not yet reached by the public visible-ref boundary.
    pub(crate) fn native_family_retention_version(
        &self, family: &str, ref_prefix: Option<&str>, fence: &NativeSourceFence,
        recorded_actor: Option<&str>,
    ) -> Result<String> {
        let lower = format!("{family}/");
        let upper = format!("{family}0");
        let prefix = ref_prefix.unwrap_or("");
        let prefix_upper = prefix_upper_bound(prefix);
        let connection = self.readers.get();
        let durable: u64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM claims
                WHERE subject>=?1 AND subject<?2 AND subject>=?3
                  AND (?4 IS NULL OR subject<?4) AND store_index<=?5
                  AND (?6 IS NULL OR actor=?6) AND {ADMITTED}"),
            params![lower, upper, prefix, prefix_upper, fence.graph_index, recorded_actor],
            |row| row.get(0),
        )?;
        let local: u64 = connection.query_row(
            "SELECT COUNT(*) FROM local_observations
             WHERE subject>=?1 AND subject<?2 AND subject>=?3
               AND (?4 IS NULL OR subject<?4) AND after_store_index<=?5 AND id<=?6
               AND (?7 IS NULL OR actor=?7)",
            params![lower, upper, prefix, prefix_upper, fence.graph_index,
                fence.local_position, recorded_actor],
            |row| row.get(0),
        )?;
        canonical_hash(&("st3.native-family-retention.v1", family, ref_prefix,
            fence.graph_index, fence.local_position, recorded_actor, durable, local))
    }

    pub(crate) fn native_admitted_claim_by_id(&self, id: &str) -> Result<Option<ClaimRecord>> {
        Ok(self.readers.get().query_row(&format!("{CLAIM_COLUMNS} WHERE claims.id=?1 AND {ADMITTED}"),
            [id], claim_from_row).optional()?)
    }

    pub(crate) fn native_source_record_exists(&self, subject: &str, position: (u64,u64)) -> Result<bool> {
        let connection = self.readers.get();
        if position.1 == 0 {
            Ok(connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM claims
                WHERE subject=?1 AND store_index=?2 AND {ADMITTED})"),
                params![subject,position.0], |row| row.get(0))?)
        } else {
            Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM local_observations
                WHERE subject=?1 AND after_store_index=?2 AND id=?3)",
                params![subject,position.0,position.1], |row| row.get(0))?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(store: &Store, subject: &str) -> ClaimRecord {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: "resource.observed".into(),
            actor: Some("person/example".into()),
            fields: BTreeMap::from([("kind".into(), json!("human.review"))]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap()
    }

    #[test]
    fn canonical_head_order_is_not_source_arrival_order() {
        let store = Store::open_memory("node").unwrap();
        let first = claim(&store, "resource/example");
        let second = claim(&store, "resource/example");
        store.connection.lock().unwrap().execute(
            "UPDATE claims SET accepted_at_unix_ms=CASE WHEN id=?1 THEN '200' ELSE '100' END
             WHERE subject='resource/example'", [&first.id],
        ).unwrap();
        store.read_snapshot(|_| {
            let fence = store.native_source_fence()?;
            let heads = store.native_subject_replicated_kind_heads(
                "resource/example", "resource.observed", None, &fence, 1)?;
            assert_eq!(heads[0].record.id, first.id);
            let next = store.native_subject_replicated_kind_heads(
                "resource/example", "resource.observed", Some(&first.id), &fence, 1)?;
            assert_eq!(next[0].record.id, second.id);
            let history = store.native_subject_history("resource/example", None, None, &fence, 1)?;
            assert_eq!(history[0].record.id, second.id);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn local_rows_at_one_graph_position_page_without_gaps() {
        let store = Store::open_memory("node").unwrap();
        let durable = claim(&store, "resource/example");
        for time in 1..=3 {
            store.connection.lock().unwrap().execute(
                "INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                 VALUES(?1,'resource/example','resource.observed','{\"fields\":{}}',?2)",
                params![durable.store_index,time],
            ).unwrap();
        }
        store.read_snapshot(|_| {
            let fence = store.native_source_fence()?;
            let mut before = None;
            let mut positions = Vec::new();
            loop {
                let page = store.native_subject_history("resource/example", None, before, &fence, 1)?;
                let Some(record) = page.first() else { break };
                let position = native_source_log_order(record);
                assert!(store.native_source_record_exists("resource/example",position)?);
                positions.push(position);
                before = Some(position);
            }
            assert_eq!(positions, vec![(durable.store_index,3),(durable.store_index,2),
                (durable.store_index,1),(durable.store_index,0)]);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn retention_version_detects_interior_pruning_but_ignores_new_rows() {
        let store = Store::open_memory("node").unwrap();
        let durable = claim(&store, "resource/example");
        let insert_local = || {
            store.connection.lock().unwrap().execute(
                "INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                 VALUES(?1,'resource/example','resource.observed','{\"fields\":{}}',1)",
                [durable.store_index],
            ).unwrap();
        };
        for _ in 0..3 { insert_local(); }
        let fence = store.native_source_fence().unwrap();
        let initial = store.native_subject_retention_version("resource/example",None,&fence).unwrap();
        insert_local();
        assert_eq!(initial,
            store.native_subject_retention_version("resource/example",None,&fence).unwrap());
        store.connection.lock().unwrap().execute(
            "DELETE FROM local_observations WHERE id=2", [],
        ).unwrap();
        let pruned = store.native_subject_retention_version("resource/example",None,&fence).unwrap();
        assert_ne!(initial, pruned);
        assert!(store.native_source_record_exists("resource/example",(durable.store_index,3)).unwrap());
        store.connection.lock().unwrap().execute(
            "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)", [&durable.id],
        ).unwrap();
        assert_ne!(pruned,
            store.native_subject_retention_version("resource/example",None,&fence).unwrap());
    }

    #[test]
    fn repaired_sources_stay_excluded_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let id = {
            let store = Store::open(&path, "node").unwrap();
            let record = claim(&store, "resource/repaired");
            store.connection.lock().unwrap().execute(
                "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)", [&record.id],
            ).unwrap();
            record.id
        };
        let store = Store::open(&path, "node").unwrap();
        store.read_snapshot(|_| {
            let fence = store.native_source_fence()?;
            assert!(store.native_subject_refs("resource",None,None,&fence,None,10)?.is_empty());
            assert!(store.native_subject_history("resource/repaired",None,None,&fence,10)?.is_empty());
            assert!(store.native_subject_replicated_kind_heads(
                "resource/repaired","resource.observed",None,&fence,10)?.is_empty());
            assert!(store.native_admitted_claim_by_id(&id)?.is_none());
            Ok(())
        }).unwrap();
    }
}
