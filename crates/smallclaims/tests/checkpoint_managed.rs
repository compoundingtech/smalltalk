//! Normal checkpoint state mutations share the installed source adapter's outer transaction.
use anyhow::{Result, bail};
use rusqlite::params;
use smallclaims::{
    Store,
    ivm::{Definition, Readiness, SourceCut, View, Views},
    store::{checkpoint::SealedIdentities, runtime::Plain},
};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

struct CheckpointView;
impl View for CheckpointView {
    fn definition(&self) -> Definition {
        Definition {
            name: "checkpoint-view",
            fingerprint: "checkpoint-managed-fixture.v1",
            kinds: &[],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
}

fn fixture() -> (Store, Arc<Views>, Arc<AtomicU8>) {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views = Arc::new(Views::new(vec![Box::new(CheckpointView)]).unwrap());
    {
        let mut writer = store.connection.write();
        views.create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE checkpoint_capture_scope(
                 id INTEGER PRIMARY KEY, managed INTEGER NOT NULL,
                 captured INTEGER NOT NULL, finalized INTEGER NOT NULL, gap TEXT);
             INSERT INTO checkpoint_capture_scope VALUES(1,0,0,0,NULL);
             CREATE TRIGGER checkpoint_capture_insert AFTER INSERT ON checkpoints BEGIN
                 UPDATE checkpoint_capture_scope SET captured=captured+1,
                     gap=CASE WHEN managed=1 THEN gap ELSE 'unmanaged checkpoint insert' END;
             END;
             CREATE TRIGGER checkpoint_capture_update AFTER UPDATE ON checkpoints BEGIN
                 UPDATE checkpoint_capture_scope SET captured=captured+1,
                     gap=CASE WHEN managed=1 THEN gap ELSE 'unmanaged checkpoint update' END;
             END;",
        )
        .unwrap();
        views
            .initialize_empty(
                &tx,
                SourceCut {
                    epoch: 1,
                    admitted: 0,
                    projected: 0,
                    local_generation: 0,
                },
            )
            .unwrap();
        views
            .install_gap_trigger(&tx, "checkpoint_capture_scope", "gap")
            .unwrap();
        tx.commit().unwrap();
    }
    let failure = Arc::new(AtomicU8::new(0));
    let prepare_failure = failure.clone();
    let finalize_failure = failure.clone();
    store
        .install_transaction_hooks(
            move |tx| {
                tx.execute("UPDATE checkpoint_capture_scope SET managed=1", [])?;
                if prepare_failure.load(Ordering::Acquire) == 1 {
                    bail!("checkpoint prepare refused");
                }
                Ok(())
            },
            move |tx| {
                if finalize_failure.load(Ordering::Acquire) == 2 {
                    bail!("checkpoint finalize refused");
                }
                tx.execute(
                    "UPDATE checkpoint_capture_scope SET managed=0,finalized=finalized+1",
                    [],
                )?;
                Ok(())
            },
        )
        .unwrap();
    (store, views, failure)
}

fn seal() -> SealedIdentities {
    SealedIdentities {
        digest: "checkpoint-fixture-seal".into(),
        count: 0,
        seal_rowid: 0,
    }
}

fn scope(store: &Store) -> (u64, u64, u64, Option<String>) {
    store
        .readers
        .get()
        .query_row(
            "SELECT managed,captured,finalized,gap FROM checkpoint_capture_scope WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn states(store: &Store) -> Vec<(String, String, Option<String>)> {
    store
        .readers
        .get()
        .prepare("SELECT id,state,detail FROM checkpoints ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn checkpoint_state_writes_share_paired_scope_without_fencing_view() {
    let (store, views, _) = fixture();
    store
        .record_checkpoint_state("checkpoint/sample", 10, "sealed", &seal(), None)
        .unwrap();
    store
        .set_checkpoint_state("checkpoint/sample", 10, "verified")
        .unwrap();
    store
        .set_checkpoint_state("checkpoint/sample", 10, "graph-changed")
        .unwrap();
    store
        .resume_checkpoints("person/reviewer", "reviewed the stopped trim")
        .unwrap();
    assert_eq!(scope(&store), (0, 4, 4, None));
    assert_eq!(states(&store)[0].1, "set-aside");
    let detail: serde_json::Value =
        serde_json::from_str(states(&store)[0].2.as_ref().unwrap()).unwrap();
    assert_eq!(detail["resumed_by"], "person/reviewer");
    assert!(matches!(
        views
            .availability(&store.readers.get(), "checkpoint-view", 1)
            .unwrap()
            .readiness,
        Readiness::Ready(_)
    ));
    // Positive control: the same physical mutation outside a managed transaction still fences.
    store
        .connection
        .write()
        .execute(
            "UPDATE checkpoints SET state=?1 WHERE id=?2",
            params!["graph-changed", "checkpoint/sample"],
        )
        .unwrap();
    assert!(scope(&store).3.is_some());
    assert!(matches!(
        views
            .availability(&store.readers.get(), "checkpoint-view", 1)
            .unwrap()
            .readiness,
        Readiness::Fenced
    ));
}

#[test]
fn checkpoint_prepare_and_finalize_failures_roll_back_source_and_scope() {
    let (store, views, failure) = fixture();
    store
        .set_checkpoint_state("checkpoint/sample", 10, "graph-changed")
        .unwrap();
    let before_rows = states(&store);
    let before_scope = scope(&store);
    let before_token = views
        .token(&store.readers.get(), "checkpoint-view", 1)
        .unwrap();
    for phase in [1, 2] {
        failure.store(phase, Ordering::Release);
        let attempts: [Result<()>; 3] = [
            store.record_checkpoint_state("checkpoint/new", 20, "sealed", &seal(), None),
            store.set_checkpoint_state("checkpoint/sample", 10, "verified"),
            store
                .resume_checkpoints("person/reviewer", "reviewed the stopped trim")
                .map_err(anyhow::Error::new),
        ];
        for result in attempts {
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains(if phase == 1 {
                "checkpoint prepare refused"
            } else {
                "checkpoint finalize refused"
            }));
        }
        assert_eq!(states(&store), before_rows);
        assert_eq!(scope(&store), before_scope);
        assert_eq!(
            views
                .token(&store.readers.get(), "checkpoint-view", 1)
                .unwrap(),
            before_token
        );
    }
    failure.store(0, Ordering::Release);
    store
        .resume_checkpoints("person/reviewer", "reviewed the stopped trim")
        .unwrap();
    assert_eq!(states(&store)[0].1, "set-aside");
    assert_eq!(scope(&store), (0, 2, 2, None));
}

#[test]
fn checkpoint_state_writes_keep_default_no_hook_behavior() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    store
        .record_checkpoint_state("checkpoint/sample", 10, "sealed", &seal(), None)
        .unwrap();
    store
        .set_checkpoint_state("checkpoint/sample", 10, "graph-changed")
        .unwrap();
    assert_eq!(states(&store)[0].1, "graph-changed");
    assert!(
        store
            .resume_checkpoints("agent/example", "invalid actor")
            .is_err()
    );
    assert!(store.resume_checkpoints("person/reviewer", " ").is_err());
    assert_eq!(states(&store)[0].1, "graph-changed");
    store
        .resume_checkpoints("person/reviewer", "reviewed the stopped trim")
        .unwrap();
    assert_eq!(states(&store)[0].1, "set-aside");
}
