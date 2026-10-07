//! Whole-producer guard for namespace coverage, including collection acknowledgements with no rows.
//!
//! The Store operator proves complete initial backfill, exact native/active membership and the
//! full source manifest. It supplies the complete UNIQUE file footprint and earliest source
//! deadline from namespace dependency indexes. Selected row evidence, SQL aggregate counts, or
//! this guard alone cannot establish that proof. No recipient enumeration happens on this path.

use super::*;

const MAX_FILES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileEvidence {
    pub(crate) path: String,
    pub(crate) identity: String,
}

impl super::Certificate {
    pub(crate) fn followed_file(&self) -> Option<FileEvidence> {
        self.follows.as_ref().map(|(path, identity)| FileEvidence {
            path: path.clone(),
            identity: identity.clone(),
        })
    }
}

/// Persist privately beside complete namespace coverage. The manifest identifies the exact
/// classifier/dependency contract; the independent Installer root and graph cut remain mandatory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Certificate {
    pub(crate) namespace: String,
    pub(crate) manifest: String,
    pub(crate) epoch: String,
    pub(crate) revision: u64,
    pub(crate) evaluation_time_ms: u64,
    pub(crate) watermark_ns: u64,
    pub(crate) deadline_ns: Option<u64>,
    pub(crate) files: Vec<FileEvidence>,
}

/// Invalidate global coverage before a normalized-row transaction, including early clock refreshes
/// with unchanged report revisions. Its acknowledgement cannot overtake an outstanding projection.
pub(super) struct Projection<'a> {
    presence: &'a Presence,
}

impl<'a> Projection<'a> {
    pub(super) fn begin(presence: &'a Presence) -> Result<Self> {
        let mut state = presence
            .source
            .lock()
            .map_err(|_| anyhow!("delivery source lock poisoned"))?;
        ensure!(
            state.owner.is_some() && !state.exhausted,
            "delivery source not installed or exhausted"
        );
        let Some(active) = state.projection_inflight.checked_add(1) else {
            state.exhausted = true;
            state.invalidate();
            bail!("delivery projection count exhausted");
        };
        state.invalidate();
        ensure!(!state.exhausted, "delivery global revision exhausted");
        state.projection_inflight = active;
        Ok(Self { presence })
    }
}

impl Drop for Projection<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.presence.source.lock() {
            state.projection_inflight = state.projection_inflight.saturating_sub(1);
        }
    }
}

fn ready(state: &State) -> Result<&Owner> {
    let owner = state
        .owner
        .as_ref()
        .ok_or_else(|| anyhow!("delivery source not installed"))?;
    ensure!(
        !state.exhausted
            && state.memory_inflight == 0
            && state.projection_inflight == 0
            && state.pending == 0
            && state.failures == 0,
        "delivery producer boundary pending, inflight, failed or exhausted"
    );
    Ok(owner)
}

fn validate_fields(certificate: &Certificate) -> Result<()> {
    ensure!(
        !certificate.namespace.is_empty()
            && certificate.namespace.len() <= 1024
            && !certificate.manifest.is_empty()
            && certificate.manifest.len() <= 2048,
        "delivery boundary requires bounded exact namespace/manifest"
    );
    ensure!(
        certificate.files.len() <= MAX_FILES,
        "delivery namespace file footprint exhausted"
    );
    let mut previous: Option<&str> = None;
    for file in &certificate.files {
        ensure!(
            !file.path.is_empty()
                && file.path.len() <= MAX_RECIPIENT_BYTES
                && !file.identity.is_empty()
                && file.identity.len() <= 512,
            "delivery file evidence unbounded or unknown"
        );
        ensure!(
            previous.is_none_or(|prior| prior < file.path.as_str()),
            "delivery file footprint must be unique and sorted"
        );
        previous = Some(&file.path);
    }
    Ok(())
}

fn state_check(presence: &Presence, certificate: &Certificate, committed: bool) -> Result<()> {
    let state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    let owner = ready(&state)?;
    ensure!(
        owner.epoch == certificate.epoch && state.global_revision == certificate.revision,
        "delivery producer boundary changed"
    );
    let now = ns(Instant::now()
        .checked_duration_since(presence.started)
        .ok_or_else(|| anyhow!("delivery clock precedes startup"))?)?;
    ensure!(
        now >= certificate.watermark_ns
            && certificate
                .deadline_ns
                .is_none_or(|deadline| now < deadline),
        "delivery namespace deadline expired"
    );
    if committed {
        ensure!(
            !state.boundary_fault && state.boundary.as_ref() == Some(certificate),
            "delivery namespace coverage not acknowledged"
        );
    }
    Ok(())
}

