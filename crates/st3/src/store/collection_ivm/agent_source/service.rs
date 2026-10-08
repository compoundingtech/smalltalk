//! Explicit native agent source composition. One Store registry, Installer and Publisher.
//! Initial publication requires complete extraction/journal, namespace closure, native graph
//! projection coverage and live whole-producer evidence in the same authoritative transaction.
use super::{SOURCE, boundary, clock, extract};
use crate::{
    api::delivery_presence::source::boundary as producer,
    store::{Store, agent_card_ivm as cards, agent_card_source as kernel},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use smallclaims::ivm::{
    SourceCut, Views,
    install::{Installer, Limits, Mutation, Namespace, Operator, Outcome, SourcePosition},
    installed::Changed,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
};

const PAGE: usize = 128;
#[cfg(test)]
pub(crate) static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
type Handles = Arc<Mutex<BTreeMap<String, Namespace>>>;

/// Retains opaque identity only. No SQL, cut, certificate or readiness is produced here.
struct Observed {
    inner: kernel::Kernel,
    handles: Handles,
}
impl Operator for Observed {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn source(&self) -> &'static str {
        self.inner.source()
    }
    fn fingerprint(&self) -> &'static str {
        self.inner.fingerprint()
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        self.inner.create_schema(c)
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        let changed = self.inner.apply(tx, ns, rows)?;
        let mut handles = self
            .handles
            .lock()
            .map_err(|_| anyhow::anyhow!("namespace identity lock poisoned"))?;
        ensure!(
            handles.contains_key(ns.as_str()) || handles.len() < 2,
            "namespace identity retention bound"
        );
        handles.insert(ns.as_str().into(), ns.clone());
        Ok(changed)
    }
    fn validate_publication(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
        self.inner.validate_publication(tx, ns)
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        ensure!((1..=PAGE).contains(&rows), "source reclaim budget");
        let used = super::work_progress::reclaim(tx, ns, rows)?;
        if used == rows {
            return Ok(false);
        }
        self.inner.reclaim(tx, ns, rows - used)
    }
}

pub(crate) struct Service {
    store: Weak<Store>,
    views: Arc<Views>,
    installer: Arc<Installer>,
    handles: Handles,
    receiver: String,
    fingerprint: String,
    job: Mutex<Option<String>>,
    producer_after: Mutex<String>,
    waiting: Mutex<Option<SourcePosition>>,
    recovery: Mutex<bool>,
    stopped: Mutex<Option<String>>,
}

pub(crate) fn open(path: &Path, receiver: &str) -> Result<Arc<Store>> {
    let preflight = preflight_reopen(path, receiver);
    let views = Arc::new(Views::new(cards::definitions())?);
    let store = Arc::new(Store::open_with_ivm_views(path, receiver, views.clone())?);
    // This is actual bounded native projection, not an invented frontier update. It precedes
    // source registration, so a preexisting legacy dependency cannot be silently adopted stale.
    let startup = match preflight {
        Ok(true) => store.replay_replication_graph(),
        Ok(false) => store.project_replication_backlog().map(|_| ()),
        Err(error) => Err(error),
    };
    let handles = Arc::new(Mutex::new(BTreeMap::new()));
    let installer = Arc::new(Installer::new(vec![Box::new(Observed {
        inner: kernel::Kernel::new(receiver),
        handles: handles.clone(),
    })])?);
    let service = Arc::new(Service {
        store: Arc::downgrade(&store),
        views,
        installer,
        handles,
        receiver: receiver.into(),
        fingerprint: super::capture_fingerprint_for(receiver)?,
        job: Mutex::new(None),
        producer_after: Mutex::new(String::new()),
        waiting: Mutex::new(None),
        recovery: Mutex::new(false),
        stopped: Mutex::new(None),
    });
    let installed = startup.and_then(|_| service.install(&store));
    if let Err(error) = installed {
        service.stop(&store, &error.to_string())?;
    }
    Ok(store)
}

impl Service {
    // A source-only refusal fences agent reads while native daemon operations remain usable.
    // It installs no fallback reader and never fabricates source/producer coverage.
    fn stop(self: &Arc<Self>, store: &Arc<Store>, reason: &str) -> Result<()> {
        *self
            .stopped
            .lock()
            .map_err(|_| anyhow::anyhow!("source stop lock poisoned"))? = Some(reason.into());
        store
            .connection
            .batched(|tx| self.fence_stopped(tx, reason))
            .map_err(anyhow::Error::msg)??;
        if store.smalltalk.ivm_installer.get().is_none() {
            store
                .smalltalk
                .ivm_installer
                .set(self.installer.clone())
                .map_err(|_| anyhow::anyhow!("stopped Installer attachment raced"))?;
        }
        if store.smalltalk.ivm_agent_service.get().is_none() {
            store
                .smalltalk
                .ivm_agent_service
                .set(self.clone())
                .map_err(|_| anyhow::anyhow!("stopped source attachment raced"))?;
        }
        tracing::warn!(%reason,"native agent collection source is unavailable");
        Ok(())
    }

