//! Typed publication/read bridge for namespace operators in the existing Views/event registry.
//! No extraction, admission, migration or authority policy is supplied here. The source owner
//! certifies the graph cut independently of Installer's mutation revision. Register explicitly
//! before installation; reads/startup never install or recover. Every live source transaction
//! must synchronize its registered namespace or fence both representations.
use super::install::{Installer, Outcome, Root, SourcePosition};
use super::{Readiness, SourceCut, Views, fence_error, note_key_change};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::BTreeSet;

pub(super) const LAYOUT: &str = "smallclaims.ivm.installed-binding.v1";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_installed_bindings(
 view TEXT PRIMARY KEY, operator_fingerprint TEXT NOT NULL,
 source TEXT NOT NULL, source_fingerprint TEXT NOT NULL, source_epoch INTEGER NOT NULL,
 namespace TEXT, revision INTEGER NOT NULL, generation INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ivm_installed_by_source ON ivm_installed_bindings(source,view);
CREATE TRIGGER IF NOT EXISTS ivm_installed_source_status AFTER UPDATE ON ivm_install_sources
WHEN (OLD.revision<>NEW.revision OR OLD.available<>NEW.available OR OLD.epoch<>NEW.epoch OR OLD.fingerprint<>NEW.fingerprint)
 AND EXISTS(SELECT 1 FROM ivm_installed_bindings WHERE source=NEW.name)
 AND EXISTS(SELECT 1 FROM ivm_install_deferred WHERE source=NEW.name)
BEGIN
 UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
 INSERT INTO ivm_view_status(view,sequence) SELECT view,(SELECT sequence FROM ivm_status_frontier WHERE singleton=1)
 FROM ivm_installed_bindings WHERE source=NEW.name
 ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
"#;

/// Exact semantic keys (including removed/old memberships), or an explicit authorized
/// bounded-window refresh. Neither variant preserves intermediate historical occurrences.
pub enum Changed<'a> {
    Keys(&'a [String]),
    Refresh,
}

/// Live sync does not advance a fenced view or reject valid admission merely for missing output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Current,
    Fenced,
}

fn schema_exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT COUNT(*)=3 FROM sqlite_schema WHERE type='table' AND name IN ('ivm_installed_bindings','ivm_install_roots','ivm_install_sources')", [], |r| r.get(0))?)
}

/// Used by ordinary Views readiness/event capture, even when the caller omits installed_root.
/// None means the namespace certificate is current; no state is repaired by this read.
pub(super) fn readiness(
    db: &Connection,
    view: &str,
    source: &str,
    raw: &str,
) -> Result<Option<Readiness>> {
    if !schema_exists(db)? {
        return Ok(Some(Readiness::Missing));
    }
    let expected = Installer::expected_fingerprint(view, source, raw)?;
    let state = db
        .query_row(
            "SELECT CASE
          WHEN b.operator_fingerprint<>?2 OR b.source<>?3 OR r.fingerprint<>b.operator_fingerprint
            OR r.source<>b.source OR r.source_fingerprint<>b.source_fingerprint
            OR s.fingerprint<>b.source_fingerprint THEN 1
          WHEN r.epoch<>b.source_epoch OR s.epoch<>b.source_epoch THEN 2
          WHEN r.ready=0 OR s.available=0 THEN 3
          WHEN b.namespace IS NULL OR r.namespace IS NOT b.namespace OR r.revision<>b.revision
            OR s.revision<>b.revision OR r.generation<>b.generation THEN 4
          ELSE 0 END
         FROM ivm_installed_bindings b JOIN ivm_install_roots r ON r.view=b.view
         JOIN ivm_install_sources s ON s.name=b.source WHERE b.view=?1",
            params![view, expected, source],
            |r| r.get::<_, u8>(0),
        )
        .optional()?;
    Ok(match state {
        Some(0) => None,
        Some(1) => Some(Readiness::VersionMismatch),
        Some(2) => Some(Readiness::EpochMismatch),
        Some(3) => Some(Readiness::Fenced),
        Some(_) => Some(Readiness::SourcePending),
        None => Some(Readiness::Missing),
    })
}

