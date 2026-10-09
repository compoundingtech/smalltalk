//! Explicit writer-owned prepare/finalize callbacks inside a managed transaction.
//!
//! This is a transaction boundary, not source-coverage or authorization evidence. The
//! adapter must capture complete old/new facts in the source transaction and bound its work.

use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use anyhow::{Result, ensure};
use rusqlite::{Savepoint, Transaction};

type TransactionCallback = Arc<dyn Fn(&Transaction<'_>) -> Result<()> + Send + Sync>;

struct TransactionHooks {
    prepare: TransactionCallback,
    finalize: TransactionCallback,
}

#[derive(Default)]
pub(super) struct TransactionFinalizers {
    hooks: OnceLock<TransactionHooks>,
}

impl TransactionFinalizers {
    pub(super) fn install(
        &self,
        prepare: impl Fn(&Transaction<'_>) -> Result<()> + Send + Sync + 'static,
        finalize: impl Fn(&Transaction<'_>) -> Result<()> + Send + Sync + 'static,
    ) -> Result<()> {
        ensure!(
            self.hooks
                .set(TransactionHooks {
                    prepare: Arc::new(prepare),
                    finalize: Arc::new(finalize),
                })
                .is_ok(),
            "transaction hooks are already installed"
        );
        Ok(())
    }

    pub(super) fn prepare(&self, transaction: &Transaction<'_>) -> Result<()> {
        if let Some(hooks) = self.hooks.get() {
            Self::invoke(&hooks.prepare, transaction, "prepare")?;
        }
        Ok(())
    }

    pub(super) fn run(&self, transaction: &Transaction<'_>) -> Result<()> {
        if let Some(hooks) = self.hooks.get() {
            Self::invoke(&hooks.finalize, transaction, "finalizer")?;
        }
        Ok(())
    }

    fn invoke(
        callback: &TransactionCallback,
        transaction: &Transaction<'_>,
        phase: &str,
    ) -> Result<()> {
        // A callback failure must undo its source transaction without killing the writer or
        // letting queued callers acknowledge a write whose derived maintenance failed.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(transaction)))
            .map_err(|_| anyhow::anyhow!("transaction {phase} panicked"))??;
        ensure!(
            !transaction.is_autocommit(),
            "transaction {phase} must not end the source transaction"
        );
        Ok(())
    }
}

/// A lent-writer transaction whose explicit commit runs the writer's finalizer first.
///
/// Dereferencing yields the ordinary transaction for existing free transaction helpers.
/// Drop and rollback retain SQLite's rollback behavior and do not invoke the callback.
/// A finalizer error (including a panic) rolls back the source and callback writes. No
/// mutable dereference or commit-on-drop setter is exposed: commit must use this wrapper.
/// Explicit raw Connection access and raw SQL transaction control remain outside this API.
pub struct WriterTransaction<'connection> {
    transaction: Transaction<'connection>,
    finalizers: Arc<TransactionFinalizers>,
}

impl<'connection> WriterTransaction<'connection> {
    pub(super) fn new(
        transaction: Transaction<'connection>,
        finalizers: Arc<TransactionFinalizers>,
    ) -> Self {
        Self {
            transaction,
            finalizers,
        }
    }

    /// Run bounded source maintenance in this transaction, then commit. Errors propagate
    /// with their original causes; SQLite storage errors remain downcastable.
    pub fn commit(self) -> Result<()> {
        self.commit_checked(|| Ok(()))
    }

    /// A writer-owned scope may refuse immediately before COMMIT, after managed finalizers.
    /// A check error drops/rolls back the transaction. It cannot cancel a completed COMMIT.
    pub(super) fn commit_checked(self, check: impl FnOnce() -> Result<()>) -> Result<()> {
        ensure!(
            !self.transaction.is_autocommit(),
            "writer transaction was ended through raw SQL"
        );
        self.finalizers.run(&self.transaction)?;
        check()?;
        self.transaction.commit()?;
        crate::profile::managed_commit_succeeded();
        Ok(())
    }

    pub fn rollback(self) -> rusqlite::Result<()> {
        self.transaction.rollback()
    }

    /// Nested savepoints do not finalize; the outer commit sees only surviving mutations.
    pub fn savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>> {
        self.transaction.savepoint()
    }
}

impl<'connection> Deref for WriterTransaction<'connection> {
    type Target = Transaction<'connection>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::{WriterConnection, WriterJob};
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    fn writer() -> WriterConnection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT);
             CREATE TABLE source(id INTEGER PRIMARY KEY,value TEXT);
             CREATE TABLE delta(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id INTEGER,old TEXT,new TEXT);
             CREATE TABLE output(id INTEGER PRIMARY KEY,value TEXT);
             CREATE TABLE allowed(id INTEGER PRIMARY KEY);
             INSERT INTO allowed VALUES(1);
             CREATE TABLE deferred_fk(id INTEGER REFERENCES allowed(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER source_insert AFTER INSERT ON source BEGIN
               INSERT INTO delta(id,new) VALUES(NEW.id,NEW.value); END;
             CREATE TRIGGER source_update AFTER UPDATE ON source BEGIN
               INSERT INTO delta(id,old,new) VALUES(OLD.id,OLD.value,NEW.value); END;
             CREATE TRIGGER source_delete AFTER DELETE ON source BEGIN
               INSERT INTO delta(id,old) VALUES(OLD.id,OLD.value); END;",
        ).unwrap();
        WriterConnection::new(connection, Arc::new(AtomicU64::new(0)))
    }

    fn drain(tx: &Transaction<'_>) -> Result<()> {
        let changes = tx
            .prepare("SELECT id,new FROM delta ORDER BY sequence LIMIT 5")?
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(changes.len() <= 4, "test adapter page exceeded");
        for (id, value) in changes {
            if let Some(value) = value {
                tx.execute("INSERT INTO output VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value", rusqlite::params![id,value])?;
            } else {
                tx.execute("DELETE FROM output WHERE id=?1", [id])?;
            }
        }
        tx.execute("DELETE FROM delta", [])?;
        Ok(())
    }

    // A free transaction helper knows no runtime, writer or view registry.
    fn free_write(tx: &Transaction<'_>, id: i64, value: &str) -> Result<()> {
        tx.execute(
            "INSERT INTO source VALUES(?1,?2)",
            rusqlite::params![id, value],
        )?;
        Ok(())
    }

    fn count(writer: &WriterConnection, table: &str) -> i64 {
        writer
            .write()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn lent_free_helper_and_same_index_replacement_finalize_before_notice() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                called.fetch_add(1, Ordering::Relaxed);
                drain(tx)
            })
            .unwrap();
        let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = observations.clone();
        let _observer = writer.observe_commits(move |connection| {
            let value: Option<String> = connection
                .query_row("SELECT value FROM output WHERE id=1", [], |r| r.get(0))
                .optional()
                .unwrap();
            observed.lock().unwrap().push(value);
        });
        {
            let mut guard = writer.write();
            let mut tx = guard.transaction().unwrap();
            free_write(&tx, 1, "first").unwrap();
            {
                let savepoint = tx.savepoint().unwrap();
                savepoint
                    .execute("UPDATE source SET value='rolled back' WHERE id=1", [])
                    .unwrap();
                // Savepoint drop rolls back source and captured old/new mutation together.
            }
            tx.commit().unwrap();
        }
        {
            let mut guard = writer.write();
            let tx = guard
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            tx.execute("UPDATE source SET value='same-index' WHERE id=1", [])
                .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            *observations.lock().unwrap(),
            vec![Some("first".into()), Some("same-index".into())]
        );
        assert_eq!(count(&writer, "claims"), 0);
        assert_eq!(count(&writer, "delta"), 0);
    }

    #[test]
    fn failed_finalizer_rolls_back_source_and_callback_sql_and_keeps_error_type() {
        let writer = writer();
        writer
            .install_transaction_finalizer(|tx| {
                drain(tx)?;
                tx.execute("INSERT INTO absent_table VALUES(1)", [])?;
                Ok(())
            })
            .unwrap();
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 1, "never committed").unwrap();
            let error = tx.commit().unwrap_err();
            assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        }
        assert_eq!(count(&writer, "source"), 0);
        assert_eq!(count(&writer, "output"), 0);
        assert_eq!(count(&writer, "delta"), 0);
    }

    #[test]
    fn sqlite_commit_failure_after_finalizer_undoes_both_and_does_not_publish_output() {
        let writer = writer();
        writer.install_transaction_finalizer(drain).unwrap();
        let notices = Arc::new(AtomicU64::new(0));
        let seen = notices.clone();
        let _observer = writer.observe_commits(move |connection| {
            let rows: i64 = connection
                .query_row("SELECT COUNT(*) FROM output", [], |r| r.get(0))
                .unwrap();
            if rows > 0 {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        });
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 1, "invalid outer commit").unwrap();
            tx.execute("INSERT INTO deferred_fk VALUES(2)", []).unwrap();
            assert!(
                tx.commit()
                    .unwrap_err()
                    .downcast_ref::<rusqlite::Error>()
                    .is_some()
            );
        }
        assert_eq!(notices.load(Ordering::Relaxed), 0);
        assert_eq!(count(&writer, "source"), 0);
        assert_eq!(count(&writer, "output"), 0);
    }

    #[test]
    fn rollback_and_drop_never_finalize_and_next_transaction_is_usable() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                called.fetch_add(1, Ordering::Relaxed);
                drain(tx)
            })
            .unwrap();
        let mut guard = writer.write();
        {
            let tx = guard.transaction().unwrap();
            free_write(&tx, 1, "drop").unwrap();
        }
        let tx = guard.transaction().unwrap();
        free_write(&tx, 2, "rollback").unwrap();
        tx.rollback().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let tx = guard.transaction().unwrap();
        free_write(&tx, 3, "kept").unwrap();
        tx.commit().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queued_batch_finalizes_once_for_surviving_savepoints_before_ack() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                called.fetch_add(1, Ordering::Relaxed);
                drain(tx)
            })
            .unwrap();
        // Holding the writer lets us deterministically enqueue one outer batch.
        let guard = writer.write();
        let mut answers = Vec::new();
        for (id, success) in [(1, true), (2, false), (3, true)] {
            let (done, answer) = mpsc::sync_channel(1);
            writer.send(WriterJob::Batched {
                run: Box::new(move |tx| {
                    free_write(tx, id, "queued").unwrap();
                    success
                }),
                profile: None,
                wait: None,
                done,
            });
            answers.push(answer);
        }
        drop(guard);
        for answer in answers {
            assert!(answer.recv().unwrap().is_ok());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(count(&writer, "source"), 2);
        assert_eq!(count(&writer, "output"), 2);
        assert_eq!(count(&writer, "delta"), 0);
    }

    #[test]
    fn failed_queued_finalizer_fails_all_batch_acknowledgements_and_writer_recovers() {
        let writer = writer();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing = fail.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                drain(tx)?;
                ensure!(!failing.load(Ordering::Relaxed), "adapter failed");
                Ok(())
            })
            .unwrap();
        let guard = writer.write();
        let mut answers = Vec::new();
        for id in [1, 2] {
            let (done, answer) = mpsc::sync_channel(1);
            writer.send(WriterJob::Batched {
                run: Box::new(move |tx| {
                    free_write(tx, id, "batch").unwrap();
                    true
                }),
                profile: None,
                wait: None,
                done,
            });
            answers.push(answer);
        }
        drop(guard);
        for answer in answers {
            assert!(
                answer
                    .recv()
                    .unwrap()
                    .unwrap_err()
                    .contains("adapter failed")
            );
        }
        assert_eq!(count(&writer, "source"), 0);
        assert_eq!(count(&writer, "output"), 0);
        fail.store(false, Ordering::Relaxed);
        writer
            .batched(|tx| free_write(tx, 3, "recovered"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "output"), 1);
    }

    #[test]
    fn panic_in_finalizer_rolls_back_and_does_not_kill_writer() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                drain(tx)?;
                if called.fetch_add(1, Ordering::Relaxed) == 0 {
                    panic!("fixture panic");
                }
                Ok(())
            })
            .unwrap();
        let failure = writer
            .batched(|tx| free_write(tx, 1, "failed"))
            .unwrap_err();
        assert!(failure.contains("finalizer panicked"));
        assert_eq!(count(&writer, "output"), 0);
        writer
            .batched(|tx| free_write(tx, 2, "after panic"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "output"), 1);
    }

    #[test]
    fn duplicate_install_cannot_replace_the_registered_source_owner() {
        let writer = writer();
        writer.install_transaction_finalizer(drain).unwrap();
        assert!(
            writer
                .install_transaction_finalizer(|_| anyhow::bail!("other owner"))
                .is_err()
        );
        writer
            .batched(|tx| free_write(tx, 1, "first owner"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "output"), 1);
    }

    #[test]
    fn raw_connection_bypass_is_not_mistaken_for_finalized_source() {
        let writer = writer();
        writer.install_transaction_finalizer(drain).unwrap();
        {
            let guard = writer.write();
            guard
                .execute("INSERT INTO source VALUES(1,'raw bypass')", [])
                .unwrap();
        }
        assert_eq!(count(&writer, "delta"), 1);
        assert_eq!(count(&writer, "output"), 0);
    }

    fn tx_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn scope_schema(writer: &WriterConnection) {
        writer.write().execute_batch(
            "CREATE TABLE managed_scope(active INTEGER NOT NULL,gap INTEGER NOT NULL,fenced INTEGER NOT NULL);
             INSERT INTO managed_scope VALUES(0,0,0);
             DROP TRIGGER source_insert;
             CREATE TRIGGER source_insert AFTER INSERT ON source BEGIN
               INSERT INTO delta(id,new) VALUES(NEW.id,NEW.value);
               UPDATE managed_scope SET gap=1 WHERE active=0;
             END;",
        ).unwrap();
    }

    fn prepare_scope(tx: &Transaction<'_>) -> Result<()> {
        tx.execute(
            "UPDATE managed_scope SET active=1,fenced=MAX(fenced,gap)",
            [],
        )?;
        Ok(())
    }

    fn finish_scope(tx: &Transaction<'_>) -> Result<()> {
        let gap: bool = tx.query_row("SELECT gap FROM managed_scope", [], |r| r.get(0))?;
        if !gap {
            drain(tx)?;
        }
        tx.execute("UPDATE managed_scope SET active=0", [])?;
        Ok(())
    }

    #[test]
    fn prepare_is_inside_lent_transaction_before_free_source_helper() {
        let writer = writer();
        scope_schema(&writer);
        writer
            .install_transaction_hooks(prepare_scope, finish_scope)
            .unwrap();
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            assert_eq!(
                tx.query_row("SELECT active FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            free_write(&tx, 1, "managed").unwrap();
            tx.commit().unwrap();
            assert_eq!(
                guard
                    .query_row("SELECT active FROM managed_scope", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(count(&writer, "output"), 1);
        assert_eq!(count(&writer, "delta"), 0);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT gap FROM managed_scope", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn paired_hooks_do_not_relabel_prior_raw_commits_as_current_transaction_capture() {
        let writer = writer();
        scope_schema(&writer);
        writer
            .install_transaction_hooks(prepare_scope, finish_scope)
            .unwrap();
        writer
            .batched(|tx| free_write(tx, 1, "certified transaction"))
            .unwrap()
            .unwrap();
        {
            let guard = writer.write();
            guard
                .execute("INSERT INTO source VALUES(2,'prior raw commit')", [])
                .unwrap();
            assert_eq!(
                guard
                    .query_row("SELECT active FROM managed_scope", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert_eq!(
                guard
                    .query_row("SELECT gap FROM managed_scope", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        // The adapter fences rather than adopting prior committed capture. Later valid source
        // input still commits, but neither it nor the historical raw capture becomes output.
        writer
            .batched(|tx| free_write(tx, 3, "admitted while fenced"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "source"), 3);
        assert_eq!(count(&writer, "output"), 1);
        assert_eq!(count(&writer, "delta"), 2);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT fenced FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            writer
                .write()
                .query_row("SELECT active FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn queued_prepare_runs_once_before_every_job_and_finalizer_sees_surviving_jobs() {
        let writer = writer();
        scope_schema(&writer);
        let prepares = Arc::new(AtomicU64::new(0));
        let prepared = prepares.clone();
        writer
            .install_transaction_hooks(
                move |tx| {
                    prepared.fetch_add(1, Ordering::Relaxed);
                    prepare_scope(tx)
                },
                finish_scope,
            )
            .unwrap();
        let guard = writer.write();
        let mut answers = Vec::new();
        for (id, success) in [(1, true), (2, false), (3, true)] {
            let (done, answer) = mpsc::sync_channel(1);
            writer.send(WriterJob::Batched {
                run: Box::new(move |tx| {
                    assert_eq!(
                        tx.query_row("SELECT active FROM managed_scope", [], |r| r
                            .get::<_, i64>(0))
                            .unwrap(),
                        1
                    );
                    free_write(tx, id, "inside prepared batch").unwrap();
                    success
                }),
                profile: None,
                wait: None,
                done,
            });
            answers.push(answer);
        }
        drop(guard);
        for answer in answers {
            assert!(answer.recv().unwrap().is_ok());
        }
        assert_eq!(prepares.load(Ordering::Relaxed), 1);
        assert_eq!(count(&writer, "output"), 2);
        assert_eq!(count(&writer, "delta"), 0);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT active FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn prepare_failure_rolls_back_prepare_sql_and_never_runs_source_or_finalize() {
        let writer = writer();
        scope_schema(&writer);
        let finalized = Arc::new(AtomicU64::new(0));
        let called = finalized.clone();
        writer
            .install_transaction_hooks(
                |tx| {
                    prepare_scope(tx)?;
                    tx.execute("INSERT INTO absent_prepare_table VALUES(1)", [])?;
                    Ok(())
                },
                move |_| {
                    called.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            )
            .unwrap();
        let ran = Arc::new(AtomicU64::new(0));
        let source = ran.clone();
        assert!(
            writer
                .batched(move |tx| {
                    source.fetch_add(1, Ordering::Relaxed);
                    free_write(tx, 1, "must not run")
                })
                .is_err()
        );
        assert_eq!(ran.load(Ordering::Relaxed), 0);
        assert_eq!(finalized.load(Ordering::Relaxed), 0);
        let mut guard = writer.write();
        let error = match guard.transaction() {
            Ok(_) => panic!("prepare should fail"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        assert_eq!(
            guard
                .query_row("SELECT active FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(tx_count(&guard, "source"), 0);
    }

    #[test]
    fn prepare_panic_rolls_back_and_writer_can_prepare_the_next_batch() {
        let writer = writer();
        scope_schema(&writer);
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_hooks(
                move |tx| {
                    prepare_scope(tx)?;
                    if called.fetch_add(1, Ordering::Relaxed) == 0 {
                        panic!("prepare fixture panic");
                    }
                    Ok(())
                },
                finish_scope,
            )
            .unwrap();
        assert!(
            writer
                .batched(|tx| free_write(tx, 1, "failed prepare"))
                .unwrap_err()
                .contains("prepare panicked")
        );
        assert_eq!(
            writer
                .write()
                .query_row("SELECT active FROM managed_scope", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        writer
            .batched(|tx| free_write(tx, 2, "next batch"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "output"), 1);
    }

    use rusqlite::OptionalExtension;

    #[test]
    fn no_hook_lent_and_queued_writes_keep_ordinary_commit_and_rollback() {
        let writer = writer();
        writer
            .batched(|tx| free_write(tx, 1, "queued"))
            .unwrap()
            .unwrap();
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 2, "lent").unwrap();
            tx.commit().unwrap();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 3, "rolled back").unwrap();
            tx.rollback().unwrap();
        }
        assert_eq!(count(&writer, "source"), 2);
        assert_eq!(count(&writer, "delta"), 2);
        assert_eq!(count(&writer, "output"), 0);
    }

    #[test]
    fn batch_finalizer_error_keeps_context_chain() {
        let writer = writer();
        writer
            .install_transaction_finalizer(|_| {
                Err(anyhow::anyhow!("original cause").context("adapter context"))
            })
            .unwrap();
        let error = writer
            .batched(|tx| free_write(tx, 1, "no commit"))
            .unwrap_err();
        assert!(error.contains("adapter context"));
        assert!(error.contains("original cause"));
        assert_eq!(count(&writer, "source"), 0);
    }

    #[test]
    fn lent_finalizer_panic_rolls_back_and_releases_writer() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |tx| {
                drain(tx)?;
                if called.fetch_add(1, Ordering::Relaxed) == 0 {
                    panic!("lent fixture");
                }
                Ok(())
            })
            .unwrap();
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 1, "failed").unwrap();
            assert!(
                tx.commit()
                    .unwrap_err()
                    .to_string()
                    .contains("finalizer panicked")
            );
        }
        assert_eq!(count(&writer, "source"), 0);
        assert_eq!(count(&writer, "output"), 0);
        writer
            .batched(|tx| free_write(tx, 2, "next"))
            .unwrap()
            .unwrap();
        assert_eq!(count(&writer, "output"), 1);
    }

    #[test]
    fn raw_sql_commit_is_a_contract_violation_not_a_reversible_managed_failure() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        writer
            .install_transaction_finalizer(move |_| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            free_write(&tx, 1, "raw committed").unwrap();
            tx.execute_batch("COMMIT").unwrap();
            let error = tx.commit().unwrap_err();
            assert!(error.downcast_ref::<rusqlite::Error>().is_none());
            assert!(error.to_string().contains("transaction"));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(count(&writer, "source"), 1);
        assert_eq!(count(&writer, "output"), 0);
    }
}
