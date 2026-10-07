//! Private producer-wide certificate bound to one complete namespace/root/graph cut.
//! The operator derives the full file footprint/deadline from dependency indexes. Capture and
//! commit_boundary run outside Writer; this SQL helper never acknowledges a producer or Ready.
use crate::api::delivery_presence::source::boundary::Certificate;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use smallclaims::ivm::{
    SourceCut,
    install::{Installer, Namespace, Root},
};

const MAX_BYTES: usize = 256 * 1024;

pub fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS local_agent_source_boundary(
        namespace TEXT PRIMARY KEY,manifest TEXT NOT NULL,source_epoch INTEGER NOT NULL,
        source_revision INTEGER NOT NULL,root_generation INTEGER NOT NULL,status_revision INTEGER NOT NULL,
        graph_epoch INTEGER NOT NULL,projected INTEGER NOT NULL,local_generation INTEGER NOT NULL,
        certificate TEXT NOT NULL)")?;
    Ok(())
}

pub(crate) fn persist(
    tx: &Transaction<'_>,
    installer: &Installer,
    view: &str,
    root: &Root,
    cut: &SourceCut,
    certificate: &Certificate,
) -> Result<()> {
    let current = installer.root(tx, view)?;
    ensure!(
        current.namespace == root.namespace
            && current.epoch == root.epoch
            && current.revision == root.revision
            && current.generation == root.generation
            && current.status_revision == root.status_revision,
        "producer boundary namespace/root changed"
    );
    let capture = super::super::status(tx)?;
    let managed: bool = tx.query_row(
        "SELECT guarded=1 AND managed=1 FROM st3_ivm_capture_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        managed && capture.gap.is_none() && super::super::clean(tx)?,
        "producer boundary requires clean managed source scope"
    );
    ensure!(
        smallclaims::ivm::source_cut(tx)? == Some(*cut),
        "producer boundary graph cut changed"
    );
    ensure!(
        certificate.namespace == root.namespace.as_str(),
        "producer boundary namespace mismatch"
    );
    ensure!(
        cut.admitted == cut.projected,
        "producer boundary graph prefix pending"
    );
    let encoded = serde_json::to_string(certificate)?;
    ensure!(
        encoded.len() <= MAX_BYTES,
        "producer namespace certificate byte budget exceeded"
    );
    tx.execute("INSERT INTO local_agent_source_boundary VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
        ON CONFLICT(namespace) DO UPDATE SET manifest=excluded.manifest,source_epoch=excluded.source_epoch,
        source_revision=excluded.source_revision,root_generation=excluded.root_generation,
        status_revision=excluded.status_revision,graph_epoch=excluded.graph_epoch,projected=excluded.projected,
        local_generation=excluded.local_generation,certificate=excluded.certificate",
        params![root.namespace.as_str(),certificate.manifest,root.epoch,root.revision,root.generation,
            root.status_revision,cut.epoch,cut.projected,cut.local_generation,encoded])?;
    Ok(())
}

/// Capture with installed_root, coverage and rows inside the authorized snapshot. Callers must
/// run source::read_boundary/read_boundary_rows around their actual read; SQL equality is not
/// producer readiness, and selected row certificates cannot replace this whole-namespace guard.
pub(crate) fn read(
    connection: &Connection,
    root: &Root,
    cut: &SourceCut,
    manifest: &str,
) -> Result<Option<Certificate>> {
    if !super::super::scope::readable(connection)? || cut.admitted != cut.projected {
        return Ok(None);
    }
    let encoded: Option<String> = connection
        .query_row(
            "SELECT certificate FROM local_agent_source_boundary
        WHERE namespace=?1 AND manifest=?2 AND source_epoch=?3 AND source_revision=?4
        AND root_generation=?5 AND status_revision=?6 AND graph_epoch=?7 AND projected=?8
        AND local_generation=?9",
            params![
                root.namespace.as_str(),
                manifest,
                root.epoch,
                root.revision,
                root.generation,
                root.status_revision,
                cut.epoch,
                cut.projected,
                cut.local_generation
            ],
            |r| r.get(0),
        )
        .optional()?;
    encoded
        .map(|encoded| {
            ensure!(
                encoded.len() <= MAX_BYTES,
                "oversized producer namespace certificate"
            );
            let certificate: Certificate = serde_json::from_str(&encoded)?;
            ensure!(
                certificate.namespace == root.namespace.as_str()
                    && certificate.manifest == manifest,
                "producer boundary payload binding mismatch"
            );
            Ok(certificate)
        })
        .transpose()
}

pub fn reclaim(tx: &Transaction<'_>, namespace: &Namespace, limit: usize) -> Result<(usize, bool)> {
    ensure!(
        (1..=128).contains(&limit),
        "invalid producer boundary reclaim budget"
    );
    let used = tx.execute(
        "DELETE FROM local_agent_source_boundary WHERE namespace=?1",
        [namespace.as_str()],
    )?;
    Ok((used, true))
}