fn atomic<T>(tx: &Transaction<'_>, f: impl FnOnce() -> Result<T>) -> Result<T> {
    tx.execute_batch("SAVEPOINT ivm_installed_bridge")?;
    match f() {
        Ok(value) => {
            tx.execute_batch("RELEASE ivm_installed_bridge")?;
            Ok(value)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO ivm_installed_bridge; RELEASE ivm_installed_bridge")?;
            Err(error)
        }
    }
}

impl Views {
    fn installed_definition<'a>(
        &self,
        installer: &'a Installer,
        view: &str,
    ) -> Result<(&'a str, &'a str)> {
        let declared = self
            .views
            .iter()
            .find(|v| v.definition().name == view)
            .context("unknown IVM view")?;
        let source = declared
            .installed_source()
            .context("legacy view is not namespace installable")?;
        let (actual, raw, fingerprint) = installer.binding_definition(view)?;
        ensure!(
            source == actual && declared.definition().fingerprint == raw,
            "installed operator/view definition mismatch"
        );
        Ok((actual, fingerprint))
    }

    /// Explicitly bind a namespace-aware compiled view/operator/source before an install.
    /// Matching existing registration is a no-op; incompatible registration is an error.
    /// This fences output and never adopts an already-published legacy/unregistered root.
    pub fn register_installed(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        view: &str,
    ) -> Result<()> {
        let (source, fingerprint) = self.installed_definition(installer, view)?;
        let position = installer.position(tx, source)?;
        let index = self
            .views
            .iter()
            .position(|v| v.definition().name == view)
            .context("unknown IVM view")?;
        let stored: String = tx.query_row(
            "SELECT fingerprint FROM ivm_views WHERE name=?1",
            [view],
            |r| r.get(0),
        )?;
        ensure!(
            stored == self.fingerprints[index],
            "registered view version mismatch"
        );
        atomic(tx, || {
            tx.execute_batch(SCHEMA)?;
            let previous=tx.query_row("SELECT operator_fingerprint,source,source_fingerprint,source_epoch FROM ivm_installed_bindings WHERE view=?1",[view],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?))).optional()?;
            if let Some(previous) = previous {
                ensure!(
                    previous
                        == (
                            fingerprint.into(),
                            source.into(),
                            position.fingerprint.clone(),
                            position.epoch
                        ),
                    "installed binding replaced; explicit lifecycle transition required"
                );
                return Ok(());
            }
            ensure!(
                tx.query_row("SELECT COUNT(*) FROM ivm_installed_bindings", [], |r| r
                    .get::<_, u64>(0))?
                    < 256,
                "installed view registry exceeds 256"
            );
            tx.execute(
                "INSERT INTO ivm_installed_bindings VALUES(?1,?2,?3,?4,?5,NULL,0,0)",
                params![
                    view,
                    fingerprint,
                    source,
                    position.fingerprint,
                    position.epoch
                ],
            )?;
            fence_error(
                tx,
                view,
                &anyhow::anyhow!("namespace installation required"),
            )?;
            Ok(())
        })
    }

    fn check_installed(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        view: &str,
        expected: &SourcePosition,
        cut: SourceCut,
    ) -> Result<()> {
        let (source, fingerprint) = self.installed_definition(installer, view)?;
        let current = installer.position(tx, source)?;
        ensure!(&current == expected, "installed source position changed");
        let binding:(String,String,String,u64)=tx.query_row("SELECT operator_fingerprint,source,source_fingerprint,source_epoch FROM ivm_installed_bindings WHERE view=?1",[view],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(
            binding
                == (
                    fingerprint.into(),
                    source.into(),
                    current.fingerprint,
                    current.epoch
                ),
            "installed binding identity mismatch"
        );
        cut.validate()?;
        let index = self
            .views
            .iter()
            .position(|v| v.definition().name == view)
            .context("unknown IVM view")?;
        let stored_fingerprint: String = tx.query_row(
            "SELECT fingerprint FROM ivm_views WHERE name=?1",
            [view],
            |r| r.get(0),
        )?;
        ensure!(
            stored_fingerprint == self.fingerprints[index],
            "registered view version mismatch"
        );
        let stored_epoch: u64 =
            tx.query_row("SELECT epoch FROM ivm_views WHERE name=?1", [view], |r| {
                r.get(0)
            })?;
        ensure!(
            cut.epoch == stored_epoch
                && cut.admitted == cut.projected
                && cut.projected == crate::store::current_index(tx)?,
            "installed graph cut pending or incompatible"
        );
        Ok(())
    }

    /// Finish one bounded Installer page and publish only a namespace completed HERE. The
    /// source owner must attest that expected_position and cut describe the same complete
    /// source snapshot/prefix/local dependencies. Neither revision is inferred from the other.
    /// New publication is explicit recovery; a preexisting ready root cannot clear a fence.
    pub fn catch_up_installed(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        job: &str,
        expected_position: &SourcePosition,
        cut: SourceCut,
        now_ms: u64,
    ) -> Result<Outcome> {
        let view = installer.progress(tx, job)?.view;
        self.check_installed(tx, installer, &view, expected_position, cut)?;
        atomic(tx, || {
            let outcome = installer.catch_up(tx, job, now_ms)?;
            if let Outcome::Stopped(reason) = &outcome
                && !installer.status(tx, &view)?.ready
            {
                self.fence(tx, &view, reason)?;
            }
            if outcome == Outcome::Published {
                let root = installer.root(tx, &view)?;
                ensure!(
                    root.namespace.as_str() == job,
                    "published namespace mismatch"
                );
                self.mirror_installed(tx, &view, &root, cut, Changed::Refresh, true)?;
            }
            Ok(outcome)
        })
    }

    /// Publish a precomputed page without operator callbacks. Partial progress remains
    /// SourcePending; only a fully caught-up root is mirrored. The supplied graph cut is
    /// separately certified by the native source owner, never inferred from the queue.
    pub fn publish_prepared_installed(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        page: &super::install::prepared::PreparedPage,
        expected_position: &SourcePosition,
        cut: SourceCut,
        changed: Changed<'_>,
        now_ms: u64,
    ) -> Result<Outcome> {
        let view = page.view();
        self.check_installed(tx, installer, view, expected_position, cut)?;
        atomic(tx, || {
            let outcome = installer.publish_prepared(tx, page, now_ms)?;
            if outcome == Outcome::Published {
                let root = installer.root(tx, view)?;
                ensure!(
                    root.namespace == *page.namespace(),
                    "prepared published namespace mismatch"
                );
                self.mirror_installed(tx, view, &root, cut, Changed::Refresh, true)?;
            } else if installer.status(tx, view)?.ready {
                let root = installer.root(tx, view)?;
                if root.namespace == *page.namespace() {
                    self.mirror_installed(tx, view, &root, cut, changed, false)?;
                }
            }
            Ok(outcome)
        })
    }

    /// Certify live maintenance of the SAME already-active namespace, in its source transaction.
    /// This never clears a later fence. Keys are complete semantic old/new memberships, not
    /// merely affected input keys. An explicit refresh wakes a bounded authorized window.
    pub fn sync_installed(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        view: &str,
        expected_position: &SourcePosition,
        cut: SourceCut,
        changed: Changed<'_>,
    ) -> Result<SyncOutcome> {
        self.installed_definition(installer, view)?;
        // Logical unavailability must not reject otherwise valid source admission or
        // advance the view certificate. Storage/transaction errors still propagate.
        let ready: bool =
            tx.query_row("SELECT ready FROM ivm_views WHERE name=?1", [view], |r| {
                r.get(0)
            })?;
        if !ready {
            return Ok(SyncOutcome::Fenced);
        }
        let status = installer.status(tx, view)?;
        if !status.ready {
            self.fence(
                tx,
                view,
                status
                    .error
                    .as_deref()
                    .unwrap_or("installed source unavailable or pending"),
            )?;
            return Ok(SyncOutcome::Fenced);
        }
        self.check_installed(tx, installer, view, expected_position, cut)?;
        let root = installer.root(tx, view)?;
        match atomic(tx, || {
            self.mirror_installed(tx, view, &root, cut, changed, false)
        }) {
            Ok(()) => Ok(SyncOutcome::Current),
            Err(error) if error.chain().any(|cause| cause.is::<rusqlite::Error>()) => Err(error),
            Err(error) => {
                self.fence(tx, view, &format!("{error:#}"))?;
                Ok(SyncOutcome::Fenced)
            }
        }
    }

    fn mirror_installed(
        &self,
        tx: &Transaction<'_>,
        view: &str,
        root: &Root,
        cut: SourceCut,
        changed: Changed<'_>,
        publication: bool,
    ) -> Result<()> {
        let (namespace, generation, revision): (Option<String>, u64, u64) = tx.query_row(
            "SELECT namespace,generation,revision FROM ivm_installed_bindings WHERE view=?1",
            [view],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            root.generation >= generation,
            "installed generation moved backwards"
        );
        let (ready, view_generation): (bool, u64) = tx.query_row(
            "SELECT ready,generation FROM ivm_views WHERE name=?1",
            [view],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !publication {
            ensure!(
                ready && namespace.as_deref() == Some(root.namespace.as_str()),
                "installed view fenced or namespace replaced; fresh installation required"
            );
            let has_error: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ivm_view_errors WHERE view=?1)",
                [view],
                |r| r.get(0),
            )?;
            ensure!(!has_error, "installed view gap requires fresh installation");
        } else {
            ensure!(
                namespace.as_deref() != Some(root.namespace.as_str()),
                "fresh namespace publication required"
            );
        }
        let delta = root.generation - generation;
        let keys = match &changed {
            Changed::Keys(keys) => *keys,
            Changed::Refresh => &[],
        };
        ensure!(
            keys.len() <= 1024
                && keys.iter().all(|k| !k.is_empty() && k.len() <= 4096)
                && keys.iter().map(|k| k.len()).sum::<usize>() <= 1024 * 1024,
            "installed changed-key bound exceeded"
        );
        ensure!(
            keys.iter().collect::<BTreeSet<_>>().len() == keys.len(),
            "duplicate installed semantic key"
        );
        ensure!(
            delta > 0 || keys.is_empty(),
            "semantic keys supplied without a changed root"
        );
        ensure!(
            delta == 0 || !keys.is_empty() || matches!(changed, Changed::Refresh),
            "changed installed root requires keys or explicit refresh"
        );
        let next = view_generation
            .checked_add(delta.max(u64::from(publication)))
            .filter(|n| *n <= i64::MAX as u64)
            .context("installed semantic generation overflow")?;
        self.publish_cut(tx, cut)?;
        tx.execute("UPDATE ivm_installed_bindings SET namespace=?2,revision=?3,generation=?4 WHERE view=?1",params![view,root.namespace.as_str(),root.revision,root.generation])?;
        tx.execute("UPDATE ivm_views SET ready=1,generation=?2,applied_claim_index=?3,applied_local_generation=?4,deferred_claim_index=0,deferred_local_generation=0 WHERE name=?1",params![view,next,cut.projected,cut.local_generation])?;
        if publication {
            tx.execute("DELETE FROM ivm_view_errors WHERE view=?1", [view])?;
        }
        for key in keys {
            note_key_change(tx, view, key)?;
        }
        if publication
            || revision != root.revision
            || (delta > 0 && matches!(changed, Changed::Refresh))
        {
            tx.execute(
                "UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1",
                [],
            )?;
            tx.execute("INSERT INTO ivm_view_status(view,sequence) SELECT ?1,sequence FROM ivm_status_frontier WHERE singleton=1 ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence",[view])?;
        }
        Ok(())
    }

    /// Capture the exact read namespace with ordinary token/readiness in the SAME snapshot
    /// as returned rows and events::capture. Never query unscoped operator tables.
    pub fn installed_root(
        &self,
        db: &Connection,
        installer: &Installer,
        view: &str,
    ) -> Result<Root> {
        self.installed_definition(installer, view)?;
        let cut = super::source_cut(db)?.context("IVM source unready")?;
        self.token(db, view, cut.epoch)?;
        let root = installer.root(db, view)?;
        let namespace: Option<String> = db.query_row(
            "SELECT namespace FROM ivm_installed_bindings WHERE view=?1",
            [view],
            |row| row.get(0),
        )?;
        ensure!(
            namespace.as_deref() == Some(root.namespace.as_str()),
            "installed namespace changed"
        );
        Ok(root)
    }
}
