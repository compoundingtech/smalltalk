//! Private, conservative native fence. No SQL hooks, schema, metric labels or public errors.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeCut {
    connection: usize,
    file: String,
    changes: u64,
    schema: i64,
    temp_schema: i64,
    data: i64,
    user: i64,
}

impl NativeCut {
    fn read(connection: &Connection) -> Option<Self> {
        if !temporary_schema_safe(connection) {
            return None;
        }
        let changes = connection.total_changes();
        // rusqlite's bundled feature selects sqlite3_total_changes64. Never interpret a
        // negative/wrapped native signed counter as a reusable positive epoch.
        if changes > i64::MAX as u64 {
            return None;
        }
        let pragma = |sql| {
            connection
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .ok()
        };
        Some(Self {
            // SAFETY: read the identity only while borrowing the live Connection exclusively
            // through its ordinary writer owner. Never dereference or export this address.
            connection: unsafe { connection.handle() } as usize,
            file: connection.path()?.to_owned(),
            changes,
            schema: pragma("PRAGMA main.schema_version")?,
            temp_schema: pragma("PRAGMA temp.schema_version")?,
            data: pragma("PRAGMA main.data_version")?,
            user: pragma("PRAGMA main.user_version")?,
        })
    }
}

fn temporary_schema_safe(connection: &Connection) -> bool {
    // Compare the actual catalog definitions, not trusted names. SQLite stores the CREATE
    // text beginning at the object name (TEMP / IF NOT EXISTS are not retained); normalize
    // whitespace only. A changed definition, trigger, view or index is ineligible.
    // https://www.sqlite.org/schematab.html and bundled sqlite3.c sqlite3EndTable.
    let check = || -> rusqlite::Result<bool> {
        let mut statement = connection.prepare("SELECT name, type, sql FROM temp.sqlite_schema")?;
        let mut rows = statement.query([])?;
        let mut replay = false;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let sql: Option<String> = row.get(2)?;
            let expected = match name.as_str() {
                "write_clock" => {
                    "CREATE TABLE write_clock(offset_ms INTEGER NOT NULL, at_ms INTEGER)"
                }
                "heal_replay_base_order" => {
                    replay = true;
                    "CREATE TABLE heal_replay_base_order( ordinal INTEGER PRIMARY KEY, claim_id TEXT NOT NULL)"
                }
                _ => return Ok(false),
            };
            if kind != "table"
                || sql
                    .as_deref()
                    .map(|sql| sql.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
                    .as_deref()
                    != Some(expected)
            {
                return Ok(false);
            }
        }
        // Replay empties its private ID-order table before returning. Never read resource
        // inputs while an unexpected retained scratch state is present.
        if replay {
            return connection.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM temp.heal_replay_base_order LIMIT 1)",
                [],
                |row| row.get(0),
            );
        }
        Ok(true)
    };
    check().unwrap_or(false)
}

pub struct Fence {
    // Ownership identity only: the numeric claim frontier is NOT a currentness proof.
    incarnation: Arc<std::sync::atomic::AtomicU64>,
    owner: Arc<()>,
    path: std::path::PathBuf,
    shared_memory: bool,
}

pub struct Prepared<T> {
    owner: Arc<()>,
    cut: Option<NativeCut>,
    pub value: Option<T>,
}

impl Fence {
    pub fn new(store: &Store) -> Self {
        Self {
            incarnation: store.committed_index.clone(),
            owner: Arc::new(()),
            path: store.path.clone(),
            shared_memory: store.shared_memory,
        }
    }

