//! Authored native controls for the private contract. These have NOT been compiled or run.
//! Fixture mutation tokens are maintained by fixture-only triggers, not a proposed Store schema.
use super::*;
use crate::sqlite::{WriterConnection, WriterJob};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

struct Fixture {
    writer: Arc<WriterConnection>,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("source.sqlite3")).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT);
             CREATE TABLE authority(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO authority VALUES(1,'allowed');
             CREATE TABLE tombstones(id INTEGER PRIMARY KEY);
             CREATE TABLE epochs(id TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO epochs VALUES('authority',0),('tombstones',0);
             CREATE TRIGGER authority_insert AFTER INSERT ON authority BEGIN
               UPDATE epochs SET value=value+1 WHERE id='authority'; END;
             CREATE TRIGGER authority_update AFTER UPDATE ON authority BEGIN
               UPDATE epochs SET value=value+1 WHERE id='authority'; END;
             CREATE TRIGGER authority_delete AFTER DELETE ON authority BEGIN
               UPDATE epochs SET value=value+1 WHERE id='authority'; END;
             CREATE TRIGGER tombstones_insert AFTER INSERT ON tombstones BEGIN
               UPDATE epochs SET value=value+1 WHERE id='tombstones'; END;
             CREATE TRIGGER tombstones_delete AFTER DELETE ON tombstones BEGIN
               UPDATE epochs SET value=value+1 WHERE id='tombstones'; END;
             CREATE TABLE output(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE parents(id INTEGER PRIMARY KEY);
             CREATE TABLE deferred_child(id INTEGER REFERENCES parents(id)
               DEFERRABLE INITIALLY DEFERRED);",
            )
            .unwrap();
        let writer = Arc::new(WriterConnection::new(
            connection,
            Arc::new(AtomicU64::new(0)),
        ));
        Self { directory, writer }
    }

    fn reader(&self) -> Connection {
        Connection::open(self.directory.path().join("source.sqlite3")).unwrap()
    }

    /// Capture on an independent consistent reader and finish that snapshot before publication.
    fn prepare(&self) -> PreparedPublication<&'static str> {
        let mut reader = self.reader();
        let snapshot = reader.transaction().unwrap();
        let reads = observations(&snapshot);
        snapshot.commit().unwrap();
        PreparedPublication {
            payload: "prepared result",
            reads,
        }
    }

    fn output_count(&self) -> u64 {
        self.reader()
            .query_row("SELECT count(*) FROM output", [], |r| r.get(0))
            .unwrap()
    }
}

fn key(name: &str) -> PredicateKey {
    PredicateKey {
        predicate: name.to_owned(),
        key: 1_i64.to_be_bytes().to_vec(),
    }
}

fn identity() -> SourceIdentity {
    SourceIdentity {
        lifetime: [1; 32],
        schema: [2; 32],
        rules: [3; 32],
        producer: [4; 32],
    }
}

fn token(connection: &Connection, table: &str) -> MutationToken {
    let epoch: u64 = connection
        .query_row("SELECT value FROM epochs WHERE id=?1", [table], |r| {
            r.get(0)
        })
        .unwrap();
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&epoch.to_be_bytes());
    MutationToken(bytes)
}

fn observations(connection: &Connection) -> ReadSet {
    let authority: Option<String> = connection
        .query_row("SELECT value FROM authority WHERE id=1", [], |r| r.get(0))
        .optional()
        .unwrap();
    let positive = match authority {
        Some(value) => Fact::Present {
            mutation: token(connection, "authority"),
            value: Sha256::digest(value.as_bytes()).into(),
        },
        None => Fact::Absent {
            mutation: token(connection, "authority"),
        },
    };
    let present: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE id=1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let negative = if present {
        Fact::Present {
            mutation: token(connection, "tombstones"),
            value: [1; 32],
        }
    } else {
        Fact::Absent {
            mutation: token(connection, "tombstones"),
        }
    };
    ReadSet::capture(
        identity(),
        BTreeSet::from([key("authority"), key("tombstones")]),
        [(key("authority"), positive), (key("tombstones"), negative)],
    )
    .unwrap()
}

fn publish_fixture(fixture: &Fixture, prepared: PreparedPublication<&str>) -> CommitKnowledge {
    // This fixed fixture command is not a generic production SQL callback/cost certificate.
    let mut guard = fixture.writer.write();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    if let Err(error) = prepared.reads.validate(&observations(&tx)) {
        return tx.reject_before_commit(anyhow::anyhow!("stale before apply: {error:?}"));
    }
    if let Err(error) = tx.execute("INSERT INTO output VALUES(1,?1)", [prepared.payload]) {
        return tx.reject_before_commit(error.into());
    }
    let outcome = tx.commit_validated(|tx| {
        prepared
            .reads
            .validate(&observations(tx))
            .map_err(|error| anyhow::anyhow!("stale after finalizers: {error:?}"))
    });
    drop(guard); // Native authority callbacks still run here. No model return proof minted.
    outcome
}

