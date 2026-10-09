//! The agents roster reduces every agent's status at one snapshot. One subject at a time, that
//! costs a dozen statements per agent; here each chunk of subjects reads what the same
//! reductions need in a few statements, and the shared folds reduce what it read.
use super::*;

/// Subjects per statement, so that no single statement holds the reader for the whole fleet.
const CHUNK: usize = 256;

/// What one fold has read at its snapshot, kept across its view and status passes.
pub(super) struct CardReads {
    index: u64,
    owned: Option<BTreeMap<String, BTreeSet<String>>>,
    desired: HashMap<String, Option<DesiredRow>>,
    actual: HashMap<String, ActualState>,
}

/// A subject's folded actual state and its selected actual-state claim: the claim, its origin
/// and the rival observations it must descend from to hold alone.
struct ActualState {
    latest: Option<Value>,
    source: Option<(String, String, Vec<String>)>,
}

fn json_list<'a>(subjects: impl IntoIterator<Item = &'a String>) -> Result<String> {
    Ok(serde_json::to_string(&subjects.into_iter().collect::<Vec<_>>())?)
}

impl CardReads {
    pub(super) fn new(index: u64) -> Self {
        Self { index, owned: None, desired: HashMap::new(), actual: HashMap::new() }
    }

    /// Read the declarations of `subjects` not read yet: one read of the set receipts, and one
    /// statement per chunk for the declaration claims of subjects no set owns.
    fn load_desired(&mut self, connection: &Connection, subjects: &[String]) -> Result<()> {
        let missing = subjects
            .iter()
            .filter(|subject| !self.desired.contains_key(*subject))
            .cloned()
            .collect::<BTreeSet<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        if self.owned.is_none() {
            self.owned = Some(
                owned_sets::owners_at(connection, Some(self.index)).map_err(anyhow::Error::new)?,
            );
        }
        let owned = self.owned.as_ref().expect("read above");
        let mut unowned = Vec::new();
        for subject in missing {
            match owned.get(&subject).map(BTreeSet::len) {
                Some(2..) => {
                    // As `owned_sets::owner` refuses it.
                    return Err(anyhow::Error::new(St3Error::new(
                        "owned-set-conflict",
                        format!("{subject} has conflicting set owners"),
                    )));
                }
                Some(_) => {
                    let row = owned_sets::desired_at(connection, &subject, self.index)?;
                    self.desired.insert(subject, row);
                }
                None => unowned.push(subject),
            }
        }
        let mut statement = connection.prepare_cached(&canonical_sql(
            "SELECT claims.subject, claims.id, claims.body, claims.predecessors
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_subject_kind_index
               ON claims.subject=subjects.value AND claims.kind='intent.desired'
             WHERE claims.store_index<=?2
               AND NOT EXISTS (
                   SELECT 1 FROM replica_records
                   WHERE replica_records.claim_id=claims.id
                     AND replica_records.state='repaired'
               )
             ORDER BY claims.subject, CANONICAL_ASC(claims)",
        ))?;
        for chunk in unowned.chunks(CHUNK) {
            let mut rows = BTreeMap::<String, Vec<(String, String, String)>>::new();
            for row in statement.query_map(params![json_list(chunk)?, self.index], |row| {
                Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
            })? {
                let (subject, row) = row?;
                rows.entry(subject).or_default().push(row);
            }
            for subject in chunk {
                let row = select_desired_row(rows.remove(subject).unwrap_or_default())?;
                self.desired.insert(subject.clone(), row);
            }
        }
        Ok(())
    }

