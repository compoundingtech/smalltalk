//! Private callback error preservation. No library or production contract is changed.
use super::*;
use rusqlite::ErrorCode;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    Fence,
    Infrastructure,
    Unclassified,
}
pub(super) fn classify(error: &anyhow::Error) -> Disposition {
    let Some(sql) = error.downcast_ref::<rusqlite::Error>() else {
        return Disposition::Fence;
    };
    match sql.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => Disposition::Fence,
        Some(
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::OutOfMemory
            | ErrorCode::ReadOnly
            | ErrorCode::OperationInterrupted
            | ErrorCode::SystemIoFailure
            | ErrorCode::DatabaseCorrupt
            | ErrorCode::DiskFull
            | ErrorCode::CannotOpen
            | ErrorCode::NotADatabase,
        ) => Disposition::Infrastructure,
        _ => Disposition::Unclassified,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Probe,
    Witness,
    Reentry,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Observed {
    pub(super) primary: Option<ErrorCode>,
    pub(super) extended: Option<i32>,
    pub(super) disposition: Disposition,
    pub(super) origin: Origin,
}
#[derive(Default)]
struct State {
    error: Option<anyhow::Error>,
    observed: Option<Observed>,
    active: bool,
}
#[derive(Default)]
pub(super) struct Cell {
    state: Mutex<State>,
    // Only the finite FULL fixture arms this real SQLite allocation probe.
    probe: AtomicBool,
}
impl Cell {
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("operator cell poisoned"))
    }
    pub(super) fn reset(&self) -> Result<()> {
        let mut state = self.state()?;
        ensure!(!state.active, "operator callback still active at prepare");
        state.error = None;
        state.observed = None;
        Ok(())
    }
    pub(super) fn take(&self) -> Result<Option<anyhow::Error>> {
        Ok(self.state()?.error.take())
    }
    pub(super) fn enter(self: &Arc<Self>) -> Result<Scope> {
        let mut state = self.state()?;
        if state.active {
            if state.error.is_none() {
                state.error = Some(anyhow::anyhow!("private operator reentry refused"));
                state.observed = Some(Observed {
                    primary: None,
                    extended: None,
                    disposition: Disposition::Unclassified,
                    origin: Origin::Reentry,
                });
            }
            anyhow::bail!("private operator reentry refused");
        }
        state.active = true;
        Ok(Scope(self.clone()))
    }
    pub(super) fn preserve(
        &self,
        error: anyhow::Error,
        origin: Origin,
    ) -> Result<Option<anyhow::Error>> {
        let disposition = classify(&error);
        let sql = error
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error());
        let observed = Observed {
            primary: sql.map(|e| e.code),
            extended: sql.map(|e| e.extended_code),
            disposition,
            origin,
        };
        let mut state = self.state()?;
        if state.observed.is_none() {
            state.observed = Some(observed);
        }
        if disposition == Disposition::Fence {
            return Ok(Some(error));
        }
        if state.error.is_none() {
            state.error = Some(error);
        }
        Ok(None)
    }
    pub(super) fn observed(&self) -> Result<Option<Observed>> {
        Ok(self.state()?.observed)
    }
    pub(super) fn arm_probe(&self, armed: bool) {
        self.probe.store(armed, Ordering::Relaxed);
    }
    pub(super) fn constraint_reason(&self) -> Result<Option<String>> {
        Ok(self
            .observed()?
            .filter(|o| o.primary == Some(ErrorCode::ConstraintViolation))
            .map(|o| {
                format!(
                    "terminal operator constraint; primary=19 extended={}",
                    o.extended.unwrap_or(19)
                )
            }))
    }
}
pub(super) struct Scope(Arc<Cell>);
impl Drop for Scope {
    fn drop(&mut self) {
        // A poisoned cell remains unusable; reset/take refuse it rather than pass.
        if let Ok(mut state) = self.0.state.lock() {
            state.active = false;
        }
    }
}
pub(super) struct DelegatingOperator {
    pub(super) cell: Arc<Cell>,
}
impl Operator for DelegatingOperator {
    fn name(&self) -> &'static str {
        TerminalGates.name()
    }
    fn source(&self) -> &'static str {
        TerminalGates.source()
    }
    fn fingerprint(&self) -> &'static str {
        "private.operator-error.v1;primary-code;constraint-fence;infrastructure+unclassified-abort;dormant.v2;eight-fixture-facts;key-refusal-retracts;runtime-observed-single-origin+six-nonselecting;72-markers;64-members;16-rendered;no-native-authority"
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        TerminalGates.create_schema(c)
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        let _scope = self.cell.enter()?;
        let probe = self.cell.probe.load(Ordering::Relaxed);
        let result = if probe {
            tx.execute(
                "INSERT INTO test_terminal_operator_probe(payload) VALUES(zeroblob(65536))",
                [],
            )
            .map(|_| false)
            .map_err(anyhow::Error::from)
        } else {
            TerminalGates.apply(tx, ns, rows)
        };
        match result {
            Ok(changed) => Ok(changed),
            Err(error) => {
                if let Some(error) = self.cell.preserve(
                    error,
                    if probe {
                        Origin::Probe
                    } else {
                        Origin::Witness
                    },
                )? {
                    return Err(error);
                }
                // The original owned error is retained until the finalizer drains it.
                anyhow::bail!("private operator abort-required error retained")
            }
        }
    }
    fn validate_publication(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
        TerminalGates.validate_publication(tx, ns)
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        TerminalGates.reclaim(tx, ns, rows)
    }
}
