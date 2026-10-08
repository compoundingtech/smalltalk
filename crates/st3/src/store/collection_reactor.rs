//! Scheduling for independent native collection sources. Sources own capture, authority,
//! installation and coverage; this module never infers a certificate or folds claim history.
// Source implementations and default registration land separately from this shared seam.
#![allow(dead_code)]
use super::*;
use smallclaims::ivm::{
    View, Views, events,
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
    /// Opt into the one Publisher's committed native-source and post-swap wakes.
    /// This is fixed expected configuration, never inferred from persisted rows.
    /// The source owns complete transactional raw/capture and lifetime coverage.
    fn progress_identity(&self) -> Option<events::SourceIdentity> {
        None
    }
    /// Cache-only sources may return zero views/operators. They still own a named,
    /// persistent source lifetime, complete cut coverage and guarded cache reads.
    fn views(&self) -> Vec<Box<dyn View>>;
    fn operators(&self) -> Vec<Box<dyn Operator>>;
    fn create_schema(&self, connection: &Connection) -> Result<()>;
    /// After native projection setup, before exposing the Store. Register capture and
    /// compatible lifetimes; start explicit bounded jobs, never scan/replay or mark Ready.
    /// Preserve any unmanaged migration/schema gap; do not heal an unavailable lifetime.
    /// A retained incompatible identity disables this source, rather than rejecting an
    /// otherwise usable Store. Keep the expected identity fixed; do not silently rebind it.
    /// Cache-only sources also register their persistent Installer source lifetime.
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
    /// Borrowed Store permits Core cache/metadata access only within this call; it must
    /// not initialize or rebuild caches inline, or escape into the owned preparation.
    fn capture(
        &self,
        store: &Store,
        connection: &Connection,
        cx: &Context,
        now_ms: u64,
        budget: Budget,
    ) -> Result<Capture>;
    /// Independently prove the selected completed publication may serve at this read cut,
    /// even for a silent collection page. Retained immutable rows keep their original full
    /// cut/evaluation clock; incomplete newer work must not relabel or invalidate that root.
    /// Current receiver/lifetime/source-gap/authority guards must still qualify it.
    /// Neither registration, a queue revision nor MAX claim index proves coverage.
    /// Borrowed Store access is prepared state/metadata only, never lazy repair/build.
    /// Check persisted lifetime availability plus the source's complete native cut.
    fn coverage(
        &self,
        store: &Store,
        connection: &Connection,
        cx: &Context,
        now_ms: u64,
    ) -> Result<bool>;
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
    /// Return more=true when work remains, including a stale page with no output applied.
    /// Logical source gaps fence source/views; storage errors propagate and roll back.
    /// Cache-only publication may perform no DML, but uses this same writer admission
    /// to recheck its native cut. Store is borrowed, never retained by a page/source.
    /// Memory/cache publication belongs in Publication::after_commit, never this callback.
    fn publish(
        self: Box<Self>,
        store: &Store,
        tx: &Transaction<'_>,
        cx: &Context,
        now_ms: u64,
    ) -> Result<Publication>;
}

pub(crate) struct Publication {
    pub more: bool,
    pub stale: bool,
    pub after_commit: Option<Box<dyn AfterCommit>>,
}

impl Publication {
    /// No precommit output was applied because the owned page's cut no longer matches.
    pub fn stale() -> Self {
        Self {
            more: true,
            stale: true,
            after_commit: None,
        }
    }
}

impl From<bool> for Publication {
    fn from(more: bool) -> Self {
        Self {
            more,
            stale: false,
            after_commit: None,
        }
    }
}