fn files_check(presence: &Presence, certificate: &Certificate) -> Result<()> {
    for file in &certificate.files {
        if !file_identity(&file.path).is_ok_and(|identity| identity == file.identity) {
            if let Ok(mut state) = presence.source.lock()
                && state
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.epoch == certificate.epoch)
                && state.global_revision == certificate.revision
            {
                state.invalidate();
                state.boundary_fault = true;
            }
            bail!("delivery namespace followed file changed or unavailable");
        }
    }
    Ok(())
}

/// Call only after the operator has proved complete namespace membership and normalized every
/// accepted relevant input (or committed a proved exclusion). None deadline is valid only when
/// the complete namespace has no time-dependent inputs. Max64 UNIQUE file paths is a hard refusal.
pub(crate) fn capture_boundary(
    namespace: &str,
    manifest: &str,
    deadline_ns: Option<u64>,
    files: &[FileEvidence],
) -> Result<Certificate> {
    capture_in(presence(), namespace, manifest, deadline_ns, files)
}

fn capture_in(
    presence: &Presence,
    namespace: &str,
    manifest: &str,
    deadline_ns: Option<u64>,
    files: &[FileEvidence],
) -> Result<Certificate> {
    ensure!(
        files.len() <= MAX_FILES,
        "delivery namespace file footprint exhausted"
    );
    let (epoch, revision, watermark_ns) = {
        let state = presence
            .source
            .lock()
            .map_err(|_| anyhow!("delivery source lock poisoned"))?;
        let owner = ready(&state)?;
        (
            owner.epoch.clone(),
            state.global_revision,
            ns(Instant::now().duration_since(presence.started))?,
        )
    };
    let certificate = Certificate {
        namespace: namespace.into(),
        manifest: manifest.into(),
        epoch,
        revision,
        evaluation_time_ms: wall_ms()?,
        watermark_ns,
        deadline_ns,
        files: files.to_vec(),
    };
    validate_fields(&certificate)?;
    state_check(presence, &certificate, false)?;
    files_check(presence, &certificate)?;
    state_check(presence, &certificate, false)?;
    Ok(certificate)
}

/// Atomically commit this exact evidence with the owner's complete namespace coverage record.
/// The closure runs outside source locks. Caller preserves dirty/retry through post-commit ack.
pub(crate) fn commit_boundary(
    certificate: &Certificate,
    commit: impl FnOnce(&Certificate) -> Result<()>,
) -> Result<()> {
    commit_in(presence(), certificate, commit)
}

fn commit_in(
    presence: &Presence,
    certificate: &Certificate,
    commit: impl FnOnce(&Certificate) -> Result<()>,
) -> Result<()> {
    validate_fields(certificate)?;
    state_check(presence, certificate, false)?;
    files_check(presence, certificate)?;
    state_check(presence, certificate, false)?;
    commit(certificate)?;
    files_check(presence, certificate)?;
    state_check(presence, certificate, false)?;
    let mut state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    let owner = ready(&state)?;
    ensure!(
        owner.epoch == certificate.epoch && state.global_revision == certificate.revision,
        "delivery producer changed at boundary acknowledgement"
    );
    let now = ns(Instant::now().duration_since(presence.started))?;
    ensure!(
        now >= certificate.watermark_ns
            && certificate
                .deadline_ns
                .is_none_or(|deadline| now < deadline),
        "delivery namespace deadline expired at acknowledgement"
    );
    state.boundary_fault = false;
    state.boundary = Some(certificate.clone());
    Ok(())
}

/// Mandatory even when no semantic keys are returned. Per-row read_certified remains mandatory;
/// both guards wrap the same authorized SQL snapshot, plus its independent Installer/membership
/// coverage. This guard checks at most64 unique files and scalar counters, with no recipient scan.
pub(crate) fn read_boundary<T>(
    certificate: &Certificate,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    read_in(presence(), certificate, read)
}

