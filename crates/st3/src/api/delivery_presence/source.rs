//! Opt-in producer certificates for committed delivery-presence rows.
//!
//! The Store owner supplies the durable sink and timer. No registry, Publisher, SQL schema, or
//! route is installed here. Consumers must guard their complete authorized snapshot with
//! `read_certified`; an assessment alone does not prove source coverage. File metadata is checked
//! at both read boundaries because followed executable replacements have no report change sink.
//! The consumer loads these certificates from the same SQL snapshot as the selected rows and
//! checks that each row uses its matching evidence. After commit acknowledgement the Store owner
//! must wake/retry existing collection consumers: a SQL notification can precede acknowledgement.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Presence, assess_beat, presence};

const MAX_RECIPIENT_BYTES: usize = 1024;
const MAX_CAPTURE_BYTES: usize = 16 * 1024;
const MAX_READ_KEYS: usize = 200;

type Sink = dyn Fn(&Change) -> Result<()> + Send + Sync;

#[derive(Default)]
pub(super) struct State {
    owner: Option<Owner>,
    active_mutations: u64,
    exhausted: bool,
    entries: HashMap<String, Entry>,
}

struct Owner {
    epoch: String,
    sink: Arc<Sink>,
}

#[derive(Default)]
struct Entry {
    revision: u64,
    active: u64,
    failed: bool,
    exhausted: bool,
    committed: HashMap<String, Certificate>,
}

/// Notification of an accepted replacement, after its complete in-memory mutation. The previous
/// certificate was revoked before mutation. Callback failure leaves the source fenced; a later
/// successful explicit capture/commit is the only recovery. The callback runs outside all locks.
#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub(crate) epoch: String,
    pub(crate) recipient: String,
    pub(crate) revision: u64,
}

/// Private source evidence stored with normalized rows; never part of public card JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Certificate {
    pub(crate) epoch: String,
    pub(crate) recipient: String,
    pub(crate) driver: String,
    pub(crate) revision: u64,
    pub(crate) evaluation_time_ms: u64,
    pub(crate) watermark_ns: u64,
    pub(crate) deadline_ns: u64,
    pub(crate) next_deadline_ms: u64,
    follows: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedAssessment {
    pub(crate) assessment: Value,
    pub(crate) certificate: Certificate,
}

/// Lifetime of the exclusive sink. Dropping it revokes every certificate; reinstallation gets a
/// new epoch. A process restart cannot adopt persisted certificates from the previous process.
pub(crate) struct Registration<'a> {
    presence: &'a Presence,
    epoch: String,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.presence.source.lock()
            && state
                .owner
                .as_ref()
                .is_some_and(|owner| owner.epoch == self.epoch)
        {
            state.owner = None;
            state.entries.clear();
        }
    }
}

pub(crate) fn install_sink(
    sink: impl Fn(&Change) -> Result<()> + Send + Sync + 'static,
) -> Result<Registration<'static>> {
    install_in(presence(), Arc::new(sink))
}

fn install_in(presence: &Presence, sink: Arc<Sink>) -> Result<Registration<'_>> {
    let mut state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    ensure!(
        state.owner.is_none(),
        "delivery source already has a sink owner"
    );
    ensure!(
        state.active_mutations == 0 && !state.exhausted,
        "delivery producer still has an active mutation or callback"
    );
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(|error| anyhow!("delivery epoch: {error}"))?;
    let epoch = hex::encode(nonce);
    state.entries.clear();
    state.owner = Some(Owner {
        epoch: epoch.clone(),
        sink,
    });
    Ok(Registration { presence, epoch })
}

/// Parent producer scope. Bookkeeping never takes a Store writer lock, and no parent memory lock
/// remains held while its sink runs. A panic or poisoned map refuses subsequent certification.
pub(super) struct Mutation<'a> {
    presence: &'a Presence,
    change: Option<Change>,
    tracked: bool,
}