    fn fence_stopped(&self, tx: &Transaction<'_>, reason: &str) -> Result<()> {
        self.views.fence_all(tx, reason)?;
        let capture:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='st3_ivm_capture_state' AND type='table')",[],|r|r.get(0))?;
        if capture {
            super::super::gap(tx, reason)?;
        }
        let sources:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ivm_install_sources' AND type='table')",[],|r|r.get(0))?;
        if sources {
            self.installer.source_gap(tx, SOURCE, reason)?;
        }
        Ok(())
    }

    fn install(self: &Arc<Self>, store: &Arc<Store>) -> Result<()> {
        let epoch = smallclaims::ivm::source_cut(&store.readers.get())?
            .context("collection graph identity missing")?
            .epoch;
        store
            .connection
            .batched(|tx| {
                self.installer.create_schema(tx)?;
                super::row_changes::create_schema(tx)?;
                super::work_progress::create_schema(tx)?;
                super::install_capture_for(tx, &self.receiver, epoch)?;
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS st3_agent_source_service(
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),schema_version INTEGER NOT NULL,
                local_generation INTEGER NOT NULL,receiver TEXT NOT NULL)",
                )?;
                let cut = smallclaims::ivm::source_cut(tx)?.context("source cut missing")?;
                tx.execute(
                    "INSERT INTO st3_agent_source_service VALUES(1,0,?1,?2) ON CONFLICT DO NOTHING",
                    params![cut.local_generation, self.receiver],
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .map_err(anyhow::Error::msg)??;
        let prepare = self.clone();
        let finalize = self.clone();
        store.install_transaction_hooks(
            move |tx| prepare.prepare(tx),
            move |tx| finalize.finalize(tx),
        )?;
        let job = store
            .connection
            .batched(|tx| {
                let retained: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ivm_install_sources WHERE name=?1)",
                    [SOURCE],
                    |r| r.get(0),
                )?;
                let job = if retained {
                    self.begin_recovery(tx, epoch)?;
                    None
                } else {
                    super::super::scope::begin_install(
                        tx,
                        &self.views,
                        &self.installer,
                        SOURCE,
                        &self.fingerprint,
                        epoch,
                    )?;
                    self.views
                        .register_installed(tx, &self.installer, cards::VIEW)?;
                    Some(self.start(tx)?)
                };
                let version: u64 = tx.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
                tx.execute(
                    "UPDATE st3_agent_source_service SET schema_version=?1 WHERE singleton=1",
                    [version],
                )?;
                Ok::<_, anyhow::Error>(job)
            })
            .map_err(anyhow::Error::msg)??;
        *self
            .job
            .lock()
            .map_err(|_| anyhow::anyhow!("source job lock poisoned"))? = job.clone();
        *self
            .recovery
            .lock()
            .map_err(|_| anyhow::anyhow!("source recovery lock poisoned"))? = job.is_none();
        self.tick(store, clock::Reason::Kernel)?;
        let registration = super::super::delivery::install_sink(store)?;
        store.retain_collection_runtime(self.installer.clone(), registration)?;
        store
            .smalltalk
            .ivm_agent_service
            .set(self.clone())
            .map_err(|_| anyhow::anyhow!("agent service already retained"))?;
        Ok(())
    }

    fn start(&self, tx: &Transaction<'_>) -> Result<String> {
        self.installer.start(
            tx,
            cards::VIEW,
            Limits {
                page_rows: PAGE,
                page_bytes: 1024 * 1024,
                pending_rows: 16384,
                pending_bytes: 16 * 1024 * 1024,
                total_rows: 1_000_000,
                callback_ms: 1000,
                lifetime_ms: 60 * 60 * 1000,
            },
            now_ms_u64()?,
        )
    }

    fn begin_recovery(&self, tx: &Transaction<'_>, epoch: u64) -> Result<()> {
        let capture = super::super::status(tx)?;
        let retained = self.installer.status(tx, cards::VIEW)?;
        ensure!(
            retained.compatible
                && retained.source.fingerprint == self.fingerprint
                && retained.source.epoch == epoch
                && capture.epoch == epoch
                && capture.fingerprint == self.fingerprint,
            "source recovery identity/epoch/operator mismatch"
        );
        ensure!(
            capture.gap.as_deref()
                != Some(
                    "capture identity or source descriptor changed; explicit replacement required"
                ),
            "source recovery descriptor changed"
        );
        ensure!(
            capture.pending_rows <= 256 && capture.pending_bytes <= 1024 * 1024,
            "source recovery staging range exceeds fixed bound"
        );
        self.views.fence_all(
            tx,
            "fresh source baseline and namespace publication pending",
        )?;
        self.installer
            .restore_source(tx, &retained.source, &self.fingerprint, epoch)?;
        // The writer is exclusive: this fixed pre-recovery range precedes all future paired
        // capture. A new full PK scan covers its current source, never replays it as managed.
        let through: u64 = tx.query_row(
            "SELECT COALESCE(max(sequence),0) FROM st3_ivm_capture",
            [],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM st3_ivm_capture WHERE sequence IN (SELECT sequence FROM st3_ivm_capture WHERE sequence<=?1 ORDER BY sequence LIMIT 256)",[through])?;
        ensure!(
            tx.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM st3_ivm_capture)",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            "source recovery staging range incomplete"
        );
        tx.execute("DELETE FROM st3_ivm_insert_before", [])?;
        tx.execute("DELETE FROM st3_ivm_update_before", [])?;
        tx.execute(
            "UPDATE st3_ivm_capture_state SET gap=NULL,guarded=1 WHERE singleton=1",
            [],
        )?;
        self.views
            .install_gap_trigger(tx, "st3_ivm_capture_state", "gap")?;
        self.views
            .register_installed(tx, &self.installer, cards::VIEW)?;
        Ok(())
    }

    fn reclaim_page(&self, store: &Store) -> Result<bool> {
        let job: Option<String> = store.readers.get().query_row(
            "SELECT j.id FROM ivm_install_jobs j WHERE view=?1 AND phase IN ('stopped','published') AND NOT EXISTS(SELECT 1 FROM ivm_install_roots r WHERE r.namespace=j.id) ORDER BY j.id LIMIT 1",
            [cards::VIEW],|r|r.get(0)).optional()?;
        let Some(job) = job else { return Ok(false) };
        let done = store
            .connection
            .batched(|tx| self.installer.reclaim(tx, &job, PAGE))
            .map_err(anyhow::Error::msg)??;
        if done {
            self.handles
                .lock()
                .map_err(|_| anyhow::anyhow!("namespace identity lock poisoned"))?
                .remove(&job);
        }
        Ok(true)
    }

    fn recovery_page(&self, store: &Store) -> Result<bool> {
        if self.reclaim_page(store)? {
            return Ok(true);
        }
        let removed = store
            .connection
            .batched(|tx| self.installer.prune_journal(tx, SOURCE, PAGE))
            .map_err(anyhow::Error::msg)??;
        if removed > 0 {
            return Ok(true);
        }
        let job = store
            .connection
            .batched(|tx| self.start(tx))
            .map_err(anyhow::Error::msg)??;
        *self
            .job
            .lock()
            .map_err(|_| anyhow::anyhow!("source job lock poisoned"))? = Some(job);
        *self
            .recovery
            .lock()
            .map_err(|_| anyhow::anyhow!("source recovery lock poisoned"))? = false;
        Ok(true)
    }

    fn prepare(&self, tx: &Transaction<'_>) -> Result<()> {
        super::super::scope::prepare(tx)
    }
    fn finalize(&self, tx: &Transaction<'_>) -> Result<()> {
        let stopped = self
            .stopped
            .lock()
            .map_err(|_| anyhow::anyhow!("source stop lock poisoned"))?
            .clone();
        if let Some(reason) = stopped {
            return self.fence_stopped(tx, &reason);
        }
        let state = super::super::status(tx)?;
        if !self.schema_current(tx)? || state.fingerprint != self.fingerprint {
            super::super::gap(tx, "agent source schema or receiver binding changed")?;
        }
        if state.gap.is_none() && state.pending_rows > 0 {
            let clock_rows: u64 = tx.query_row(
                "SELECT COUNT(*) FROM st3_ivm_capture WHERE source_table='local_agent_card_clock'",
                [],
                |r| r.get(0),
            )?;
            if clock_rows > 1 {
                super::super::gap(
                    tx,
                    "multiple captured maintenance clocks exceed transaction budget",
                )?;
            }
            if clock_rows == 0 {
                if let Err(error) = clock::tick(tx, crate::store::now_ms(), clock::Reason::Kernel) {
                    super::super::gap(
                        tx,
                        &format!("captured maintenance clock unsupported: {error}"),
                    )?;
                }
            }
        }
        let changed = super::super::status(tx)?.pending_rows > 0;
        let outcome =
            super::super::scope::finalize(tx, &self.views, &self.installer, SOURCE, |_, _| {
                Ok(super::super::scope::Coverage::Complete)
            })?;
        if changed && outcome == super::super::scope::Coverage::Complete {
            let local: u64 = tx.query_row(
                "SELECT local_generation FROM st3_agent_source_service WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            let next = local
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .context("native local source generation exhausted")?;
            tx.execute(
                "UPDATE st3_agent_source_service SET local_generation=?1 WHERE singleton=1",
                [next],
            )?;
        }
        // Coverage/source publication is separate from merely dispatching captured input.
        Ok(())
    }

    fn schema_current(&self, c: &Connection) -> Result<bool> {
        let version: u64 = c.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
        let expected:Option<u64>=c.query_row("SELECT schema_version FROM st3_agent_source_service WHERE singleton=1 AND receiver=?1",[&self.receiver],|r|r.get(0)).optional()?;
        // During first explicit setup this private marker is not yet initialized.
        Ok(expected.is_some_and(|expected| expected == 0 || expected == version))
    }

    /// One finite page/tick per call. The daemon runs this on a blocking maintenance worker.
    pub(crate) fn pump(&self) -> Result<bool> {
        let store = self.store.upgrade().context("agent Store closed")?;
        if let Some(reason) = self
            .stopped
            .lock()
            .map_err(|_| anyhow::anyhow!("source stop lock poisoned"))?
            .as_ref()
        {
            anyhow::bail!("agent collection source unavailable: {reason}");
        }
        ensure!(
            super::super::status(&store.readers.get())?.gap.is_none(),
            "source gap requires explicit fresh namespace recovery"
        );
        if store.replication_projection_deferred() {
            return Ok(false);
        }
        let recovering = *self
            .recovery
            .lock()
            .map_err(|_| anyhow::anyhow!("source recovery lock poisoned"))?;
        if recovering {
            return self.recovery_page(&store);
        }
        if !store.replication_projection_deferred() {
            let behind=store.read_snapshot(|_| {
                let c=store.readers.get();
                let index=smallclaims::store::current_index(&c)?;
                let frontier:Option<(String,u64)>=c.query_row("SELECT status,last_good_store_index FROM projection_health WHERE aggregate='graph'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                Ok::<_,anyhow::Error>(frontier.is_some_and(|(state,through)|state=="healthy" && through<index))
            })?;
            if behind {
                // Invoke the actual native projector. Never rewrite its frontier metadata.
                // Its captured legacy replacements must still drain before publication.
                return store.project_replication_backlog();
            }
        }
        let job = self
            .job
            .lock()
            .map_err(|_| anyhow::anyhow!("source job lock poisoned"))?
            .clone();
        if let Some(job) = job {
            let progress = self.installer.progress(&store.readers.get(), &job)?;
            match progress.phase.as_str() {
                "scan" => {
                    let page = store.read_snapshot(|_| {
                        extract::scan_page_for(
                            &store.readers.get(),
                            &self.installer,
                            &job,
                            &self.receiver,
                            PAGE,
                            1024 * 1024,
                        )
                    })?;
                    store
                        .connection
                        .batched(|tx| self.installer.scan(tx, &page, now_ms_u64()?))
                        .map_err(anyhow::Error::msg)??;
                    return Ok(true);
                }
                "catchup" => {
                    let position = self.installer.position(&store.readers.get(), SOURCE)?;
                    if progress.applied_revision != position.revision || progress.queued_rows != 0 {
                        store
                            .connection
                            .batched(|tx| self.installer.catch_up(tx, &job, now_ms_u64()?))
                            .map_err(anyhow::Error::msg)??;
                        return Ok(true);
                    }
                    let namespace = self.handle(&job)?;
                    if self.refresh_producer(&store, &namespace)? {
                        return Ok(true);
                    }
                    if !self.try_publish(&store, &namespace, Some(&job))? {
                        return self.tick_work(&store, &namespace);
                    }
                }
                "published" => {
                    *self
                        .job
                        .lock()
                        .map_err(|_| anyhow::anyhow!("source job lock poisoned"))? = None;
                }
                _ => anyhow::bail!("agent source installation stopped: {}", progress.phase),
            }
        } else {
            let root = self.installer.root(&store.readers.get(), cards::VIEW)?;
            if self.refresh_producer(&store, &root.namespace)? {
                return Ok(true);
            }
            if self.current_boundary(&store)? {
                return self.reclaim_page(&store);
            }
            if !self.try_publish(&store, &root.namespace, None)? {
                return self.tick_work(&store, &root.namespace);
            }
        }
        Ok(true)
    }

    // One native classifier key per worker page. All five assessments acknowledge the same
    // producer recipient; no recipient enumeration or window-only producer proof is used.
    fn refresh_producer(&self, store: &Store, ns: &Namespace) -> Result<bool> {
        let registration = store
            .smalltalk
            .ivm_delivery_source
            .get()
            .context("producer registration missing")?;
        let requests = kernel::producer_requests(
            &store.readers.get(),
            ns,
            registration.epoch(),
            now_ms_u64()?,
            1,
        )?;
        let Some((recipient, _)) = requests.first() else {
            return Ok(false);
        };
        for driver in ["claude", "codex", "opencode", "pi", "omp"] {
            let captured = crate::api::delivery_presence::source::capture(recipient, driver)?;
            super::super::delivery::commit(store, &captured)?;
        }
        Ok(true)
    }

    // Same-epoch producer revocation can precede any successful SQL sink write. A refused
    // whole-source certificate therefore schedules a finite indexed native-classifier sweep;
    // selected public rows never decide which producer inputs are covered.
    fn retry_producer(&self, store: &Store, ns: &Namespace) -> Result<()> {
        let mut after = self
            .producer_after
            .lock()
            .map_err(|_| anyhow::anyhow!("producer cursor poisoned"))?;
        let recipient:Option<String>=store.readers.get().query_row(
            "SELECT agent FROM local_agent_card_source_native WHERE namespace=?1 AND agent>?2 ORDER BY agent,driver LIMIT 1",
            params![ns.as_str(),&*after],|r|r.get(0)).optional()?;
        let Some(recipient) = recipient else {
            after.clear();
            return Ok(());
        };
        *after = recipient.clone();
        drop(after);
        for driver in ["claude", "codex", "opencode", "pi", "omp"] {
            let captured = crate::api::delivery_presence::source::capture(&recipient, driver)?;
            super::super::delivery::commit(store, &captured)?;
        }
        Ok(())
    }

    fn current_boundary(&self, store: &Store) -> Result<bool> {
        store.read_snapshot(|_| {
            let reader = store.readers.get();
            let c = &*reader;
            if !super::super::scope::readable(c)? || !self.schema_current(c)? {
                return Ok(false);
            }
            let Ok(cut) = self.current_cut(c, store) else {
                return Ok(false);
            };
            let Ok(root) = self.views.installed_root(c, &self.installer, cards::VIEW) else {
                return Ok(false);
            };
            if smallclaims::ivm::source_cut(c)? != Some(cut) {
                return Ok(false);
            }
            if !cards::coverage(c, &root, &cut, crate::store::now_ms())? {
                return Ok(false);
            }
            let Some(certificate) = boundary::read(c, &root, &cut, &boundary::manifest())? else {
                return Ok(false);
            };
            Ok(producer::read_boundary(&certificate, || Ok(true)).unwrap_or(false))
        })
    }

    fn handle(&self, job: &str) -> Result<Namespace> {
        self.handles
            .lock()
            .map_err(|_| anyhow::anyhow!("namespace identity lock poisoned"))?
            .get(job)
            .cloned()
            .context("staging identity not observed; restart requires fresh qualified namespace")
    }
    fn tick(&self, store: &Store, reason: clock::Reason) -> Result<()> {
        store
            .connection
            .batched(|tx| clock::tick(tx, crate::store::now_ms(), reason))
            .map_err(anyhow::Error::msg)??;
        Ok(())
    }

    // Only real queue acknowledgments/cursor advances justify immediate continuation.
    // An unchanged pending source gets one tick, then waits for new input or a due deadline.
    fn tick_work(&self, store: &Store, ns: &Namespace) -> Result<bool> {
        let position = self.installer.position(&store.readers.get(), SOURCE)?;
        let waiting = self
            .waiting
            .lock()
            .map_err(|_| anyhow::anyhow!("source wait lock poisoned"))?
            .clone();
        if waiting.as_ref() == Some(&position) {
            let c = store.readers.get();
            let captured: String = c.query_row(
                "SELECT at FROM local_agent_card_source_clock WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get(0),
            )?;
            let captured = captured.parse::<u128>()?;
            let now = crate::store::now_ms();
            if kernel::next_deadline(&c, ns)?
                .is_none_or(|deadline| deadline <= captured || now < deadline)
            {
                return Ok(false);
            }
        }
        let before = super::work_progress::counter(&store.readers.get(), ns)?;
        self.tick(store, clock::Reason::Kernel)?;
        let c = store.readers.get();
        let progressed = super::work_progress::counter(&c, ns)? > before;
        *self
            .waiting
            .lock()
            .map_err(|_| anyhow::anyhow!("source wait lock poisoned"))? = if progressed {
            None
        } else {
            Some(self.installer.position(&c, SOURCE)?)
        };
        Ok(progressed)
    }

    fn current_cut(&self, c: &Connection, store: &Store) -> Result<SourceCut> {
        ensure!(
            self.schema_current(c)? && super::super::status(c)?.fingerprint == self.fingerprint,
            "native source schema/binding changed"
        );
        ensure!(
            !store.replication_projection_deferred(),
            "native projection deferred"
        );
        let index = smallclaims::store::current_index(c)?;
        let health:Option<(String,u64)>=c.query_row("SELECT status,last_good_store_index FROM projection_health WHERE aggregate='graph'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        // Actual native projection coverage is required for consumed legacy dependencies;
        // absence of an unhealthy row and a largest numeric index are never this evidence.
        ensure!(
            index == 0
                || health.is_some_and(|(state, through)| state == "healthy" && through == index),
            "native projection prefix not certified current"
        );
        ensure!(
            !c.query_row(
                "SELECT EXISTS(SELECT 1 FROM projection_health WHERE status<>'healthy')",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            "native dependency projection unavailable"
        );
        let previous = smallclaims::ivm::source_cut(c)?.context("source graph identity missing")?;
        let local_generation = c.query_row(
            "SELECT local_generation FROM st3_agent_source_service WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        Ok(SourceCut {
            epoch: previous.epoch,
            admitted: index,
            projected: index,
            local_generation,
        })
    }

    fn try_publish(&self, store: &Arc<Store>, ns: &Namespace, job: Option<&str>) -> Result<bool> {
        let candidate = store.read_snapshot(|_| -> Result<_> {
            let reader = store.readers.get();
            let c = &*reader;
            if !super::super::scope::readable(c)? {
                return Ok(None);
            }
            let position = self.installer.position(c, SOURCE)?;
            if position.fingerprint != self.fingerprint {
                return Ok(None);
            }
            if let Some(job) = job {
                let p = self.installer.progress(c, job)?;
                if p.phase != "catchup"
                    || p.applied_revision != position.revision
                    || p.queued_rows != 0
                {
                    return Ok(None);
                }
            }
            let footprint = match kernel::footprint(c, ns, &self.receiver) {
                Ok(f) => f,
                Err(_) => return Ok(None),
            };
            let cut = match self.current_cut(c, store) {
                Ok(cut) => cut,
                Err(_) => return Ok(None),
            };
            let at: String = c.query_row(
                "SELECT at FROM local_agent_card_source_clock WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get(0),
            )?;
            Ok(Some((position, cut, footprint, at.parse::<u128>()?)))
        })?;
        let Some((position, cut, footprint, at)) = candidate else {
            return Ok(false);
        };
        let certificate = match producer::capture_boundary(
            ns.as_str(),
            &boundary::manifest(),
            footprint.earliest_monotonic_deadline,
            &footprint.files,
        ) {
            Ok(c) => c,
            Err(_) => {
                self.retry_producer(store, ns)?;
                return Ok(false);
            }
        };
        producer::commit_boundary(&certificate, |certificate| {
            store
                .connection
                .batched(|tx| {
                    ensure!(
                        self.installer.position(tx, SOURCE)? == position
                            && self.current_cut(tx, store)? == cut,
                        "namespace source changed before publication"
                    );
                    kernel::certify_coverage(tx, ns, &self.receiver, &position, &cut, at)?;
                    if let Some(job) = job {
                        ensure!(
                            matches!(
                                self.views.catch_up_installed(
                                    tx,
                                    &self.installer,
                                    job,
                                    &position,
                                    cut,
                                    now_ms_u64()?
                                )?,
                                Outcome::Published
                            ),
                            "staged source did not publish exact namespace"
                        );
                    } else {
                        let keys = super::row_changes::page(tx, ns)?;
                        let root = self.installer.root(tx, cards::VIEW)?;
                        let bound_generation: u64 = tx.query_row(
                            "SELECT generation FROM ivm_installed_bindings WHERE view=?1",
                            [cards::VIEW],
                            |r| r.get(0),
                        )?;
                        let changes = match &keys {
                            Some(keys)
                                if root.generation == bound_generation && keys.is_empty() =>
                            {
                                Changed::Keys(keys)
                            }
                            Some(keys)
                                if !keys.is_empty() && root.generation > bound_generation =>
                            {
                                Changed::Keys(keys)
                            }
                            _ => Changed::Refresh,
                        };
                        ensure!(
                            self.views.sync_installed(
                                tx,
                                &self.installer,
                                cards::VIEW,
                                &position,
                                cut,
                                changes
                            )? == smallclaims::ivm::installed::SyncOutcome::Current,
                            "live installed namespace remains fenced"
                        );
                    }
                    let root = self
                        .views
                        .installed_root(tx, &self.installer, cards::VIEW)?;
                    ensure!(
                        root.namespace == *ns,
                        "published namespace identity mismatch"
                    );
                    boundary::persist(tx, &self.installer, cards::VIEW, &root, &cut, certificate)?;
                    super::row_changes::ack(tx, ns)?;
                    Ok::<_, anyhow::Error>(())
                })
                .map_err(anyhow::Error::msg)??;
            Ok(())
        })?;
        Ok(true)
    }
}
// Inspect the persisted physical schema before native open-time helpers can write. A
// changed schema/receiver/graph epoch requires another binding lifecycle, never auto-adoption.
fn preflight_reopen(path: &Path, receiver: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let c = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='st3_agent_source_service')",[],|r|r.get(0))?;
    if !exists {
        return Ok(false);
    }
    let (version, bound): (u64, String) = c.query_row(
        "SELECT schema_version,receiver FROM st3_agent_source_service WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let actual: u64 = c.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    let capture = super::super::status(&c)?;
    let cut = smallclaims::ivm::source_cut(&c)?.context("retained graph identity missing")?;
    ensure!(
        version == actual
            && bound == receiver
            && capture.fingerprint == super::capture_fingerprint_for(receiver)?
            && capture.epoch == cut.epoch,
        "retained agent capture schema/receiver/epoch requires explicit binding replacement"
    );
    // A same-index canonical correction can leave consumed projected desired/mission/step
    // tables stale. Only the actual native replay closes that dependency after a raw/fenced
    // lifetime; a healthy scalar frontier or no-op backlog check cannot do so.
    let fenced: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM ivm_views WHERE name=?1 AND ready=0)",
        [cards::VIEW],
        |r| r.get(0),
    )?;
    Ok(capture.gap.is_some() || fenced)
}