fn read_in<T>(
    presence: &Presence,
    certificate: &Certificate,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    validate_fields(certificate)?;
    state_check(presence, certificate, true)?;
    files_check(presence, certificate)?;
    state_check(presence, certificate, true)?;
    let result = read()?;
    files_check(presence, certificate)?;
    state_check(presence, certificate, true)?;
    Ok(result)
}

/// Explicit durable native/active namespace classifier proof for an irrelevant recipient. No
/// automatic exclusion exists. The closure commits that proof/removal under the same Store cut;
/// a stale Change or concurrent replacement cannot clear pending source work.
pub(crate) fn commit_exclusion(
    change: &Change,
    commit: impl FnOnce(&Change) -> Result<()>,
) -> Result<()> {
    exclusion_in(presence(), change, commit)
}

fn exclusion_in(
    presence: &Presence,
    change: &Change,
    commit: impl FnOnce(&Change) -> Result<()>,
) -> Result<()> {
    let check = || -> Result<()> {
        let state = presence
            .source
            .lock()
            .map_err(|_| anyhow!("delivery source lock poisoned"))?;
        let owner = state
            .owner
            .as_ref()
            .ok_or_else(|| anyhow!("delivery source not installed"))?;
        let entry = state
            .entries
            .get(&change.recipient)
            .ok_or_else(|| anyhow!("delivery recipient absent"))?;
        ensure!(
            !state.exhausted
                && owner.epoch == change.epoch
                && entry.active == 0
                && !entry.exhausted
                && entry.revision == change.revision,
            "delivery exclusion producer changed"
        );
        Ok(())
    };
    check()?;
    let _projection = Projection::begin(presence)?;
    commit(change)?;
    check()?;
    let mut state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    let owner = state
        .owner
        .as_ref()
        .ok_or_else(|| anyhow!("delivery source not installed"))?;
    let entry = state
        .entries
        .get(&change.recipient)
        .ok_or_else(|| anyhow!("delivery recipient absent"))?;
    ensure!(
        !state.exhausted
            && owner.epoch == change.epoch
            && entry.active == 0
            && !entry.exhausted
            && entry.revision == change.revision,
        "delivery exclusion changed at acknowledgement"
    );
    state.acknowledge(&change.recipient, None);
    Ok(())
}

/// Combined guard for materialized rows. It authenticates every row certificate and its exact
/// footprint membership, then checks the COMPLETE deduplicated namespace files at both boundaries.
/// This avoids repeating metadata syscalls for each selected row. Root/membership proof is still
/// independent; empty rows are permitted only because read_boundary certifies the producer cut.
pub(crate) fn read_boundary_rows<T>(
    boundary: &Certificate,
    rows: &[super::Certificate],
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    read_rows_in(presence(), boundary, rows, read)
}