impl<'a> Mutation<'a> {
    pub(super) fn begin(presence: &'a Presence, recipient: &str) -> Self {
        let mut tracked = false;
        let change = presence.source.lock().ok().and_then(|mut state| {
            if let Some(active) = state.active_mutations.checked_add(1) {
                state.active_mutations = active;
                tracked = true;
            } else {
                state.exhausted = true;
                return None;
            }
            if recipient.len() > MAX_RECIPIENT_BYTES {
                return None;
            }
            let epoch = state.owner.as_ref()?.epoch.clone();
            let entry = state.entries.entry(recipient.to_owned()).or_default();
            entry.committed.clear();
            if let (Some(revision), Some(active)) =
                (entry.revision.checked_add(1), entry.active.checked_add(1))
            {
                entry.revision = revision;
                entry.active = active;
                Some(Change {
                    epoch,
                    recipient: recipient.to_owned(),
                    revision,
                })
            } else {
                entry.failed = true;
                entry.exhausted = true;
                None
            }
        });
        Self {
            presence,
            change,
            tracked,
        }
    }

    pub(super) fn finish(mut self, updated: bool) {
        let Some(change) = self.change.take() else {
            return;
        };
        let sink = self.end(&change, !updated);
        if updated && let Some(sink) = sink {
            let outcome = catch_unwind(AssertUnwindSafe(|| sink(&change)));
            if !matches!(outcome, Ok(Ok(()))) {
                // Even a callback that committed and then failed cannot leave certified coverage.
                if let Ok(mut state) = self.presence.source.lock()
                    && state
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.epoch == change.epoch)
                    && let Some(entry) = state.entries.get_mut(&change.recipient)
                {
                    entry.failed = true;
                    entry.committed.clear();
                }
                tracing::warn!(recipient = %change.recipient, "delivery normalized source callback failed; coverage fenced");
            }
        }
    }

    fn end(&self, change: &Change, failed: bool) -> Option<Arc<Sink>> {
        let mut state = self.presence.source.lock().ok()?;
        let owner = state.owner.as_ref()?;
        if owner.epoch != change.epoch {
            return None;
        }
        let sink = owner.sink.clone();
        let entry = state.entries.get_mut(&change.recipient)?;
        entry.active = entry.active.saturating_sub(1);
        entry.failed |= failed;
        Some(sink)
    }
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        if let Some(change) = self.change.take() {
            self.end(&change, true);
        }
        if self.tracked
            && let Ok(mut state) = self.presence.source.lock()
        {
            state.active_mutations = state.active_mutations.saturating_sub(1);
        }
    }
}

/// Capture outside a Store writer transaction. The returned JSON exactly follows the legacy
/// assessment, including second-changing reasons and ages. No missing file identity is certified.
pub(crate) fn capture(recipient: &str, driver: &str) -> Result<CapturedAssessment> {
    capture_at(presence(), recipient, driver, Instant::now(), wall_ms()?)
}

fn wall_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn ns(duration: Duration) -> Result<u64> {
    Ok(duration.as_nanos().try_into()?)
}

