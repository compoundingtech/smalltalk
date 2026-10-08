//! Opt-in transaction budgets for an explicitly owned busy/progress policy.
//!
//! Existing constructors retain their handlers and have unknown ownership. Budgeting refuses
//! those writers before replacing any hook. SQLite offers no safe handler introspection: an
//! owner using raw Connection setters must invalidate this declaration BEFORE doing so.
//! This is a managed transaction seam, not a scheduler, a repair certificate or a hard wall
//! cap on filesystem work, rollback, Rust callbacks or the later writer-return observers.

use super::*;
use std::panic::{AssertUnwindSafe, RefUnwindSafe, catch_unwind, resume_unwind};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub enum OwnedBusyPolicy {
    Timeout(Duration),
    Callback(fn(i32) -> bool),
}

#[derive(Clone)]
pub struct OwnedProgressPolicy {
    pub every_vm_steps: i32,
    pub callback: Arc<dyn Fn() -> bool + Send + Sync + RefUnwindSafe>,
}

#[derive(Clone)]
pub struct OwnedHandlerPolicy {
    pub busy: OwnedBusyPolicy,
    pub progress: Option<OwnedProgressPolicy>,
}

impl OwnedHandlerPolicy {
    pub(super) fn install(&self, connection: &Connection) -> Result<()> {
        // Restore progress even when restoring busy fails. A budget's expired callback
        // must never leak into the next ordinary writer operation.
        match &self.progress {
            Some(progress) => {
                anyhow::ensure!(
                    progress.every_vm_steps > 0,
                    "owner progress interval must be positive"
                );
                let callback = progress.callback.clone();
                connection.progress_handler(progress.every_vm_steps, Some(move || callback()));
            }
            None => connection.progress_handler(0, None::<fn() -> bool>),
        }
        match self.busy {
            OwnedBusyPolicy::Timeout(timeout) => connection.busy_timeout(timeout)?,
            OwnedBusyPolicy::Callback(callback) => connection.busy_handler(Some(callback))?,
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WriterBudget {
    /// SQLite progress callbacks account approximately this many VM instructions. Actual
    /// statement VM counters should be retained separately by deterministic controls.
    pub vm_steps: u64,
    pub elapsed: Duration,
    pub callback_interval: i32,
}

#[derive(Clone, Debug)]
pub struct BudgetWork {
    pub progress_vm_steps: u64,
    pub elapsed: Duration,
    pub interrupted: bool,
}

/// Success means the managed COMMIT succeeded. Expiry after COMMIT is recorded rather than
/// retroactively cancelling a durable write. A failed handler restore invalidates ownership;
/// it does not turn an accepted write into a reported rejection.
pub struct BudgetCommit<T> {
    pub value: T,
    pub work: BudgetWork,
    pub restore_error: Option<String>,
    pub writer_reusable: bool,
}

enum Clock {
    Live(Instant),
    #[cfg(test)]
    Controlled(Arc<AtomicU64>),
}

impl Clock {
    fn elapsed(&self) -> Duration {
        match self {
            Self::Live(started) => started.elapsed(),
            #[cfg(test)]
            Self::Controlled(nanos) => Duration::from_nanos(nanos.load(Ordering::Relaxed)),
        }
    }
}

struct Running {
    instructions: AtomicU64,
    interrupted: AtomicBool,
    started: Clock,
    callback_panic: Mutex<Option<String>>,
}

fn refusal(message: &str) -> anyhow::Error {
    crate::error::Error::new("writer-budget-unsupported", message).into()
}

fn gcd(mut a: i32, mut b: i32) -> i32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl WriterConnection {
    /// The caller explicitly owns these exact handlers at connection handoff. This installs
    /// its declared policy once; it does not infer or capture an unknown preinstalled handler.
    /// Any subsequent raw busy/progress setter requires invalidate_handler_policy first.
    pub fn new_with_handler_policy(
        connection: Connection,
        committed_index: Arc<AtomicU64>,
        policy: OwnedHandlerPolicy,
    ) -> Result<Self> {
        Self::new_inner(connection, committed_index, None, Some(policy))
    }

    pub fn invalidate_handler_policy(&self) {
        self.handler_policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

impl WriterGuard<'_> {
    pub fn invalidate_handler_policy(&self) {
        self.handler_policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// A single BEGIN IMMEDIATE attempt with no busy wait, followed by managed prepare,
    /// caller work, finalization and COMMIT inside the same progress/deadline scope. The
    /// closure must not alter handlers or perform raw transaction control. A body/finalizer
    /// error or panic rolls back; the original owner callbacks are restored before return.
    ///
    /// The queued lend, rollback and subsequent guard-return observers are not bounded by
    /// this transaction API. Callers must drop the guard before acknowledging a write.
    pub fn budgeted_transaction<T>(
        &mut self,
        budget: WriterBudget,
        work: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<BudgetCommit<T>> {
        self.budgeted_transaction_inner(budget, work, Clock::Live(Instant::now()))
    }

    fn budgeted_transaction_inner<T>(
        &mut self,
        budget: WriterBudget,
        work: impl FnOnce(&Transaction<'_>) -> Result<T>,
        clock: Clock,
    ) -> Result<BudgetCommit<T>> {
        let policy = self
            .handler_policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| refusal("busy/progress handler ownership is unknown"))?;
        if !self.is_autocommit() {
            return Err(refusal("budget requires a writer with no open transaction"));
        }
        if budget.vm_steps == 0 || budget.elapsed.is_zero() || budget.callback_interval <= 0 {
            return Err(refusal(
                "budget limits and callback interval must be positive",
            ));
        }
        let interval = policy
            .progress
            .as_ref()
            .map_or(budget.callback_interval, |owner| {
                gcd(budget.callback_interval, owner.every_vm_steps)
            });
        let running = Arc::new(Running {
            instructions: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
            started: clock,
            callback_panic: Mutex::new(None),
        });
        if let Err(error) = self.busy_timeout(Duration::ZERO) {
            if policy.install(self).is_err() {
                self.invalidate_handler_policy();
                drop(self.connection.take());
            }
            return Err(error.into());
        }
        let active = running.clone();
        let prior = policy.progress.clone();
        let mut owner_instructions = 0;
        self.progress_handler(
            interval,
            Some(move || {
                let steps = active
                    .instructions
                    .fetch_add(interval as u64, Ordering::Relaxed)
                    .saturating_add(interval as u64);
                let mut owner_stop = false;
                if let Some(owner) = &prior {
                    owner_instructions += interval;
                    if owner_instructions >= owner.every_vm_steps {
                        owner_instructions = 0;
                        owner_stop = match catch_unwind(AssertUnwindSafe(|| (owner.callback)())) {
                            Ok(stop) => stop,
                            Err(panic) => {
                                let message = panic
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or("non-text owner progress panic")
                                    .chars()
                                    .take(256)
                                    .collect::<String>();
                                active
                                    .callback_panic
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .get_or_insert(message);
                                true
                            }
                        };
                    }
                }
                let expired =
                    steps >= budget.vm_steps || active.started.elapsed() >= budget.elapsed;
                if expired {
                    active.interrupted.store(true, Ordering::Relaxed);
                }
                owner_stop || expired
            }),
        );
        let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<T> {
            let transaction =
                self.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let value = work(&transaction)?;
            transaction.commit_checked(|| {
                if running.started.elapsed() >= budget.elapsed
                    || running.instructions.load(Ordering::Relaxed) >= budget.vm_steps
                {
                    running.interrupted.store(true, Ordering::Relaxed);
                    return Err(crate::error::Error::new(
                        "writer-budget-exhausted",
                        "managed transaction exceeded its budget before COMMIT",
                    )
                    .into());
                }
                Ok(())
            })?;
            Ok(value)
        }));
        // Rust transaction drop normally rolled back already. Remove only OUR callback for
        // recovery cleanup so an expired budget cannot interrupt a necessary ROLLBACK.
        self.progress_handler(0, None::<fn() -> bool>);
        let rollback = if self.is_autocommit() {
            Ok(())
        } else {
            self.execute_batch("ROLLBACK")
        };
        let restored = policy.install(self);
        if restored.is_err() {
            self.invalidate_handler_policy();
            // Restoration cannot be proved: this connection must never be reused by
            // ordinary queued writes. A successful COMMIT still remains committed.
            drop(self.connection.take());
        }
        let recorded = BudgetWork {
            progress_vm_steps: running.instructions.load(Ordering::Relaxed),
            elapsed: running.started.elapsed(),
            interrupted: running.interrupted.load(Ordering::Relaxed),
        };
        if let Err(error) = rollback {
            // Never return a connection carrying an unresolved transaction to the queue.
            drop(self.connection.take());
            match outcome {
                Err(panic) => resume_unwind(panic),
                _ => {
                    return Err(
                        anyhow::Error::from(error).context("budget rollback failed; writer closed")
                    );
                }
            }
        }
        match outcome {
            Err(panic) => resume_unwind(panic),
            Ok(Err(error)) => {
                if let Some(cause) = running
                    .callback_panic
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                {
                    return Err(error.context(crate::error::Error::new(
                        "writer-handler-panicked",
                        format!("owner progress callback panicked: {cause}"),
                    )));
                }
                if recorded.interrupted {
                    return Err(error.context(crate::error::Error::new(
                        "writer-budget-exhausted",
                        "managed transaction interrupted before a successful COMMIT",
                    )));
                }
                Err(error)
            }
            Ok(Ok(value)) => Ok(BudgetCommit {
                value,
                work: recorded,
                writer_reusable: restored.is_ok(),
                restore_error: restored.err().map(|error| format!("{error:#}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ErrorCode;

    fn connection(path: Option<&Path>) -> Connection {
        let connection = match path {
            Some(path) => Connection::open(path).unwrap(),
            None => Connection::open_in_memory().unwrap(),
        };
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
            CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT);
            CREATE TABLE source(id INTEGER PRIMARY KEY,value TEXT);
            CREATE TABLE output(id INTEGER PRIMARY KEY,value TEXT);",
            )
            .unwrap();
        connection
    }

    fn policy(calls: Arc<AtomicU64>) -> OwnedHandlerPolicy {
        OwnedHandlerPolicy {
            busy: OwnedBusyPolicy::Timeout(Duration::from_millis(73)),
            progress: Some(OwnedProgressPolicy {
                every_vm_steps: 1,
                callback: Arc::new(move || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            }),
        }
    }

    fn budget() -> WriterBudget {
        WriterBudget {
            vm_steps: 10_000,
            elapsed: Duration::from_secs(10),
            callback_interval: 1,
        }
    }

    const HEAVY: &str = "WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT SUM(x) FROM n";

    #[test]
    fn unknown_handlers_refuse_before_replacing_any_callback() {
        let connection = connection(None);
        let calls = Arc::new(AtomicU64::new(0));
        let capture = calls.clone();
        connection.progress_handler(
            1,
            Some(move || {
                capture.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        connection.busy_timeout(Duration::from_millis(91)).unwrap();
        let writer = WriterConnection::new(connection, Arc::new(AtomicU64::new(0)));
        let mut guard = writer.write();
        let before = calls.load(Ordering::Relaxed);
        let ran = AtomicBool::new(false);
        let error = match guard.budgeted_transaction(budget(), |_| {
            ran.store(true, Ordering::Relaxed);
            Ok(())
        }) {
            Ok(_) => panic!("unknown ownership accepted"),
            Err(error) => error,
        };
        assert_eq!(crate::error::typed(error).code, "writer-budget-unsupported");
        assert!(!ran.load(Ordering::Relaxed));
        guard
            .query_row("SELECT COUNT(*) FROM source", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap();
        assert!(calls.load(Ordering::Relaxed) > before);
        assert_eq!(
            guard
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, u64>(0))
                .unwrap(),
            91
        );
    }

    #[test]
    fn budget_interrupts_before_commit_and_restores_owner_hooks() {
        let calls = Arc::new(AtomicU64::new(0));
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            policy(calls.clone()),
        )
        .unwrap();
        let mut guard = writer.write();
        let error = match guard.budgeted_transaction(budget(), |tx| {
            tx.execute("INSERT INTO source VALUES(1,'uncommitted')", [])?;
            tx.query_row(HEAVY, [], |row| row.get::<_, i64>(0))?;
            Ok(())
        }) {
            Ok(_) => panic!("unbounded SQL committed"),
            Err(error) => error,
        };
        assert_eq!(crate::error::typed(error).code, "writer-budget-exhausted");
        assert!(guard.is_autocommit());
        assert_eq!(
            guard
                .query_row("SELECT COUNT(*) FROM source", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        let before = calls.load(Ordering::Relaxed);
        guard
            .execute("INSERT INTO source VALUES(2,'ordinary')", [])
            .unwrap();
        assert!(
            calls.load(Ordering::Relaxed) > before,
            "the exact owner's callback is restored"
        );
        assert_eq!(
            guard
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, u64>(0))
                .unwrap(),
            73
        );
        drop(guard);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT value FROM source WHERE id=2", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "ordinary"
        );
    }

    #[test]
    fn budget_no_wait_begin_preserves_the_original_busy_callback() {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        fn owner_busy(_: i32) -> bool {
            CALLS.fetch_add(1, Ordering::Relaxed);
            false
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("writer.db");
        let writer = WriterConnection::new_with_handler_policy(
            connection(Some(&path)),
            Arc::new(AtomicU64::new(0)),
            OwnedHandlerPolicy {
                busy: OwnedBusyPolicy::Callback(owner_busy),
                progress: None,
            },
        )
        .unwrap();
        let mut blocker = Connection::open(&path).unwrap();
        let lock = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut guard = writer.write();
        let before = CALLS.load(Ordering::Relaxed);
        let error = match guard.budgeted_transaction(budget(), |_| Ok(())) {
            Ok(_) => panic!("contended budget committed"),
            Err(error) => error,
        };
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy)
        );
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            before,
            "no busy callback/retry runs for repair BEGIN"
        );
        assert!(
            guard
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .is_err()
        );
        assert!(
            CALLS.load(Ordering::Relaxed) > before,
            "ordinary writes retain the original function callback"
        );
        lock.rollback().unwrap();
        assert_eq!(
            guard
                .budgeted_transaction(budget(), |tx| {
                    tx.execute("INSERT INTO source VALUES(1,'accepted')", [])?;
                    Ok(7)
                })
                .unwrap()
                .value,
            7
        );
    }

    #[test]
    fn budget_scope_covers_prepare_and_finalizer_sql() {
        for phase in ["prepare", "finalizer"] {
            let calls = Arc::new(AtomicU64::new(0));
            let writer = WriterConnection::new_with_handler_policy(
                connection(None),
                Arc::new(AtomicU64::new(0)),
                policy(calls),
            )
            .unwrap();
            writer
                .install_transaction_hooks(
                    move |tx| {
                        if phase == "prepare" {
                            tx.query_row(HEAVY, [], |row| row.get::<_, i64>(0))?;
                        }
                        Ok(())
                    },
                    move |tx| {
                        tx.execute("INSERT INTO output VALUES(1,'derived')", [])?;
                        if phase == "finalizer" {
                            tx.query_row(HEAVY, [], |row| row.get::<_, i64>(0))?;
                        }
                        Ok(())
                    },
                )
                .unwrap();
            let mut guard = writer.write();
            let ran = AtomicBool::new(false);
            let error = match guard.budgeted_transaction(budget(), |tx| {
                ran.store(true, Ordering::Relaxed);
                tx.execute("INSERT INTO source VALUES(1,'source')", [])?;
                Ok(())
            }) {
                Ok(_) => panic!("unbounded {phase} accepted"),
                Err(error) => error,
            };
            assert_eq!(crate::error::typed(error).code, "writer-budget-exhausted");
            assert_eq!(ran.load(Ordering::Relaxed), phase == "finalizer");
            assert!(guard.is_autocommit());
            assert_eq!(
                guard
                    .query_row(
                        "SELECT (SELECT COUNT(*) FROM source)+(SELECT COUNT(*) FROM output)",
                        [],
                        |row| row.get::<_, u64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn body_panic_rolls_back_and_does_not_poison_the_next_writer() {
        let calls = Arc::new(AtomicU64::new(0));
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            policy(calls.clone()),
        )
        .unwrap();
        let mut guard = writer.write();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _: Result<BudgetCommit<()>> = guard.budgeted_transaction(budget(), |tx| {
                tx.execute("INSERT INTO source VALUES(1,'rolled back')", [])?;
                panic!("intentional repair body panic")
            });
        }));
        assert!(panic.is_err());
        assert!(guard.is_autocommit());
        assert_eq!(
            guard
                .query_row("SELECT COUNT(*) FROM source", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        let before = calls.load(Ordering::Relaxed);
        guard
            .execute("INSERT INTO source VALUES(2,'after unwind')", [])
            .unwrap();
        assert!(calls.load(Ordering::Relaxed) > before);
        drop(guard);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT COUNT(*) FROM source", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn invalidated_owner_refuses_without_replacing_the_new_raw_handler() {
        let calls = Arc::new(AtomicU64::new(0));
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            policy(calls),
        )
        .unwrap();
        let mut guard = writer.write();
        guard.invalidate_handler_policy();
        let replacement = Arc::new(AtomicU64::new(0));
        let capture = replacement.clone();
        guard.progress_handler(
            1,
            Some(move || {
                capture.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let error = match guard.budgeted_transaction(budget(), |_| Ok(())) {
            Ok(_) => panic!("invalidated owner accepted"),
            Err(error) => error,
        };
        assert_eq!(crate::error::typed(error).code, "writer-budget-unsupported");
        let before = replacement.load(Ordering::Relaxed);
        guard
            .execute("INSERT INTO source VALUES(1,'retained raw hook')", [])
            .unwrap();
        assert!(replacement.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn prior_progress_interruption_is_not_reclassified_as_budget_exhaustion() {
        let stop = Arc::new(AtomicBool::new(false));
        let capture = stop.clone();
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            OwnedHandlerPolicy {
                busy: OwnedBusyPolicy::Timeout(Duration::ZERO),
                progress: Some(OwnedProgressPolicy {
                    every_vm_steps: 1,
                    callback: Arc::new(move || capture.load(Ordering::Relaxed)),
                }),
            },
        )
        .unwrap();
        let mut guard = writer.write();
        stop.store(true, Ordering::Relaxed);
        let error = match guard.budgeted_transaction(budget(), |_| Ok(())) {
            Ok(_) => panic!("owner stop ignored"),
            Err(error) => error,
        };
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(ErrorCode::OperationInterrupted)
        );
        stop.store(false, Ordering::Relaxed);
        assert!(guard.is_autocommit());
        assert_eq!(
            guard
                .budgeted_transaction(budget(), |_| Ok(9))
                .unwrap()
                .value,
            9
        );
    }

    #[test]
    fn owner_callback_panic_retains_cause_and_rolls_back_before_restoration() {
        let stop = Arc::new(AtomicBool::new(false));
        let capture = stop.clone();
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            OwnedHandlerPolicy {
                busy: OwnedBusyPolicy::Timeout(Duration::ZERO),
                progress: Some(OwnedProgressPolicy {
                    every_vm_steps: 1,
                    callback: Arc::new(move || {
                        assert!(
                            !capture.load(Ordering::Relaxed),
                            "intentional owner callback cause"
                        );
                        false
                    }),
                }),
            },
        )
        .unwrap();
        let mut guard = writer.write();
        stop.store(true, Ordering::Relaxed);
        let error = match guard.budgeted_transaction(budget(), |_| Ok(())) {
            Ok(_) => panic!("owner callback panic ignored"),
            Err(error) => error,
        };
        let typed = crate::error::typed(error);
        assert_eq!(typed.code, "writer-handler-panicked");
        assert!(typed.message.contains("intentional owner callback cause"));
        stop.store(false, Ordering::Relaxed);
        assert!(guard.is_autocommit());
        assert_eq!(
            guard
                .budgeted_transaction(budget(), |_| Ok(3))
                .unwrap()
                .value,
            3
        );
    }

    #[test]
    fn invalid_and_nested_scopes_do_not_change_the_owner_policy() {
        let calls = Arc::new(AtomicU64::new(0));
        let writer = WriterConnection::new_with_handler_policy(
            connection(None),
            Arc::new(AtomicU64::new(0)),
            policy(calls.clone()),
        )
        .unwrap();
        let mut guard = writer.write();
        for invalid in [
            WriterBudget {
                vm_steps: 0,
                ..budget()
            },
            WriterBudget {
                elapsed: Duration::ZERO,
                ..budget()
            },
            WriterBudget {
                callback_interval: 0,
                ..budget()
            },
        ] {
            let error = match guard.budgeted_transaction(invalid, |_| Ok(())) {
                Ok(_) => panic!("invalid scope accepted"),
                Err(error) => error,
            };
            assert_eq!(crate::error::typed(error).code, "writer-budget-unsupported");
        }
        guard.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = match guard.budgeted_transaction(budget(), |_| Ok(())) {
            Ok(_) => panic!("nested scope accepted"),
            Err(error) => error,
        };
        assert_eq!(crate::error::typed(error).code, "writer-budget-unsupported");
        assert!(
            !guard.is_autocommit(),
            "refusal must not finish its caller's transaction"
        );
        guard.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            guard
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, u64>(0))
                .unwrap(),
            73
        );
        let before = calls.load(Ordering::Relaxed);
        guard
            .execute("INSERT INTO source VALUES(1,'still owned')", [])
            .unwrap();
        assert!(calls.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn known_expired_rust_work_and_finalizer_refuse_before_commit() {
        for expire_finalizer in [false, true] {
            let clock = Arc::new(AtomicU64::new(0));
            let writer = WriterConnection::new_with_handler_policy(
                connection(None),
                Arc::new(AtomicU64::new(0)),
                OwnedHandlerPolicy {
                    busy: OwnedBusyPolicy::Timeout(Duration::ZERO),
                    progress: None,
                },
            )
            .unwrap();
            if expire_finalizer {
                let finalizer_clock = clock.clone();
                writer
                    .install_transaction_hooks(
                        |_| Ok(()),
                        move |_| {
                            finalizer_clock.store(5_000_000, Ordering::Relaxed);
                            Ok(())
                        },
                    )
                    .unwrap();
            }
            let mut guard = writer.write();
            let error = match guard.budgeted_transaction_inner(
                WriterBudget {
                    vm_steps: 1_000_000,
                    elapsed: Duration::from_millis(1),
                    callback_interval: 1_000_000,
                },
                |tx| {
                    tx.execute("INSERT INTO source VALUES(1,'uncommitted')", [])?;
                    if !expire_finalizer {
                        clock.store(5_000_000, Ordering::Relaxed);
                    }
                    Ok(())
                },
                Clock::Controlled(clock.clone()),
            ) {
                Ok(_) => panic!("known expired Rust work committed"),
                Err(error) => error,
            };
            assert_eq!(crate::error::typed(error).code, "writer-budget-exhausted");
            assert!(guard.is_autocommit());
            assert_eq!(
                guard
                    .query_row("SELECT COUNT(*) FROM source", [], |row| row
                        .get::<_, u64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn success_after_actual_commit_is_retained_not_retroactively_cancelled() {
        let clock = Arc::new(AtomicU64::new(0));
        let advance = clock.clone();
        let connection = connection(None);
        // This real SQLite hook runs after the explicit pre-COMMIT check. The controlled
        // clock avoids shared-runner timing as an oracle; the SQL commit/row proof is real.
        connection.commit_hook(Some(move || {
            advance.store(5_000_000, Ordering::Relaxed);
            false
        }));
        let writer = WriterConnection::new_with_handler_policy(
            connection,
            Arc::new(AtomicU64::new(0)),
            OwnedHandlerPolicy {
                busy: OwnedBusyPolicy::Timeout(Duration::ZERO),
                progress: None,
            },
        )
        .unwrap();
        let mut guard = writer.write();
        let committed = guard
            .budgeted_transaction_inner(
                WriterBudget {
                    vm_steps: 1_000_000,
                    elapsed: Duration::from_millis(1),
                    callback_interval: 1_000_000,
                },
                |tx| {
                    tx.execute("INSERT INTO source VALUES(1,'durable')", [])?;
                    Ok("accepted")
                },
                Clock::Controlled(clock),
            )
            .unwrap();
        assert_eq!(committed.value, "accepted");
        assert_eq!(committed.work.elapsed, Duration::from_millis(5));
        assert!(committed.writer_reusable);
        drop(guard);
        assert_eq!(
            writer
                .write()
                .query_row("SELECT value FROM source WHERE id=1", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "durable"
        );
    }
}
