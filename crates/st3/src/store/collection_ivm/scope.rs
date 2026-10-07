//! Paired transaction scope for explicit old/new capture. This never publishes a cut.
use super::{Captured, ack, clean, gap, page, status};
use anyhow::{Result, ensure};
use rusqlite::Transaction;
use smallclaims::ivm::{Views, install::Installer};

/// Install inside the first managed setup transaction after paired hooks are attached.
/// Source registration/extraction and canonical/local dispatch remain caller attestations.
/// Clearing staging metadata cannot restore a fenced registry or installation root.
pub fn activate(tx: &Transaction<'_>, views: &Views) -> Result<()> {
    views.install_gap_trigger(tx, "st3_ivm_capture_state", "gap")?;
    ensure!(clean(tx)?, "unclean capture cannot activate managed scope");
    let managed: bool = tx.query_row(
        "SELECT managed FROM st3_ivm_capture_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(managed, "scope activation requires transaction prepare");
    tx.execute(
        "UPDATE st3_ivm_capture_state SET guarded=1 WHERE singleton=1",
        [],
    )?;
    Ok(())
}

/// Run inside the newly begun outer transaction, before any job/helper source mutation.
/// Earlier committed capture is fenced, never adopted as this transaction's input.
pub fn prepare(tx: &Transaction<'_>) -> Result<()> {
    let state = status(tx)?;
    let retained: bool = tx.query_row(
        "SELECT managed FROM st3_ivm_capture_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if state.pending_rows != 0 || retained {
        gap(tx, "source capture predates managed transaction")?;
    }
    tx.execute(
        "UPDATE st3_ivm_capture_state SET managed=1 WHERE singleton=1",
        [],
    )?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Coverage {
    Complete,
    Unsupported(String),
}

fn fence(
    tx: &Transaction<'_>,
    views: &Views,
    installer: &Installer,
    source: &str,
    reason: &str,
) -> Result<()> {
    gap(tx, reason)?;
    // Protect both representations, without installing/replacing a root or promoting Ready.
    installer.source_gap(tx, source, reason)?;
    views.fence_all(tx, reason)?;
    Ok(())
}

/// Record only this transaction's bounded exact replacements, then dispatch affected keys.
/// The callback must use complete admitted/canonical/local old+new dependencies and cannot
/// publish a source cut per page. Unsupported coverage fences while allowing source commit;
/// storage/callback errors abort normally. A complete result is capture completion only.
/// Source owners may publish a proved cut only after all their other inputs are complete.
pub fn finalize(
    tx: &Transaction<'_>,
    views: &Views,
    installer: &Installer,
    source: &str,
    mut dispatch: impl FnMut(&Transaction<'_>, &[Captured]) -> Result<Coverage>,
) -> Result<Coverage> {
    let state = status(tx)?;
    let (guarded, managed): (bool, bool) = tx.query_row(
        "SELECT guarded,managed FROM st3_ivm_capture_state WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let reason = state
        .gap
        .or_else(|| (!managed).then(|| "missing managed source scope".into()))
        .or_else(|| {
            (!guarded && state.pending_rows != 0)
                .then(|| "source capture guard not activated".into())
        });
    let outcome = if let Some(reason) = reason {
        fence(tx, views, installer, source, &reason)?;
        Coverage::Unsupported(reason)
    } else {
        let mut outcome = Coverage::Complete;
        // At most256 captured statements /1MiB; two indexed pages, never a backlog drain.
        for _ in 0..2 {
            let captured = page(tx, 128)?;
            if captured.is_empty() {
                break;
            }
            for change in &captured {
                for replacement in &change.replacements {
                    installer.record(tx, source, replacement)?;
                }
            }
            match dispatch(tx, &captured)? {
                Coverage::Complete => ack(tx, &captured)?,
                Coverage::Unsupported(reason) => {
                    fence(tx, views, installer, source, &reason)?;
                    outcome = Coverage::Unsupported(reason);
                    break;
                }
            }
        }
        if outcome == Coverage::Complete && !clean(tx)? {
            let reason = "source capture completion budget exceeded";
            fence(tx, views, installer, source, reason)?;
            outcome = Coverage::Unsupported(reason.into());
        }
        outcome
    };
    tx.execute(
        "UPDATE st3_ivm_capture_state SET managed=0 WHERE singleton=1",
        [],
    )?;
    Ok(outcome)
}

/// Same-snapshot capture gate, independent of registry flags. A raw COMMIT can retain
/// a managed marker; readers must reject it before the next prepare has a chance to fence.
pub fn readable(connection: &rusqlite::Connection) -> Result<bool> {
    Ok(clean(connection)?
        && connection.query_row(
            "SELECT guarded=1 AND managed=0 FROM st3_ivm_capture_state WHERE singleton=1",
            [],
            |r| r.get::<_, bool>(0),
        )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        Store,
        collection_ivm::{self, Table},
    };
    use smallclaims::ivm::{Definition, Readiness, View, events};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Empty;
    impl View for Empty {
        fn definition(&self) -> Definition {
            Definition {
                name: "fixture.scope",
                fingerprint: "fixture.scope.v1",
                kinds: &[],
                local_kinds: &[],
                max_contributions: 1,
            }
        }
    }
    const TABLES: &[Table] = &[Table {
        name: "fixture_source",
        columns: &["id", "owner"],
        key: &["id"],
    }];
    struct Fixture {
        _directory: tempfile::TempDir,
        store: Store,
        views: Arc<Views>,
        installer: Arc<Installer>,
        calls: Arc<AtomicUsize>,
    }
    fn fixture(unsupported: bool) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let views = Arc::new(Views::new(vec![Box::new(Empty)]).unwrap());
        let store = Store::open_with_ivm_views(
            &directory.path().join("scope.sqlite"),
            "alder",
            views.clone(),
        )
        .unwrap();
        let installer = Arc::new(Installer::new(vec![]).unwrap());
        store.connection.batched(|tx| {
            tx.execute_batch("CREATE TABLE fixture_source(id TEXT PRIMARY KEY,owner TEXT NOT NULL); CREATE TABLE fixture_audit(sequence INTEGER PRIMARY KEY,old_owner TEXT,new_owner TEXT)")?;
            installer.create_schema(tx)?;
            collection_ivm::install(tx, TABLES, "fixture.capture.v1", 1)?;
            Ok::<_, anyhow::Error>(())
        }).unwrap().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let finishing = (views.clone(), installer.clone(), calls.clone());
        store
            .install_transaction_hooks(prepare, move |tx| {
                finalize(
                    tx,
                    &finishing.0,
                    &finishing.1,
                    "fixture.source",
                    |tx, captured| {
                        finishing.2.fetch_add(1, Ordering::Relaxed);
                        if unsupported {
                            return Ok(Coverage::Unsupported(
                                "uncaptured fixture dependency".into(),
                            ));
                        }
                        for change in captured {
                            for replacement in &change.replacements {
                                tx.execute(
                                    "INSERT INTO fixture_audit(old_owner,new_owner) VALUES(?1,?2)",
                                    rusqlite::params![
                                        replacement.old.as_ref().and_then(|v| v["owner"].as_str()),
                                        replacement.new.as_ref().and_then(|v| v["owner"].as_str())
                                    ],
                                )?;
                            }
                        }
                        Ok(Coverage::Complete)
                    },
                )?;
                Ok(())
            })
            .unwrap();
        store
            .connection
            .batched(|tx| {
                activate(tx, &views)?;
                installer.register_source(tx, "fixture.source", "fixture.source.v1", 1)?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        Fixture {
            _directory: directory,
            store,
            views,
            installer,
            calls,
        }
    }
    fn boundary(f: &Fixture) -> events::Boundary {
        f.store
            .read_snapshot(|_| events::capture(&f.store.readers.get(), &f.views, "fixture.scope"))
            .unwrap()
    }
    #[test]
    fn paired_scope_records_current_replacements_and_rolls_back_with_source() {
        let f = fixture(false);
        f.store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO fixture_source VALUES('agent/a','person/avery')",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        let before = boundary(&f);
        assert_eq!(
            f.installer
                .position(&f.store.readers.get(), "fixture.source")
                .unwrap()
                .revision,
            1
        );
        assert!(readable(&f.store.readers.get()).unwrap());
        {
            let mut writer = f.store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute("UPDATE fixture_source SET owner='person/intruder'", [])
                .unwrap();
            tx.rollback().unwrap();
        }
        let connection = f.store.readers.get();
        assert!(readable(&connection).unwrap());
        assert_eq!(
            f.installer
                .position(&connection, "fixture.source")
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT owner FROM fixture_source", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "person/avery"
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM fixture_audit", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            1
        );
        drop(connection);
        assert_eq!(boundary(&f), before);
    }
    #[test]
    fn raw_autocommit_fences_same_transaction_and_later_writes_do_not_adopt_it() {
        let f = fixture(false);
        let publisher = f.store.ivm_publisher().unwrap().unwrap();
        let mut notices = publisher.subscribe();
        let before = boundary(&f);
        {
            let writer = f.store.connection.write();
            writer
                .execute(
                    "INSERT INTO fixture_source VALUES('agent/raw','person/avery')",
                    [],
                )
                .unwrap();
        }
        assert!(notices.try_recv().is_ok());
        let after = boundary(&f);
        assert!(matches!(after.availability.readiness, Readiness::Fenced));
        assert_eq!(after.source_cut, before.source_cut);
        assert_eq!(after.keys, before.keys);
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
        assert!(!readable(&f.store.readers.get()).unwrap());
        f.store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO fixture_source VALUES('agent/later','person/avery')",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        let connection = f.store.readers.get();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM fixture_source", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            2
        );
        assert!(f.installer.position(&connection, "fixture.source").is_err());
        assert_eq!(
            connection
                .query_row("SELECT revision FROM ivm_install_sources", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn escaped_raw_commit_is_rejected_independently_of_registry_readiness() {
        let f = fixture(false);
        {
            let mut writer = f.store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute(
                "INSERT INTO fixture_source VALUES('agent/raw','person/avery')",
                [],
            )
            .unwrap();
            tx.execute_batch("COMMIT").unwrap();
        }
        assert!(!readable(&f.store.readers.get()).unwrap());
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
        f.store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO fixture_source VALUES('agent/later','person/avery')",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        assert!(matches!(
            boundary(&f).availability.readiness,
            Readiness::Fenced
        ));
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            f.store
                .readers
                .get()
                .query_row("SELECT count(*) FROM fixture_source", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn unsupported_dependency_preserves_source_and_fences_both_representations() {
        let f = fixture(true);
        let before = boundary(&f);
        f.store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO fixture_source VALUES('agent/a','person/avery')",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        let after = boundary(&f);
        assert!(matches!(after.availability.readiness, Readiness::Fenced));
        assert_eq!(after.source_cut, before.source_cut);
        assert_eq!(after.keys, before.keys);
        assert!(
            f.installer
                .position(&f.store.readers.get(), "fixture.source")
                .is_err()
        );
        assert_eq!(
            f.store
                .readers
                .get()
                .query_row("SELECT count(*) FROM fixture_source", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            1
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn quota_fences_without_dispatch_or_rejecting_valid_source_rows() {
        let f = fixture(false);
        f.store
            .connection
            .batched(|tx| {
                for n in 0..257 {
                    tx.execute(
                        "INSERT INTO fixture_source VALUES(?1,'person/avery')",
                        [format!("agent/{n}")],
                    )?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert!(matches!(
            boundary(&f).availability.readiness,
            Readiness::Fenced
        ));
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
        let connection = f.store.readers.get();
        assert!(!readable(&connection).unwrap());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM fixture_source", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            257
        );
        assert_eq!(status(&connection).unwrap().pending_rows, 256);
    }
}
