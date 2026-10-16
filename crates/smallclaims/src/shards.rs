//! Physical authority domains. Opening one store never opens or advances another.
//!
//! Source ownership and signer authorization are control-plane decisions. Callers must
//! verify those facts before they name a shard here; file presence grants no authority.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::store::{Runtime, Store};

/// A stable wire name and a single safe path component.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ShardId(String);

impl ShardId {
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.len() > 64
            || value.starts_with('-')
            || value.ends_with('-')
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            bail!("invalid shard ID `{value}`");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn database_path(&self, state_dir: &Path) -> PathBuf {
        state_dir
            .join("shards")
            .join(&self.0)
            .join("claims.sqlite3")
    }
}

/// Each entry is a complete Store: its own file, writer, envelope inventory,
/// admission cursors, checkpoint proof and trim frontier.
/// An empty set is the default and creates no shard files.
pub struct ShardStores {
    stores: BTreeMap<ShardId, Arc<Store>>,
}

impl ShardStores {
    /// `runtime` transfers a new runtime instance for each named database. A runtime
    /// holds projection caches, so it must not be shared with the default store.
    pub fn open(
        state_dir: &Path,
        origin: &str,
        mut runtime: impl FnMut(&ShardId) -> Box<dyn Runtime>,
        ids: impl IntoIterator<Item = ShardId>,
    ) -> Result<Self> {
        let ids = ids.into_iter().collect::<Vec<_>>();
        let mut unique = BTreeSet::new();
        for id in &ids {
            if !unique.insert(id) {
                bail!("duplicate shard ID `{}`", id.as_str());
            }
        }
        let mut stores = BTreeMap::new();
        for id in ids {
            let path = id.database_path(state_dir);
            std::fs::create_dir_all(path.parent().expect("shard database has a parent"))?;
            let store = Arc::new(Store::open(&path, origin, Arc::from(runtime(&id)))?);
            store.bind_shard(&id)?;
            stores.insert(id, store);
        }
        Ok(Self { stores })
    }