fn assert_rolled_back(outcome: CommitKnowledge) -> String {
    assert_eq!(
        outcome.resolution(),
        ResolutionKnowledge::CurrentTransactionRolledBack
    );
    match outcome {
        CommitKnowledge::CommitNotAttempted {
            cause,
            rollback: RollbackResolution::CurrentTransactionRolledBack,
        } => cause.to_string(),
        other => panic!("expected explicit current-transaction rollback, got {other:?}"),
    }
}

#[test]
fn matching_fixture_publishes_but_does_not_mint_native_completion() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    assert_eq!(
        prepared.certification(),
        Err(UnprovidedContract::TransactionalAllMutatorEvidence)
    );
    let outcome =
        UncertifiedCompletion::after_validated_commit(publish_fixture(&fixture, prepared));
    assert_eq!(fixture.output_count(), 1);
    assert!(matches!(&outcome.commit, CommitKnowledge::Committed));
    assert_eq!(
        outcome.commit.acknowledgement(),
        Err(CompletionGap::AuthorityUnobserved)
    );
    assert!(
        outcome
            .missing
            .contains(&NativeObligation::PhysicalQueueReturn)
    );
}

#[test]
fn same_index_authority_revocation_refuses_without_publishing() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .writer
        .write()
        .execute("UPDATE authority SET value='revoked' WHERE id=1", [])
        .unwrap();
    assert_eq!(fixture.writer.committed_index.load(Ordering::Acquire), 0);
    let cause = assert_rolled_back(publish_fixture(&fixture, prepared));
    assert!(cause.contains("stale before apply"));
    assert_eq!(fixture.output_count(), 0);
}

#[test]
fn negative_insert_and_delete_reinsert_aba_refuse_at_unchanged_claim_index() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .writer
        .write()
        .execute("INSERT INTO tombstones VALUES(1)", [])
        .unwrap();
    assert_rolled_back(publish_fixture(&fixture, prepared));
    fixture
        .writer
        .write()
        .execute("DELETE FROM tombstones", [])
        .unwrap();
    let prepared = fixture.prepare();
    // Restore exactly the same visible authority value and key, but a different mutation.
    fixture
        .writer
        .write()
        .execute_batch(
            "DELETE FROM authority WHERE id=1; INSERT INTO authority VALUES(1,'allowed');",
        )
        .unwrap();
    assert_rolled_back(publish_fixture(&fixture, prepared));
    assert_eq!(fixture.output_count(), 0);
    assert_eq!(fixture.writer.committed_index.load(Ordering::Acquire), 0);
}

#[test]
fn stale_negative_aba_can_prepare_again_after_confirmed_fixture_rollback() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .writer
        .write()
        .execute_batch("INSERT INTO tombstones VALUES(1); DELETE FROM tombstones WHERE id=1;")
        .unwrap();
    // The visible negative predicate is unchanged; storage-issued fixture evidence changed.
    assert_rolled_back(publish_fixture(&fixture, prepared));
    assert_eq!(fixture.output_count(), 0);
    // Fresh preparation is outside the native loan, after actual rollback. This is the
    // closed fixture's normal conflict flow, not permission to resubmit an unknown command.
    let fresh = fixture.prepare();
    assert!(matches!(
        publish_fixture(&fixture, fresh),
        CommitKnowledge::Committed
    ));
    assert_eq!(fixture.output_count(), 1);
}