/// Owned cache action, executed only after successful managed commit and writer return.
/// The reactor supplies one fresh short committed snapshot. Use only this connection and
/// accessor-only Store methods; no writer acquisition, nested snapshot, reduction or repair.
/// Recheck the full captured cut, source lifetime/availability and publication generation
/// before swapping immutable prepared state. Return true to recapture stale work.
/// An opted-in source calls the prepared Publisher's publication_completed with the
/// original committed SourceToken after its check and swap, before returning false.
/// Stale/refused actions never emit Published; it is a wake hint, not a Ready certificate.
/// This action cannot undo the committed page. Errors/panics refuse reads and fence the
/// source; adapters must retain independent cut/lifetime checks and Registry::coverage.
pub(crate) trait AfterCommit: Send {
    fn publish(
        self: Box<Self>,
        store: &Store,
        connection: &Connection,
        cx: &Context,
        now_ms: u64,
    ) -> Result<bool>;
}

pub(crate) struct Registry {
    pub cx: Context,
    sources: Vec<Arc<dyn Source>>,
    names: Vec<Vec<&'static str>>,
    retries: Vec<AtomicU64>,
    unavailable: Vec<AtomicBool>,
    stale_publications: Vec<AtomicU64>,
    progress_identities: Vec<Option<events::SourceIdentity>>,
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
        let mut progress_identities = Vec::new();
        for source in &sources {
            anyhow::ensure!(
                !source.name().is_empty()
                    && source.name().len() <= 1024
                    && source_names.insert(source.name()),
                "invalid or duplicate collection source"
            );
            let views = source.views();
            let identity = source.progress_identity();
            if let Some(identity) = &identity {
                anyhow::ensure!(
                    identity.name == source.name()
                        && identity.name.len() <= 128
                        && !identity.name.contains('\0')
                        && !identity.fingerprint.is_empty()
                        && identity.fingerprint.len() <= 4096
                        && !identity.fingerprint.contains('\0')
                        && identity.epoch <= i64::MAX as u64,
                    "invalid collection source progress identity"
                );
            }
            progress_identities.push(identity);
            let ops = source.operators();
            anyhow::ensure!(
                views.len() <= 16 && views.len() == ops.len(),
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
            unavailable: sources.iter().map(|_| AtomicBool::new(false)).collect(),
            stale_publications: sources.iter().map(|_| AtomicU64::new(0)).collect(),
            progress_identities,
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
        for index in 0..self.sources.len() {
            self.hook(tx, index, true)?;
        }
        Ok(())
    }

    pub fn commit(&self, tx: &Transaction<'_>) -> Result<()> {
        for index in 0..self.sources.len() {
            self.hook(tx, index, false)?;
        }
        Ok(())
    }

    fn hook(&self, tx: &Transaction<'_>, index: usize, begin: bool) -> Result<()> {
        // Undo a failed callback's bookkeeping without rejecting otherwise valid native
        // admission. Storage errors still propagate and roll back the whole transaction.
        tx.execute_batch("SAVEPOINT collection_source_hook")?;
        let source = &self.sources[index];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if begin {
                source.begin(tx, &self.cx)
            } else {
                source.commit(tx, &self.cx)
            }
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("collection capture hook panicked")));
        match result {
            Ok(()) => tx.execute_batch("RELEASE collection_source_hook")?,
            Err(error) => {
                tx.execute_batch(
                    "ROLLBACK TO collection_source_hook; RELEASE collection_source_hook",
                )?;
                if storage_failure(&error) {
                    return Err(error);
                }
                self.fence(tx, index, &format!("collection capture hook: {error:#}"))?;
            }
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

    pub fn publisher_sources(&self) -> Vec<events::SourceIdentity> {
        self.progress_identities.iter().flatten().cloned().collect()
    }

    fn invalidate_publication(&self, store: &Store, index: usize, reason: &str) {
        let Some(identity) = &self.progress_identities[index] else {
            return;
        };
        let result = store
            .prepared_ivm_publisher()
            .context("collection source publisher not prepared")
            .and_then(|publisher| publisher.publication_invalidated(identity, reason));
        if let Err(error) = result {
            // Guard refusal already holds, including when its durable fence failed.
            tracing::warn!(source=identity.name, %error, "collection source invalidation wake failed");
        }
    }

    pub fn stale_publications(&self, source: &str) -> Result<u64> {
        let index = self
            .sources
            .iter()
            .position(|s| s.name() == source)
            .context("unknown collection source")?;
        Ok(self.stale_publications[index].load(Ordering::Relaxed))
    }

    /// The source owner calls this after explicit repair/recovery. Ordinary commits and
    /// producer wakes never restart a source whose callback failed and was fenced.
    pub fn retry_source(&self, source: &str) -> Result<()> {
        let index = self
            .sources
            .iter()
            .position(|s| s.name() == source)
            .context("unknown collection source")?;
        // The caller has explicitly repaired the source; this clears only the process
        // guard. Its independent native/Installer coverage must still qualify every read.
        self.unavailable[index].store(false, Ordering::Release);
        self.retries[index].fetch_add(1, Ordering::AcqRel);
        self.wake();
        Ok(())
    }

    /// Source factories must use this guard for adapter reads, including silent pages.
    /// A failed durable fence cannot leave that Store's old namespace served.
    pub fn coverage(
        &self,
        source: &str,
        store: &Store,
        connection: &Connection,
        now_ms: u64,
    ) -> Result<bool> {
        let index = self
            .sources
            .iter()
            .position(|s| s.name() == source)
            .context("unknown collection source")?;
        if self.unavailable[index].load(Ordering::Acquire) {
            return Ok(false);
        }
        let covered = self.sources[index].coverage(store, connection, &self.cx, now_ms)?;
        Ok(covered && !self.unavailable[index].load(Ordering::Acquire))
    }

    fn fence(&self, tx: &Transaction<'_>, index: usize, error: &str) -> Result<()> {
        self.unavailable[index].store(true, Ordering::Release);
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
    let mut backoffs = BTreeMap::<usize, StaleBackoff>::new();
    loop {
        let mut more = false;
        let mut deadline = None::<u64>;
        for offset in 0..registry.sources.len() {
            let index = (first + offset) % registry.sources.len();
            let retry = registry.retries[index].load(Ordering::Acquire);
            if failed.get(&index) == Some(&retry) {
                continue;
            }
            if registry.unavailable[index].load(Ordering::Acquire) {
                let Some(reader) = store.upgrade() else {
                    return;
                };
                // A hook may fence a source without an owned page. Its commit observer
                // wakes this pass after writer return; notify waiters once and suspend.
                registry.invalidate_publication(&reader, index, "collection source unavailable");
                failed.insert(index, retry);
                continue;
            }
            if let Some(backoff) = backoffs.get(&index) {
                if backoff.retry != retry {
                    backoffs.remove(&index);
                } else if backoff.until_ms > clock_ms() {
                    deadline =
                        Some(deadline.map_or(backoff.until_ms, |old| old.min(backoff.until_ms)));
                    continue;
                }
            }
            let Some(reader) = store.upgrade() else {
                return;
            };
            let worker = registry.clone();
            let stop = stopped.clone();
            let result =
                tokio::task::spawn_blocking(move || -> Result<(bool, Option<u64>, bool)> {
                    if stop.load(Ordering::Acquire) {
                        return Ok((false, None, false));
                    }
                    let source = &worker.sources[index];
                    let mut committed = false;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let capture = reader.read_snapshot(|_| {
                            source.capture(
                                &reader,
                                &reader.readers.get(),
                                &worker.cx,
                                clock_ms(),
                                BUDGET,
                            )
                        })?;
                        // The source snapshot has ended before preparation or writer admission.
                        let page = capture
                            .work
                            .map(|facts| facts.prepare(&worker.cx))
                            .transpose()?;
                        let Some(page) = page else {
                            return Ok((false, capture.wake_at_unix_ms, false));
                        };
                        if stop.load(Ordering::Acquire) {
                            return Ok((false, None, false));
                        }
                        let mut writer = reader.connection.write_background();
                        if stop.load(Ordering::Acquire) {
                            return Ok((false, None, false));
                        }
                        let tx = writer.transaction()?;
                        let publication = page.publish(&reader, &tx, &worker.cx, clock_ms())?;
                        tx.commit()?;
                        committed = true;
                        // Guard return completes commit observers before any cache is exposed.
                        // A refused/rolled-back commit drops the action without executing it.
                        drop(writer);
                        let stale = if let Some(action) = publication.after_commit {
                            if stop.load(Ordering::Acquire) {
                                return Ok((false, None, false));
                            }
                            reader.read_snapshot(|_| {
                                anyhow::ensure!(
                                    !worker.unavailable[index].load(Ordering::Acquire),
                                    "collection source unavailable after commit"
                                );
                                action.publish(
                                    &reader,
                                    &reader.readers.get(),
                                    &worker.cx,
                                    clock_ms(),
                                )
                            })?
                        } else {
                            false
                        };
                        let stale = publication.stale || stale;
                        Ok((publication.more || stale, capture.wake_at_unix_ms, stale))
                    }))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("collection source callback panicked"))
                    });
                    if let Err(error) = &result {
                        // A storage failure is not evidence of missing native capture. Preserve
                        // the rolled-back source lifetime and retry through the same queue.
                        if !committed && storage_failure(error) {
                            return result;
                        }
                        // Refuse reads immediately, including when writer admission or the
                        // durable fence itself fails. Source factories use Registry::coverage.
                        worker.unavailable[index].store(true, Ordering::Release);
                        // Fence separately: precommit errors rolled back the page; postcommit
                        // action errors cannot undo it and must refuse the committed state too.
                        if stop.load(Ordering::Acquire) {
                            return result;
                        }
                        let fence_result = (|| -> Result<()> {
                            let mut writer = reader.connection.write_background();
                            if stop.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            let tx = writer.transaction()?;
                            worker.fence(&tx, index, &format!("collection reactor: {error:#}"))?;
                            tx.commit()?;
                            Ok(())
                        })();
                        // The managed fence transaction and writer have returned on either
                        // Result path. A failed durable fence still has the process guard.
                        worker.invalidate_publication(&reader, index, &format!("{error:#}"));
                        fence_result?;
                    }
                    result
                })
                .await;
            match result {
                Ok(Ok((pending, wake, stale))) => {
                    if stale {
                        registry.stale_publications[index].fetch_add(1, Ordering::Relaxed);
                        let backoff = backoffs.entry(index).or_insert(StaleBackoff {
                            retry,
                            failures: 0,
                            until_ms: 0,
                        });
                        backoff.failures = backoff.failures.saturating_add(1);
                        let delay = (25u64 << backoff.failures.saturating_sub(1).min(7)).min(2000);
                        backoff.until_ms = clock_ms().saturating_add(delay);
                        deadline = Some(
                            deadline.map_or(backoff.until_ms, |old| old.min(backoff.until_ms)),
                        );
                    } else {
                        backoffs.remove(&index);
                        more |= pending;
                    }
                    if let Some(wake) = wake {
                        deadline = Some(deadline.map_or(wake, |old| old.min(wake)));
                    }
                }
                Ok(Err(error)) => {
                    if retryable_storage_failure(&error)
                        && !registry.unavailable[index].load(Ordering::Acquire)
                    {
                        more = true;
                    } else {
                        failed.insert(index, retry);
                    }
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

struct StaleBackoff {
    retry: u64,
    failures: u32,
    until_ms: u64,
}

fn storage_failure(error: &anyhow::Error) -> bool {
    use rusqlite::ErrorCode::*;
    matches!(error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code,_)) if matches!(code.code,
            DatabaseBusy | DatabaseLocked | OutOfMemory | SystemIoFailure | DiskFull |
            CannotOpen | DatabaseCorrupt | NotADatabase | FileLockingProtocolFailed |
            ReadOnly | PermissionDenied))
}

fn retryable_storage_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code,_)) if matches!(code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

fn clock_ms() -> u64 {
    now_ms().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests;
