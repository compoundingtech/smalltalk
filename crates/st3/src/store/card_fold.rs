//! The agents roster reduces every agent's status at one snapshot. One subject at a time, that
//! costs a dozen statements per agent; here each chunk of subjects reads what the same
//! reductions need in a few statements, and the shared folds reduce what it read.
use super::*;

#[path = "harness_health_capture.rs"]
#[cfg(test)]
mod health_capture;

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

/// An actual-state claim as the chunked read returns it: id, kind, origin, body, and whether
/// the folded state includes it.
type ActualRow = (String, String, String, String, bool);

/// The leading terms of a claim's canonical key: accepted time, writer, writer sequence, batch.
type BatchKey = (String, String, Option<i64>, String);

/// One subject's `rows` in canonical order: by their canonical key's leading terms, and claims
/// that tie on every leading term, in one batch, by their position in it and then by id.
fn canonical_order(
    connection: &Connection,
    mut rows: Vec<(ActualRow, BatchKey)>,
) -> Result<Vec<ActualRow>> {
    // As SQLite orders them: accepted time by length and then text, writer and batch as text,
    // and a missing writer sequence first.
    rows.sort_by(|(_, left), (_, right)| {
        (left.0.len(), &left.0, &left.1, left.2, &left.3)
            .cmp(&(right.0.len(), &right.0, &right.1, right.2, &right.3))
    });
    let mut start = 0;
    while start < rows.len() {
        let end = start
            + rows[start..]
                .iter()
                .take_while(|(_, key)| *key == rows[start].1)
                .count();
        if end - start > 1 {
            let ids = rows[start..end].iter().map(|(row, _)| &row.0).collect::<Vec<_>>();
            let mut positions = HashMap::new();
            for row in connection
                .prepare_cached(&format!(
                    "SELECT claims.id, {} FROM claims
                     WHERE claims.id IN (SELECT value FROM json_each(?1))",
                    smallclaims::store::canonical::position_sql("claims")
                ))?
                .query_map([serde_json::to_string(&ids)?], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
            {
                let (id, position) = row?;
                positions.insert(id, position);
            }
            rows[start..end].sort_by(|(left, _), (right, _)| {
                (positions.get(&left.0), &left.0).cmp(&(positions.get(&right.0), &right.0))
            });
        }
        start = end;
    }
    Ok(rows.into_iter().map(|(row, _)| row).collect())
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
        // the actual-state claims, the fold leaving out `harness.` in any case. The cross join
        // reads them one subject at a time, and each subject's rows are put in canonical order
        // here (`canonical_order`) instead of by a sorter that would copy every body.
        let mut statement = connection.prepare_cached(&format!(
            "SELECT claims.subject, claims.id, claims.kind, claims.origin, claims.body,
                    claims.kind NOT LIKE 'harness.%',
                    claims.accepted_at_unix_ms, batches.origin, batches.replica_sequence,
                    claims.batch_id
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_subject_kind_index
               ON claims.subject=subjects.value
             JOIN batches ON batches.id=claims.batch_id
             WHERE {ACTUAL_STATE_CLAIM} AND claims.store_index<=?2"
        ))?;
        // Each subject is folded as soon as its rows end, so a chunk holds one subject's
        // history at a time, as a one-subject read does.
        let mut chunks = missing.chunks(CHUNK).map(<[String]>::to_vec).collect::<VecDeque<_>>();
        while let Some(chunk) = chunks.pop_front() {
            let mut rows = statement.query(params![json_list(&chunk)?, self.index])?;
            let mut current: Option<(String, Vec<(ActualRow, BatchKey)>)> = None;
            let mut folded = HashSet::new();
            let mut again = BTreeSet::new();
            while let Some(row) = rows.next()? {
                let subject = row.get::<_, String>(0)?;
                let item = (
                    (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?),
                    (row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?),
                );
                match current.as_mut() {
                    Some((held, items)) if *held == subject => items.push(item),
                    _ => {
                        // A subject whose rows did not arrive together is read again alone.
                        if !folded.insert(subject.clone()) {
                            again.insert(subject.clone());
                        }
                        if let Some((held, items)) = current.replace((subject, vec![item])) {
                            let items = canonical_order(connection, items)?;
                            self.fold_actual(held, items)?;
                        }
                    }
                }
            }
            if let Some((held, items)) = current {
                let items = canonical_order(connection, items)?;
                self.fold_actual(held, items)?;
            }
            drop(rows);
            for subject in &chunk {
                if !self.actual.contains_key(subject) {
                    self.fold_actual(subject.clone(), Vec::new())?;
                }
            }
            for subject in again {
                self.actual.remove(&subject);
                chunks.push_back(vec![subject]);
            }
        }
        let mut registers = connection.prepare_cached(
            "SELECT subject,kind,body FROM latest_values v
             WHERE subject IN (SELECT value FROM json_each(?1)) AND kind='workspace.observed'
             AND source_at>=COALESCE((SELECT CAST(c.accepted_at_unix_ms AS INTEGER)
                 FROM claims c INDEXED BY claims_subject_kind_accepted_index
                 WHERE c.subject=v.subject AND c.kind=v.kind AND c.store_index<=?2
                 ORDER BY length(c.accepted_at_unix_ms) DESC,c.accepted_at_unix_ms DESC LIMIT 1),0)
             ORDER BY source_at,source_id",
        )?;
        for chunk in missing.chunks(CHUNK) {
            for row in registers.query_map(params![json_list(chunk)?, self.index], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (subject, kind, body) = row?;
                if let Some(actual) = self.actual.get_mut(&subject) {
                    let workspace: Value = serde_json::from_str(&body)?;
                    let fields = workspace.get("fields").unwrap_or(&workspace);
                    if actual
                        .latest
                        .as_ref()
                        .and_then(|actual| actual["host"].as_str())
                        .is_some_and(|host| fields["host"].as_str() != Some(host))
                    {
                        continue;
                    }
                    actual.latest = fold_latest_values([
                        (String::new(), actual.latest.take().unwrap_or(Value::Null)),
                        (kind, workspace),
                    ])?;
                }
            }
        }
        Ok(())
    }

    /// Fold one subject's actual-state rows, as `latest_actual_at` and
    /// `selected_actual_source_at` do.
    fn fold_actual(&mut self, subject: String, rows: Vec<ActualRow>) -> Result<()> {
        // Each body is parsed once, for the fold and for source selection both.
        let mut parsed = Vec::with_capacity(rows.len());
        for (id, kind, origin, body, folded) in rows {
            let value = if folded {
                serde_json::from_str::<Value>(&body)?
            } else if kind == "runtime.observed" {
                serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            parsed.push((id, kind, origin, value, folded));
        }
        let host = self
            .desired
            .get(&subject)
            .and_then(Option::as_ref)
            .and_then(|row| row.member.as_deref())
            .and_then(|value| serde_json::from_str::<crate::model::MemberSpec>(value).ok())
            .map(|member| member.host);
        // Source selection reads the runtime bodies in place; then the fold takes them.
        let source = {
            let null = Value::Null;
            let rows = parsed
                .iter()
                .map(|(id, kind, origin, value, _)| {
                    let body = if kind == "runtime.observed" { value } else { &null };
                    (id.as_str(), kind.as_str(), origin.as_str(), body)
                })
                .collect::<Vec<_>>();
            select_actual_source(&rows, host.as_deref())
        };
        let latest = fold_latest_values(
            parsed
                .into_iter()
                .filter(|(.., folded)| *folded)
                .map(|(_, kind, _, value, _)| (kind, value)),
        )?;
        self.actual.insert(subject, ActualState { latest, source });
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

/// Each subject's newest claim in canonical order at or before `?2`, for the subjects `?1`.
/// The accepted-time index gives each subject's claims newest time first, so SQLite sorts only
/// the claims of the newest millisecond, not the subject's whole history.
pub(super) fn newest_claims_sql() -> String {
    canonical_sql(
        "SELECT subjects.value,
                (SELECT id FROM claims INDEXED BY claims_subject_accepted_index
                 WHERE subject=subjects.value AND store_index<=?2
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1)
         FROM json_each(?1) subjects",
    )
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
            // Owners are runs and generations. Their actual states are read before any
            // declaration of theirs, so their selected source ignores a declared host; that holds
            // only because no owner is itself a runtime this fold shows.
            debug_assert!(owner_subjects.iter().all(|owner| !runtime_subject(owner)));
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
        // An undeclared card's provenance is its newest claim in canonical order.
        let mut newest = connection.prepare_cached(&newest_claims_sql())?;
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

/// What the roster's per-card loop reads besides each agent's status, read for many agents at
/// once: their harnesses and last activity, and which of them can have a suspension or a rollout
/// to show, so that those readers run only for the agents that can have one.
pub(crate) struct AgentCardReads {
    index: u64,
    harness: HashMap<String, Option<crate::model::CurrentHarnessView>>,
    activity: HashMap<String, Option<u128>>,
    suspensions: HashSet<String>,
    rollouts: HashSet<String>,
    #[cfg(test)]
    health: BTreeMap<String, health_capture::CardCapture>,
}

impl AgentCardReads {
    /// Prepared input extension. Call only while `agent_card_reads` still holds
    /// its snapshot connection, after selecting desired/runtime claims and the
    /// harness at `self.index`. No caller is installed until native admission and
    /// source qualification can consume these candidates honestly.
    #[cfg(test)]
    fn prepare_health_chunk(
        &mut self,
        connection: &Connection,
        namespace: &str,
        now_ms: u64,
        selected: &[(&str, Option<&str>, Option<&str>)],
    ) -> Result<()> {
        let requests = selected.iter().map(|(subject, runtime_claim, desired_claim)| {
            health_capture::Request {
                subject,
                runtime_claim: *runtime_claim,
                desired_claim: *desired_claim,
                harness_claim: self.harness.get(*subject).and_then(Option::as_ref)
                    .map(|harness| harness.claim.as_str()),
            }
        }).collect::<Vec<_>>();
        self.health.extend(health_capture::capture(
            connection, namespace, self.index, now_ms, &requests,
        )?);
        Ok(())
    }

    /// There is no fallback reader acquisition or attempt to fill an absent input.
    /// Candidate rows require native authority/coverage validation before reduction.
    #[cfg(test)]
    fn take_health(&mut self, subject: &str) -> Option<health_capture::CardCapture> {
        self.health.remove(subject)
    }

    /// [`Store::observed_harness_at`] of `subject`.
    pub(crate) fn take_harness(
        &mut self,
        store: &Store,
        subject: &str,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        match self.harness.remove(subject) {
            Some(harness) => Ok(harness),
            None => store.observed_harness_at(subject, self.index),
        }
    }

    /// [`Store::agent_last_activity_at`] of `subject` with the harness it was read with.
    pub(crate) fn last_activity_at(
        &self,
        store: &Store,
        subject: &str,
        incarnation: Option<&str>,
    ) -> Result<Option<u128>> {
        match self.activity.get(subject) {
            Some(time) => Ok(*time),
            None => store.agent_last_activity_at(subject, incarnation, self.index),
        }
    }

    /// Whether `subject` has a suspend or resume request, without which
    /// [`crate::suspension::current`] has nothing to show.
    pub(crate) fn may_have_suspension(&self, subject: &str) -> bool {
        smallclaims::touched::note_read(|| subject.to_owned());
        self.suspensions.contains(subject)
    }

    /// Whether `subject` is a member of an owned set or staged for one, without which
    /// [`crate::rollout::status`] has nothing to show.
    pub(crate) fn may_have_rollout(&self, subject: &str) -> bool {
        smallclaims::touched::note_read(|| subject.to_owned());
        self.rollouts.contains(subject)
    }
}

/// `(subject, incarnation)` pairs as one JSON array for `json_each`.
fn json_pairs<'a>(pairs: impl IntoIterator<Item = (&'a String, &'a String)>) -> Result<String> {
    Ok(serde_json::to_string(&pairs.into_iter().collect::<Vec<_>>())?)
}

fn parse_time(time: Option<String>) -> Option<u128> {
    time.and_then(|time| time.parse::<u128>().ok())
}

impl Store {
    /// The [`AgentCardReads`] of the agents `subjects` at `index`, a chunk of agents per
    /// statement. `actual_claims` holds, for the agents whose status was reduced at `index`,
    /// the actual-state claim it selected: an agent's newest runtime observation whenever it has
    /// one, so the harness read takes it by id instead of sorting the agent's observations.
    pub(crate) fn agent_card_reads(
        &self,
        subjects: &[String],
        index: u64,
        actual_claims: &HashMap<String, Option<String>>,
    ) -> Result<AgentCardReads> {
        let connection = self.readers.get();
        let mut reads = AgentCardReads {
            index,
            harness: HashMap::new(),
            activity: HashMap::new(),
            suspensions: HashSet::new(),
            rollouts: HashSet::new(),
            #[cfg(test)]
            health: BTreeMap::new(),
        };
        // A harness view needs a runtime observation naming an incarnation; then a running one,
        // or an admission refusal of that incarnation, is folded as `current_harness_fold_at`
        // folds it. Every other agent has none.
        let mut newest_runtime = connection.prepare_cached(&format!(
            "SELECT subjects.value, runtime.body
             FROM json_each(?1) subjects
             JOIN claims runtime ON runtime.id=(
                 SELECT claims.id FROM claims INDEXED BY claims_subject_kind_accepted_index
                 JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.subject=subjects.value AND claims.kind='runtime.observed'
                   AND +claims.store_index<=?2
                 ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1)"
        ))?;
        let mut claims_by_id = connection.prepare_cached(
            "SELECT claims.id, claims.kind, claims.body FROM claims
             WHERE claims.id IN (SELECT value FROM json_each(?1))",
        )?;
        let mut admission = connection.prepare_cached(
            "SELECT json_extract(pairs.value, '$[0]') FROM json_each(?1) pairs
             WHERE EXISTS(
                 SELECT 1 FROM claims
                 WHERE subject=json_extract(pairs.value, '$[0]') AND kind='harness.diagnostic'
                   AND store_index<=?2
                   AND json_extract(body, '$.fields.code')='harness-admission-failed'
                   AND json_extract(body, '$.fields.incarnation_id')=json_extract(pairs.value, '$[1]'))",
        )?;
        // The statements of `agent_last_activity_at`, a chunk of agents at a time.
        let mut activity = connection.prepare_cached(
            "SELECT subjects.value,
                    (SELECT accepted_at_unix_ms FROM claims
                     WHERE actor=subjects.value AND kind IN ('work.progress','work.submitted')
                       AND store_index<=?2 ORDER BY store_index DESC LIMIT 1),
                    (SELECT accepted_at_unix_ms FROM claims
                     WHERE kind='message.sent' AND json_extract(body, '$.fields.from')=subjects.value
                       AND store_index<=?2 ORDER BY store_index DESC LIMIT 1),
                    (SELECT accepted_at_unix_ms FROM claims
                     WHERE kind='message.sent' AND json_extract(body, '$.fields.to')=subjects.value
                       AND store_index<=?2 ORDER BY store_index DESC LIMIT 1)
             FROM json_each(?1) subjects",
        )?;
        let mut timeline = connection.prepare_cached(
            "SELECT json_extract(pairs.value, '$[0]'),
                    (SELECT observed_at_unix_ms FROM local_observations
                     WHERE subject=json_extract(pairs.value, '$[0]') AND kind='harness.timeline'
                       AND json_extract(body, '$.fields.incarnation_id')=json_extract(pairs.value, '$[1]')
                       AND json_extract(body, '$.fields.entry_type') IN ('message','content','tool_call','tool_result')
                       AND after_store_index<=?2 ORDER BY id DESC LIMIT 1),
                    (SELECT accepted_at_unix_ms FROM claims
                     WHERE subject=json_extract(pairs.value, '$[0]') AND kind='harness.timeline'
                       AND json_extract(body, '$.fields.incarnation_id')=json_extract(pairs.value, '$[1]')
                       AND json_extract(body, '$.fields.entry_type') IN ('message','content','tool_call','tool_result')
                       AND store_index<=?2 ORDER BY store_index DESC LIMIT 1)
             FROM json_each(?1) pairs",
        )?;
        // `suspension::current` reads every suspend and resume request, at any index.
        let mut suspensions = connection.prepare_cached(
            "SELECT DISTINCT claims.subject
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_subject_kind_index
               ON claims.subject=subjects.value AND claims.kind='runtime.action.requested'
             WHERE claims.actor IS NOT NULL
               AND json_extract(claims.body, CASE WHEN json_type(claims.body, '$.fields') IS NOT NULL
                                                  THEN '$.fields.action' ELSE '$.action' END)
                   IN ('suspend','resume')",
        )?;
        // A member no set owns has no rollout, unless a staged claim makes `guard_member` refuse.
        let mut staged = connection.prepare_cached(
            "SELECT DISTINCT claims.subject
             FROM json_each(?1) subjects
             CROSS JOIN claims INDEXED BY claims_owned_set_subject_index ON claims.subject=subjects.value
             WHERE json_extract(claims.body, '$.owned_set') IS NOT NULL",
        )?;
        let mut members = owned_sets::owners_at(&connection, None)
            .map_err(anyhow::Error::new)?
            .into_keys()
            .collect::<HashSet<_>>();
        for view in owned_sets::selected(&connection, None).map_err(anyhow::Error::new)? {
            members.extend(
                owned_sets::effective_members(&connection, &view, None)
                    .map_err(anyhow::Error::new)?
                    .into_keys(),
            );
        }
        let unique = subjects.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        for chunk in unique.chunks(CHUNK) {
            let list = json_list(chunk)?;
            let mut incarnations = BTreeMap::new();
            let mut fold = Vec::new();
            // Each agent's newest runtime observation: its selected actual claim when that is
            // one, none when the selected claim is of another kind or there is none, and read
            // by the canonical sort only for agents whose selection is not known.
            let (known, unknown): (Vec<_>, Vec<_>) =
                chunk.iter().partition(|subject| actual_claims.contains_key(*subject));
            let selected = known
                .iter()
                .filter_map(|subject| Some((actual_claims[*subject].as_ref()?, *subject)))
                .collect::<HashMap<_, _>>();
            let mut runtimes = Vec::new();
            if !selected.is_empty() {
                for row in claims_by_id.query_map([json_list(selected.keys().copied())?], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })? {
                    let (id, kind, body) = row?;
                    if kind == "runtime.observed" {
                        runtimes.push((selected[&id].clone(), body));
                    }
                }
            }
            if !unknown.is_empty() {
                for row in newest_runtime.query_map(params![json_list(unknown)?, index], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    runtimes.push(row?);
                }
            }
            for (subject, body) in runtimes {
                let body: Value = serde_json::from_str(&body)?;
                let fields = body.get("fields").unwrap_or(&body);
                let Some(incarnation) = fields.get("incarnation_id").and_then(Value::as_str) else {
                    continue;
                };
                if fields.get("status").and_then(Value::as_str) == Some("running") {
                    fold.push(subject.clone());
                }
                incarnations.insert(subject, incarnation.to_owned());
            }
            let refused = incarnations
                .iter()
                .filter(|(subject, _)| !fold.contains(subject))
                .collect::<Vec<_>>();
            if !refused.is_empty() {
                for row in admission.query_map(params![json_pairs(refused)?, index], |row| {
                    row.get::<_, String>(0)
                })? {
                    fold.push(row?);
                }
            }
            for subject in chunk {
                reads.harness.insert(subject.clone(), None);
            }
            for subject in fold {
                let mut harness =
                    current_harness_fold_at(&connection, &subject, Some(index), false, false)?;
                if let Some(view) = harness.as_mut() {
                    seat_status::enrich_harness(&connection, &subject, Some(index), view)?;
                }
                reads.harness.insert(subject, harness);
            }
            for row in activity.query_map(params![list, index], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    [row.get::<_, Option<String>>(1)?, row.get(2)?, row.get(3)?],
                ))
            })? {
                let (subject, times) = row?;
                reads.activity.insert(subject, times.into_iter().filter_map(parse_time).max());
            }
            let observed = chunk
                .iter()
                .filter_map(|subject| {
                    let harness = reads.harness.get(subject)?.as_ref()?;
                    Some((subject, &harness.incarnation_id))
                })
                .collect::<Vec<_>>();
            if !observed.is_empty() {
                for row in timeline.query_map(params![json_pairs(observed)?, index], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })? {
                    let (subject, local, replicated) = row?;
                    let times = local
                        .map(|time| time.max(0) as u128)
                        .into_iter()
                        .chain(parse_time(replicated));
                    let newest = reads.activity.entry(subject).or_default();
                    *newest = (*newest).into_iter().chain(times).max();
                }
            }
            for row in suspensions.query_map(params![list], |row| row.get::<_, String>(0))? {
                reads.suspensions.insert(row?);
            }
            for row in staged.query_map(params![list], |row| row.get::<_, String>(0))? {
                reads.rollouts.insert(row?);
            }
            reads
                .rollouts
                .extend(chunk.iter().filter(|subject| members.contains(*subject)).cloned());
        }
        Ok(reads)
    }
}