#[test]
fn finalizer_invalidation_rolls_back_its_own_change_and_prepared_output() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .writer
        .install_transaction_finalizer(|tx| {
            tx.execute("UPDATE authority SET value='revoked' WHERE id=1", [])?;
            Ok(())
        })
        .unwrap();
    let resolved = Arc::new(Mutex::new(Vec::<(String, u64)>::new()));
    let seen = resolved.clone();
    let observer = fixture.writer.observe_commits(move |connection| {
        let current = connection
            .query_row(
                "SELECT authority.value, epochs.value FROM authority, epochs
             WHERE authority.id=1 AND epochs.id='authority'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        seen.lock().unwrap().push(current);
    });
    let cause = assert_rolled_back(publish_fixture(&fixture, prepared));
    drop(observer);
    assert!(cause.contains("stale after finalizers"));
    assert_eq!(fixture.output_count(), 0);
    let value: String = fixture
        .reader()
        .query_row("SELECT value FROM authority WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "allowed");
    assert_eq!(*resolved.lock().unwrap(), vec![("allowed".to_owned(), 0)]);
    // Provisional source/evidence rolled back together, then the native observer read old
    // resolved authority. The committed-authority callback never ran on provisional rows.
}

#[test]
fn finalizer_failure_is_not_mistaken_for_a_commit_receipt() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .writer
        .install_transaction_finalizer(|tx| {
            tx.execute("INSERT INTO tombstones VALUES(1)", [])?;
            anyhow::bail!("fixture finalizer refused")
        })
        .unwrap();
    let outcome =
        UncertifiedCompletion::after_validated_commit(publish_fixture(&fixture, prepared));
    assert_eq!(
        outcome.commit.acknowledgement(),
        Err(CompletionGap::NoCommittedAuthorityReceipt)
    );
    assert_eq!(
        assert_rolled_back(outcome.commit),
        "fixture finalizer refused"
    );
    assert_eq!(fixture.output_count(), 0);
    let tombstones: u64 = fixture
        .reader()
        .query_row("SELECT count(*) FROM tombstones", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tombstones, 0);
}

#[test]
fn sqlite_commit_error_stays_unknown_instead_of_authorizing_retry() {
    let fixture = Fixture::new();
    let mut guard = fixture.writer.write();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("INSERT INTO output VALUES(1,'not durable')", [])
        .unwrap();
    tx.execute("INSERT INTO deferred_child VALUES(99)", [])
        .unwrap();
    let outcome = UncertifiedCompletion::after_validated_commit(tx.commit_validated(|_| Ok(())));
    drop(guard);
    assert!(matches!(
        &outcome.commit,
        CommitKnowledge::CommitOutcomeUnknown { .. }
    ));
    assert_eq!(
        outcome.commit.acknowledgement(),
        Err(CompletionGap::NoCommittedAuthorityReceipt)
    );
    assert_eq!(fixture.output_count(), 0);
    // This fixture's failed deferred-FK COMMIT rolled back. An error from SQLite COMMIT
    // itself still supplies no authoritative rollback/reapply receipt to the typed caller.
}

#[test]
fn rollback_failure_preserves_original_refusal_and_resolution_error() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    let fixture = Fixture::new();
    let mut guard = fixture.writer.write();
    guard.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    let tx = guard.transaction().unwrap();
    tx.execute("INSERT INTO output VALUES(1,'provisional')", [])
        .unwrap();
    let outcome = tx.reject_before_commit(anyhow::anyhow!("original stale refusal"));
    // Restore the hook and explicitly clean up before asserting: failed rollback and Drop
    // are not success receipts, and the unresolved connection must not return to the queue.
    guard.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let still_open = !guard.is_autocommit();
    guard.execute_batch("ROLLBACK").unwrap();
    drop(guard);
    assert!(still_open);
    assert_eq!(outcome.resolution(), ResolutionKnowledge::Unknown);
    match outcome {
        CommitKnowledge::CommitNotAttempted {
            cause,
            rollback: RollbackResolution::Failed(_),
        } => {
            assert_eq!(cause.to_string(), "original stale refusal");
        }
        other => panic!("expected refusal plus failed rollback, got {other:?}"),
    }
    assert_eq!(fixture.output_count(), 0);
}

#[test]
fn raw_commit_before_check_failure_is_not_a_rollback_receipt() {
    let fixture = Fixture::new();
    let mut guard = fixture.writer.write();
    let tx = guard.transaction().unwrap();
    tx.execute("INSERT INTO output VALUES(1,'already durable')", [])
        .unwrap();
    let outcome = tx.commit_validated(|tx| {
        tx.execute_batch("COMMIT")?;
        anyhow::bail!("raw committed before refusal")
    });
    drop(guard);
    assert!(matches!(
        &outcome,
        CommitKnowledge::CommitNotAttempted {
            rollback: RollbackResolution::TransactionAlreadyEnded,
            ..
        }
    ));
    assert_eq!(fixture.output_count(), 1);
    assert_eq!(outcome.resolution(), ResolutionKnowledge::Unknown);
    assert!(outcome.acknowledgement().is_err());
}