    pub fn prepare<T>(
        &self,
        store: &Store,
        read: impl FnOnce(&Connection) -> T,
    ) -> Prepared<T> {
        let mut prepared = Prepared {
            owner: self.owner.clone(),
            cut: None,
            value: None,
        };
        // Shared-cache snapshots hold table read locks until their transaction ends. The
        // frontier read locks sqlite_sequence and can reject an unrelated AUTOINCREMENT
        // publication with SQLITE_LOCKED. Keep shared-memory callers on the unchanged
        // canonical writer path; reject BEFORE acquiring even a reader or seed loan.
        // Do not join an older ambient snapshot or end an unregistered request transaction.
        if store.shared_memory
            || !Arc::ptr_eq(&self.incarnation, &store.committed_index)
            || store.path != self.path
            || store.readers.path != self.path
            || store.shared_memory != self.shared_memory
            || store.readers.has_request_reader()
            || !store.readers.get().is_autocommit()
        {
            return prepared;
        }
        {
            // Seed from the actual writer at autocommit. This short loan reads only native
            // counters/cookies, never history. Drop it BEFORE starting the fresh read cut.
            #[cfg(any(test, feature = "test-support"))]
            let queued = std::time::Instant::now();
            let connection = store.connection.write();
            #[cfg(any(test, feature = "test-support"))]
            let waited = queued.elapsed();
            #[cfg(any(test, feature = "test-support"))]
            let started = std::time::Instant::now();
            #[cfg(any(test, feature = "test-support"))]
            let work = smallclaims::sqlite::work::SqliteWorkScope::start();
            if connection.is_autocommit() {
                prepared.cut = NativeCut::read(&connection);
            }
            drop(connection);
            #[cfg(any(test, feature = "test-support"))]
            COUNTS.with(|counts| {
                counts
                    .borrow_mut()
                    .seed
                    .push((waited, started.elapsed(), work.finish()))
            });
        }
        if prepared.cut.as_ref().is_none_or(|cut| {
            // File identity also uses the retained Store incarnation and constructor pairing;
            // matching schema cookies alone never identifies a file.
            if self.shared_memory {
                !cut.file.is_empty()
            } else {
                cut.file.is_empty() || std::path::Path::new(&cut.file) != self.path
            }
        }) {
            return prepared;
        }
        #[cfg(any(test, feature = "test-support"))]
        let read_started = std::time::Instant::now();
        #[cfg(any(test, feature = "test-support"))]
        let read_work = smallclaims::sqlite::work::SqliteWorkScope::start();
        let value = store.read_snapshot(|_| {
            let connection = store.readers.get();
            if connection.path() != prepared.cut.as_ref().map(|cut| cut.file.as_str()) {
                return Ok(None);
            }
            let dirty: bool =
                connection.query_row("PRAGMA read_uncommitted", [], |row| row.get(0))?;
            if dirty {
                return Ok(None);
            }
            if !temporary_schema_safe(&connection) {
                return Ok(None);
            }
            let schema: i64 =
                connection.query_row("PRAGMA main.schema_version", [], |row| row.get(0))?;
            if prepared.cut.as_ref().is_none_or(|cut| cut.schema != schema) {
                return Ok(None);
            }
            Ok(Some(read(&connection)))
        });
        #[cfg(any(test, feature = "test-support"))]
        COUNTS.with(|counts| {
            counts
                .borrow_mut()
                .preparation
                .push((read_started.elapsed(), read_work.finish()))
        });
        // A failed/unusable cut is a stale attempt, not a new public error. Reader guards and
        // its pin have ended before queueing validation. The two-attempt caller owns fallback.
        prepared.value = value.ok().flatten();
        prepared
    }

    pub fn current<T>(
        &self,
        store: &Store,
        connection: &Connection,
        prepared: &Prepared<T>,
    ) -> bool {
        if !Arc::ptr_eq(&self.incarnation, &store.committed_index)
            || store.path != self.path
            || !Arc::ptr_eq(&self.owner, &prepared.owner)
            || prepared.value.is_none()
        {
            return false;
        }
        let Some(expected) = &prepared.cut else {
            return false;
        };
        // Native values from THIS transaction close the same-batch gap. No committed hint
        // participates: earlier job/trigger/repair/delete DML changes the counter immediately.
        NativeCut::read(connection).as_ref() == Some(expected)
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Default)]
pub struct Counts {
    pub fast: usize,
    pub cached: usize,
    pub invalidated: usize,
    pub fallback: usize,
    pub seed: Vec<(
        std::time::Duration,
        std::time::Duration,
        smallclaims::sqlite::work::SqliteWork,
    )>,
    pub preparation: Vec<(std::time::Duration, smallclaims::sqlite::work::SqliteWork)>,
    pub until_callback: Vec<Option<std::time::Duration>>,
    pub acknowledged: Vec<std::time::Duration>,
    pub writer: Vec<(
        bool,
        std::time::Duration,
        smallclaims::sqlite::work::SqliteWork,
    )>,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static COUNTS: std::cell::RefCell<Counts> = std::cell::RefCell::new(Counts::default());
}

