//! Bounded joins of one advertisement and node-local Git record receipts. No
//! object fetch, per-change Git command, or GitHub API request occurs here.
use super::discovery::Observation;
use super::transport::{Advertisement, validate_advertisement};
use super::{PREFIX, change_id, oid};
use crate::store::Store;
use crate::store::git_changes::IndexedChange;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Indexed,
    SyncNeeded,
    MarkerConflict,
    RemoteMissing,
    Offline,
    Unobserved,
    NotFound,
}

#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub change_id: String,
    pub status: Status,
    pub advertised_current: Option<String>,
    /// Populated only for the exact advertised object, even after a failed refresh.
    /// Metadata is cooperative state; this does not establish landing authorization.
    pub indexed_current: Option<IndexedChange>,
    /// Offline/missing history is explicitly distinguished from current remote state.
    pub last_indexed: Option<IndexedChange>,
    /// Evidence is outside the discovery pattern and requires its own verification.
    pub evidence_indexed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub repository: String,
    pub observation: Freshness,
    pub items: Vec<Change>,
    /// Full ID seek key, not an offset through metadata/claim history.
    pub next_after: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Freshness {
    pub refs_observed_at: Option<String>,
    pub ref_count: Option<usize>,
    pub attempted_at: Option<String>,
    pub operation_ms: Option<u64>,
    pub error: Option<String>,
    pub offline: bool,
    pub reused: bool,
}