#[cfg(test)]
mod current_workspace_tests {
    use super::*;

    fn input(kind: &str, fields: Value) -> ClaimInput {
        ClaimInput {
            subject: "agent/cedar".into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        }
    }

    fn card_actual(store: &Store, index: u64) -> Option<Value> {
        let connection = store.readers.get();
        let mut cards = CardReads::new(index);
        cards
            .load_actual(&connection, &["agent/cedar".into()])
            .unwrap();
        cards.latest(&connection, "agent/cedar").unwrap()
    }

    #[test]
    fn workspace_cards_and_actual_reads_preserve_mixed_version_shadowing_and_host_fences() {
        let store = Store::open_memory("owner").unwrap();
        store
            .append_claim(&input(
                "runtime.observed",
                json!({"status":"running","host":"owner","incarnation_id":"one"}),
            ))
            .unwrap();
        let modern = input(
            "workspace.observed",
            json!({"host":"owner","workspace":"/register"}),
        );
        latest_values::append(&store.graph, &modern, 1, None).unwrap();
        let cut = store.index().unwrap();
        let old = input(
            "workspace.observed",
            json!({"host":"owner","workspace":"/legacy"}),
        );
        append_legacy_graph_observation_fenced(&store.graph, &old, 2, None).unwrap();
        let index = store.index().unwrap();
        let connection = store.readers.get();
        assert_eq!(
            latest_actual_at(&connection, "agent/cedar", Some(index))
                .unwrap()
                .unwrap()["workspace"],
            "/legacy"
        );
        assert_eq!(card_actual(&store, index).unwrap()["workspace"], "/legacy");
        assert_eq!(
            latest_actual_at(&connection, "agent/cedar", Some(cut))
                .unwrap()
                .unwrap()["workspace"],
            "/register"
        );
        assert_eq!(card_actual(&store, cut).unwrap()["workspace"], "/register");
        drop(connection);

        let handoff = Store::open_memory("owner").unwrap();
        handoff
            .append_claim(&input(
                "runtime.observed",
                json!({"status":"running","host":"owner","incarnation_id":"one"}),
            ))
            .unwrap();
        latest_values::append(&handoff.graph, &modern, now_ms(), None).unwrap();
        handoff
            .append_claim(&input(
                "runtime.observed",
                json!({"status":"running","host":"destination","incarnation_id":"two"}),
            ))
            .unwrap();
        let index = handoff.index().unwrap();
        let actual = handoff.latest_actual_value("agent/cedar").unwrap().unwrap();
        let card = card_actual(&handoff, index).unwrap();
        assert_eq!(actual["host"], "destination");
        assert_eq!(card["host"], "destination");
        assert!(actual.get("workspace").is_none());
        assert!(card.get("workspace").is_none());
    }

    #[test]
    fn a_newer_legacy_working_observation_wins_over_an_older_register() {
        let store = Store::open_memory("owner").unwrap();
        let idle = input(
            "harness.observed",
            json!({"state":"idle","driver":"codex","incarnation_id":"one"}),
        );
        latest_values::append(&store.graph, &idle, 1, None).unwrap();
        let working = input(
            "harness.observed",
            json!({"state":"working","driver":"codex","incarnation_id":"one"}),
        );
        append_legacy_graph_observation_fenced(&store.graph, &working, 2, None).unwrap();
        let connection = store.readers.get();
        assert!(
            agent_working_since_at(&connection, "agent/cedar", "one", store.index().unwrap())
                .unwrap()
                .is_some()
        );
    }
}