fn read_rows_in<T>(
    presence: &Presence,
    boundary: &Certificate,
    rows: &[super::Certificate],
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ensure!(
        rows.len() <= MAX_READ_KEYS,
        "delivery row evidence exhausted"
    );
    for row in rows {
        ensure!(
            row.epoch == boundary.epoch
                && boundary
                    .deadline_ns
                    .is_some_and(|deadline| deadline <= row.deadline_ns),
            "delivery namespace deadline does not cover selected row"
        );
        if let Some(file) = row.followed_file() {
            let position = boundary
                .files
                .binary_search_by(|expected| expected.path.cmp(&file.path))
                .map_err(|_| anyhow!("selected row missing from complete file footprint"))?;
            ensure!(
                boundary.files[position] == file,
                "selected row file identity differs from namespace footprint"
            );
        }
    }
    read_in(presence, boundary, || {
        check_batch(presence, rows, Instant::now)?;
        let result = read()?;
        check_batch(presence, rows, Instant::now)?;
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::{record_in, record_legacy_in};
    use super::super::tests::{captured, fixture, poll};
    use super::*;

    const RECIPIENT: &str = "agent/source-control";
    const DRIVERS: [&str; 5] = ["claude", "codex", "opencode", "pi", "omp"];

    fn normalize(p: &Presence) -> Vec<super::super::Certificate> {
        DRIVERS
            .into_iter()
            .map(|driver| super::super::commit_in(p, &captured(p, driver), |_| Ok(())).unwrap())
            .collect()
    }
    fn cover(p: &Presence, rows: &[super::super::Certificate]) -> Certificate {
        let deadline = rows.iter().map(|row| row.deadline_ns).min();
        let mut files: Vec<_> = rows.iter().filter_map(|row| row.followed_file()).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup();
        let certificate = capture_in(
            p,
            "namespace-source-control",
            "full-native-clock-manifest.v1",
            deadline,
            &files,
        )
        .unwrap();
        commit_in(p, &certificate, |_| Ok(())).unwrap();
        certificate
    }

    #[test]
    fn silent_ack_denies_new_recipient_and_requires_all_five_driver_normalizations() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        let empty = cover(&p, &[]);
        assert!(read_in(&p, &empty, || Ok(Vec::<String>::new())).is_ok());
        poll(&p);
        assert!(read_in(&p, &empty, || Ok(Vec::<String>::new())).is_err());
        for driver in DRIVERS.into_iter().take(4) {
            super::super::commit_in(&p, &captured(&p, driver), |_| Ok(())).unwrap();
            assert!(capture_in(&p, "ns", "manifest", None, &[]).is_err());
        }
        let fifth = super::super::commit_in(&p, &captured(&p, "omp"), |_| Ok(())).unwrap();
        assert_eq!(p.source.lock().unwrap().pending, 0);
        let current = cover(&p, &[fifth]);
        assert!(read_in(&p, &current, || Ok(Vec::<String>::new())).is_ok());
        record_legacy_in(&p, "agent/new-silent-member", "native", 7);
        assert!(read_in(&p, &current, || Ok(Vec::<String>::new())).is_err());
    }

    #[test]
    fn accepted_replacement_and_early_clock_projection_invalidate_global_boundary_before_change() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let rows = normalize(&p);
        let certificate = cover(&p, &rows);
        let mutation = Mutation::begin(&p, RECIPIENT);
        assert!(read_in(&p, &certificate, || Ok(())).is_err());
        mutation.finish(true);
        let rows = normalize(&p);
        let current = cover(&p, &rows);
        let projection = Projection::begin(&p).unwrap();
        assert!(read_in(&p, &current, || Ok(())).is_err());
        assert!(capture_in(&p, "ns", "manifest", None, &[]).is_err());
        drop(projection);
        assert!(read_in(&p, &current, || Ok(())).is_err());
        assert!(read_in(&p, &cover(&p, &rows), || Ok(())).is_ok());
    }

    #[test]
    fn failure_inflight_epoch_and_revision_exhaustion_refuse_global_coverage() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| bail!("sink failed"))).unwrap();
        poll(&p);
        assert!(capture_in(&p, "ns", "manifest", None, &[]).is_err());
        let rows = normalize(&p);
        let old = cover(&p, &rows);
        assert!(read_in(&p, &old, || Ok(())).is_ok());
        let projection = Projection::begin(&p).unwrap();
        assert!(commit_in(&p, &old, |_| Ok(())).is_err());
        drop(projection);
        p.source.lock().unwrap().global_revision = u64::MAX;
        poll(&p);
        assert!(capture_in(&p, "ns", "manifest", None, &[]).is_err());
        let fresh = fixture();
        let _fresh = install_in(&fresh, Arc::new(|_| Ok(()))).unwrap();
        assert!(read_in(&fresh, &old, || Ok(())).is_err());
    }

    #[test]
    fn namespace_ack_and_snapshot_reject_a_concurrent_replacement_without_delivering_rows() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let rows = normalize(&p);
        let certificate = cover(&p, &rows);
        assert!(
            read_in(&p, &certificate, || {
                poll(&p);
                Ok("stale namespace")
            })
            .is_err()
        );
        let rows = normalize(&p);
        let candidate = capture_in(&p, "ns", "manifest", Some(rows[0].deadline_ns), &[]).unwrap();
        assert!(
            commit_in(&p, &candidate, |_| {
                poll(&p);
                Ok(())
            })
            .is_err()
        );
        assert!(read_in(&p, &candidate, || Ok(())).is_err());
    }

    #[test]
    fn exclusions_need_explicit_durable_classifier_and_current_change() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let change = {
            let state = p.source.lock().unwrap();
            Change {
                epoch: state.owner.as_ref().unwrap().epoch.clone(),
                recipient: RECIPIENT.into(),
                revision: state.entries[RECIPIENT].revision,
            }
        };
        assert!(exclusion_in(&p, &change, |_| bail!("classifier transaction failed")).is_err());
        assert!(capture_in(&p, "ns", "manifest", None, &[]).is_err());
        poll(&p);
        assert!(exclusion_in(&p, &change, |_| Ok(())).is_err());
        let latest = {
            let state = p.source.lock().unwrap();
            Change {
                revision: state.entries[RECIPIENT].revision,
                ..change
            }
        };
        exclusion_in(&p, &latest, |_| Ok(())).unwrap();
        assert_eq!(p.source.lock().unwrap().pending, 0);
        let empty = cover(&p, &[]);
        assert!(read_in(&p, &empty, || Ok(())).is_ok());
    }

    #[test]
    fn complete_file_footprint_is_bounded_and_file_replacement_fences_silent_ack() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("st");
        std::fs::write(&path, "first").unwrap();
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        record_in(
            &p,
            RECIPIENT,
            &serde_json::json!({"transport":"native","image":"new","ready":true,"follows":path})
                .to_string(),
        );
        let rows = normalize(&p);
        let certificate = cover(&p, &rows);
        assert_eq!(certificate.files.len(), 1);
        assert!(read_in(&p, &certificate, || Ok(Vec::<String>::new())).is_ok());
        assert!(
            capture_in(
                &p,
                "ns",
                "manifest",
                None,
                &vec![certificate.files[0].clone(); MAX_FILES + 1]
            )
            .is_err()
        );
        let replacement = directory.path().join("new");
        std::fs::write(&replacement, "second").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(read_in(&p, &certificate, || Ok(Vec::<String>::new())).is_err());
        assert!(p.source.lock().unwrap().boundary_fault);
        let rows = normalize(&p);
        let recovered = cover(&p, &rows);
        assert!(read_in(&p, &recovered, || Ok(())).is_ok());
    }

    #[test]
    fn deduplicated_namespace_files_cover_rows_without_selected_only_footprint_inference() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("st");
        std::fs::write(&path, "image").unwrap();
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        record_in(
            &p,
            RECIPIENT,
            &serde_json::json!({"transport":"native","image":"new","follows":path}).to_string(),
        );
        let rows = normalize(&p);
        let certificate = cover(&p, &rows);
        assert!(read_rows_in(&p, &certificate, &rows, || Ok("five rows")).is_ok());
        let mut omitted = certificate.clone();
        omitted.files.clear();
        assert!(read_rows_in(&p, &omitted, &rows, || Ok(())).is_err());
        let mut extended = certificate.clone();
        extended.deadline_ns = None;
        assert!(read_rows_in(&p, &extended, &rows, || Ok(())).is_err());
        assert!(read_rows_in(&p, &certificate, &[], || Ok(())).is_ok());
    }

    #[test]
    fn real_store_namespace_coverage_transaction_failure_never_acknowledges() {
        let store = crate::store::Store::open_memory("global-delivery-control").unwrap();
        store.connection.write().execute_batch("CREATE TABLE namespace_delivery_fixture(namespace TEXT PRIMARY KEY,certificate TEXT);").unwrap();
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let rows = normalize(&p);
        let candidate = capture_in(
            &p,
            "ns",
            "manifest",
            Some(rows.iter().map(|r| r.deadline_ns).min().unwrap()),
            &[],
        )
        .unwrap();
        assert!(
            commit_in(&p, &candidate, |certificate| {
                let mut writer = store.connection.write();
                let transaction = writer.transaction()?;
                transaction.execute(
                    "INSERT INTO namespace_delivery_fixture VALUES (?1,?2)",
                    rusqlite::params![certificate.namespace, serde_json::to_string(certificate)?],
                )?;
                bail!("rollback coverage")
            })
            .is_err()
        );
        assert!(read_in(&p, &candidate, || Ok(())).is_err());
        commit_in(&p, &candidate, |certificate| {
            let mut writer = store.connection.write();
            let transaction = writer.transaction()?;
            transaction.execute(
                "INSERT INTO namespace_delivery_fixture VALUES (?1,?2)",
                rusqlite::params![certificate.namespace, serde_json::to_string(certificate)?],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
        let persisted: String = read_in(&p, &candidate, || {
            Ok(store.readers.get().query_row(
                "SELECT certificate FROM namespace_delivery_fixture WHERE namespace='ns'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Certificate>(&persisted).unwrap(),
            candidate
        );
    }
}