fn now_ms_u64() -> Result<u64> {
    u64::try_from(crate::store::now_ms()).context("native clock exceeds producer range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallclaims::ivm::{Readiness, events};

    fn settle(store: &Store) {
        for _ in 0..128 {
            store.maintain_agent_collections().unwrap();
            let service = store.smalltalk.ivm_agent_service.get().unwrap();
            if service.current_boundary(store).unwrap() {
                return;
            }
        }
        let c = store.readers.get();
        let service = store.smalltalk.ivm_agent_service.get().unwrap();
        let job = service.job.lock().unwrap().clone();
        panic!(
            "source did not close: job={:?}, capture={:?}",
            job.map(|job| service.installer.progress(&c, &job).unwrap()),
            super::super::super::status(&c).unwrap()
        );
    }

    #[test]
    fn observation_retention_and_count_trim_keep_managed_capture_bounded() {
        let _lock = TEST_LOCK.blocking_lock();
        let directory = tempfile::tempdir().unwrap();
        let store =
            Store::open_with_agent_collections(&directory.path().join("trim.sqlite"), "node")
                .unwrap();
        settle(&store);
        let append = |entry: usize| {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: "agent/node.trim".into(),
                    kind: "harness.timeline".into(),
                    actor: Some("agent/node.trim".into()),
                    fields: BTreeMap::from([
                        ("operation".into(), serde_json::json!("append")),
                        (
                            "entry_id".into(),
                            serde_json::json!(format!("entry-{entry}")),
                        ),
                        ("revision".into(), serde_json::json!(1)),
                        ("role".into(), serde_json::json!("assistant")),
                        ("entry_type".into(), serde_json::json!("content")),
                        ("final".into(), serde_json::json!(true)),
                        (
                            "body".into(),
                            serde_json::json!({"media_type":"text/plain","text":"trim control"}),
                        ),
                        ("driver".into(), serde_json::json!("codex")),
                        ("incarnation_id".into(), serde_json::json!("inc-trim")),
                        ("sequence".into(), serde_json::json!(entry)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("trim:{entry}")),
                })
                .unwrap();
        };
        for entry in 0..260 {
            append(entry);
        }
        assert_eq!(
            store
                .trim_local_observations(u128::MAX, 5000, 5000)
                .unwrap(),
            259
        );
        assert!(super::super::super::scope::readable(&store.readers.get()).unwrap());
        settle(&store);
        for entry in 260..520 {
            append(entry);
        }
        assert_eq!(store.trim_local_observations(0, 2, 5000).unwrap(), 259);
        assert!(super::super::super::scope::readable(&store.readers.get()).unwrap());
        settle(&store);
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT COUNT(*) FROM local_observations WHERE subject='agent/node.trim'",
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn native_source_publication_pending_commit_and_raw_refusal_use_real_store() {
        let _lock = TEST_LOCK.blocking_lock();
        let directory = tempfile::tempdir().unwrap();
        let store =
            Store::open_with_agent_collections(&directory.path().join("source.sqlite"), "node")
                .unwrap();
        let views = store.ivm_views().unwrap();
        assert!(store.has_agent_collection_source());
        assert!(!matches!(
            events::capture(&store.readers.get(), &views, cards::VIEW)
                .unwrap()
                .availability
                .readiness,
            Readiness::Ready(_)
        ));
        settle(&store);
        let before = events::capture(&store.readers.get(), &views, cards::VIEW).unwrap();
        assert!(matches!(before.availability.readiness, Readiness::Ready(_)));
        let publisher = store.ivm_publisher().unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &publisher,
            &store.ivm_publisher().unwrap().unwrap()
        ));
        store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/node.amber".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([
                    ("status".into(), serde_json::json!("running")),
                    ("runtime_id".into(), serde_json::json!("node.amber")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let pending = events::capture(&store.readers.get(), &views, cards::VIEW).unwrap();
        assert!(!matches!(
            pending.availability.readiness,
            Readiness::Ready(_)
        ));
        assert!(
            !store
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&store)
                .unwrap()
        );
        settle(&store);
        let ready = events::capture(&store.readers.get(), &views, cards::VIEW).unwrap();
        assert!(matches!(ready.availability.readiness, Readiness::Ready(_)));
        assert!(ready.source_cut.projected > before.source_cut.projected);
        assert!(super::super::receiver_readable(&store.readers.get(), "node").unwrap());
        assert!(!super::super::receiver_readable(&store.readers.get(), "other-node").unwrap());
        // Journal boundary control uses the genuine published namespace and synthetic
        // uncommitted derived rows. It never fabricates a Root/cut/coverage certificate or
        // serves these rows; rollback must preserve the prior certified public output.
        let service = store.smalltalk.ivm_agent_service.get().unwrap();
        let root = service
            .views
            .installed_root(&store.readers.get(), &service.installer, cards::VIEW)
            .unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        for index in 0..1024 {
            tx.execute("INSERT INTO local_agent_card_rows SELECT namespace,?2,name,state,current,key_generation,body,body_hash,request_time,number_bits FROM local_agent_card_rows WHERE namespace=?1 AND agent='agent/node.amber'",params![root.namespace.as_str(),format!("agent/key-control-{index:04}")]).unwrap();
        }
        assert_eq!(
            super::super::row_changes::page(&tx, &root.namespace)
                .unwrap()
                .unwrap()
                .len(),
            1024
        );
        // Replacing an already journaled ID at capacity must not force a refresh.
        tx.execute("UPDATE local_agent_card_rows SET key_generation=key_generation+1 WHERE namespace=?1 AND agent='agent/key-control-0000'",[root.namespace.as_str()]).unwrap();
        assert_eq!(
            super::super::row_changes::page(&tx, &root.namespace)
                .unwrap()
                .unwrap()
                .len(),
            1024
        );
        tx.execute("INSERT INTO local_agent_card_rows SELECT namespace,'agent/key-control-overflow',name,state,current,key_generation,body,body_hash,request_time,number_bits FROM local_agent_card_rows WHERE namespace=?1 AND agent='agent/node.amber'",[root.namespace.as_str()]).unwrap();
        assert!(
            super::super::row_changes::page(&tx, &root.namespace)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM st3_agent_row_changes WHERE namespace=?1",
                [root.namespace.as_str()],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            1024
        );
        tx.rollback().unwrap();
        drop(writer);
        assert!(
            super::super::row_changes::page(&store.readers.get(), &root.namespace)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        // A managed same-index predecessor replacement introduces an unresolved parent.
        // Waiting must not repeatedly tick the clock; restoration is a new captured input.
        let claim: (String,String) = store.readers.get().query_row(
            "SELECT id,predecessors FROM claims WHERE subject='agent/node.amber' AND kind='runtime.observed' LIMIT 1", [],
            |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE claims SET predecessors='[\"missing-parent\"]' WHERE id=?1",
                    [&claim.0],
                )
            })
            .unwrap()
            .unwrap();
        let mut waited = false;
        for _ in 0..64 {
            if !store.maintain_agent_collections().unwrap() {
                waited = true;
                break;
            }
        }
        assert!(
            waited,
            "pending missing parent never entered bounded waiting"
        );
        let revision: u64 = store
            .readers
            .get()
            .query_row("SELECT revision FROM local_agent_card_clock", [], |r| {
                r.get(0)
            })
            .unwrap();
        for _ in 0..32 {
            assert!(!store.maintain_agent_collections().unwrap());
        }
        assert_eq!(
            store
                .readers
                .get()
                .query_row("SELECT revision FROM local_agent_card_clock", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            revision
        );
        assert!(
            !store
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&store)
                .unwrap()
        );
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE claims SET predecessors=?2 WHERE id=?1",
                    params![claim.0, claim.1],
                )
            })
            .unwrap()
            .unwrap();
        // Same-index canonical changes conservatively fence the installed registry.
        // Restoring the physical input cannot revive the old root through sync_installed.
        assert!(!service.current_boundary(&store).unwrap());

        // Raw DDL invalidates reads immediately, before any managed prepare can notice it.
        store
            .connection
            .write()
            .execute_batch("CREATE TABLE raw_schema_probe(value TEXT)")
            .unwrap();
        assert!(!super::super::receiver_readable(&store.readers.get(), "node").unwrap());
        assert!(
            !store
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&store)
                .unwrap()
        );
    }
    #[test]
    fn reopen_raw_gap_and_interrupted_scan_publish_fresh_namespace_without_resetting_identity() {
        let _lock = TEST_LOCK.blocking_lock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reopen.sqlite");
        let first = Store::open_with_agent_collections(&path, "node").unwrap();
        first
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/node.reopen".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), serde_json::json!("running")),
                    ("runtime_id".into(), serde_json::json!("node.reopen")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        settle(&first);
        let service = first.smalltalk.ivm_agent_service.get().unwrap();
        let before = service
            .views
            .installed_root(&first.readers.get(), &service.installer, cards::VIEW)
            .unwrap();
        let cut = smallclaims::ivm::source_cut(&first.readers.get())
            .unwrap()
            .unwrap();
        let position = service
            .installer
            .position(&first.readers.get(), SOURCE)
            .unwrap();
        // A raw, same-value source replacement still loses managed capture coverage.
        first
            .connection
            .write()
            .execute(
                "UPDATE claims SET predecessors=predecessors WHERE subject='agent/node.reopen'",
                [],
            )
            .unwrap();
        assert!(!super::super::receiver_readable(&first.readers.get(), "node").unwrap());
        drop(first);
        let interrupted = Store::open_with_agent_collections(&path, "node").unwrap();
        assert!(
            !interrupted
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&interrupted)
                .unwrap()
        );
        // Reach a new active scan, then close without completing it. Restart must reclaim
        // that stopped unattached namespace with bounded pages before another scan starts.
        for _ in 0..128 {
            interrupted.maintain_agent_collections().unwrap();
            if interrupted
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .job
                .lock()
                .unwrap()
                .is_some()
            {
                break;
            }
        }
        assert!(
            interrupted
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .job
                .lock()
                .unwrap()
                .is_some()
        );
        interrupted.maintain_agent_collections().unwrap();
        drop(interrupted);
        let recovered = Store::open_with_agent_collections(&path, "node").unwrap();
        assert!(
            !recovered
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&recovered)
                .unwrap()
        );
        settle(&recovered);
        let service = recovered.smalltalk.ivm_agent_service.get().unwrap();
        let after = service
            .views
            .installed_root(&recovered.readers.get(), &service.installer, cards::VIEW)
            .unwrap();
        assert_ne!(after.namespace, before.namespace);
        assert!(after.generation >= before.generation);
        let after_cut = smallclaims::ivm::source_cut(&recovered.readers.get())
            .unwrap()
            .unwrap();
        assert_eq!(after_cut.epoch, cut.epoch);
        assert!(
            after_cut.admitted >= cut.admitted
                && after_cut.projected >= cut.projected
                && after_cut.local_generation >= cut.local_generation
        );
        let after_position = service
            .installer
            .position(&recovered.readers.get(), SOURCE)
            .unwrap();
        assert_eq!(after_position.epoch, position.epoch);
        assert_eq!(after_position.fingerprint, position.fingerprint);
        assert!(after_position.revision >= position.revision);
        // Drain the now-unattached formerly published namespace; its journal/state rows
        // must vanish without touching the current namespace or its certified boundary.
        for _ in 0..128 {
            if !recovered.maintain_agent_collections().unwrap() {
                break;
            }
        }
        assert!(
            !recovered
                .readers
                .get()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM ivm_install_jobs WHERE id=?1)",
                    [before.namespace.as_str()],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert!(service.current_boundary(&recovered).unwrap());
        drop(recovered);
        let unavailable = Store::open_with_agent_collections(&path, "other-node").unwrap();
        assert!(unavailable.has_agent_collection_source());
        assert!(unavailable.maintain_agent_collections().is_err());
        assert!(
            !unavailable
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&unavailable)
                .unwrap()
        );
        // Source refusal must not abort native daemon admission.
        unavailable
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/other-node.safe".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), serde_json::json!("running"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        drop(unavailable);
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch("CREATE TABLE unrecognized_schema_change(value TEXT)")
            .unwrap();
        drop(raw);
        let unavailable = Store::open_with_agent_collections(&path, "node").unwrap();
        assert!(unavailable.maintain_agent_collections().is_err());
        assert!(
            !unavailable
                .smalltalk
                .ivm_agent_service
                .get()
                .unwrap()
                .current_boundary(&unavailable)
                .unwrap()
        );
    }
}