fn capture_at(
    presence: &Presence,
    recipient: &str,
    driver: &str,
    now: Instant,
    wall: u64,
) -> Result<CapturedAssessment> {
    ensure!(
        !recipient.is_empty() && recipient.len() <= MAX_RECIPIENT_BYTES,
        "unbounded delivery recipient"
    );
    ensure!(
        ["claude", "codex", "opencode", "pi", "omp"].contains(&driver),
        "unsupported delivery driver"
    );
    let image = presence
        .image
        .as_deref()
        .ok_or_else(|| anyhow!("daemon file identity unavailable"))?;
    let (epoch, revision) = {
        let mut state = presence
            .source
            .lock()
            .map_err(|_| anyhow!("delivery source lock poisoned"))?;
        let epoch = state
            .owner
            .as_ref()
            .ok_or_else(|| anyhow!("delivery source not installed"))?
            .epoch
            .clone();
        let entry = state.entries.entry(recipient.to_owned()).or_default();
        ensure!(
            entry.active == 0 && !entry.exhausted,
            "delivery replacement in progress or revision exhausted"
        );
        (epoch, entry.revision)
    };
    let mut beat = presence
        .beats
        .lock()
        .map_err(|_| anyhow!("delivery beat lock poisoned"))?
        .get(recipient)
        .map(|beat| {
            bounded_report(&beat.report)?;
            Ok::<_, anyhow::Error>((beat.at, beat.report.clone()))
        })
        .transpose()?;
    if driver == "claude" {
        let monitors = presence
            .monitors
            .lock()
            .map_err(|_| anyhow!("delivery monitor lock poisoned"))?;
        if let Some(monitor) = monitors.get(recipient)
            && monitor.report.ready == Some(false)
        {
            bounded_report(&monitor.report)?;
            beat = Some((monitor.at, monitor.report.clone()));
        }
    }
    let mut follows = None;
    if let Some((_, report)) = beat.as_mut()
        && let Some(path) = report.follows.as_deref()
    {
        ensure!(
            path.len() <= MAX_RECIPIENT_BYTES,
            "unbounded followed executable path"
        );
        let identity = file_identity(path).inspect_err(|_| {
            fence_file(presence, &epoch, recipient, revision);
        })?;
        report.follows_image = Some(identity.clone());
        follows = Some((path.to_owned(), identity));
    }
    let uptime = now
        .checked_duration_since(presence.started)
        .ok_or_else(|| anyhow!("delivery clock precedes startup"))?;
    let base = beat.as_ref().map_or(presence.started, |(at, _)| *at);
    let age = now
        .checked_duration_since(base)
        .ok_or_else(|| anyhow!("delivery clock precedes report"))?;
    // Ages/reasons change every second. Strict stale >45s additionally changes one nanosecond
    // after the 45s boundary; attachment <=10s is retained as a conservative source deadline.
    let mut expiry = base
        .checked_add(Duration::from_secs(
            age.as_secs()
                .checked_add(1)
                .ok_or_else(|| anyhow!("delivery age overflow"))?,
        ))
        .ok_or_else(|| anyhow!("delivery deadline overflow"))?;
    for offset in [
        Duration::from_secs(20),
        Duration::from_secs(45) + Duration::from_nanos(1),
        Duration::from_secs(10) + Duration::from_nanos(1),
    ] {
        let threshold_base = if offset == Duration::from_secs(20) {
            presence.started
        } else {
            base
        };
        let threshold = threshold_base
            .checked_add(offset)
            .ok_or_else(|| anyhow!("delivery deadline overflow"))?;
        if threshold > now {
            expiry = expiry.min(threshold);
        }
    }
    let assessment = assess_beat(
        uptime,
        Some(image),
        beat.map(|(_, report)| (age, report)),
        driver,
    )
    .to_value();
    ensure!(
        serde_json::to_vec(&assessment)?.len() <= MAX_CAPTURE_BYTES,
        "unbounded delivery assessment"
    );
    let certificate = Certificate {
        epoch,
        recipient: recipient.to_owned(),
        driver: driver.to_owned(),
        revision,
        evaluation_time_ms: wall,
        watermark_ns: ns(uptime)?,
        deadline_ns: ns(expiry.duration_since(presence.started))?,
        // Round down, so a wall-driven timer may run early but cannot extend coverage.
        next_deadline_ms: wall
            .checked_add(u64::try_from(expiry.duration_since(now).as_millis())?)
            .ok_or_else(|| anyhow!("delivery wall deadline overflow"))?,
        follows,
    };
    validate_current(presence, &certificate, now, false)?;
    Ok(CapturedAssessment {
        assessment,
        certificate,
    })
}

// Check borrowed report fields before cloning so normalized capture has a finite allocation
// bound even if the legacy in-memory report is large. Producer parsing is an existing API cost.
fn bounded_report(report: &super::Report) -> Result<()> {
    let lengths = [
        report.transport.as_deref(),
        report.image.as_deref(),
        report.follows.as_deref(),
        report.reason.as_deref(),
        report
            .channel
            .as_ref()
            .and_then(|channel| channel.image.as_deref()),
    ];
    let mut bytes = 0_usize;
    for field in lengths.into_iter().flatten() {
        bytes = bytes
            .checked_add(field.len())
            .ok_or_else(|| anyhow!("delivery report size overflow"))?;
        ensure!(bytes <= MAX_CAPTURE_BYTES, "unbounded delivery report");
    }
    Ok(())
}