#[test]
fn rollback_of_a_raw_replacement_transaction_is_not_command_rebase_authority() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    let mut guard = fixture.writer.write();
    let tx = guard.transaction().unwrap();
    tx.execute("INSERT INTO output VALUES(1,'earlier raw commit')", [])
        .unwrap();
    let outcome = tx.commit_validated(|tx| {
        tx.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
        anyhow::bail!("replacement transaction refused")
    });
    drop(guard);
    assert_rolled_back(outcome); // Only the CURRENT transaction is known to have rolled back.
    assert_eq!(fixture.output_count(), 1);
    assert_eq!(
        prepared.certification(),
        Err(UnprovidedContract::TransactionalAllMutatorEvidence)
    );
    // Unsupported raw boundaries prevent whole-command certification. No automatic rebase
    // or resubmit capability is produced by CurrentTransactionRolledBack alone.
}

fn observer_gate_holds(batch: bool) {
    let fixture = Fixture::new();
    let (entered, observing) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let first = AtomicBool::new(true);
    let observer = fixture.writer.observe_commits(move |connection| {
        let count: u64 = connection
            .query_row("SELECT count(*) FROM output", [], |r| r.get(0))
            .unwrap();
        if count == 1 && first.swap(false, Ordering::AcqRel) {
            entered.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }
    });
    let writer = fixture.writer.clone();
    let (ack, acknowledged) = mpsc::sync_channel(1);
    let publishing = std::thread::spawn(move || {
        let result: anyhow::Result<()> = if batch {
            writer
                .batched(|tx| tx.execute("INSERT INTO output VALUES(1,'durable')", []))
                .map_err(anyhow::Error::msg)
                .and_then(|result| result.map(|_| ()).map_err(anyhow::Error::from))
        } else {
            let mut guard = writer.write();
            let tx = guard.transaction().unwrap();
            tx.execute("INSERT INTO output VALUES(1,'durable')", [])
                .unwrap();
            let committed = tx.commit();
            drop(guard); // The test stalls in the real guard-return observer.
            committed
        };
        ack.send(result).unwrap();
    });
    let started = observing.recv_timeout(Duration::from_secs(5));
    let before_ack = acknowledged.try_recv();
    let (lent, next_loan) = mpsc::sync_channel(1);
    let (give_back, returned) = mpsc::sync_channel(1);
    // This send has actually queued a native loan before the negative observation. There
    // is no "thread about to call write" signal. The 50ms negative is explicitly timing-
    // based, not a proof that every possible scheduler can never return early.
    fixture.writer.send(WriterJob::Lend {
        lent,
        returned,
        holder: crate::windows::holds::Holder::current(),
        enqueued: std::time::Instant::now(),
        origin: None,
    });
    let before_return = next_loan.recv_timeout(Duration::from_millis(50));
    // Release BEFORE unregistering: handle removal waits for an in-flight callback.
    release.send(()).unwrap();
    drop(observer);
    publishing.join().unwrap();
    // Return even an unexpectedly early loan before asserting, so the native queue closes.
    let blocked = matches!(&before_return, Err(mpsc::RecvTimeoutError::Timeout));
    let connection = match before_return {
        Ok(connection) => connection,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            next_loan.recv_timeout(Duration::from_secs(5)).unwrap()
        }
        Err(error) => panic!("native lend disconnected: {error}"),
    };
    give_back.send(connection).unwrap();
    assert!(started.is_ok());
    assert!(matches!(before_ack, Err(mpsc::TryRecvError::Empty)));
    assert!(
        blocked,
        "queued loan returned during the bounded observer gate"
    );
    acknowledged.recv().unwrap().unwrap();
    let outcome = UncertifiedCompletion::after_validated_commit(CommitKnowledge::Committed);
    assert_eq!(
        outcome.commit.acknowledgement(),
        Err(CompletionGap::AuthorityUnobserved)
    );
    assert!(
        outcome
            .missing
            .contains(&NativeObligation::PhysicalQueueReturn)
    );
}

#[test]
fn batch_ack_waits_for_authority_and_next_dispatch_requires_actual_connection() {
    observer_gate_holds(true);
}

#[test]
fn lent_ack_waits_for_guard_observer_and_next_loan_requires_actual_return() {
    observer_gate_holds(false);
}