#[cfg(any(test, feature = "test-support"))]
pub fn take_counts() -> Counts {
    COUNTS.with(|counts| std::mem::take(&mut *counts.borrow_mut()))
}

pub struct Inputs {
    pub active_subscriptions: Vec<(String, SubscriptionSpec)>,
    pub normalized_observation: BTreeMap<String, Value>,
    pub previous: Option<Value>,
    pub baseline: bool,
    pub repository_facts: Value,
    pub legacy_collections: bool,
    pub changed_fields: Vec<String>,
    pub repository_changed_fields: Vec<String>,
    pub recorded_items: HashMap<String, Option<(Value, u128)>>,
    pub discovered: Vec<(&'static str, DiscoveredItem)>,
    pub observer_health_is_current: bool,
    pub subscription_states: BTreeMap<String, (bool, &'static str, bool)>,
    pub changed: bool,
    pub should_record: bool,
}

pub fn inputs(
    transaction: &Connection,
    observer: &str,
    desired_revision: &str,
    attempt: Option<&str>,
    resource: &str,
    facts: &Value,
    subscriptions: &[(String, SubscriptionSpec)],
) -> Result<Inputs, St3Error> {
    let mut active_subscriptions = Vec::new();
    for (subject, expected) in subscriptions {
        let Some(desired) = current_desired_row_tx(transaction, subject).map_err(internal)? else {
            continue;
        };
        if desired.kind != "subscription" {
            continue;
        }
        let value = serde_json::from_str(&desired.body).map_err(internal)?;
        if crate::graph::subscription_spec(&value).as_ref() == Some(expected) && !expected.stopped {
            active_subscriptions.push((subject.clone(), expected.clone()));
        }
    }
    let normalized_observation = normalize_resource_observation(
        transaction,
        &ClaimInput {
            subject: resource.into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([("facts".into(), facts.clone())]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        },
    )?
    .expect("a resource observation is normalized");
    let facts = normalized_observation
        .get("facts")
        .expect("a normalized resource observation has facts");
    let previous = latest_actual(transaction, resource)
        .map_err(internal)?
        .and_then(|actual| actual.get("facts").cloned());
    let baseline = previous.is_none();
    let previous_object = previous.as_ref().and_then(Value::as_object);
    let current_object = facts.as_object().ok_or_else(|| {
        St3Error::new(
            "invalid-resource-observation",
            "resource observation facts must be an object",
        )
    })?;
    // A repository's collections name the items an observation saw. Each item is its
    // own resource, so the repository resource keeps only its own facts and a change
    // to one item records only that item.
    let collection = |field: &str| {
        REPOSITORY_COLLECTIONS
            .iter()
            .any(|(name, _, _)| *name == field)
    };
    let repository_facts = Value::Object(
        current_object
            .iter()
            .filter(|(field, _)| !collection(field))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect(),
    );
    let legacy_collections =
        previous_object.is_some_and(|previous| previous.keys().any(|field| collection(field)));
    let mut changed_fields = BTreeSet::new();
    for field in current_object
        .keys()
        .chain(previous_object.into_iter().flat_map(serde_json::Map::keys))
        .filter(|field| !collection(field))
    {
        if previous_object.and_then(|object| object.get(field)) != current_object.get(field) {
            changed_fields.insert(field.clone());
        }
    }
    let repository_changed_fields = changed_fields.iter().cloned().collect::<Vec<_>>();
    let mut recorded_items = HashMap::new();
    for (field, segment, _) in REPOSITORY_COLLECTIONS {
        for number in current_object
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("number").and_then(Value::as_u64))
        {
            let subject = format!("{resource}/{segment}/{number}");
            let recorded = latest_actual(transaction, &subject)
                .map_err(internal)?
                .and_then(|actual| actual.get("facts").cloned());
            let observed_at = transaction
                .query_row(
                    &canonical_sql(
                        "SELECT accepted_at_unix_ms FROM claims
                                     WHERE subject=?1 AND kind='resource.observed'
                                     ORDER BY COALESCE(json_extract(body, '$.fields.attribution_only'), 0), CANONICAL_DESC(claims) LIMIT 1",
                    ),
                    [&subject],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?
                .and_then(|at| at.parse::<u128>().ok())
                .unwrap_or_default();
            recorded_items.insert(subject, recorded.map(|facts| (facts, observed_at)));
        }
    }
    let mut discovered = Vec::new();
    for (field, _, _) in REPOSITORY_COLLECTIONS {
        let items = discovered_collection_items(
            resource,
            field,
            previous.as_ref(),
            &facts,
            &mut |subject| {
                recorded_items
                    .get(subject)
                    .cloned()
                    .flatten()
                    .map(|(facts, _)| facts)
            },
        );
        changed_fields.extend(
            items
                .iter()
                .flat_map(|item| item.data_types.iter().cloned()),
        );
        discovered.extend(items.into_iter().map(|item| (field, item)));
    }
    let changed_fields = changed_fields.into_iter().collect::<Vec<_>>();
    let observer_health_is_current = latest_actual(transaction, observer)
        .map_err(internal)?
        .is_some_and(|actual| {
            actual.get("state").and_then(Value::as_str) == Some("healthy")
                && actual.get("revision").and_then(Value::as_str) == Some(desired_revision)
                && actual.get("reason").is_none()
        });
    let mut subscription_states = BTreeMap::new();
    for (subscription_subject, subscription) in &active_subscriptions {
        let target_exists = if subscription.delivery == "message" {
            transaction
                .query_row(
                    "SELECT 1 FROM desired WHERE subject=?1 AND kind='agent'",
                    [&subscription.to],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some()
        } else {
            true
        };
        let status = if target_exists { "active" } else { "pending" };
        let current_status = latest_actual(transaction, subscription_subject)
            .map_err(internal)?
            .and_then(|actual| {
                actual
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        subscription_states.insert(
            subscription_subject.clone(),
            (
                target_exists,
                status,
                current_status.as_deref() != Some(status),
            ),
        );
    }
    let changed = baseline || !changed_fields.is_empty() || !discovered.is_empty();
    let should_record = changed
        || legacy_collections
        || attempt.is_some()
        || !observer_health_is_current
        || subscription_states.values().any(|(_, _, changed)| *changed);
    Ok(Inputs {
        active_subscriptions,
        normalized_observation,
        previous,
        baseline,
        repository_facts,
        legacy_collections,
        changed_fields,
        repository_changed_fields,
        recorded_items,
        discovered,
        observer_health_is_current,
        subscription_states,
        changed,
        should_record,
    })
}

#[cfg(any(test, feature = "test-support"))]
pub type ValidateHook = Box<dyn FnMut(&Store, usize)>;
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static BEFORE_VALIDATE: std::cell::RefCell<Option<ValidateHook>> = const { std::cell::RefCell::new(None) };
}
#[cfg(any(test, feature = "test-support"))]
pub fn before_validate(store: &Store, pass: usize) {
    BEFORE_VALIDATE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(store, pass);
        }
    });
}

#[cfg(any(test, feature = "test-support"))]
pub struct WriterMeasure<'a> {
    sink: &'a Mutex<Option<(std::time::Duration, smallclaims::sqlite::work::SqliteWork)>>,
    start: std::time::Instant,
    work: Option<smallclaims::sqlite::work::SqliteWorkScope>,
}
#[cfg(any(test, feature = "test-support"))]
impl<'a> WriterMeasure<'a> {
    pub fn new(
        sink: &'a Mutex<Option<(std::time::Duration, smallclaims::sqlite::work::SqliteWork)>>,
    ) -> Self {
        Self {
            sink,
            start: std::time::Instant::now(),
            work: Some(smallclaims::sqlite::work::SqliteWorkScope::start()),
        }
    }
}
#[cfg(any(test, feature = "test-support"))]
impl Drop for WriterMeasure<'_> {
    fn drop(&mut self) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((self.start.elapsed(), self.work.take().unwrap().finish()));
    }
}

#[cfg(test)]
mod tests;