fn file_identity(path: &str) -> Result<String> {
    Ok(st_drivers::reexec::ImageIdentity::of(std::path::Path::new(path))?.token())
}

// A discovered file failure/replacement stays fenced even if the file is restored. A fresh
// capture at the advanced producer revision, followed by a successful durable commit, recovers.
fn fence_file(presence: &Presence, epoch: &str, recipient: &str, revision: u64) {
    if let Ok(mut state) = presence.source.lock()
        && state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.epoch == epoch)
        && let Some(entry) = state.entries.get_mut(recipient)
        && entry.revision == revision
    {
        entry.failed = true;
        entry.committed.clear();
        if let Some(next) = entry.revision.checked_add(1) {
            entry.revision = next;
        } else {
            entry.exhausted = true;
        }
    }
}

fn validate_current(
    presence: &Presence,
    certificate: &Certificate,
    now: Instant,
    committed: bool,
) -> Result<()> {
    let elapsed = now
        .checked_duration_since(presence.started)
        .ok_or_else(|| anyhow!("delivery clock precedes startup"))?;
    ensure!(
        ns(elapsed)? >= certificate.watermark_ns && ns(elapsed)? < certificate.deadline_ns,
        "delivery source deadline expired"
    );
    if let Some((path, identity)) = &certificate.follows
        && !file_identity(path).is_ok_and(|current| current == *identity)
    {
        fence_file(
            presence,
            &certificate.epoch,
            &certificate.recipient,
            certificate.revision,
        );
        bail!("followed executable identity changed or unavailable");
    }
    let state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    let owner = state
        .owner
        .as_ref()
        .ok_or_else(|| anyhow!("delivery source not installed"))?;
    ensure!(!state.exhausted, "delivery producer revision exhausted");
    ensure!(
        owner.epoch == certificate.epoch,
        "delivery source epoch changed"
    );
    let entry = state
        .entries
        .get(&certificate.recipient)
        .ok_or_else(|| anyhow!("delivery recipient not captured"))?;
    ensure!(
        entry.active == 0 && !entry.exhausted && entry.revision == certificate.revision,
        "delivery source revision changed"
    );
    if committed {
        ensure!(
            !entry.failed && entry.committed.get(&certificate.driver) == Some(certificate),
            "delivery normalized commit unavailable"
        );
    }
    Ok(())
}

/// The closure must durably commit the assessment and its exact certificate together, through the
/// owner’s normal Store route. Success acknowledges only an unchanged, still-live producer. The
/// closure runs with no source lock held. SQL/source races leave persisted evidence unservable.
pub(crate) fn commit_capture(
    capture: &CapturedAssessment,
    commit: impl FnOnce(&CapturedAssessment) -> Result<()>,
) -> Result<Certificate> {
    commit_in(presence(), capture, commit)
}

fn commit_in(
    presence: &Presence,
    capture: &CapturedAssessment,
    commit: impl FnOnce(&CapturedAssessment) -> Result<()>,
) -> Result<Certificate> {
    validate_current(presence, &capture.certificate, Instant::now(), false)?;
    commit(capture)?;
    validate_current(presence, &capture.certificate, Instant::now(), false)?;
    let mut state = presence
        .source
        .lock()
        .map_err(|_| anyhow!("delivery source lock poisoned"))?;
    let owner = state
        .owner
        .as_ref()
        .ok_or_else(|| anyhow!("delivery source not installed"))?;
    ensure!(
        owner.epoch == capture.certificate.epoch,
        "delivery source epoch changed during commit"
    );
    let entry = state
        .entries
        .get_mut(&capture.certificate.recipient)
        .ok_or_else(|| anyhow!("delivery recipient not captured"))?;
    ensure!(
        entry.active == 0 && !entry.exhausted && entry.revision == capture.certificate.revision,
        "delivery source changed during commit"
    );
    entry.failed = false;
    entry.committed.insert(
        capture.certificate.driver.clone(),
        capture.certificate.clone(),
    );
    Ok(capture.certificate.clone())
}