#[cfg(target_os = "linux")]
#[test]
fn observer_panic_after_commit_does_not_turn_durable_output_into_rollback_or_ack() {
    const CHILD: &str = "SMALLCLAIMS_U0_OBSERVER_PANIC_CHILD";
    const WITNESS_PATH: &str = "SMALLCLAIMS_U0_OBSERVER_PANIC_WITNESS_PATH";
    const WITNESS_NONCE: &str = "SMALLCLAIMS_U0_OBSERVER_PANIC_WITNESS_NONCE";
    if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
        // Bound the ENTIRE child, including WriterConnection::drop/join, using reactive
        // pidfd readiness. No detached test thread can leave this test job stuck forever.
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        // Fresh parent-owned directory and nonce: exit 0 from an unmatched --exact filter
        // cannot satisfy this witness. Keep the directory alive until child kill/reap.
        let witness_directory = tempfile::tempdir().unwrap();
        let witness = witness_directory.path().join("completed");
        let nonce = format!("observer-panic-native-teardown:{}", uuid::Uuid::now_v7());
        assert!(!witness.exists());
        // Includes child startup, SQLite setup, native thread spawn and join. This is a
        // failure bound, not a performance claim; load-induced flakes must remain adverse.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut child = ChildGuard(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sqlite::publication_contract::tests::observer_panic_after_commit_does_not_turn_durable_output_into_rollback_or_ack", "--nocapture"])
            .env(CHILD, "1")
            .env(WITNESS_PATH, &witness)
            .env(WITNESS_NONCE, &nonce)
            .spawn().unwrap());
        // SAFETY: pidfd_open addresses the owned, unreaped child; a successful descriptor
        // is newly owned here and closed by OwnedFd. The child contains only native threads.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.0.id(), 0) };
        assert!(
            raw >= 0,
            "pidfd_open failed: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let exited = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break false;
            }
            // SAFETY: poll points to one initialized descriptor valid through the call.
            let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(
                result >= 0,
                "pidfd poll failed: {}",
                std::io::Error::last_os_error()
            );
            break result > 0 && poll.revents & libc::POLLIN != 0;
        };
        if !exited {
            child.0.kill().unwrap();
        }
        let status = child.0.wait().unwrap();
        assert!(
            exited,
            "observer panic fixture exceeded 15s including native teardown; child killed/reaped"
        );
        assert!(status.success(), "observer panic fixture failed: {status}");
        let completed = std::fs::read(&witness)
            .expect("child exited successfully without completing the selected native fixture");
        assert_eq!(
            completed,
            nonce.as_bytes(),
            "child completion witness mismatch"
        );
        return;
    }
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    let observer = fixture
        .writer
        .observe_commits(|_| panic!("fixture authority failure"));
    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        publish_fixture(&fixture, prepared)
    }));
    assert!(attempted.is_err());
    drop(observer);
    assert_eq!(fixture.output_count(), 1);
    // Unwinding drops the guard fields, including give_back. With no returned connection,
    // returned.recv sees channel disconnection and write_queue exits. The parent deadline
    // discriminates that expected termination from a hang; this control remains UNRUN.
    drop(fixture); // Include the writer thread's join inside the supervised child lifetime.
    // Write only AFTER this exact branch's assertions and native teardown. No other test
    // or a zero-test selection can mint the parent's fresh completion witness.
    use std::io::Write as _;
    let path = std::env::var_os(WITNESS_PATH).expect("child witness path missing");
    let nonce = std::env::var(WITNESS_NONCE).expect("child witness nonce missing");
    let mut completed = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    completed.write_all(nonce.as_bytes()).unwrap();
    // Caller saw no successful return. Recovery must inspect authoritative committed state;
    // no generic Result-to-rollback or idempotent reapply protocol is invented here.
}

#[test]
fn raw_capture_gaps_do_not_become_certified_by_matching_owned_facts() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    // Explicit bypass defeats fixture capture: same bytes/key/index and restored epoch.
    // A comparer cannot discover omitted writes. There is deliberately no certificate/ACK.
    fixture
        .writer
        .write()
        .execute_batch(
            "DROP TRIGGER authority_delete; DROP TRIGGER authority_insert;
         DELETE FROM authority; INSERT INTO authority VALUES(1,'allowed');",
        )
        .unwrap();
    let reader = fixture.reader();
    assert!(prepared.reads.validate(&observations(&reader)).is_ok());
    assert_eq!(
        prepared.certification(),
        Err(UnprovidedContract::TransactionalAllMutatorEvidence)
    );
    let outcome = UncertifiedCompletion::after_validated_commit(CommitKnowledge::Committed);
    assert!(outcome.commit.acknowledgement().is_err());
    assert_eq!(outcome.missing.len(), 3);
    // The fixture identity wasn't maintained across DDL: source-coverage certification would
    // have to reject this adapter. No generic bool claims that it was complete.
}

#[test]
fn missing_negative_mutation_or_predicate_is_refused_before_submission() {
    let required = BTreeSet::from([key("tombstones")]);
    assert!(matches!(
        Fact::absent(&key("tombstones"), None),
        Err(Refusal::MissingMutation(_))
    ));
    assert!(matches!(
        ReadSet::capture(identity(), required, []),
        Err(Refusal::MissingPredicate(_))
    ));
}