    /// Read the actual states of `subjects` not read yet, one statement per chunk. A subject's
    /// source selection uses its declared host, so declarations are read first.
    fn load_actual(&mut self, connection: &Connection, subjects: &[String]) -> Result<()> {
        let missing = subjects
            .iter()
            .filter(|subject| !self.actual.contains_key(*subject))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        // The same claims `latest_actual_at` folds and `selected_actual_source_at` selects from:
        // the actual-state claims in canonical order, the fold leaving out `harness.` in any case.
        let mut statement = connection.prepare_cached(&format!(
            "SELECT claims.subject, claims.id, claims.kind, claims.origin, claims.body,
                    claims.kind NOT LIKE 'harness.%'
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_subject_kind_index
               ON claims.subject=subjects.value
             JOIN batches ON batches.id=claims.batch_id
             WHERE {ACTUAL_STATE_CLAIM} AND claims.store_index<=?2
             ORDER BY claims.subject, {CANONICAL_ORDER}"
        ))?;
        type Row = (String, String, String, String, bool);
        for chunk in missing.chunks(CHUNK) {
            let mut rows = BTreeMap::<String, Vec<Row>>::new();
            for row in statement.query_map(params![json_list(chunk)?, self.index], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?),
                ))
            })? {
                let (subject, row) = row?;
                rows.entry(subject).or_default().push(row);
            }
            for subject in chunk {
                let rows = rows.remove(subject).unwrap_or_default();
                let latest = fold_latest_actual(
                    rows.iter()
                        .filter(|(.., folded)| *folded)
                        .map(|(_, kind, _, body, _)| (kind.clone(), body.clone())),
                )?;
                let sources = rows
                    .into_iter()
                    .map(|(id, kind, origin, body, _)| {
                        let body = if kind == "runtime.observed" {
                            serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        };
                        (id, kind, origin, body)
                    })
                    .collect::<Vec<_>>();
                let host = self
                    .desired
                    .get(subject)
                    .and_then(Option::as_ref)
                    .and_then(|row| row.member.as_deref())
                    .and_then(|value| serde_json::from_str::<crate::model::MemberSpec>(value).ok())
                    .map(|member| member.host);
                let source = select_actual_source(&sources, host.as_deref());
                self.actual.insert(subject.clone(), ActualState { latest, source });
            }
        }
        Ok(())
    }

    fn desired(&self, connection: &Connection, subject: &str) -> Result<Option<DesiredRow>> {
        match self.desired.get(subject) {
            Some(row) => Ok(row.clone()),
            None => desired_row_at(connection, subject, Some(self.index)),
        }
    }

    fn latest(&self, connection: &Connection, subject: &str) -> Result<Option<Value>> {
        match self.actual.get(subject) {
            Some(actual) => Ok(actual.latest.clone()),
            None => latest_actual_at(connection, subject, Some(self.index)),
        }
    }

    /// The owning runs and generations of the declarations read for `subjects`.
    fn owners(&self, subjects: &[String]) -> Vec<String> {
        subjects
            .iter()
            .filter_map(|subject| self.desired.get(subject).and_then(Option::as_ref))
            .flat_map(|row| row.owner_run.iter().chain(row.owner_generation.iter()))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Each subject's newest claim index at or before `index`, one statement per chunk.
fn heads(connection: &Connection, subjects: &[String], index: u64) -> Result<HashMap<String, u64>> {
    let mut statement = connection.prepare_cached(
        "SELECT subjects.value,
                (SELECT COALESCE(MAX(store_index), 0) FROM claims
                 WHERE subject=subjects.value AND store_index<=?2)
         FROM json_each(?1) subjects",
    )?;
    let mut heads = HashMap::new();
    for chunk in subjects.chunks(CHUNK) {
        for row in statement.query_map(params![json_list(chunk)?, index], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })? {
            let (subject, head) = row?;
            heads.insert(subject, head);
        }
    }
    Ok(heads)
}

