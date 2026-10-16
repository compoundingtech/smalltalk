use super::*;
use crate::store::{ClaimInput, append_claim_record_tx};
use serde_json::json;
use smallclaims::ivm::install::Root;
use smallclaims::sqlite::WriterJob;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

mod operator_errors;

type Saved = (Root, (i64, bool, Option<String>, bool), String, String);

struct Fixture {
    adapter: Arc<Adapter>,
    store: Store,
    dir: tempfile::TempDir,
}
fn binding() -> Binding {
    Binding {
        origin: "node".into(),
        mission: "one".into(),
        run: "mission-run/one".into(),
        generation: "run-generation/one".into(),
        step: "step-run/one/prepare".into(),
        path: "prepare".into(),
        gate: "prepared".into(),
        exec: "exec/one/probe".into(),
    }
}
impl Fixture {
    fn new(seed: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
        let adapter = Adapter::install(&store).unwrap();
        adapter.bootstrap(&store, binding(), seed).unwrap();
        let fixture = Self {
            adapter,
            store,
            dir,
        };
        fixture.reset_counts();
        fixture
    }
    fn source(&self) -> &NativeSource {
        self.adapter.source.get().unwrap()
    }
    fn read<T>(&self, read: impl FnOnce(&Transaction<'_>) -> T) -> T {
        // A pinned read, never a managed writer transaction or a hook invocation.
        let mut c = rusqlite::Connection::open_with_flags(
            self.dir.path().join("claims.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        read(&tx)
    }
    fn line(&self) -> Value {
        self.read(|tx| self.source().doctor_line(tx))
    }
    fn saved(&self) -> Saved {
        self.read(|tx| (self.source().installer.root(tx, VIEW).unwrap(), self.source().scoped_state(tx).unwrap(),
            tx.query_row("SELECT body FROM claims WHERE subject='exec/one/probe' AND kind='runtime.observed' ORDER BY store_index DESC LIMIT 1", [], |r|r.get(0)).unwrap(),
            tx.query_row("SELECT body FROM test_terminal_members WHERE key='gate/native'", [], |r|r.get(0)).unwrap()))
    }
    fn reset_counts(&self) {
        self.adapter.prepare_calls.store(0, Ordering::Relaxed);
        self.adapter.finalize_calls.store(0, Ordering::Relaxed);
        self.adapter.work.lock().unwrap().clear();
    }
    fn counts(&self) -> (usize, usize) {
        (
            self.adapter.prepare_calls.load(Ordering::Relaxed),
            self.adapter.finalize_calls.load(Ordering::Relaxed),
        )
    }
    fn raw(&self, job: impl FnOnce(&mut rusqlite::Connection)) {
        let mut writer = self.store.connection.write();
        job(writer.connection.as_mut().unwrap());
    }
    fn commit(&self, job: impl FnOnce(&Transaction<'_>)) {
        let mut writer = self.store.connection.write();
        let tx = writer.transaction().unwrap();
        job(&tx);
        tx.commit().unwrap();
    }
    fn enqueue(
        &self,
        job: impl FnOnce(&Transaction<'_>) -> bool + Send + 'static,
    ) -> Receiver<Result<(), String>> {
        let (done, answer) = sync_channel(1);
        self.store.connection.send(WriterJob::Batched {
            run: Box::new(job),
            profile: None,
            wait: None,
            holder: smallclaims::windows::holds::Holder::current(),
            enqueued: Instant::now(),
            done,
        });
        answer
    }
}
fn update_exit(tx: &rusqlite::Connection, code: i64) {
    tx.execute("UPDATE claims SET body=json_set(body,'$.fields.exit_code',?1) WHERE subject='exec/one/probe' AND kind='runtime.observed'", [code]).unwrap();
}
fn answer(rx: Receiver<Result<(), String>>) -> Result<(), String> {
    rx.recv_timeout(Duration::from_secs(10)).unwrap()
}
fn observation(f: &Fixture, code: i64) -> ClaimInput {
    let desired: String = f.read(|tx| {
        tx.query_row(
            "SELECT claim_id FROM desired WHERE subject='exec/one/probe'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    ClaimInput {
        subject: "exec/one/probe".into(),
        kind: "runtime.observed".into(),
        actor: None,
        fields: BTreeMap::from([
            ("status".into(), json!("exited")),
            ("exit_code".into(), json!(code)),
            ("incarnation_id".into(), json!(format!("managed-{code}"))),
        ]),
        evidence: vec![desired],
        expected_subject: None,
        idempotency_key: None,
    }
}

#[test]
fn managed_gate_explicit_bootstrap_slot_and_missing_metadata_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
    let adapter = Adapter::install(&store).unwrap();
    assert!(Adapter::install(&store).is_err());
    // No implicit bootstrap: a queued job cannot run before the explicit caller token.
    let ran = Arc::new(AtomicUsize::new(0));
    let job_ran = ran.clone();
    let result = store.connection.batched(move |_| {
        job_ran.fetch_add(1, Ordering::Relaxed);
        Ok::<_, anyhow::Error>(())
    });
    assert!(result.is_err());
    assert_eq!(ran.load(Ordering::Relaxed), 0);
    // An authorized transaction that omits initialization still cannot COMMIT.
    {
        let token = adapter.token().unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(tx.commit().unwrap_err().to_string().contains("bootstrap"));
        drop(writer);
        drop(token);
        assert!(adapter.token().is_err()); // token cannot be reused after a failed factory
    }
    // Discard that failed factory. No new attestation of nonempty input is allowed.
    drop(store);
    let f = Fixture::new(false);
    assert_eq!(f.line()["status"], "unknown");
    assert_eq!(f.counts(), (0, 0));
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(NativeSource::attach(&tx, binding()).is_err());
    tx.rollback().unwrap();
    drop(writer);
    f.raw(|c| {
        c.execute("DELETE FROM test_native_terminal_guard", [])
            .unwrap();
    });
    let mut writer = f.store.connection.write();
    assert!(writer.transaction().is_err());
    drop(writer);
    assert_eq!(f.line()["status"], "unknown");
    let nonempty = Store::open_memory("node").unwrap();
    nonempty
        .append_claim(&ClaimInput {
            subject: "exec/preexisting".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), json!("exited"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let denied = Adapter::install(&nonempty).unwrap();
    assert!(denied.bootstrap(&nonempty, binding(), false).is_err());
    assert!(denied.source.get().is_none());
    let occupied = Store::open_memory("node").unwrap();
    occupied
        .connection
        .install_transaction_finalizer(|_| Ok(()))
        .unwrap();
    assert!(Adapter::install(&occupied).is_err());
    let ordinary = Store::open_memory("node").unwrap();
    let writer = ordinary.connection.write();
    let exists: bool = writer.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name LIKE 'test_native_%' OR name LIKE 'test_terminal_%')", [], |r|r.get(0)).unwrap();
    assert!(!exists);
}

#[test]
fn managed_gate_native_append_and_local_latest_paths_finalize_before_ack() {
    let f = Fixture::new(true);
    let first = f.store.append_claim(&observation(&f, 7)).unwrap();
    assert_eq!(f.counts(), (1, 1));
    assert!(
        f.line()["message"]
            .as_str()
            .unwrap()
            .contains("exit code 7")
    );
    assert_eq!(f.line()["status"], "warn");
    assert!(f.read(|tx| {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1)",
            [&first.id],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    }));
    let mut local = observation(&f, 0);
    local.subject = "agent/local".into();
    local.kind = "workspace.observed".into();
    local.fields = BTreeMap::from([
        ("host".into(), json!("node")),
        ("workspace".into(), json!("/tmp")),
    ]);
    local.evidence.clear();
    let before = f.saved().0;
    f.store.append_claim(&local).unwrap(); // actual latest-retention public branch
    assert_eq!(f.counts(), (2, 2));
    local.kind = "render.applied".into();
    local.subject = "exec/unrelated".into();
    local.fields = BTreeMap::from([("writes".into(), json!([])), ("warnings".into(), json!([]))]);
    f.store.append_claim(&local).unwrap(); // actual local-retention public branch
    assert_eq!(f.counts(), (3, 3));
    assert_eq!(f.saved().0, before);
    // Native validation refusal is still an error, never an acknowledged write.
    let retained = f.saved();
    let mut missing = observation(&f, 0);
    missing.evidence = vec!["claim/not-stored".into()];
    assert_eq!(
        f.store.append_claim(&missing).unwrap_err().code,
        "missing-evidence"
    );
    assert_eq!(f.saved(), retained);
    let mut invalid = observation(&f, 0);
    invalid.kind = "not-a-native-kind".into();
    assert!(f.store.append_claim(&invalid).is_err());
    assert_eq!(f.saved().0, before);
    f.store.append_claim(&observation(&f, 0)).unwrap();
    assert_eq!(f.line()["status"], "unknown");
    assert!(!f.read(|tx| f.source().scoped_state(tx).unwrap().3));
}

#[test]
fn managed_gate_queued_native_savepoints_share_one_final_publication() {
    // The public send calls synchronously enqueue three real writer jobs while the lend
    // prevents consumption. Only batch-layout setup can be retried, never an assertion.
    for setup in 0..4 {
        let f = Fixture::new(true);
        let writer = f.store.connection.write();
        let batch_before = f.store.connection.batches.0.load(Ordering::Relaxed);
        let first = f.enqueue(|tx| {
            append_claim_record_tx(
                tx,
                "node",
                "exec/one/probe",
                "runtime.action.succeeded",
                None,
                &json!({"fields":{}}),
                &[],
                None,
            )
            .unwrap();
            true
        });
        let second = f.enqueue(|tx| {
            update_exit(tx, 9);
            true
        });
        let failed = f.enqueue(|tx| {
            update_exit(tx, 88);
            false
        });
        drop(writer); // all three sends have returned: no sleeping or queue guess
        let outcomes = [answer(first), answer(second), answer(failed)];
        let batches = f.store.connection.batches.0.load(Ordering::Relaxed) - batch_before;
        eprintln!(
            "managed batch setup={setup} observed_batches={batches} acknowledgements={outcomes:?}"
        );
        if batches != 1 {
            continue;
        } // bounded setup refusal, before semantic assertions
        assert!(outcomes.iter().all(Result::is_ok));
        assert_eq!(f.counts(), (1, 1));
        let line = f.line();
        assert_eq!(line["status"], "warn");
        assert!(line["message"].as_str().unwrap().contains("exit code 9"));
        assert!(!line["message"].as_str().unwrap().contains("88"));
        assert!(!f.read(|tx| f.source().scoped_state(tx).unwrap().3));
        return;
    }
    panic!("could not establish one writer batch in four setup attempts");
}

#[test]
fn managed_gate_prepare_finalize_panic_and_commit_errors_roll_back_all_callers() {
    for fault in [2, 3] {
        let mut established = false;
        for setup in 0..4 {
            let f = Fixture::new(true);
            let before = f.saved();
            f.adapter.fault.store(fault, Ordering::Relaxed);
            let writer = f.store.connection.write();
            let batches = f.store.connection.batches.0.load(Ordering::Relaxed);
            let a = f.enqueue(|tx| {
                update_exit(tx, 5);
                true
            });
            let b = f.enqueue(|tx| {
                update_exit(tx, 6);
                true
            });
            drop(writer);
            let outcomes = [answer(a), answer(b)];
            let count = f.store.connection.batches.0.load(Ordering::Relaxed) - batches;
            eprintln!(
                "managed failure setup={setup} fault={fault} batches={count} acknowledgements={outcomes:?}"
            );
            if count != 1 {
                continue;
            }
            assert!(outcomes.iter().all(Result::is_err));
            assert_eq!(f.counts(), (1, 1));
            assert_eq!(f.saved(), before); // source, cut, generation and physical witness
            established = true;
            break;
        }
        assert!(established, "failed to establish failure batch setup");
    }
    let f = Fixture::new(true);
    let before = f.saved();
    f.adapter.fault.store(1, Ordering::Relaxed);
    let ran = Arc::new(AtomicUsize::new(0));
    let job_ran = ran.clone();
    let a = f.enqueue(move |_| {
        job_ran.fetch_add(1, Ordering::Relaxed);
        true
    });
    assert!(answer(a).is_err());
    assert_eq!(ran.load(Ordering::Relaxed), 0);
    assert_eq!(f.counts(), (1, 0));
    assert_eq!(f.saved(), before);
    f.adapter.fault.store(0, Ordering::Relaxed);
    f.raw(|c| {
        c.execute_batch("CREATE TEMP TRIGGER test_managed_storage_failure BEFORE UPDATE OF generation ON main.ivm_install_roots WHEN NEW.generation>OLD.generation BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    });
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        update_exit(&tx, 100);
        let error = tx.commit().unwrap_err();
        assert!(
            error.downcast_ref::<rusqlite::Error>().is_some(),
            "{error:#}"
        );
    }
    assert_eq!(f.saved(), before);
    f.raw(|c| {
        c.execute_batch("DROP TRIGGER test_managed_storage_failure")
            .unwrap();
    });
    f.reset_counts();
    f.raw(|c| { c.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE test_managed_parent(id INTEGER PRIMARY KEY); CREATE TABLE test_managed_child(parent INTEGER REFERENCES test_managed_parent DEFERRABLE INITIALLY DEFERRED);").unwrap(); });
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    update_exit(&tx, 99);
    tx.execute("INSERT INTO test_managed_child VALUES(404)", [])
        .unwrap();
    assert!(tx.commit().is_err()); // succeeds through finalize, fails at SQLite COMMIT
    drop(writer);
    assert_eq!(f.counts(), (1, 1));
    assert_eq!(f.saved(), before);
}

#[test]
fn managed_gate_lent_rollback_drop_and_semantic_noop_preserve_committed_root() {
    let f = Fixture::new(true);
    let before = f.saved();
    assert_eq!(f.line()["status"], "warn");
    assert_eq!(f.counts(), (0, 0));
    assert_eq!(f.saved(), before);
    {
        let mut writer = f.store.connection.write();
        let mut tx = writer.transaction().unwrap();
        {
            let mut savepoint = tx.savepoint().unwrap();
            update_exit(&savepoint, 77);
            savepoint.rollback().unwrap();
        }
        assert_eq!(f.source().doctor_line(&tx)["status"], "unknown"); // marker alone
        tx.rollback().unwrap();
    }
    assert_eq!(f.counts(), (1, 0));
    assert_eq!(f.saved(), before);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        update_exit(&tx, 78);
        // Drop deliberately does not run finalize.
    }
    assert_eq!(f.counts(), (2, 0));
    assert_eq!(f.saved(), before);
    f.commit(|_| {});
    assert_eq!(f.saved(), before);
    assert_eq!(f.counts(), (3, 1));
    let generation = before.0.generation;
    f.commit(|tx| update_exit(tx, 2));
    assert_eq!(f.saved().0.generation, generation);
    assert_eq!(f.saved().3, before.3);
    assert_eq!(f.counts(), (4, 2));
}

#[test]
fn managed_gate_raw_bypasses_and_leaked_scope_never_become_ready() {
    for mode in 0..6 {
        let f = Fixture::new(true);
        let root = f.saved().0;
        match mode {
            0 => f.raw(|c| {
                c.execute_batch("BEGIN").unwrap();
                update_exit_raw(c, 4);
                c.execute_batch("COMMIT").unwrap();
            }),
            1 => f.raw(|c| {
                let tx = c.unchecked_transaction().unwrap();
                update_exit(&tx, 4);
                tx.commit().unwrap();
            }),
            2 => f.raw(|c| {
                let tx = rusqlite::Transaction::new(c, rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                update_exit(&tx, 4);
                tx.commit().unwrap();
            }),
            3 => f.raw(|c| update_exit_raw(c, 4)),
            4 => {
                let mut c =
                    rusqlite::Connection::open(f.dir.path().join("claims.sqlite3")).unwrap();
                smallclaims::store::configure_projection_writer(&c).unwrap();
                let tx = c.transaction().unwrap();
                update_exit(&tx, 4);
                tx.commit().unwrap();
            }
            5 => {
                let mut writer = f.store.connection.write();
                let tx = writer.transaction().unwrap();
                // Raw COMMIT violates the managed wrapper. Already committed state cannot
                // be rolled back. Even an unchanged old root is not reader-eligible.
                tx.execute_batch("COMMIT").unwrap();
                assert!(tx.commit().is_err());
            }
            _ => unreachable!(),
        }
        assert_eq!(f.counts().1, 0, "mode {mode}");
        assert_eq!(f.line()["status"], "unknown", "mode {mode}");
        assert_eq!(
            f.read(|tx| f.source().installer.root(tx, VIEW).ok()),
            if mode == 5 { Some(root) } else { None }
        );
        f.commit(|_| {});
        assert_eq!(f.line()["status"], "unknown");
        assert!(f.read(|tx| f.source().state(tx).unwrap().2.is_some()));
        assert!(f.read(|tx| f.source().installer.root(tx, VIEW).is_err()));
        f.commit(|_| {}); // persistent gap; never reattest from the old witness
        assert_eq!(f.line()["status"], "unknown");
    }
}
fn update_exit_raw(c: &rusqlite::Connection, code: i64) {
    c.execute("UPDATE claims SET body=json_set(body,'$.fields.exit_code',?1) WHERE subject='exec/one/probe' AND kind='runtime.observed'",[code]).unwrap();
}

#[test]
fn managed_gate_clean_reopen_open_time_writes_and_callback_lifetime() {
    let f = Fixture::new(true);
    let weak = Arc::downgrade(&f.adapter);
    let Fixture {
        adapter,
        store,
        dir,
    } = f;
    drop(adapter);
    drop(store);
    assert!(weak.upgrade().is_none()); // callback has no Store ownership cycle
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
    let adapter = Adapter::matching_reopen(&store, binding()).unwrap();
    let f = Fixture {
        adapter,
        store,
        dir,
    };
    let pending = f.read(|tx| f.source().scoped_state(tx).unwrap().1);
    f.commit(|_| {});
    // A clean open is conditional on NO captured writes before hook installation.
    if pending {
        assert_eq!(f.line()["status"], "unknown");
    } else {
        assert_eq!(f.line()["status"], "warn");
    }
    let Fixture {
        adapter,
        store,
        dir,
    } = f;
    drop(adapter);
    drop(store);
    {
        let c = rusqlite::Connection::open(dir.path().join("claims.sqlite3")).unwrap();
        smallclaims::store::configure_projection_writer(&c).unwrap();
        // Force the real open_projections replay on this tiny PRIVATE source. The edit
        // itself changes only unprotected meta: the next Store::open changes source rows.
        c.execute(
            "DELETE FROM meta WHERE key='canonical_shared_projection_rules'",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
    let adapter = Adapter::matching_reopen(&store, binding()).unwrap();
    let f = Fixture {
        adapter,
        store,
        dir,
    };
    assert!(f.read(|tx| f.source().scoped_state(tx).unwrap().1));
    assert_eq!(f.line()["status"], "unknown");
    f.commit(|_| {});
    assert!(f.read(|tx| f.source().state(tx).unwrap().2.is_some()));
    assert!(f.read(|tx| f.source().installer.root(tx, VIEW).is_err()));
    f.commit(|_| {});
    assert_eq!(f.line()["status"], "unknown");
}

#[test]
fn managed_gate_work_at_zero_1024_and_100000_unrelated_rows() {
    for n in [0, 1024, 100_000] {
        let f = Fixture::new(true);
        // Setup is explicitly raw and unrelated. No source capture, no cost claim.
        f.raw(|c| {
            c.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES('foreign','node',999,'fixture-foreign','1')",[]).unwrap();
            if n>0 { c.execute("WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<?1) INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) SELECT 'foreign/'||n,'foreign','exec/unrelated/'||n,'runtime.observed','node','{}','[]','1' FROM rows",[n]).unwrap(); }
        });
        for operation in ["idle", "unrelated", "noop"] {
            f.reset_counts();
            let mut writer = f.store.connection.write();
            let whole = SqliteWorkScope::start();
            let tx = writer.transaction().unwrap();
            match operation {
                "unrelated" => {
                    tx.execute("UPDATE claims SET body=body WHERE id='foreign/1'", [])
                        .unwrap();
                }
                "noop" => update_exit(&tx, 2),
                _ => (),
            }
            tx.commit().unwrap();
            // Includes BEGIN, hooks, job, COMMIT; excludes lend wait/return and read proof.
            let whole = whole.finish();
            drop(writer);
            eprintln!(
                "managed native rows={n} operation={operation} whole={whole:?} finalizers={}",
                f.counts().1
            );
            assert_eq!(f.counts(), (1, 1));
            for (phase, work) in f.adapter.work.lock().unwrap().iter() {
                eprintln!(
                    "managed native rows={n} operation={operation} phase={phase} work={work:?}"
                );
                assert_eq!(work.fullscan_steps, 0, "{phase} {work:?}");
                match *phase {
                    "prepare" => {
                        assert!(work.statements <= 3 && work.vm_steps <= 120, "{work:?}");
                    }
                    "finalize-marker-read" => {
                        assert_eq!(work.statements, 1);
                        assert!(work.vm_steps <= 40, "{work:?}");
                    }
                    "source-finish" if operation != "noop" => {
                        assert_eq!(work.statements, 1);
                        assert!(work.vm_steps <= 40, "{work:?}");
                    }
                    "source-finish" => {
                        assert!(work.statements <= 96 && work.vm_steps <= 20_000, "{work:?}");
                    }
                    "marker-clear" => {
                        assert_eq!(work.statements, 1);
                        assert!(work.vm_steps <= 40, "{work:?}");
                    }
                    _ => panic!("unrecognized measurement phase"),
                }
            }
            assert!(whole.statements > 0 && whole.vm_steps > 0);
            assert_eq!(whole.fullscan_steps, 0);
            assert_eq!(f.line()["status"], "warn");
        }
    }
}
