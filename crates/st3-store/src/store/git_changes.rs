//! Node-local, rebuildable index of verified Git metadata objects. This is not a
//! claim kind, replicated scheduling authority, or a signed landing observation.
use super::*;
use crate::changes::{Revision, change_id, oid};

const MAX_RECORD_BYTES: usize = 16 * 1024;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_git_change_records (
    repository TEXT NOT NULL,
    object_id TEXT NOT NULL,
    change_id TEXT NOT NULL,
    record_json TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    PRIMARY KEY(repository, object_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_git_change_latest (
    repository TEXT NOT NULL,
    change_id TEXT NOT NULL,
    object_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY(repository, change_id)
) WITHOUT ROWID;
"#;

#[derive(Clone, Debug, Serialize)]
pub struct IndexedChange {
    pub revision: Revision,
    pub indexed_at: String,
}

fn repository_key(key: &str) -> Result<()> {
    anyhow::ensure!(
        !key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control),
        "invalid configured repository identity"
    );
    Ok(())
}

impl Store {
    pub fn install_git_change_registry(&self, registry: Arc<crate::changes::service::Registry>) -> Result<()> {
        self.smalltalk.git_change_registry.set(registry).map_err(|_| anyhow::anyhow!("Git change registry already installed"))
    }

    pub fn git_change_registry(&self) -> Option<Arc<crate::changes::service::Registry>> {
        self.smalltalk.git_change_registry.get().cloned()
    }

    /// Call only after Git object/source/parent validation outside the writer. Immutable
    /// object rows and the latest pointer are published in one short transaction.
    pub fn index_git_change(&self, repository: &str, revision: &Revision) -> Result<()> {
        repository_key(repository)?;
        oid(&revision.oid)?;
        revision.record.validate()?;
        let generation = i64::try_from(revision.record.generation)?;
        let body = serde_json::to_string(&revision.record)?;
        anyhow::ensure!(
            body.len() <= MAX_RECORD_BYTES,
            "Git change record exceeds index byte budget"
        );
        let indexed_at = chrono::Utc::now().to_rfc3339();
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let existing: Option<String> = transaction.query_row(
            "SELECT record_json FROM local_git_change_records WHERE repository=?1 AND object_id=?2",
            params![repository, revision.oid], |row| row.get(0),
        ).optional()?;
        if let Some(existing) = existing {
            anyhow::ensure!(
                existing == body,
                "indexed Git object has conflicting metadata"
            );
        } else {
            transaction.execute(
                "INSERT INTO local_git_change_records(repository,object_id,change_id,record_json,indexed_at)
                 VALUES(?1,?2,?3,?4,?5)",
                params![repository, revision.oid, revision.record.change_id, body, indexed_at],
            )?;
        }
        transaction.execute(
            "INSERT INTO local_git_change_latest(repository,change_id,object_id,generation)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(repository,change_id) DO UPDATE SET object_id=excluded.object_id,generation=excluded.generation
             WHERE excluded.generation>local_git_change_latest.generation",
            params![repository, revision.record.change_id, revision.oid, generation],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// One bounded page of exact advertised object IDs. Unknown records stay unknown;
    /// a latest pointer is never substituted for a different advertised current object.
    pub fn indexed_git_changes(
        &self,
        repository: &str,
        objects: &[String],
    ) -> Result<BTreeMap<String, IndexedChange>> {
        repository_key(repository)?;
        anyhow::ensure!(
            objects.len() <= 50,
            "Git change lookup page exceeds 50 objects"
        );
        for object in objects {
            oid(object)?;
        }
        let connection = self.readers.get();
        let transaction = connection.unchecked_transaction()?;
        let mut statement = transaction.prepare_cached(
            "SELECT record_json,indexed_at FROM local_git_change_records WHERE repository=?1 AND object_id=?2",
        )?;
        let mut records = BTreeMap::new();
        for object in objects {
            let row: Option<(String, String)> = statement
                .query_row(params![repository, object], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?;
            if let Some((body, indexed_at)) = row {
                anyhow::ensure!(
                    body.len() <= MAX_RECORD_BYTES,
                    "indexed Git record exceeds byte budget"
                );
                let record: crate::changes::Record = serde_json::from_str(&body)?;
                record.validate()?;
                records.insert(
                    object.clone(),
                    IndexedChange {
                        revision: Revision {
                            oid: object.clone(),
                            record,
                        },
                        indexed_at,
                    },
                );
            }
        }
        Ok(records)
    }

    /// The seeked ID window used to include previously indexed, now missing changes.
    /// It does not scan immutable record history or graph claims.
    pub fn indexed_git_change_ids(
        &self,
        repository: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        repository_key(repository)?;
        if !after.is_empty() {
            change_id(after)?;
        }
        anyhow::ensure!((1..=51).contains(&limit), "invalid Git change ID page size");
        let connection = self.readers.get();
        Ok(connection.prepare_cached(
            "SELECT change_id FROM local_git_change_latest WHERE repository=?1 AND change_id>?2 ORDER BY change_id LIMIT ?3",
        )?.query_map(params![repository, after, limit], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn latest_indexed_git_change(
        &self,
        repository: &str,
        id: &str,
    ) -> Result<Option<IndexedChange>> {
        repository_key(repository)?;
        change_id(id)?;
        let connection = self.readers.get();
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT records.object_id,records.record_json,records.indexed_at
             FROM local_git_change_latest latest JOIN local_git_change_records records
               ON records.repository=latest.repository AND records.object_id=latest.object_id
             WHERE latest.repository=?1 AND latest.change_id=?2",
                params![repository, id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(object, body, indexed_at)| {
            anyhow::ensure!(
                body.len() <= MAX_RECORD_BYTES,
                "indexed Git record exceeds byte budget"
            );
            let record: crate::changes::Record = serde_json::from_str(&body)?;
            record.validate()?;
            Ok(IndexedChange {
                revision: Revision {
                    oid: object,
                    record,
                },
                indexed_at,
            })
        })
        .transpose()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    use super::*;
    use crate::changes::{Record, State, TrailerStatus};
    #[cfg(test)]
    use smallclaims::sqlite::work::SqliteWorkScope;

    pub fn revision() -> Revision {
        Revision {
            oid: "a".repeat(40),
            record: Record {
                schema_version: 1,
                change_id: format!("I{}", "4".repeat(40)),
                generation: 1,
                previous_record: None,
                source_commit: "1".repeat(40),
                source_tree: "2".repeat(40),
                base_commit: "3".repeat(40),
                trailer_status: TrailerStatus::Present,
                state: State::Draft,
                next: None,
                depends_on: vec![],
                actor: "agent/fixture/builder".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                claim: None,
                mirror: None,
            },
        }
    }

    #[test]
    fn immutable_index_is_local_and_conflicting_object_body_rolls_back() {
        let store = Store::open_memory("fixture").unwrap();
        let before = smallclaims::store::graph_digest(&store.readers.get()).unwrap();
        let revision = revision();
        store.index_git_change("fixture/repo", &revision).unwrap();
        let indexed = store
            .latest_indexed_git_change("fixture/repo", &revision.record.change_id)
            .unwrap()
            .unwrap();
        store.index_git_change("fixture/repo", &revision).unwrap();
        assert_eq!(
            store
                .latest_indexed_git_change("fixture/repo", &revision.record.change_id)
                .unwrap()
                .unwrap()
                .indexed_at,
            indexed.indexed_at
        );
        let mut conflicting = revision.clone();
        conflicting.record.actor = "agent/fixture/other".into();
        assert!(
            store
                .index_git_change("fixture/repo", &conflicting)
                .is_err()
        );
        assert_eq!(
            store
                .latest_indexed_git_change("fixture/repo", &revision.record.change_id)
                .unwrap()
                .unwrap()
                .revision,
            revision
        );
        assert_eq!(
            smallclaims::store::graph_digest(&store.readers.get()).unwrap(),
            before
        );
        assert!(
            store
                .latest_indexed_git_change("other/repo", &revision.record.change_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_lookup_never_substitutes_latest_and_late_old_records_do_not_regress_it() {
        let store = Store::open_memory("fixture").unwrap();
        let first = revision();
        let mut second = first.clone();
        second.oid = "b".repeat(40);
        second.record.generation = 2;
        second.record.previous_record = Some(first.oid.clone());
        store.index_git_change("fixture/repo", &second).unwrap();
        store.index_git_change("fixture/repo", &first).unwrap();
        assert_eq!(
            store
                .latest_indexed_git_change("fixture/repo", &first.record.change_id)
                .unwrap()
                .unwrap()
                .revision,
            second
        );
        let rows = store
            .indexed_git_changes("fixture/repo", &[first.oid.clone(), "c".repeat(40)])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&first.oid].revision, first);
        assert!(
            store
                .indexed_git_changes("fixture/repo", &vec!["c".repeat(40); 51])
                .is_err()
        );
    }

    #[test]
    fn bounded_reads_do_not_scan_unrelated_records_or_history() {
        let store = Store::open_memory("fixture").unwrap();
        let revision = revision();
        store.index_git_change("fixture/repo", &revision).unwrap();
        // Keep the end-of-repository boundary shape equal in both samples. An empty
        // B-tree tail executes one fewer VM step than a seek stopped by a foreign key.
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("INSERT INTO local_git_change_records VALUES('other/repo',printf('%040x',0),'I'||printf('%040x',0),'{}','fixture')", []).unwrap();
            tx.execute("INSERT INTO local_git_change_latest VALUES('other/repo','I'||printf('%040x',0),printf('%040x',0),1)", []).unwrap();
            tx.commit().unwrap();
        }
        let measure = || {
            let work = SqliteWorkScope::start();
            let rows = store
                .indexed_git_changes("fixture/repo", std::slice::from_ref(&revision.oid))
                .unwrap();
            let ids = store
                .indexed_git_change_ids("fixture/repo", "", 51)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(ids, vec![revision.record.change_id.clone()]);
            work.finish()
        };
        let before = measure();
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<100000)
                INSERT INTO local_git_change_records SELECT 'other/repo',printf('%040x',n),'I'||printf('%040x',n),'{}','fixture' FROM rows", []).unwrap();
            tx.execute("WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<100000)
                INSERT INTO local_git_change_latest SELECT 'other/repo','I'||printf('%040x',n),printf('%040x',n),1 FROM rows", []).unwrap();
            tx.commit().unwrap();
        }
        let after = measure();
        assert_eq!(before.vm_steps, after.vm_steps);
        assert_eq!(after.fullscan_steps, 0);
        assert_eq!(after.sorts, 0);
    }
}
