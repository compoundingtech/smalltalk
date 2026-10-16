//! Dormant adapter for an explicitly owned private fixture, using the existing outer hooks.
//! No Store is retained by callbacks; no default factory or reader installs this module.
use super::*;
use crate::store::Store;
use smallclaims::sqlite::work::{SqliteWork, SqliteWorkScope};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::ThreadId;

mod operator_error;

#[derive(Default)]
struct Adapter {
    source: OnceLock<NativeSource>,
    operator_error: Arc<operator_error::Cell>,
    // Explicit one-use caller token. Uninitialized state by itself grants no permission.
    bootstrap: Mutex<Option<ThreadId>>,
    bootstrap_claimed: AtomicUsize,
    prepare_calls: AtomicUsize,
    finalize_calls: AtomicUsize,
    fault: AtomicUsize,
    work: Mutex<Vec<(&'static str, SqliteWork)>>,
}
struct BootstrapToken(Arc<Adapter>);
impl Drop for BootstrapToken {
    fn drop(&mut self) {
        *self.0.bootstrap.lock().unwrap() = None;
    }
}
impl Adapter {
    fn install(store: &Store) -> Result<Arc<Self>> {
        let adapter = Arc::new(Self::default());
        let before = adapter.clone();
        let after = adapter.clone();
        store.connection.install_transaction_hooks(
            move |tx| before.prepare(tx),
            move |tx| after.finalize(tx),
        )?;
        Ok(adapter)
    }
    fn token(self: &Arc<Self>) -> Result<BootstrapToken> {
        let mut token = self.bootstrap.lock().unwrap();
        ensure!(
            self.source.get().is_none()
                && token.is_none()
                && self.bootstrap_claimed.swap(1, Ordering::Relaxed) == 0,
            "bootstrap token refused"
        );
        *token = Some(std::thread::current().id());
        Ok(BootstrapToken(self.clone()))
    }
    fn measured<T>(&self, phase: &'static str, job: impl FnOnce() -> Result<T>) -> Result<T> {
        let scope = SqliteWorkScope::start();
        let result = job();
        self.work.lock().unwrap().push((phase, scope.finish()));
        result
    }
    fn prepare(&self, tx: &Transaction<'_>) -> Result<()> {
        self.operator_error.reset()?;
        self.prepare_calls.fetch_add(1, Ordering::Relaxed);
        self.measured("prepare", || {
            let Some(source) = self.source.get() else {
                ensure!(
                    *self.bootstrap.lock().unwrap() == Some(std::thread::current().id()),
                    "explicit bootstrap token required"
                );
                return Ok(());
            };
            let (_, pending, gap, active) = source.scoped_state(tx)?;
            if pending || active {
                // Committed raw capture predates this scope. Never bless it by extraction.
                let reason = gap
                    .as_deref()
                    .unwrap_or("preexisting raw pending or managed scope");
                tx.execute(
                    "UPDATE test_native_terminal_guard SET gap=?1,pending=1 WHERE id=1",
                    [reason],
                )?;
                source.installer.source_gap(tx, SOURCE, reason)?;
            }
            tx.execute(
                "UPDATE test_native_terminal_guard SET managed_scope=1 WHERE id=1",
                [],
            )?;
            if self.fault.load(Ordering::Relaxed) == 1 {
                anyhow::bail!("injected prepare failure");
            }
            Ok(())
        })
    }
    fn finalize(&self, tx: &Transaction<'_>) -> Result<()> {
        self.finalize_calls.fetch_add(1, Ordering::Relaxed);
        let source = self.source.get().context("bootstrap was not completed")?;
        self.measured("finalize-marker-read", || {
            let active: bool = tx.query_row(
                "SELECT managed_scope FROM test_native_terminal_guard WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            ensure!(active, "missing managed scope");
            Ok(())
        })?;
        let finished = self.measured("source-finish", || source.finish(tx));
        // Installer may have converted the callback fault into an unready root.
        // Drain even if finish failed during its subsequent savepoint cleanup.
        if let Some(error) = self.operator_error.take()? {
            return Err(error);
        }
        finished?;
        if let Some(reason) = self.operator_error.constraint_reason()? {
            self.measured("operator-constraint-fence", || {
                tx.execute(
                    "UPDATE test_native_terminal_guard SET gap=?1,pending=1 WHERE id=1",
                    [&reason],
                )?;
                source.installer.source_gap(tx, SOURCE, &reason)
            })?;
        }
        match self.fault.load(Ordering::Relaxed) {
            2 => anyhow::bail!("injected finalizer storage failure"),
            3 => panic!("injected finalizer panic"),
            _ => (),
        }
        self.measured("marker-clear", || {
            ensure!(
                tx.execute(
                    "UPDATE test_native_terminal_guard SET managed_scope=0 WHERE id=1",
                    []
                )? == 1,
                "missing scope row"
            );
            Ok(())
        })
    }
    fn bootstrap(self: &Arc<Self>, store: &Store, binding: Binding, seed: bool) -> Result<()> {
        let _token = self.token()?;
        let mut writer = store.connection.write();
        let tx = writer.transaction()?;
        let source = NativeSource::attach_with_installer(&tx, binding, self.installer()?)?;
        self.source
            .set(source)
            .map_err(|_| anyhow::anyhow!("source already attached"))?;
        tx.execute(
            "UPDATE test_native_terminal_guard SET managed_scope=1 WHERE id=1",
            [],
        )?;
        if seed {
            super::tests::seed_native(&tx, &self.source.get().unwrap().binding);
        }
        tx.commit()?;
        Ok(())
    }
    fn matching_reopen(store: &Store, binding: Binding) -> Result<Arc<Self>> {
        let adapter = Self::install(store)?;
        binding.validate()?;
        adapter
            .source
            .set(NativeSource {
                binding,
                installer: adapter.installer()?,
            })
            .map_err(|_| anyhow::anyhow!("source already attached"))?;
        // No attestation or empty registration here. The first prepare validates the saved
        // binding and permanently gaps pending open-time/raw writes, including a leaked scope.
        Ok(adapter)
    }
    fn installer(&self) -> Result<Installer> {
        Installer::new(vec![Box::new(operator_error::DelegatingOperator {
            cell: self.operator_error.clone(),
        })])
    }
}

#[cfg(test)]
mod tests;
