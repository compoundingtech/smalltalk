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
    (
        source.record.store_index,
        source.local_position.unwrap_or(0),
    )
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

fn family_bounds(family: &str, ref_prefix: Option<&str>) -> (String, String) {
    let mut lower = format!("{family}/");
    let mut upper = format!("{family}0");
    if let Some(prefix) = ref_prefix {
        if prefix > lower.as_str() {
            lower = prefix.to_owned();
        }
        if let Some(prefix_upper) = prefix_upper_bound(prefix) {
            upper = upper.min(prefix_upper);
        }
    }
    (lower, upper)
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
            local_window_ms: observations
                .retention_ms()
                .expect("valid default retention"),
            local_max_per_subject_kind: observations.max_per_subject_kind,
            checkpoint_enabled: crate::config::CheckpointConfig::default().enabled,
        }
    }
}

impl Store {
    /// Bind the advertised limits to the same validated configuration used by
    /// the daemon's observation trimmer and checkpoint scheduler.
    pub fn configure_native_retention(
        &self,
        observations: &crate::config::ObservationsConfig,
        checkpoint_enabled: bool,
    ) -> Result<()> {
        *self
            .native_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = NativeRetentionLimits {
            local_window_ms: observations.retention_ms()?,
            local_max_per_subject_kind: observations.max_per_subject_kind,
            checkpoint_enabled,
        };
        Ok(())
    }

