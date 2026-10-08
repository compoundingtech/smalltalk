//! Scheduling for independent native collection sources. Sources own capture, authority,
//! installation and coverage; this module never infers a certificate or folds claim history.
// Source implementations and default registration land separately from this shared seam.
#![allow(dead_code)]
use super::*;
use smallclaims::ivm::{
    View, Views,
    install::{Installer, Operator},
};
use std::sync::{
    Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
pub(crate) struct Budget {
    pub rows: usize,
    pub bytes: usize,
}

const BUDGET: Budget = Budget {
    rows: 64,
    bytes: 256 * 1024,
};
const PAUSE: std::time::Duration = std::time::Duration::from_millis(25);

pub(crate) struct Context {
    pub origin: String,
    pub views: Arc<Views>,
    pub installer: Arc<Installer>,
}

/// Every source has its own receiver-bound identity and deferred journal. All callbacks
/// must bound their rows/bytes, including reverse dependencies, output writes and checks.
/// Source implementations must not retain a strong Store or another runtime owner.
pub(crate) trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    fn views(&self) -> Vec<Box<dyn View>>;
    fn operators(&self) -> Vec<Box<dyn Operator>>;
    fn create_schema(&self, connection: &Connection) -> Result<()>;
    /// After native projection setup, before exposing the Store. Register capture and
    /// compatible lifetimes; start explicit bounded jobs, never scan/replay or mark Ready.
    /// Preserve any unmanaged migration/schema gap; do not heal an unavailable lifetime.
    fn open(&self, tx: &Transaction<'_>, cx: &Context) -> Result<()>;
    /// Scope/capture bookkeeping only. Native triggers retain immutable OLD/NEW images
    /// at Installer revisions; callbacks must not extract bodies or run operators.
    /// Also called around startup projection setup, before open. Tolerate a source not
    /// yet installed; clear/check any persisted scope before opening the new scope.
    fn begin(&self, _tx: &Transaction<'_>, _cx: &Context) -> Result<()> {
        Ok(())
    }
    fn commit(&self, _tx: &Transaction<'_>, _cx: &Context) -> Result<()> {
        Ok(())
    }
    /// Indexed bounded facts in one read snapshot, including exact source identity/revision
    /// and all required native/local/producer evidence. Return None for unrelated commits.
    fn capture(
        &self,
        connection: &Connection,
        cx: &Context,
        now_ms: u64,
        budget: Budget,
    ) -> Result<Capture>;
    /// Independently prove coverage at this read cut, even for a silent collection page.
    /// Neither registration, a queue revision nor MAX claim index proves coverage.
    fn coverage(&self, connection: &Connection, cx: &Context, now_ms: u64) -> Result<bool>;
}

pub(crate) struct Capture {
    pub work: Option<Box<dyn Captured>>,
    pub wake_at_unix_ms: Option<u64>,
}

/// Owns immutable facts and Installer reference rows. No connection/read guard may escape
/// capture; prepare runs after releasing its snapshot, using these facts only.
pub(crate) trait Captured: Send {
    fn prepare(self: Box<Self>, cx: &Context) -> Result<Box<dyn Page>>;
}

pub(crate) trait Page: Send {
    /// Apply precomputed bounded writes, rechecking native identity, source position and
    /// captured rows. Recheck independent coverage before publishing through Views.
    /// Return true when more work remains, including a stale page with no output applied.
    /// Logical source gaps fence source/views; storage errors propagate and roll back.
    fn publish(self: Box<Self>, tx: &Transaction<'_>, cx: &Context, now_ms: u64) -> Result<bool>;
}

pub(crate) struct Registry {
    pub cx: Context,
    sources: Vec<Arc<dyn Source>>,
    names: Vec<Vec<&'static str>>,
    retries: Vec<AtomicU64>,
    wake: Arc<Notify>,
}

impl Registry {
    pub fn new(origin: String, sources: Vec<Arc<dyn Source>>) -> Result<Arc<Self>> {
        anyhow::ensure!(
            !sources.is_empty() && sources.len() <= 16,
            "collection source registry bound"
        );
        let mut source_names = BTreeSet::new();
        let mut definitions = Vec::new();
        let mut operators = Vec::new();
        let mut names = Vec::new();
        for source in &sources {
            anyhow::ensure!(
                !source.name().is_empty()
                    && source.name().len() <= 1024
                    && source_names.insert(source.name()),
                "invalid or duplicate collection source"
            );
            let views = source.views();
            let ops = source.operators();
            anyhow::ensure!(
                !views.is_empty() && views.len() <= 16 && views.len() == ops.len(),
                "collection view/operator registry mismatch"
            );
            let mut view_names = BTreeSet::new();
            for view in &views {
                let definition = view.definition();
                anyhow::ensure!(
                    view_names.insert(definition.name)
                        && view.installed_source() == Some(source.name()),
                    "collection view source mismatch"
                );
                anyhow::ensure!(
                    ops.iter().any(|op| op.name() == definition.name
                        && op.source() == source.name()
                        && op.fingerprint() == definition.fingerprint),
                    "collection operator binding mismatch"
                );
            }
            names.push(view_names.into_iter().collect());
            definitions.extend(views);
            operators.extend(ops);
        }
        anyhow::ensure!(definitions.len() <= 64, "collection view registry bound");
        Ok(Arc::new(Self {
            cx: Context {
                origin,
                views: Arc::new(Views::new(definitions)?),
                installer: Arc::new(Installer::new(operators)?),
            },
            retries: sources.iter().map(|_| AtomicU64::new(0)).collect(),
            sources,
            names,
            wake: Arc::new(Notify::new()),
        }))
    }

    pub fn create_schema(&self, connection: &Connection) -> Result<()> {
        self.cx.installer.create_schema(connection)?;
        for source in &self.sources {
            source.create_schema(connection)?;
        }
        Ok(())
    }

    pub fn open(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in &self.sources {
            source.open(tx, &self.cx)?;
        }
        Ok(())
    }

    pub fn begin(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in &self.sources {
            source.begin(tx, &self.cx)?;
        }
        Ok(())
    }

    pub fn commit(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in &self.sources {
            source.commit(tx, &self.cx)?;
        }
        Ok(())
    }

    pub fn install_hooks(
        self: &Arc<Self>,
        writer: &smallclaims::sqlite::WriterConnection,
    ) -> Result<()> {
        let (begin, commit) = (self.clone(), self.clone());
        writer.install_transaction_hooks(move |tx| begin.begin(tx), move |tx| commit.commit(tx))
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// The source owner calls this after explicit repair/recovery. Ordinary commits and
    /// producer wakes never restart a source whose callback failed and was fenced.
    pub fn retry_source(&self, source: &str) -> Result<()> {
        let index = self
            .sources
            .iter()
            .position(|s| s.name() == source)
            .context("unknown collection source")?;
        self.retries[index].fetch_add(1, Ordering::AcqRel);
        self.wake();
        Ok(())
    }

    pub fn coverage(&self, source: &str, connection: &Connection, now_ms: u64) -> Result<bool> {
        self.sources
            .iter()
            .find(|s| s.name() == source)
            .context("unknown collection source")?
            .coverage(connection, &self.cx, now_ms)
    }

    fn fence(&self, tx: &Transaction<'_>, index: usize, error: &str) -> Result<()> {
        self.cx
            .installer
            .source_gap(tx, self.sources[index].name(), error)?;
        for name in &self.names[index] {
            self.cx.views.fence(tx, name, error)?;
        }
        Ok(())
    }
}

pub(crate) struct Reactor {
    stopped: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    _observer: smallclaims::sqlite::CommitObserver,
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.task.abort();
    }
}

impl Reactor {
    pub fn start(store: &Arc<Store>, registry: Arc<Registry>) -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let wake = registry.wake.clone();
        // No SQL or projection work runs on the admission writer callback.
        let observer = store.observe_commits(move |_| wake.notify_one());
        let task = runtime.spawn(run(Arc::downgrade(store), registry, stopped.clone()));
        Ok(Self {
            stopped,
            task,
            _observer: observer,
        })
    }
}

async fn run(store: Weak<Store>, registry: Arc<Registry>, stopped: Arc<AtomicBool>) {
    let mut first = 0;
    let mut failed = BTreeMap::<usize, u64>::new();
    loop {
        let mut more = false;
        let mut deadline = None::<u64>;
        for offset in 0..registry.sources.len() {
            let index = (first + offset) % registry.sources.len();
            let retry = registry.retries[index].load(Ordering::Acquire);
            if failed.get(&index) == Some(&retry) {
                continue;
            }
            let Some(reader) = store.upgrade() else {
                return;
            };
            let worker = registry.clone();
            let stop = stopped.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<(bool, Option<u64>)> {
                if stop.load(Ordering::Acquire) {
                    return Ok((false, None));
                }
                let source = &worker.sources[index];
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let capture = reader.read_snapshot(|_| {
                        source.capture(&reader.readers.get(), &worker.cx, clock_ms(), BUDGET)
                    })?;
                    // The source snapshot has ended before preparation or writer admission.
                    let page = capture
                        .work
                        .map(|facts| facts.prepare(&worker.cx))
                        .transpose()?;
                    let Some(page) = page else {
                        return Ok((false, capture.wake_at_unix_ms));
                    };
                    if stop.load(Ordering::Acquire) {
                        return Ok((false, None));
                    }
                    let mut writer = reader.connection.write_background();
                    if stop.load(Ordering::Acquire) {
                        return Ok((false, None));
                    }
                    let tx = writer.transaction()?;
                    let more = page.publish(&tx, &worker.cx, clock_ms())?;
                    tx.commit()?;
                    Ok((more, capture.wake_at_unix_ms))
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("collection source callback panicked")));
                if let Err(error) = &result {
                    // Fence derived state in a separate transaction after rolling back the page.
                    if stop.load(Ordering::Acquire) {
                        return result;
                    }
                    let mut writer = reader.connection.write_background();
                    if stop.load(Ordering::Acquire) {
                        return result;
                    }
                    let tx = writer.transaction()?;
                    worker.fence(&tx, index, &format!("collection reactor: {error:#}"))?;
                    tx.commit()?;
                }
                result
            })
            .await;
            match result {
                Ok(Ok((pending, wake))) => {
                    more |= pending;
                    if let Some(wake) = wake {
                        deadline = Some(deadline.map_or(wake, |old| old.min(wake)));
                    }
                }
                Ok(Err(error)) => {
                    failed.insert(index, retry);
                    tracing::warn!(source=registry.sources[index].name(), %error, "collection source reactor failed");
                }
                Err(error) => {
                    failed.insert(index, retry);
                    tracing::error!(%error, "collection source reactor worker failed");
                }
            }
            tokio::task::yield_now().await;
        }
        first = (first + 1) % registry.sources.len();
        if more || deadline.is_some_and(|at| at <= clock_ms()) {
            tokio::time::sleep(PAUSE).await;
        } else {
            let due = async {
                match deadline {
                    Some(at) => {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            at.saturating_sub(clock_ms()),
                        ))
                        .await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! { _ = registry.wake.notified() => {}, _ = due => {} }
        }
    }
}

fn clock_ms() -> u64 {
    now_ms().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests;