    pub fn get(&self, id: &ShardId) -> Option<&Arc<Store>> {
        self.stores.get(id)
    }
    pub fn ids(&self) -> impl Iterator<Item = &ShardId> {
        self.stores.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::ClaimInput;
    use crate::store::runtime::Plain;
    use std::collections::BTreeMap;

    #[test]
    fn names_are_safe_path_components_and_default_opens_nothing() {
        for invalid in ["", "../other", "a/b", ".", "UPPER", "-a", "a-", "a_b"] {
            assert!(ShardId::parse(invalid).is_err(), "{invalid}");
        }
        let root = tempfile::tempdir().unwrap();
        let stores = ShardStores::open(root.path(), "node", |_| Box::new(Plain), []).unwrap();
        assert_eq!(stores.ids().count(), 0);
        assert!(!root.path().join("shards").exists());
        let github = ShardId::parse("github").unwrap();
        assert!(
            ShardStores::open(
                root.path(),
                "node",
                |_| Box::new(Plain),
                [github.clone(), github]
            )
            .is_err()
        );
        assert!(!root.path().join("shards").exists());
    }

    #[test]
    fn named_stores_have_independent_files_and_writer_locks() {
        let root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let work = ShardId::parse("work").unwrap();
        let stores = ShardStores::open(
            root.path(),
            "node",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        assert!(github.database_path(root.path()).exists());
        assert!(work.database_path(root.path()).exists());
        let first = stores.get(&github).unwrap();
        let second = stores.get(&work).unwrap();
        let _first_writer = first.connection.write();
        // A second writer is acquired while the first is still held. A shared lock
        // would deadlock this test, so the distinct locks are the required behavior.
        let _second_writer = second.connection.write();
    }

    #[test]
    fn envelopes_and_reopen_are_scoped_to_the_named_store() {
        let root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let work = ShardId::parse("work").unwrap();
        let first = ShardStores::open(
            root.path(),
            "node",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        let github_store = first.get(&github).unwrap();
        let work_store = first.get(&work).unwrap();
        for store in [github_store, work_store] {
            store
                .bind_fleet("5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12")
                .unwrap();
        }
        github_store
            .append_claim(&ClaimInput {
                subject: "note/github".into(),
                kind: "example.note".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(
            github_store
                .replication_inventory()
                .unwrap()
                .envelopes
                .len(),
            1
        );
        assert!(
            work_store
                .replication_inventory()
                .unwrap()
                .envelopes
                .is_empty()
        );
        drop(first);
        let reopened = ShardStores::open(
            root.path(),
            "node",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        assert_eq!(
            reopened
                .get(&github)
                .unwrap()
                .bound_shard()
                .unwrap()
                .as_deref(),
            Some("github")
        );
        assert_eq!(
            reopened
                .get(&work)
                .unwrap()
                .bound_shard()
                .unwrap()
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            reopened
                .get(&github)
                .unwrap()
                .replication_inventory()
                .unwrap()
                .envelopes
                .len(),
            1
        );
        assert!(
            reopened
                .get(&work)
                .unwrap()
                .replication_inventory()
                .unwrap()
                .envelopes
                .is_empty()
        );
    }

    #[test]
    fn replicating_one_shard_never_advances_another_shard() {
        const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let work = ShardId::parse("work").unwrap();
        let first = ShardStores::open(
            first_root.path(),
            "first",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        let second = ShardStores::open(
            second_root.path(),
            "second",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        for stores in [&first, &second] {
            for id in [&github, &work] {
                stores.get(id).unwrap().bind_fleet(FLEET).unwrap();
            }
        }
        let source = first.get(&github).unwrap();
        let target = second.get(&github).unwrap();
        source
            .append_claim(&ClaimInput {
                subject: "note/github".into(),
                kind: "example.note".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exchange = source
            .export_replication_exchange(FLEET, &target.replication_inventory().unwrap())
            .unwrap();
        target
            .receive_replication_exchange("first", FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        assert!(target.project_replication_backlog().unwrap());
        assert_eq!(target.claims_for("note/github", None).unwrap().len(), 1);
        assert!(
            second
                .get(&work)
                .unwrap()
                .replication_inventory()
                .unwrap()
                .envelopes
                .is_empty()
        );
    }

    #[test]
    fn checkpoint_tombstones_are_scoped_to_one_shard() {
        use crate::store::EnvelopeTombstone;

        const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";
        let root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let work = ShardId::parse("work").unwrap();
        let stores = ShardStores::open(
            root.path(),
            "node",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        let github_store = stores.get(&github).unwrap();
        let work_store = stores.get(&work).unwrap();
        for store in [github_store, work_store] {
            store.bind_fleet(FLEET).unwrap();
        }
        github_store
            .apply_checkpoint_drop(
                "checkpoint/2026-09-27",
                &[EnvelopeTombstone {
                    writer: "source".into(),
                    sequence: 1,
                    envelope_hash: format!("{:064x}", 1),
                    accepted_at_unix_ms: 1,
                }],
                &[],
            )
            .unwrap();
        assert_eq!(github_store.checkpointed_envelopes().unwrap(), 1);
        assert_eq!(work_store.checkpointed_envelopes().unwrap(), 0);
        assert!(
            work_store
                .replication_inventory()
                .unwrap()
                .envelopes
                .is_empty()
        );
    }

    #[test]
    fn an_exchange_for_one_shard_cannot_advance_another_or_the_default() {
        const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";
        let root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let work = ShardId::parse("work").unwrap();
        let stores = ShardStores::open(
            root.path(),
            "source",
            |_| Box::new(Plain),
            [github.clone(), work.clone()],
        )
        .unwrap();
        let source = stores.get(&github).unwrap();
        let wrong_shard = stores.get(&work).unwrap();
        let default = Store::open(
            &root.path().join("default.sqlite3"),
            "default",
            Arc::new(Plain),
        )
        .unwrap();
        for store in [source.as_ref(), wrong_shard.as_ref(), &default] {
            store.bind_fleet(FLEET).unwrap();
        }
        source
            .append_claim(&ClaimInput {
                subject: "note/github".into(),
                kind: "example.note".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exchange = source
            .export_replication_exchange(FLEET, &wrong_shard.replication_inventory().unwrap())
            .unwrap();
        assert_eq!(exchange.shard_id.as_deref(), Some("github"));
        for wrong in [wrong_shard.as_ref(), &default] {
            let before_index = wrong.index().unwrap();
            let before_inventory = wrong.replication_inventory().unwrap().digest;
            let error = wrong
                .receive_replication_exchange("source", FLEET, &exchange)
                .unwrap_err();
            assert_eq!(error.code, "shard-mismatch");
            assert_eq!(wrong.index().unwrap(), before_index);
            assert_eq!(
                wrong.replication_inventory().unwrap().digest,
                before_inventory
            );
            assert!(wrong.replication_inventory().unwrap().envelopes.is_empty());
        }
        // An older/default exchange omits the new field on the wire. It cannot enter a shard.
        let legacy = default
            .export_replication_exchange(FLEET, &source.replication_inventory().unwrap())
            .unwrap();
        let wire = serde_json::to_value(&legacy).unwrap();
        assert!(wire.get("shard_id").is_none());
        let legacy = serde_json::from_value(wire).unwrap();
        let before_index = source.index().unwrap();
        let error = source
            .receive_replication_exchange("default", FLEET, &legacy)
            .unwrap_err();
        assert_eq!(error.code, "shard-mismatch");
        assert_eq!(source.index().unwrap(), before_index);
    }

    #[test]
    fn a_used_default_file_cannot_be_relabelled_as_a_shard() {
        use crate::store::EnvelopeTombstone;

        let root = tempfile::tempdir().unwrap();
        let github = ShardId::parse("github").unwrap();
        let fleet_bound = Store::open(
            &root.path().join("fleet-bound.sqlite3"),
            "node",
            Arc::new(Plain),
        )
        .unwrap();
        fleet_bound.bind_fleet("fleet").unwrap();
        assert!(fleet_bound.bind_shard(&github).is_err());

        let trimmed = Store::open(
            &root.path().join("trimmed.sqlite3"),
            "node",
            Arc::new(Plain),
        )
        .unwrap();
        trimmed
            .apply_checkpoint_drop(
                "checkpoint/2026-09-27",
                &[EnvelopeTombstone {
                    writer: "source".into(),
                    sequence: 1,
                    envelope_hash: format!("{:064x}", 1),
                    accepted_at_unix_ms: 1,
                }],
                &[],
            )
            .unwrap();
        assert_eq!(trimmed.index().unwrap(), 0);
        assert!(trimmed.bind_shard(&github).is_err());

        let named = Store::open(
            &root.path().join("named.sqlite3"),
            "node",
            Arc::new(Plain),
        )
        .unwrap();
        named.bind_shard(&github).unwrap();
        drop(named);
        let moved = Store::open(
            &root.path().join("named.sqlite3"),
            "node",
            Arc::new(Plain),
        )
        .unwrap();
        assert!(moved.bind_shard(&ShardId::parse("work").unwrap()).is_err());
    }
}