    pub(crate) fn native_retention_coverage(&self) -> Value {
        let limits = self
            .native_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
                "SELECT COALESCE(MAX(id),0) FROM local_observations",
                [],
                |row| row.get(0),
            )?;
            Ok(NativeSourceFence {
                graph_index,
                local_position,
            })
        })
    }

    /// Literal binary prefix ranges (never LIKE patterns), exclusive subject seek.
    /// The slash-terminated family and optional prefix ranges intersect.
    pub(crate) fn native_subject_refs(
        &self,
        family: &str,
        ref_prefix: Option<&str>,
        after_ref: Option<&str>,
        fence: &NativeSourceFence,
        recorded_actor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        let (lower, upper) = family_bounds(family, ref_prefix);
        let connection = self.readers.get();
        let local_cutoff = native_sources::local_cutoff(&connection, fence)?;
        let (seek, exclusive) = after_ref
            .filter(|after| *after >= lower.as_str())
            .map_or((lower.as_str(), false), |after| (after, true));
        // MIN(subject) seeks one eligible row, then each recursive step skips
        // that subject's entire history. Keep both seed comparisons static so
        // an exclusive cursor does not scan all rows at its previous subject.
        macro_rules! refs_sql {
            ($comparison:literal) => {
                concat!(
                    "WITH RECURSIVE durable(subject) AS (
                       SELECT MIN(record.subject) FROM claims record INDEXED BY native_claims_cover
                       WHERE record.subject", $comparison, "?1 AND record.subject<?2
                         AND record.store_index<=?3 AND (?5 IS NULL OR record.actor=?5)
                         AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
                                        WHERE repaired.id=record.id)
                       UNION ALL
                       SELECT (SELECT MIN(record.subject)
                               FROM claims record INDEXED BY native_claims_cover
                               WHERE record.subject>durable.subject AND record.subject<?2
                                 AND record.store_index<=?3 AND (?5 IS NULL OR record.actor=?5)
                                 AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
                                                WHERE repaired.id=record.id))
                       FROM durable WHERE subject IS NOT NULL LIMIT ?6
                     ), local(subject) AS (
                       SELECT MIN(record.subject)
                       FROM local_observations record INDEXED BY native_local_cover
                       WHERE record.subject", $comparison, "?1 AND record.subject<?2
                         AND record.id<=?4 AND (?5 IS NULL OR record.actor=?5)
                       UNION ALL
                       SELECT (SELECT MIN(record.subject)
                               FROM local_observations record INDEXED BY native_local_cover
                               WHERE record.subject>local.subject AND record.subject<?2
                                 AND record.id<=?4 AND (?5 IS NULL OR record.actor=?5))
                       FROM local WHERE subject IS NOT NULL LIMIT ?6
                     )
                     SELECT subject FROM durable WHERE subject IS NOT NULL
                     UNION SELECT subject FROM local WHERE subject IS NOT NULL
                     ORDER BY subject LIMIT ?6"
                )
            };
        }
        let mut statement = connection.prepare_cached(if exclusive {
            refs_sql!(">")
        } else {
            refs_sql!(">=")
        })?;
        Ok(statement
            .query_map(
                params![
                    seek,
                    upper,
                    fence.graph_index,
                    local_cutoff,
                    recorded_actor,
                    sql_limit(limit)
                ],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Source-log order DESC: durable `(store_index,0)`, local `(after_store_index,id)`.
    /// Each source query is independently limited before a bounded merge. Exclusive
    /// tuple cursors preserve all local rows sharing the same graph index.
    pub(crate) fn native_subject_history(
        &self,
        subject: &str,
        kind: Option<&str>,
        before: Option<(u64, u64)>,
        fence: &NativeSourceFence,
        limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let (before_graph, before_local) = before.map_or((None, None), |(g, l)| (Some(g), Some(l)));
        let mut durable = connection.prepare(&format!(
            "{CLAIM_COLUMNS} WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
             AND store_index<=?3 AND {ADMITTED}
             AND (?4 IS NULL OR (store_index,0)<(?4,?5))
             ORDER BY store_index DESC LIMIT ?6"
        ))?;
        let mut records = durable
            .query_map(
                params![
                    subject,
                    kind,
                    fence.graph_index,
                    before_graph,
                    before_local,
                    sql_limit(limit)
                ],
                |row| {
                    Ok(NativeSourceRecord {
                        record: claim_from_row(row)?,
                        local_position: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut local = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND (?2 IS NULL OR kind=?2)
             AND after_store_index<=?3 AND id<=?4
             AND (?5 IS NULL OR (after_store_index,id)<(?5,?6))
             ORDER BY after_store_index DESC,id DESC LIMIT ?7"
        ))?;
        records.extend(
            local
                .query_map(
                    params![
                        subject,
                        kind,
                        fence.graph_index,
                        fence.local_position,
                        before_graph,
                        before_local,
                        sql_limit(limit)
                    ],
                    |row| {
                        Ok(NativeSourceRecord {
                            record: local_observation_from_row(&self.origin, row)?,
                            local_position: Some(row.get(0)?),
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        records.sort_unstable_by_key(|record| std::cmp::Reverse(native_source_log_order(record)));
        records.truncate(limit);
        Ok(records)
    }

    /// Local head candidates, newest actual local position first. Invalid or
    /// cross-subject cursors yield no candidates, never silently restart a page.
    pub(crate) fn native_subject_local_kind_heads(
        &self,
        subject: &str,
        kind: &str,
        before_id: Option<&str>,
        fence: &NativeSourceFence,
        limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let prefix = format!("{LOCAL_OBSERVATION_ID_PREFIX}{}/", self.origin);
        let local_before = before_id
            .map(|id| {
                id.strip_prefix(&prefix)
                    .and_then(|position| position.parse::<u64>().ok())
                    .context("invalid local head cursor")
            })
            .transpose()?;
        let mut local = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND kind=?2
             AND after_store_index<=?3 AND id<=?4
             AND (?5 IS NULL OR (id<?5 AND EXISTS(SELECT 1 FROM local_observations boundary
                 WHERE boundary.id=?5 AND boundary.subject=?1 AND boundary.kind=?2)))
             ORDER BY id DESC LIMIT ?6"
        ))?;
        Ok(local
            .query_map(
                params![
                    subject,
                    kind,
                    fence.graph_index,
                    fence.local_position,
                    local_before,
                    sql_limit(limit)
                ],
                |row| {
                    Ok(NativeSourceRecord {
                        record: local_observation_from_row(&self.origin, row)?,
                        local_position: Some(row.get(0)?),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replicated candidates in canonical DESC order, NOT arrival order.
    /// Exclusive ids resolve their canonical tuple in SQL; authorize each
    /// candidate before choosing the head and continue past hidden candidates.
    pub(crate) fn native_subject_replicated_kind_heads(
        &self,
        subject: &str,
        kind: &str,
        before_id: Option<&str>,
        fence: &NativeSourceFence,
        limit: usize,
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
        Ok(durable
            .query_map(
                params![
                    subject,
                    kind,
                    fence.graph_index,
                    before_id,
                    sql_limit(limit)
                ],
                |row| {
                    Ok(NativeSourceRecord {
                        record: claim_from_row(row)?,
                        local_position: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Resolve an audience's earlier admitted durable episode in canonical order.
    /// The boundary may be another kind/subject (an answer or reply), and its local
    /// arrival index must not exclude a request admitted later by this host.
    pub(crate) fn native_preceding_kind_heads(
        &self,
        subject: &str,
        kind: &str,
        before_id: &str,
        fence: &NativeSourceFence,
        limit: usize,
    ) -> Result<Vec<NativeSourceRecord>> {
        let connection = self.readers.get();
        let seek = canonical::after_sql("boundary", "claims");
        let mut query = connection.prepare(&canonical_sql(&format!(
            "{CLAIM_COLUMNS} INDEXED BY claims_subject_kind_accepted_index
             WHERE subject=?1 AND kind=?2 AND store_index<=?3 AND {ADMITTED}
             AND EXISTS(SELECT 1 FROM claims boundary WHERE boundary.id=?4 AND {seek})
             ORDER BY CANONICAL_DESC(claims) LIMIT ?5"
        )))?;
        Ok(query
            .query_map(
                params![
                    subject,
                    kind,
                    fence.graph_index,
                    before_id,
                    sql_limit(limit)
                ],
                |row| {
                    Ok(NativeSourceRecord {
                        record: claim_from_row(row)?,
                        local_position: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Internal continuation fingerprint of retained source membership under the
    /// original upper fences. Source identities are immutable and append-only;
    /// deletes/repairs therefore reduce a count even when the cursor boundary
    /// survives. New observations above either fence do not invalidate a page.
    /// Never expose the underlying counts or source identities to clients.
    pub(crate) fn native_subject_retention_version(
        &self,
        subject: &str,
        kind: Option<&str>,
        fence: &NativeSourceFence,
    ) -> Result<String> {
        let connection = self.readers.get();
        let durable = native_sources::membership(
            &connection,
            0,
            fence.graph_index,
            &native_sources::SourceSelection {
                subject: Some(subject),
                kind,
                lower: "",
                end: "",
                recorded_actor: None,
            },
        )?;
        let local_cutoff = native_sources::local_cutoff(&connection, fence)?;
        let local = native_sources::membership(
            &connection,
            1,
            local_cutoff,
            &native_sources::SourceSelection {
                subject: Some(subject),
                kind,
                lower: "",
                end: "",
                recorded_actor: None,
            },
        )?;
        let durable = (durable.count, durable.first);
        let local = (local.count, local.first, local.last);
        // Keyed with the per-store secret: a cursor holder must not enumerate small count
        // spaces offline to recover retained membership it cannot read.
        keyed_canonical_hash(
            &native_sources::cursor_secret(&connection)?,
            &(
                "st3.native-subject-retention.v1",
                subject,
                kind,
                fence.graph_index,
                fence.local_position,
                durable,
                local,
            ),
        )
    }

    /// Family continuations pin retained membership as well as their upper
    /// fences. Pruning or repairing any admitted source invalidates the page,
    /// including sources not yet reached by the public visible-ref boundary.
    pub(crate) fn native_family_retention_version(
        &self,
        family: &str,
        ref_prefix: Option<&str>,
        fence: &NativeSourceFence,
        recorded_actor: Option<&str>,
    ) -> Result<String> {
        let (lower, upper) = family_bounds(family, ref_prefix);
        let connection = self.readers.get();
        let durable = native_sources::family_count(
            &connection,
            0,
            fence.graph_index,
            &lower,
            &upper,
            recorded_actor,
        )?;
        let local_cutoff = native_sources::local_cutoff(&connection, fence)?;
        let local = native_sources::family_count(
            &connection,
            1,
            local_cutoff,
            &lower,
            &upper,
            recorded_actor,
        )?;
        // Keyed like the per-subject fingerprint: counts include sources the cursor
        // holder cannot read, so they must not be guessable offline.
        keyed_canonical_hash(
            &native_sources::cursor_secret(&connection)?,
            &(
                "st3.native-family-retention.v1",
                family,
                ref_prefix,
                fence.graph_index,
                fence.local_position,
                recorded_actor,
                durable,
                local,
            ),
        )
    }

    pub(crate) fn native_admitted_claim_by_id(&self, id: &str) -> Result<Option<ClaimRecord>> {
        Ok(self
            .readers
            .get()
            .query_row(
                &format!("{CLAIM_COLUMNS} WHERE claims.id=?1 AND {ADMITTED}"),
                [id],
                claim_from_row,
            )
            .optional()?)
    }

    pub(crate) fn native_source_record_exists(
        &self,
        subject: &str,
        position: (u64, u64),
    ) -> Result<bool> {
        let connection = self.readers.get();
        if position.1 == 0 {
            Ok(connection.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM claims
                WHERE subject=?1 AND store_index=?2 AND {ADMITTED})"
                ),
                params![subject, position.0],
                |row| row.get(0),
            )?)
        } else {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM local_observations
                WHERE subject=?1 AND after_store_index=?2 AND id=?3)",
                params![subject, position.0, position.1],
                |row| row.get(0),
            )?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(store: &Store, subject: &str) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "resource.observed".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::from([("kind".into(), json!("human.review"))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }

    #[test]
    fn canonical_head_order_is_not_source_arrival_order() {
        let store = Store::open_memory("node").unwrap();
        let first = claim(&store, "resource/example");
        let second = claim(&store, "resource/example");
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=CASE WHEN id=?1 THEN '200' ELSE '100' END
             WHERE subject='resource/example'",
                [&first.id],
            )
            .unwrap();
        store
            .read_snapshot(|_| {
                let fence = store.native_source_fence()?;
                let heads = store.native_subject_replicated_kind_heads(
                    "resource/example",
                    "resource.observed",
                    None,
                    &fence,
                    1,
                )?;
                assert_eq!(heads[0].record.id, first.id);
                let next = store.native_subject_replicated_kind_heads(
                    "resource/example",
                    "resource.observed",
                    Some(&first.id),
                    &fence,
                    1,
                )?;
                assert_eq!(next[0].record.id, second.id);
                let history =
                    store.native_subject_history("resource/example", None, None, &fence, 1)?;
                assert_eq!(history[0].record.id, second.id);
                Ok(())
            })
            .unwrap();
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
        store
            .read_snapshot(|_| {
                let fence = store.native_source_fence()?;
                let mut before = None;
                let mut positions = Vec::new();
                loop {
                    let page = store.native_subject_history(
                        "resource/example",
                        None,
                        before,
                        &fence,
                        1,
                    )?;
                    let Some(record) = page.first() else { break };
                    let position = native_source_log_order(record);
                    assert!(store.native_source_record_exists("resource/example", position)?);
                    positions.push(position);
                    before = Some(position);
                }
                assert_eq!(
                    positions,
                    vec![
                        (durable.store_index, 3),
                        (durable.store_index, 2),
                        (durable.store_index, 1),
                        (durable.store_index, 0)
                    ]
                );
                Ok(())
            })
            .unwrap();
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
        for _ in 0..3 {
            insert_local();
        }
        let fence = store.native_source_fence().unwrap();
        let initial = store
            .native_subject_retention_version("resource/example", None, &fence)
            .unwrap();
        insert_local();
        assert_eq!(
            initial,
            store
                .native_subject_retention_version("resource/example", None, &fence)
                .unwrap()
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM local_observations WHERE id=2", [])
            .unwrap();
        let pruned = store
            .native_subject_retention_version("resource/example", None, &fence)
            .unwrap();
        assert_ne!(initial, pruned);
        assert!(
            store
                .native_source_record_exists("resource/example", (durable.store_index, 3))
                .unwrap()
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                [&durable.id],
            )
            .unwrap();
        assert_ne!(
            pruned,
            store
                .native_subject_retention_version("resource/example", None, &fence)
                .unwrap()
        );
    }

    fn insert_local(store: &Store, subject: &str, actor: &str) -> u64 {
        let graph = store.native_source_fence().unwrap().graph_index;
        let connection = store.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO local_observations(after_store_index,subject,kind,actor,body,observed_at_unix_ms)
             VALUES(?1,?2,'resource.observed',?3,'{\"fields\":{}}',1)",
            params![graph,subject,actor],
        ).unwrap();
        u64::try_from(connection.last_insert_rowid()).unwrap()
    }

    #[test]
    fn family_membership_tracks_unseen_sources_but_not_unrelated_mutations() {
        let store = Store::open_memory("node").unwrap();
        let first = claim(&store, "resource/selected/a");
        let unseen = claim(&store, "resource/selected/z");
        let outside_prefix = claim(&store, "resource/elsewhere");
        let other_actor = store
            .append_claim(&ClaimInput {
                subject: "resource/selected/other".into(),
                kind: "resource.observed".into(),
                actor: Some("person/other".into()),
                fields: BTreeMap::from([("kind".into(), json!("human.review"))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let local = insert_local(&store, "resource/selected/local", "person/example");
        let other_local = insert_local(&store, "resource/selected/other-local", "person/other");
        let fence = store.native_source_fence().unwrap();
        let version = || {
            store
                .native_family_retention_version(
                    "resource",
                    Some("resource/selected/"),
                    &fence,
                    Some("person/example"),
                )
                .unwrap()
        };
        let initial = version();
        assert_eq!(
            store
                .native_subject_refs(
                    "resource",
                    Some("resource/selected/"),
                    None,
                    &fence,
                    Some("person/example"),
                    1
                )
                .unwrap(),
            vec![first.subject.clone()]
        );
        let later = claim(&store, "resource/selected/later");
        let later_local = insert_local(&store, "resource/selected/later-local", "person/example");
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "DELETE FROM claims WHERE id IN (?1,?2,?3)",
                    params![outside_prefix.id, other_actor.id, later.id],
                )
                .unwrap();
            connection
                .execute(
                    "DELETE FROM local_observations WHERE id IN (?1,?2)",
                    params![other_local, later_local],
                )
                .unwrap();
        }
        assert_eq!(initial, version());
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                [&unseen.id],
            )
            .unwrap();
        let repaired = version();
        assert_ne!(initial, repaired);
        assert_eq!(
            store
                .native_subject_refs(
                    "resource",
                    Some("resource/selected/"),
                    Some(&first.subject),
                    &fence,
                    Some("person/example"),
                    10
                )
                .unwrap(),
            vec!["resource/selected/local"]
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM claims WHERE id=?1", [&unseen.id])
            .unwrap();
        assert_eq!(
            repaired,
            version(),
            "a repaired source must not be removed twice"
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM local_observations WHERE id=?1", [local])
            .unwrap();
        assert_ne!(repaired, version());
    }

    #[test]
    fn source_indexes_match_original_fenced_membership_and_cursor_hashes() {
        let store = Store::open_memory("node").unwrap();
        let mut records = Vec::new();
        let mut fences = Vec::new();
        // Grow history while keeping family cardinality fixed and retaining old fences.
        for index in 0..270 {
            records.push(claim(
                &store,
                if index % 2 == 0 {
                    "resource/selected/a"
                } else {
                    "resource/selected/z"
                },
            ));
            insert_local(&store, "resource/selected/a", "person/example");
            if [0, 14, 15, 16, 254, 255, 256, 269].contains(&index) {
                fences.push(store.native_source_fence().unwrap());
            }
        }
        let assert_membership = || {
            let connection = store.readers.get();
            for fence in &fences {
                for subject in [
                    None,
                    Some("resource/selected/a"),
                    Some("resource/selected/z"),
                ] {
                    for kind in [None, Some("resource.observed"), Some("missing.kind")] {
                        if subject.is_none() && kind.is_some() {
                            continue;
                        }
                        for source in 0..=1 {
                            let expected: (u64, Option<u64>, Option<u64>) =
                                if source == 0 {
                                    connection
                                        .query_row(
                                            &format!(
                                    "SELECT COUNT(*),MIN(store_index),MAX(store_index) FROM claims
                                     WHERE subject>='resource/' AND subject<'resource0'
                                       AND (?1 IS NULL OR subject=?1) AND (?2 IS NULL OR kind=?2)
                                       AND store_index<=?3 AND {ADMITTED}"
                                ),
                                            params![subject, kind, fence.graph_index],
                                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                                        )
                                        .unwrap()
                                } else {
                                    connection.query_row(
                                    "SELECT COUNT(*),MIN(id),MAX(id) FROM local_observations
                                     WHERE subject>='resource/' AND subject<'resource0'
                                       AND (?1 IS NULL OR subject=?1) AND (?2 IS NULL OR kind=?2)
                                       AND after_store_index<=?3 AND id<=?4",
                                    params![subject,kind,fence.graph_index,fence.local_position],
                                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap()
                                };
                            let upper = if source == 0 {
                                fence.graph_index
                            } else {
                                native_sources::local_cutoff(&connection, fence).unwrap()
                            };
                            let actual = native_sources::membership(
                                &connection,
                                source,
                                upper,
                                &native_sources::SourceSelection {
                                    subject,
                                    kind,
                                    lower: "resource/",
                                    end: "resource0",
                                    recorded_actor: None,
                                },
                            )
                            .unwrap();
                            assert_eq!((actual.count, actual.first, actual.last), expected);
                        }
                    }
                }
                let durable: (u64, Option<u64>) = connection
                    .query_row(
                        &format!(
                            "SELECT COUNT(*),MIN(store_index) FROM claims
                     WHERE subject='resource/selected/a' AND store_index<=?1 AND {ADMITTED}"
                        ),
                        [fence.graph_index],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                let local: (u64, Option<u64>, Option<u64>) = connection
                    .query_row(
                        "SELECT COUNT(*),MIN(id),MAX(id) FROM local_observations
                     WHERE subject='resource/selected/a' AND after_store_index<=?1 AND id<=?2",
                        params![fence.graph_index, fence.local_position],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
                let legacy = keyed_canonical_hash(
                    &native_sources::cursor_secret(&connection).unwrap(),
                    &(
                        "st3.native-subject-retention.v1",
                        "resource/selected/a",
                        None::<&str>,
                        fence.graph_index,
                        fence.local_position,
                        durable,
                        local,
                    ),
                )
                .unwrap();
                assert_eq!(
                    store
                        .native_subject_retention_version("resource/selected/a", None, fence)
                        .unwrap(),
                    legacy
                );
                let durable_count: u64 = connection
                    .query_row(
                        &format!(
                    "SELECT COUNT(*) FROM claims WHERE subject>='resource/' AND subject<'resource0'
                     AND store_index<=?1 AND {ADMITTED}"
                ),
                        [fence.graph_index],
                        |row| row.get(0),
                    )
                    .unwrap();
                let local_count: u64 = connection.query_row(
                    "SELECT COUNT(*) FROM local_observations WHERE subject>='resource/' AND subject<'resource0'
                     AND after_store_index<=?1 AND id<=?2",
                    params![fence.graph_index,fence.local_position], |row| row.get(0)).unwrap();
                let legacy_family = keyed_canonical_hash(
                    &native_sources::cursor_secret(&connection).unwrap(),
                    &(
                        "st3.native-family-retention.v1",
                        "resource",
                        None::<&str>,
                        fence.graph_index,
                        fence.local_position,
                        None::<&str>,
                        durable_count,
                        local_count,
                    ),
                )
                .unwrap();
                assert_eq!(
                    store
                        .native_family_retention_version("resource", None, fence, None)
                        .unwrap(),
                    legacy_family
                );
            }
        };
        assert_membership();
        {
            let connection = store.connection.lock().unwrap();
            for index in [0, 14, 15, 16, 254, 255, 256, 269] {
                connection
                    .execute("DELETE FROM claims WHERE id=?1", [&records[index].id])
                    .unwrap();
                connection
                    .execute(
                        "DELETE FROM local_observations WHERE id=?1",
                        [u64::try_from(index + 1).unwrap()],
                    )
                    .unwrap();
            }
            connection
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                    [&records[100].id],
                )
                .unwrap();
        }
        assert_membership();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM projection_digest_repaired_claims WHERE id=?1",
                [&records[100].id],
            )
            .unwrap();
        assert_membership();
        {
            let mut connection = store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            native_sources::open(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_membership();
    }

    #[test]
    fn cursor_fingerprints_are_keyed_per_store() {
        let version = |store: &Store| {
            store.read_snapshot(|_| {
                let fence = store.native_source_fence()?;
                store.native_family_retention_version("resource", None, &fence, None)
            })
        };
        let unkeyed = |store: &Store| {
            let connection = store.readers.get();
            let durable: u64 = connection
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM claims WHERE subject>='resource/' AND subject<'resource0'
                         AND {ADMITTED}"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let local: u64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM local_observations WHERE subject>='resource/'
                     AND subject<'resource0'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let fence = store.native_source_fence().unwrap();
            canonical_hash(&(
                "st3.native-family-retention.v1",
                "resource",
                None::<&str>,
                fence.graph_index,
                fence.local_position,
                None::<&str>,
                durable,
                local,
            ))
            .unwrap()
        };
        let first = Store::open_memory("node-a").unwrap();
        claim(&first, "resource/selected/a");
        let second = Store::open_memory("node-b").unwrap();
        claim(&second, "resource/selected/a");
        // Identical claims and fences: only the per-store secret differs.
        let first_version = version(&first).unwrap();
        let second_version = version(&second).unwrap();
        assert_ne!(first_version, second_version);
        // The fingerprint must not be the offline-guessable unkeyed hash of the same
        // retained counts, or a cursor holder could enumerate small count spaces.
        assert_ne!(first_version, unkeyed(&first));
        assert_ne!(second_version, unkeyed(&second));
    }

    #[test]
    fn source_refs_intersect_literal_prefix_actor_and_original_fences() {
        let store = Store::open_memory("node").unwrap();
        claim(&store, "resource/%_one");
        claim(&store, "resource/plain");
        let local = insert_local(&store, "resource/%_local", "person/other");
        let fence = store.native_source_fence().unwrap();
        let later = claim(&store, "resource/%_later");
        insert_local(&store, "resource/%_later-local", "person/example");
        assert_eq!(
            store
                .native_subject_refs("resource", Some("resource/%_"), None, &fence, None, 10)
                .unwrap(),
            vec!["resource/%_local", "resource/%_one"]
        );
        assert_eq!(
            store
                .native_subject_refs(
                    "resource",
                    Some("resource/%_"),
                    None,
                    &fence,
                    Some("person/example"),
                    10
                )
                .unwrap(),
            vec!["resource/%_one"]
        );
        let graph_only = NativeSourceFence {
            graph_index: fence.graph_index,
            local_position: u64::MAX,
        };
        assert_eq!(
            store
                .native_subject_refs("resource", Some("resource/%_"), None, &graph_only, None, 10)
                .unwrap(),
            vec!["resource/%_local", "resource/%_one"]
        );
        let local_only = NativeSourceFence {
            graph_index: later.store_index,
            local_position: local - 1,
        };
        assert_eq!(
            store
                .native_subject_refs("resource", Some("resource/%_"), None, &local_only, None, 10)
                .unwrap(),
            vec!["resource/%_later", "resource/%_one"]
        );
    }

    #[test]
    fn subject_kind_retention_ignores_removal_of_other_kinds() {
        let store = Store::open_memory("node").unwrap();
        let durable = claim(&store, "resource/example");
        let local = insert_local(&store, "resource/example", "person/example");
        {
            let connection = store.connection.lock().unwrap();
            connection.execute(
                "INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                 VALUES(?1,'resource/example','probe.other','{}',1)", [durable.store_index],
            ).unwrap();
        }
        let fence = store.native_source_fence().unwrap();
        let selected = store
            .native_subject_retention_version("resource/example", Some("resource.observed"), &fence)
            .unwrap();
        let all = store
            .native_subject_retention_version("resource/example", None, &fence)
            .unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM local_observations WHERE kind='probe.other'",
                [],
            )
            .unwrap();
        assert_eq!(
            selected,
            store
                .native_subject_retention_version(
                    "resource/example",
                    Some("resource.observed"),
                    &fence
                )
                .unwrap()
        );
        assert_ne!(
            all,
            store
                .native_subject_retention_version("resource/example", None, &fence)
                .unwrap()
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM local_observations WHERE id=?1", [local])
            .unwrap();
        assert_ne!(
            selected,
            store
                .native_subject_retention_version(
                    "resource/example",
                    Some("resource.observed"),
                    &fence
                )
                .unwrap()
        );
    }

    #[test]
    fn replicated_sources_and_host_local_sources_keep_separate_membership() {
        let source = Store::open_memory("source").unwrap();
        let target = Store::open_memory("target").unwrap();
        let durable = claim(&source, "resource/shared");
        insert_local(&source, "resource/source-local", "person/example");
        insert_local(&target, "resource/target-local", "person/example");
        let exchange =
            super::super::tests::exchange_from(&source, &target.replication_inventory().unwrap());
        super::super::tests::receive_and_project(&target, "source", &exchange);
        let fence = target.native_source_fence().unwrap();
        assert_eq!(
            target
                .native_subject_refs("resource", None, None, &fence, None, 10)
                .unwrap(),
            vec!["resource/shared", "resource/target-local"]
        );
        let history = target
            .native_subject_history("resource/shared", None, None, &fence, 10)
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].record.id, durable.id);
        assert!(history[0].local_position.is_none());
        let initial = target
            .native_family_retention_version("resource", None, &fence, None)
            .unwrap();
        claim(&source, "resource/shared");
        let exchange =
            super::super::tests::exchange_from(&source, &target.replication_inventory().unwrap());
        super::super::tests::receive_and_project(&target, "source", &exchange);
        assert_eq!(
            initial,
            target
                .native_family_retention_version("resource", None, &fence, None)
                .unwrap()
        );
        target
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM claims WHERE id=?1", [&durable.id])
            .unwrap();
        assert_ne!(
            initial,
            target
                .native_family_retention_version("resource", None, &fence, None)
                .unwrap()
        );
        let target_fence = target.native_source_fence().unwrap();
        assert_eq!(
            target
                .native_subject_refs("resource", None, None, &target_fence, None, 10)
                .unwrap(),
            vec!["resource/shared", "resource/target-local"]
        );
    }

    #[test]
    fn repaired_sources_stay_excluded_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let id = {
            let store = Store::open(&path, "node").unwrap();
            let record = claim(&store, "resource/repaired");
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                    [&record.id],
                )
                .unwrap();
            record.id
        };
        let store = Store::open(&path, "node").unwrap();
        store
            .read_snapshot(|_| {
                let fence = store.native_source_fence()?;
                assert!(
                    store
                        .native_subject_refs("resource", None, None, &fence, None, 10)?
                        .is_empty()
                );
                assert!(
                    store
                        .native_subject_history("resource/repaired", None, None, &fence, 10)?
                        .is_empty()
                );
                assert!(
                    store
                        .native_subject_replicated_kind_heads(
                            "resource/repaired",
                            "resource.observed",
                            None,
                            &fence,
                            10
                        )?
                        .is_empty()
                );
                assert!(store.native_admitted_claim_by_id(&id)?.is_none());
                Ok(())
            })
            .unwrap();
    }
}