impl Store {
    /// [`runtime_view_entry`] of each runtime of `subjects`, read a chunk at a time.
    pub(super) fn runtime_view_entries(
        &self,
        connection: &Connection,
        subjects: &[String],
        store_index: u64,
        owners: bool,
        reads: &mut CardReads,
    ) -> Result<HashMap<String, ViewEntry>> {
        debug_assert_eq!(reads.index, store_index);
        let mut statement = connection.prepare_cached(&format!(
            "SELECT subjects.value,
                    (SELECT COALESCE(MAX(store_index), 0) FROM claims
                     WHERE subject=subjects.value AND store_index<=?2),
                    EXISTS(SELECT 1 FROM claims INDEXED BY claims_current_view_index
                           WHERE subject=subjects.value AND store_index<=?2
                             AND {CURRENT_VIEW_CLAIM})
             FROM json_each(?1) subjects"
        ))?;
        let mut entries = HashMap::new();
        for chunk in subjects.chunks(CHUNK) {
            let mut read = Vec::with_capacity(chunk.len());
            for row in statement.query_map(params![json_list(chunk)?, store_index], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?, row.get::<_, bool>(2)?))
            })? {
                read.push(row?);
            }
            let candidates = read
                .iter()
                .filter(|(_, _, candidate)| *candidate)
                .map(|(subject, ..)| subject.clone())
                .collect::<Vec<_>>();
            reads.load_desired(connection, &candidates)?;
            let owner_subjects = if owners { reads.owners(&candidates) } else { Vec::new() };
            let mut actual = candidates.clone();
            actual.extend(owner_subjects.iter().cloned());
            reads.load_actual(connection, &actual)?;
            let owner_heads = heads(connection, &owner_subjects, store_index)?;
            for (subject, head, candidate) in read {
                let reads = &*reads;
                let entry = runtime_view_entry_from(
                    connection,
                    &subject,
                    store_index,
                    owners,
                    head,
                    candidate,
                    &mut || reads.desired(connection, &subject),
                    &mut |of| reads.latest(connection, of),
                    &mut |owner| match owner_heads.get(owner) {
                        Some(head) => Ok(*head),
                        None => subject_head_at(connection, owner, store_index),
                    },
                )?;
                entries.insert(subject, entry);
            }
        }
        Ok(entries)
    }

    /// The agent card status ([`SubjectStatusMode::AgentCard`]) of each of `subjects` at
    /// `store_index`, read a chunk at a time.
    pub(super) fn agent_card_statuses(
        &self,
        connection: &Connection,
        subjects: &[String],
        store_index: u64,
        reads: &mut CardReads,
    ) -> Result<HashMap<String, (SubjectStatus, Option<PlannedAction>)>> {
        debug_assert_eq!(reads.index, store_index);
        let mut statuses = HashMap::new();
        let (single, batched): (Vec<_>, Vec<_>) =
            subjects.iter().cloned().partition(|subject| subject.starts_with("arrangement/"));
        for subject in single {
            let status = subject_status_at_with_mode(
                connection, &subject, Some(store_index), None, SubjectStatusMode::AgentCard,
            )?
            .expect("a reduction without an owner filter always has a status");
            statuses.insert(subject, status);
        }
        // `has_unknown_claim_at` at a snapshot: an idempotency conflict, then an unknown kind.
        let mut conflicts = connection.prepare_cached(
            "SELECT subjects.value,
                    (SELECT operations.id FROM operations
                     CROSS JOIN claims INDEXED BY claims_operation_index
                       ON +operations.id=json_extract(claims.body, '$._operation.id')
                     WHERE operations.state='conflict'
                       AND json_extract(claims.body, '$._operation.id') IS NOT NULL
                       AND claims.subject=subjects.value AND claims.store_index<=?2
                     ORDER BY operations.id LIMIT 1)
             FROM json_each(?1) subjects",
        )?;
        let mut kinds = connection.prepare_cached(
            "SELECT DISTINCT claims.subject, claims.kind
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_subject_kind_index
               ON claims.subject=subjects.value
             WHERE claims.store_index<=?2",
        )?;
        // An undeclared card's provenance is its newest claim in canonical order. The index
        // gives the newest accepted time first, so only that millisecond's claims are sorted.
        let mut newest = connection.prepare_cached(&canonical_sql(
            "SELECT subjects.value,
                    (SELECT id FROM claims INDEXED BY claims_subject_accepted_index
                     WHERE subject=subjects.value AND store_index<=?2
                     ORDER BY CANONICAL_DESC(claims) LIMIT 1)
             FROM json_each(?1) subjects",
        ))?;
        for chunk in batched.chunks(CHUNK) {
            let chunk = chunk.to_vec();
            reads.load_desired(connection, &chunk)?;
            let mut actual = chunk.clone();
            actual.extend(reads.owners(&chunk));
            reads.load_actual(connection, &actual)?;
            let list = json_list(&chunk)?;
            let mut conflict = HashMap::new();
            for row in conflicts.query_map(params![list, store_index], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })? {
                let (subject, operation) = row?;
                conflict.insert(subject, operation);
            }
            let mut subject_kinds = HashMap::<String, Vec<String>>::new();
            for row in kinds.query_map(params![list, store_index], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (subject, kind) = row?;
                subject_kinds.entry(subject).or_default().push(kind);
            }
            let undeclared = chunk
                .iter()
                .filter(|subject| reads.desired.get(*subject).is_some_and(Option::is_none))
                .collect::<Vec<_>>();
            let mut newest_claim = HashMap::new();
            if !undeclared.is_empty() {
                for row in newest.query_map(params![json_list(undeclared)?, store_index], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })? {
                    let (subject, claim) = row?;
                    newest_claim.insert(subject, claim);
                }
            }
            for subject in chunk {
                #[cfg(test)]
                SUBJECT_REDUCTIONS.with(|reductions| reductions.set(reductions.get() + 1));
                let desired = reads.desired(connection, &subject)?;
                let member = desired
                    .as_ref()
                    .and_then(|row| row.member.as_deref())
                    .and_then(|value| serde_json::from_str::<crate::model::MemberSpec>(value).ok());
                let actual = reads.latest(connection, &subject)?;
                let actual_source = match reads.actual.get(&subject).and_then(|a| a.source.as_ref()) {
                    None => (None, None, false),
                    Some((selected, origin, rivals)) => {
                        let rivals = rivals.iter().map(String::as_str).collect::<Vec<_>>();
                        let conflict = !rivals.is_empty()
                            && !subject_descends_from_all(
                                connection, &subject, store_index, selected, &rivals,
                            )?;
                        (Some(selected.clone()), Some(origin.clone()), conflict)
                    }
                };
                let claims = if desired.is_some() {
                    Vec::new()
                } else {
                    newest_claim.remove(&subject).flatten().into_iter().collect()
                };
                let unknown_claim = match conflict.remove(&subject).flatten() {
                    Some(operation) => Some(format!("idempotency-conflict:{operation}")),
                    None => {
                        let mut kinds = subject_kinds.remove(&subject).unwrap_or_default();
                        kinds.sort();
                        first_unknown_kind(kinds)
                    }
                };
                let inputs = SubjectStatusInputs {
                    desired,
                    member,
                    actual,
                    actual_source,
                    harness: None,
                    claims,
                    conflicts: Vec::new(),
                    unknown_claim,
                };
                let reads = &*reads;
                let status = assemble_subject_status(
                    connection, &subject, Some(store_index), inputs,
                    &mut |owner| reads.latest(connection, owner),
                )?;
                statuses.insert(subject, status);
            }
        }
        Ok(statuses)
    }
}
