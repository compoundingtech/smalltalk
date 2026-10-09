//! The glasses and arrangements a daemon publishes for collection windows: every person's rows
//! of each, kept off the request path by the attention list's refresher and served under the
//! cut they were read at. A refresh reads again only the people whose glass claims or
//! arrangement rows changed since the newest publication; a projection change or a forgotten
//! view reads everyone again. Everything here is volatile cache state, cleared by
//! `forget_views`.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Which person-owned collection a list holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerView {
    Glasses,
    Arrangements,
}

impl OwnerView {
    pub(crate) fn collection(self) -> &'static str {
        match self {
            Self::Glasses => "glasses",
            Self::Arrangements => "arrangements",
        }
    }

    fn list(self, store: &Store) -> &OwnerList {
        match self {
            Self::Glasses => &store.smalltalk.glass_list,
            Self::Arrangements => &store.smalltalk.arrangement_list,
        }
    }

    /// Every person who has rows of this view, or had them: read when nothing is published.
    fn owners(self, connection: &Connection) -> Result<BTreeSet<String>> {
        match self {
            Self::Glasses => {
                // The current heads name every person with a glass, plus the subjects whose
                // heads are not flushed yet: no read of glass history.
                let mut statement = connection.prepare_cached(
                    "SELECT DISTINCT person FROM glass_heads
                     UNION SELECT claims.subject FROM local_glass_head_pending
                         JOIN claims ON claims.id=local_glass_head_pending.claim_id
                     UNION SELECT subject FROM local_glass_head_dirty",
                )?;
                let names = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(names
                    .iter()
                    .filter_map(|name| {
                        if name.starts_with("glass/") { glass_owner(name) } else { Some(name.clone()) }
                    })
                    .collect())
            }
            Self::Arrangements => {
                let mut statement =
                    connection.prepare_cached("SELECT DISTINCT owner FROM arrangements")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()?)
            }
        }
    }

    /// The people whose rows may differ between `after` and `through`, by the claims admitted
    /// between them (glasses) or the rows they changed (arrangements).
    fn changed_owners(
        self,
        connection: &Connection,
        after: u64,
        through: u64,
    ) -> Result<BTreeSet<String>> {
        match self {
            Self::Glasses => {
                let mut statement = connection.prepare_cached(
                    "SELECT subject FROM claims INDEXED BY claims_kind_index
                     WHERE kind IN ('glass.upserted','glass.deleted')
                       AND store_index>?1 AND store_index<=?2",
                )?;
                let subjects = statement
                    .query_map(params![after, through], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(subjects.iter().filter_map(|subject| glass_owner(subject)).collect())
            }
            Self::Arrangements => {
                let mut statement = connection.prepare_cached(
                    "SELECT DISTINCT owner FROM arrangements INDEXED BY arrangements_changed_index
                     WHERE changed_index>?1 AND changed_index<=?2",
                )?;
                Ok(statement
                    .query_map(
                        params![after.min(i64::MAX as u64), through.min(i64::MAX as u64)],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()?)
            }
        }
    }

    /// One person's rows at `through`, as the collection's full read returns them.
    fn rows(self, connection: &Connection, person: &str, through: u64) -> Result<Vec<Value>> {
        match self {
            Self::Glasses => glasses::glasses_at(connection, person, through),
            Self::Arrangements => arrangements::arrangements_at(connection, person, through),
        }
    }
}

fn glass_owner(subject: &str) -> Option<String> {
    st3_schema::glasses::owner(subject).ok().map(str::to_owned)
}

/// Every person's rows of one view at one cut. A person with no rows has no entry.
pub(crate) struct OwnerListPublication {
    pub(crate) cut: u64,
    pub(crate) projection: Option<(String, u64)>,
    pub(crate) published_at_unix_ms: u128,
    pub(crate) owners: Arc<HashMap<String, Arc<Vec<Value>>>>,
}

#[derive(Default)]
pub(crate) struct OwnerList {
    published: Mutex<Option<Arc<OwnerListPublication>>>,
    /// Counts `forget`s. A refresh that began before one may not publish what it read.
    forgotten: AtomicU64,
    /// People read again, in total, and full reads of everyone.
    people_read: AtomicU64,
    full_reads: AtomicU64,
}

impl OwnerList {
    pub(super) fn forget(&self) {
        let mut published = self.published.lock().unwrap_or_else(PoisonError::into_inner);
        self.forgotten.fetch_add(1, AtomicOrdering::AcqRel);
        *published = None;
    }
}

impl Store {
    /// The newest publication of `view`, served under its own cut. Only while the attention
    /// list's refresher runs, which also keeps these published; a window then never reads them
    /// itself.
    pub(crate) fn published_owner_list(&self, view: OwnerView) -> Option<Arc<OwnerListPublication>> {
        self.attention_list_refresher_running().then_some(())?;
        self.note_published_view_read();
        view.list(self)
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Publish `view` at the current cut, reading again only the people whose rows may have
    /// changed. Returns whether any person's rows changed. One short snapshot.
    pub(crate) fn refresh_owner_list(&self, view: OwnerView, published_at_unix_ms: u128) -> Result<bool> {
        let list = view.list(self);
        let (generation, previous) = {
            let published = list.published.lock().unwrap_or_else(PoisonError::into_inner);
            (list.forgotten.load(AtomicOrdering::Acquire), published.clone())
        };
        self.read_snapshot(|index| {
            let projection = self.attention_projection_frontier()?;
            let connection = self.readers.get();
            let (mut owners, people) = match &previous {
                Some(previous) if previous.cut <= index => {
                    // Deferred replication projection catching up under the same claims rereads
                    // only the people its projected claims name.
                    let projected = match (&previous.projection, &projection) {
                        (before, now) if before == now => Some(BTreeSet::new()),
                        (Some((status, after)), Some((now_status, through)))
                            if status == now_status && after <= through =>
                        {
                            Some(view.changed_owners(&connection, *after, *through)?)
                        }
                        _ => None,
                    };
                    match projected {
                        Some(projected) => {
                            if previous.cut == index && previous.projection == projection {
                                return Ok(false);
                            }
                            let mut people = view.changed_owners(&connection, previous.cut, index)?;
                            people.extend(projected);
                            ((*previous.owners).clone(), people)
                        }
                        None => {
                            list.full_reads.fetch_add(1, AtomicOrdering::Relaxed);
                            (HashMap::new(), view.owners(&connection)?)
                        }
                    }
                }
                _ => {
                    list.full_reads.fetch_add(1, AtomicOrdering::Relaxed);
                    (HashMap::new(), view.owners(&connection)?)
                }
            };
            let mut changed = previous.is_none();
            for person in &people {
                let rows = view.rows(&connection, person, index)?;
                let before = owners.get(person);
                if before.is_some_and(|before| **before == rows) || before.is_none() && rows.is_empty() {
                    continue;
                }
                changed = true;
                if rows.is_empty() {
                    owners.remove(person);
                } else {
                    owners.insert(person.clone(), Arc::new(rows));
                }
            }
            list.people_read.fetch_add(people.len() as u64, AtomicOrdering::Relaxed);
            drop(connection);
            let owners = match &previous {
                Some(previous) if !changed => Arc::clone(&previous.owners),
                _ => Arc::new(owners),
            };
            let mut published = list.published.lock().unwrap_or_else(PoisonError::into_inner);
            if list.forgotten.load(AtomicOrdering::Acquire) != generation
                || published.as_ref().is_some_and(|newest| newest.cut > index)
            {
                return Ok(false);
            }
            *published = Some(Arc::new(OwnerListPublication {
                cut: index,
                projection,
                published_at_unix_ms,
                owners,
            }));
            Ok(changed)
        })
    }

    /// People read again by refreshes of `view`, and full reads of everyone.
    #[cfg(test)]
    pub(crate) fn owner_list_reads(&self, view: OwnerView) -> (u64, u64) {
        let list = view.list(self);
        (
            list.people_read.load(AtomicOrdering::Relaxed),
            list.full_reads.load(AtomicOrdering::Relaxed),
        )
    }
}

#[cfg(test)]
#[path = "owner_lists/tests.rs"]
mod tests;