/// Guard all source certificates used by one authorized database snapshot, including each selected
/// local card. Empty input does not certify a collection. No assessment is recomputed by this guard.
pub(crate) fn read_certified<T>(
    certificates: &[Certificate],
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    read_in(presence(), certificates, read)
}

fn read_in<T>(
    presence: &Presence,
    certificates: &[Certificate],
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ensure!(
        !certificates.is_empty() && certificates.len() <= MAX_READ_KEYS,
        "delivery read requires bounded source certificates"
    );
    for certificate in certificates {
        validate_current(presence, certificate, Instant::now(), true)?;
    }
    let result = read()?;
    for certificate in certificates {
        validate_current(presence, certificate, Instant::now(), true)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::{Beat, Report, record_fenced_in, record_in, record_legacy_in};
    use super::*;
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const RECIPIENT: &str = "agent/source-control";

    fn fixture() -> Presence {
        Presence {
            started: Instant::now() - Duration::from_secs(100),
            image: Some("new".into()),
            beats: Mutex::new(HashMap::new()),
            monitors: Mutex::new(HashMap::new()),
            source: Mutex::new(State::default()),
        }
    }
    fn captured(p: &Presence, driver: &str) -> CapturedAssessment {
        capture_at(p, RECIPIENT, driver, Instant::now(), 100_000).unwrap()
    }
    fn commit(p: &Presence, driver: &str) -> Certificate {
        commit_in(p, &captured(p, driver), |_| Ok(())).unwrap()
    }
    fn poll(p: &Presence) {
        record_in(
            p,
            RECIPIENT,
            r#"{"transport":"native","image":"new","ready":true}"#,
        );
    }

    #[test]
    fn source_is_opt_in_exclusive_and_old_process_evidence_cannot_be_adopted() {
        let p = fixture();
        poll(&p);
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        let registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        assert!(install_in(&p, Arc::new(|_| Ok(()))).is_err());
        let certificate = commit(&p, "codex");
        let persisted: Certificate =
            serde_json::from_str(&serde_json::to_string(&certificate).unwrap()).unwrap();
        assert!(read_in(&p, std::slice::from_ref(&persisted), || Ok(())).is_ok());
        drop(registration);
        assert!(read_in(&p, std::slice::from_ref(&persisted), || Ok(())).is_err());
        let _new = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        let fresh = commit(&p, "codex");
        assert_ne!(fresh.epoch, persisted.epoch);
        assert!(read_in(&p, &[persisted], || Ok(())).is_err());
        let restarted = fixture();
        let _restart = install_in(&restarted, Arc::new(|_| Ok(()))).unwrap();
        assert!(read_in(&restarted, &[fresh], || Ok(())).is_err());
    }

    #[test]
    fn every_accepted_producer_path_revokes_all_driver_certificates() {
        let p = fixture();
        let changes = Arc::new(AtomicUsize::new(0));
        let seen = changes.clone();
        let _registration = install_in(
            &p,
            Arc::new(move |change| {
                assert_eq!(change.recipient, RECIPIENT);
                assert!(!change.epoch.is_empty());
                assert!(change.revision > 0);
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
        poll(&p);
        let codex = commit(&p, "codex");
        let claude = commit(&p, "claude");
        record_in(&p, RECIPIENT, "invalid");
        assert!(read_in(&p, &[codex.clone(), claude.clone()], || Ok(())).is_ok());
        record_legacy_in(&p, RECIPIENT, "legacy", 5);
        assert!(read_in(&p, &[codex], || Ok(())).is_err());
        assert!(read_in(&p, &[claude], || Ok(())).is_err());
        let legacy = commit(&p, "codex");
        let title = crate::mailbox::Fence::new(RECIPIENT, "current", "title");
        record_fenced_in(&p, &title, r#"{"transport":"native"}"#);
        assert!(read_in(&p, std::slice::from_ref(&legacy), || Ok(())).is_ok());
        record_fenced_in(
            &p,
            &title,
            r#"{"transport":"claude-channel","ready":false,"reason":"provider absent"}"#,
        );
        assert!(read_in(&p, &[legacy], || Ok(())).is_err());
        assert_eq!(
            captured(&p, "claude").assessment["reason"],
            "provider absent"
        );
        let monitored = commit(&p, "claude");
        let delivery = crate::mailbox::Fence::new(RECIPIENT, "current", "delivery");
        record_fenced_in(
            &p,
            &delivery,
            r#"{"transport":"native","image":"new","ready":true}"#,
        );
        assert!(read_in(&p, &[monitored], || Ok(())).is_err());
        assert!(p.monitors.lock().unwrap().get(RECIPIENT).is_none());
        assert_eq!(changes.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn mutation_fences_before_memory_change_and_overlapping_updates_cannot_certify() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let old = commit(&p, "codex");
        let first = Mutation::begin(&p, RECIPIENT);
        assert!(read_in(&p, &[old], || Ok(())).is_err());
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        let second = Mutation::begin(&p, RECIPIENT);
        second.finish(true);
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        first.finish(true);
        assert!(read_in(&p, &[commit(&p, "codex")], || Ok(())).is_ok());
        drop(Mutation::begin(&p, RECIPIENT));
        assert!(p.source.lock().unwrap().entries[RECIPIENT].failed);
    }

    #[test]
    fn sink_failure_or_panic_never_certifies_a_replacement_and_explicit_commit_recovers() {
        for panic in [false, true] {
            let p = fixture();
            poll(&p);
            let _registration = install_in(
                &p,
                Arc::new(move |_| {
                    if panic {
                        panic!("injected source sink panic");
                    }
                    bail!("injected source sink failure")
                }),
            )
            .unwrap();
            let old = commit(&p, "codex");
            poll(&p);
            assert!(read_in(&p, &[old], || Ok(())).is_err());
            assert!(p.source.lock().unwrap().entries[RECIPIENT].failed);
            let recovered = commit(&p, "codex");
            assert!(read_in(&p, &[recovered], || Ok(())).is_ok());
        }
    }

    #[test]
    fn source_and_database_changes_during_commit_or_snapshot_are_refused() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let capture = captured(&p, "codex");
        assert!(
            commit_in(&p, &capture, |_| {
                poll(&p);
                Ok(())
            })
            .is_err()
        );
        assert!(read_in(&p, &[capture.certificate], || Ok(())).is_err());
        let current = commit(&p, "codex");
        assert!(
            read_in(&p, &[current], || {
                record_legacy_in(&p, RECIPIENT, "native", 7);
                Ok("stale SQL row")
            })
            .is_err()
        );
        let failed = captured(&p, "codex");
        assert!(commit_in(&p, &failed, |_| bail!("database commit failed")).is_err());
        assert!(read_in(&p, &[failed.certificate], || Ok(())).is_err());
    }

    #[test]
    fn followed_file_replacement_and_missing_file_persistently_fence_old_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("st");
        std::fs::write(&path, "first image").unwrap();
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        record_in(
            &p,
            RECIPIENT,
            &json!({"transport":"native","image":"new","ready":true,"follows":path}).to_string(),
        );
        let old = commit(&p, "codex");
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, "second image").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(read_in(&p, std::slice::from_ref(&old), || Ok(())).is_err());
        assert!(
            commit_in(
                &p,
                &CapturedAssessment {
                    assessment: json!({}),
                    certificate: old
                },
                |_| Ok(())
            )
            .is_err()
        );
        let replacement = commit(&p, "codex");
        assert!(read_in(&p, std::slice::from_ref(&replacement), || Ok(())).is_ok());
        let saved = directory.path().join("saved");
        std::fs::rename(&path, &saved).unwrap();
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        std::fs::rename(&saved, &path).unwrap();
        assert!(read_in(&p, &[replacement], || Ok(())).is_err());
        assert!(read_in(&p, &[commit(&p, "codex")], || Ok(())).is_ok());
    }

    #[test]
    fn captured_json_matches_full_assessment_at_grace_strict_expiry_and_presentation_boundaries() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        for age in [
            Duration::ZERO,
            Duration::from_millis(19_999),
            Duration::from_secs(20),
            Duration::from_secs(45),
            Duration::from_secs(45) + Duration::from_nanos(1),
            Duration::from_secs(90),
        ] {
            let now = p.started + age;
            let absent = capture_at(&p, RECIPIENT, "codex", now, 0).unwrap();
            assert_eq!(
                absent.assessment,
                assess_beat(age, Some("new"), None, "codex").to_value()
            );
            for report in [
                Report { transport: Some("native".into()), image: Some("new".into()), ready: Some(true), ..Report::default() },
                Report { transport: Some("native".into()), image: Some("old".into()), ready: Some(true), ..Report::default() },
                Report { transport: Some("legacy".into()), legacy: true, pid: Some(9), ..Report::default() },
                serde_json::from_value(json!({"transport":"claude-channel","image":"new","ready":true,"channel":{"pid":4,"image":"new","age_ms":10000}})).unwrap(),
            ] {
                for driver in ["codex", "claude"] {
                    p.beats.lock().unwrap().insert(RECIPIENT.into(), Beat { at: p.started, report: report.clone(), fence: None });
                    let capture = capture_at(&p, RECIPIENT, driver, now, 0).unwrap();
                    assert_eq!(capture.assessment, assess_beat(age, Some("new"), Some((age, report.clone())), driver).to_value());
                    if age == Duration::from_secs(45) { assert_eq!(capture.certificate.deadline_ns, ns(age).unwrap() + 1); }
                    validate_current(&p, &capture.certificate, now, false).unwrap();
                    assert!(validate_current(&p, &capture.certificate, p.started + Duration::from_nanos(capture.certificate.deadline_ns), false).is_err());
                }
            }
            p.beats.lock().unwrap().clear();
        }
    }

    #[test]
    fn guard_refuses_overdue_rows_before_timer_commits_and_cannot_extend_a_certificate() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let current = commit(&p, "codex");
        let mut forged = current.clone();
        forged.deadline_ns += 1_000_000_000;
        assert!(read_in(&p, &[forged], || Ok(())).is_err());
        assert!(
            validate_current(
                &p,
                &current,
                p.started + Duration::from_nanos(current.deadline_ns),
                true
            )
            .is_err()
        );
        let called = AtomicUsize::new(0);
        assert!(
            read_in(&p, &[], || {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
        );
        assert!(
            read_in(&p, &vec![current; MAX_READ_KEYS + 1], || {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn real_store_normalized_row_and_certificate_commit_together_and_rollback_never_acknowledges() {
        let store = crate::store::Store::open_memory("delivery-source-control").unwrap();
        store.connection.write().execute_batch("CREATE TABLE delivery_source_fixture(recipient TEXT, driver TEXT, assessment TEXT, certificate TEXT, PRIMARY KEY(recipient,driver));").unwrap();
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let first = captured(&p, "codex");
        let certificate = commit_in(&p, &first, |capture| {
            let mut writer = store.connection.write();
            let transaction = writer.transaction()?;
            transaction.execute(
                "INSERT INTO delivery_source_fixture VALUES (?1,?2,?3,?4)",
                rusqlite::params![
                    RECIPIENT,
                    "codex",
                    capture.assessment.to_string(),
                    serde_json::to_string(&capture.certificate)?
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
        let assessment: String = read_in(&p, std::slice::from_ref(&certificate), || {
            let connection = store.readers.get();
            Ok(connection.query_row("SELECT assessment FROM delivery_source_fixture WHERE recipient=?1 AND driver='codex'", [RECIPIENT], |row| row.get(0))?)
        }).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&assessment).unwrap(),
            first.assessment
        );
        record_legacy_in(&p, RECIPIENT, "legacy", 33);
        let failed = captured(&p, "codex");
        assert!(
            commit_in(&p, &failed, |capture| {
                let mut writer = store.connection.write();
                let transaction = writer.transaction()?;
                transaction.execute(
                    "UPDATE delivery_source_fixture SET assessment=?1",
                    [capture.assessment.to_string()],
                )?;
                bail!("injected rollback after normalized row update")
            })
            .is_err()
        );
        let connection = store.readers.get();
        let stored: String = connection
            .query_row(
                "SELECT assessment FROM delivery_source_fixture",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, assessment);
        assert!(read_in(&p, &[certificate], || Ok(stored)).is_err());
        assert!(read_in(&p, &[failed.certificate], || Ok(())).is_err());
    }
    #[test]
    fn epoch_reinstallation_refuses_inflight_mutations_even_without_a_prior_sink() {
        let p = fixture();
        let before_install = Mutation::begin(&p, RECIPIENT);
        assert!(install_in(&p, Arc::new(|_| Ok(()))).is_err());
        before_install.finish(true);
        let registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        let old = commit(&p, "codex");
        let active = Mutation::begin(&p, RECIPIENT);
        drop(registration);
        assert!(install_in(&p, Arc::new(|_| Ok(()))).is_err());
        active.finish(true);
        let _new = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        assert!(read_in(&p, &[old], || Ok(())).is_err());
    }

    #[test]
    fn callback_can_capture_commit_and_read_without_holding_producer_locks() {
        let p = Arc::new(fixture());
        let weak = Arc::downgrade(&p);
        let _registration = install_in(
            &p,
            Arc::new(move |change| {
                let p = weak.upgrade().unwrap();
                let capture = captured(&p, "codex");
                ensure!(
                    capture.certificate.epoch == change.epoch,
                    "callback owner changed"
                );
                let certificate = commit_in(&p, &capture, |_| {
                    // A real sink acquires its writer here. Every producer lock remains obtainable.
                    assert!(p.source.try_lock().is_ok());
                    assert!(p.beats.try_lock().is_ok());
                    assert!(p.monitors.try_lock().is_ok());
                    Ok(())
                })?;
                read_in(&p, &[certificate], || Ok(()))?;
                // A new owner cannot adopt memory while this callback still owns publication.
                assert!(p.source.lock().unwrap().active_mutations > 0);
                Ok(())
            }),
        )
        .unwrap();
        poll(&p);
        let state = p.source.lock().unwrap();
        assert_eq!(state.active_mutations, 0);
        assert!(!state.entries[RECIPIENT].failed);
        assert!(state.entries[RECIPIENT].committed.contains_key("codex"));
    }

    #[test]
    fn bounds_and_revision_exhaustion_refuse_capture_instead_of_truncating_or_wrapping() {
        let p = fixture();
        let _registration = install_in(&p, Arc::new(|_| Ok(()))).unwrap();
        poll(&p);
        assert!(
            capture_at(
                &p,
                &"a".repeat(MAX_RECIPIENT_BYTES + 1),
                "codex",
                Instant::now(),
                0
            )
            .is_err()
        );
        assert!(capture_at(&p, RECIPIENT, "remote", Instant::now(), 0).is_err());
        record_in(
            &p,
            RECIPIENT,
            &json!({"ready":false,"reason":"a".repeat(MAX_CAPTURE_BYTES + 1)}).to_string(),
        );
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        poll(&p);
        p.source
            .lock()
            .unwrap()
            .entries
            .get_mut(RECIPIENT)
            .unwrap()
            .revision = u64::MAX;
        poll(&p);
        assert!(capture_at(&p, RECIPIENT, "codex", Instant::now(), 0).is_err());
        let unknown_image = Presence {
            image: None,
            ..fixture()
        };
        let _unknown = install_in(&unknown_image, Arc::new(|_| Ok(()))).unwrap();
        assert!(capture_at(&unknown_image, RECIPIENT, "codex", Instant::now(), 0).is_err());
    }
}