impl From<&Observation> for Freshness {
    fn from(observation: &Observation) -> Self {
        Self {
            refs_observed_at: observation
                .advertisement()
                .map(|advertisement| advertisement.observed_at.clone()),
            ref_count: observation
                .advertisement()
                .map(|advertisement| advertisement.refs.len()),
            attempted_at: observation.attempted_at.clone(),
            operation_ms: observation.operation_ms,
            error: observation.error.clone(),
            offline: observation.offline,
            reused: observation.reused,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Detail {
    pub repository: String,
    pub observation: Freshness,
    pub item: Change,
}

pub(crate) fn advertised_ids(
    advertisement: &Advertisement,
) -> Result<BTreeMap<String, Option<String>>> {
    let mut ids = BTreeMap::new();
    ensure!(
        advertisement.refs.len() <= 5000,
        "change namespace exceeds read inventory budget; use the PR path"
    );
    for (name, object) in &advertisement.refs {
        let suffix = name
            .strip_prefix(PREFIX)
            .ok_or_else(|| anyhow::anyhow!("unexpected Git namespace"))?;
        let (id, marker) = suffix
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("malformed change ref"))?;
        change_id(id)?;
        oid(object)?;
        let current = ids.entry(id.to_owned()).or_insert(None);
        if marker == "current" {
            *current = Some(object.to_owned());
        }
    }
    Ok(ids)
}

pub fn list(
    store: &Store,
    repository: &str,
    observation: Observation,
    after: &str,
    limit: usize,
) -> Result<Page> {
    ensure!(
        (1..=50).contains(&limit),
        "change page limit must be 1 through 50"
    );
    if !after.is_empty() {
        change_id(after)?;
    }
    let advertised = observation.advertised_ids();
    let mut ids: BTreeSet<_> = store
        .indexed_git_change_ids(repository, after, limit + 1)?
        .into_iter()
        .collect();
    ids.extend(
        advertised
            .range::<str, _>((Excluded(after), Unbounded))
            .map(|(id, _)| id)
            .take(limit + 1)
            .cloned(),
    );
    let has_more = ids.len() > limit;
    let ids: Vec<_> = ids.into_iter().take(limit).collect();
    let objects: Vec<_> = ids
        .iter()
        .filter_map(|id| advertised.get(id).and_then(Clone::clone))
        .collect();
    let indexed = store.indexed_git_changes(repository, &objects)?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        items.push(join(
            store,
            repository,
            &observation,
            advertised,
            &indexed,
            id,
        )?);
    }
    let next_after = if has_more {
        items.last().map(|item| item.change_id.clone())
    } else {
        None
    };
    Ok(Page {
        repository: repository.into(),
        observation: Freshness::from(&observation),
        items,
        next_after,
    })
}

pub fn show(store: &Store, repository: &str, observation: Observation, id: &str) -> Result<Detail> {
    change_id(id)?;
    let advertised = observation.advertised_ids();
    let objects: Vec<_> = advertised
        .get(id)
        .and_then(Clone::clone)
        .into_iter()
        .collect();
    let indexed = store.indexed_git_changes(repository, &objects)?;
    let item = join(
        store,
        repository,
        &observation,
        advertised,
        &indexed,
        id.into(),
    )?;
    Ok(Detail {
        repository: repository.into(),
        observation: Freshness::from(&observation),
        item,
    })
}

fn join(
    store: &Store,
    repository: &str,
    observation: &Observation,
    advertised: &BTreeMap<String, Option<String>>,
    indexed: &BTreeMap<String, IndexedChange>,
    id: String,
) -> Result<Change> {
    let current = advertised.get(&id).and_then(Clone::clone);
    let record = current
        .as_ref()
        .and_then(|object| indexed.get(object))
        .cloned();
    let last_indexed = if record.is_none() {
        store.latest_indexed_git_change(repository, &id)?
    } else {
        None
    };
    let status = if let Some(advertisement) = observation.advertisement() {
        if !advertised.contains_key(&id) {
            if last_indexed.is_some() {
                Status::RemoteMissing
            } else {
                Status::NotFound
            }
        } else if current.is_none() {
            Status::MarkerConflict
        } else if let Some(record) = &record {
            if record.revision.record.change_id != id
                || validate_advertisement(advertisement, &record.revision).is_err()
            {
                Status::MarkerConflict
            } else if observation.offline {
                Status::Offline
            } else {
                Status::Indexed
            }
        } else {
            Status::SyncNeeded
        }
    } else if observation.offline {
        Status::Offline
    } else {
        Status::Unobserved
    };
    Ok(Change {
        change_id: id,
        status,
        advertised_current: current,
        indexed_current: record,
        last_indexed,
        evidence_indexed_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::transport::markers;
    use crate::store::git_changes::tests::revision;

    fn observation(refs: BTreeMap<String, String>) -> Observation {
        let mut observation = Observation::from_advertisement(Advertisement {
            observed_at: "2026-01-01T00:00:00Z".into(),
            refs,
        })
        .unwrap();
        observation.attempted_at = Some("2026-01-01T00:00:00Z".into());
        observation.operation_ms = Some(1);
        observation
    }

    #[test]
    fn unknown_current_is_sync_needed_and_stale_index_is_never_presented_as_current() {
        let store = Store::open_memory("fixture").unwrap();
        let revision = revision();
        let observed = observation(markers(&revision).unwrap());
        let id = &revision.record.change_id;
        let unknown = show(&store, "fixture/repo", observed.clone(), id).unwrap();
        assert!(matches!(unknown.item.status, Status::SyncNeeded));
        assert!(unknown.item.indexed_current.is_none());
        store.index_git_change("fixture/repo", &revision).unwrap();
        let indexed = show(&store, "fixture/repo", observed.clone(), id).unwrap();
        assert!(matches!(indexed.item.status, Status::Indexed));
        assert_eq!(indexed.item.indexed_current.unwrap().revision, revision);
        let mut revised = revision.clone();
        revised.oid = "b".repeat(40);
        revised.record.generation = 2;
        revised.record.previous_record = Some(revision.oid.clone());
        let unknown = show(
            &store,
            "fixture/repo",
            observation(markers(&revised).unwrap()),
            id,
        )
        .unwrap();
        assert!(matches!(unknown.item.status, Status::SyncNeeded));
        assert!(unknown.item.indexed_current.is_none());
        assert_eq!(unknown.item.last_indexed.unwrap().revision, revision);
        assert!(unknown.item.evidence_indexed_at.is_none());
    }

    #[test]
    fn missing_refs_and_conflicting_markers_do_not_become_terminal_state() {
        let store = Store::open_memory("fixture").unwrap();
        let revision = revision();
        store.index_git_change("fixture/repo", &revision).unwrap();
        let missing = list(&store, "fixture/repo", observation(BTreeMap::new()), "", 50).unwrap();
        assert_eq!(missing.items.len(), 1);
        assert!(matches!(missing.items[0].status, Status::RemoteMissing));
        assert!(missing.items[0].indexed_current.is_none());
        assert!(missing.items[0].last_indexed.is_some());
        let mut refs = markers(&revision).unwrap();
        refs.insert(
            format!("{PREFIX}{}/state/ready", revision.record.change_id),
            revision.oid.clone(),
        );
        let conflict = show(
            &store,
            "fixture/repo",
            observation(refs),
            &revision.record.change_id,
        )
        .unwrap();
        assert!(matches!(conflict.item.status, Status::MarkerConflict));
        let missing_current = show(
            &store,
            "fixture/repo",
            observation(BTreeMap::from([(
                format!("{PREFIX}{}/head", revision.record.change_id),
                revision.record.source_commit.clone(),
            )])),
            &revision.record.change_id,
        )
        .unwrap();
        assert!(matches!(
            missing_current.item.status,
            Status::MarkerConflict
        ));
        let unknown = show(
            &store,
            "fixture/repo",
            observation(BTreeMap::new()),
            &format!("I{}", "e".repeat(40)),
        )
        .unwrap();
        assert!(matches!(unknown.item.status, Status::NotFound));
    }

    #[test]
    fn offline_and_failed_observation_keep_original_age_and_separate_evidence_freshness() {
        let store = Store::open_memory("fixture").unwrap();
        let revision = revision();
        store.index_git_change("fixture/repo", &revision).unwrap();
        let mut observed = observation(markers(&revision).unwrap());
        observed.offline = true;
        observed.reused = true;
        observed.error = Some("synthetic transport failure".into());
        observed.attempted_at = Some("2026-01-02T00:00:00Z".into());
        let page = list(&store, "fixture/repo", observed, "", 50).unwrap();
        assert!(matches!(page.items[0].status, Status::Offline));
        assert_eq!(
            page.observation.refs_observed_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            page.observation.attempted_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        assert!(page.observation.error.is_some());
        assert!(page.items[0].evidence_indexed_at.is_none());
        assert!(
            serde_json::to_value(page).unwrap()["observation"]
                .get("advertisement")
                .is_none()
        );
    }

    #[test]
    fn pagination_merges_live_and_previously_indexed_ids_without_history_scans() {
        let store = Store::open_memory("fixture").unwrap();
        let mut refs = BTreeMap::new();
        let mut expected = Vec::new();
        for number in 1..=6 {
            let mut revision = revision();
            revision.oid = format!("{number:040x}");
            revision.record.change_id = format!("I{number:040x}");
            expected.push(revision.record.change_id.clone());
            if number % 2 == 0 {
                store.index_git_change("fixture/repo", &revision).unwrap();
            }
            if number != 2 {
                refs.extend(markers(&revision).unwrap());
            }
        }
        let observed = observation(refs);
        let mut after = String::new();
        let mut actual = Vec::new();
        loop {
            let page = list(&store, "fixture/repo", observed.clone(), &after, 2).unwrap();
            assert!(page.items.len() <= 2);
            actual.extend(page.items.into_iter().map(|item| item.change_id));
            match page.next_after {
                Some(cursor) => after = cursor,
                None => break,
            }
        }
        assert_eq!(actual, expected);
    }
}
