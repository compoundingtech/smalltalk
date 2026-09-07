//! Canonical durable state for native message delivery.
//!
//! The ledger records one monotone evidence chain per inbox filename. Drivers translate their
//! harness-specific observations into [`Evidence`]; this module owns persistence, phase grading,
//! retry authorization, and binding isolation. Inbox archive remains the recipient's settlement
//! authority and is reconciled through [`Ledger::prune`].
//!
//! One phase can reach this ledger without this build observing anything: an attempt an earlier
//! release made and left behind. Such a phase holds exactly as far as a phase holds — no harness
//! profile proves `Attempted`, so nothing is re-sent — and [`Attestation`] records that this
//! build never watched it, which is what makes the leftover countable and therefore removable.
//! The translation itself lives outside this module, behind the one seam in [`Ledger::open`].

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::message;

pub(crate) const LEDGER_SCHEMA: &str = "st2.delivery-ledger.v1";
pub(crate) const LEDGER_FILE: &str = "delivery-ledger.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Codex,
    OpenCode,
}

impl Harness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            other => anyhow::bail!("unknown native delivery harness '{other}'"),
        }
    }

    pub fn profile(self) -> Profile {
        Profile { harness: self }
    }
}

/// The evidence vocabulary one harness can honestly produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    harness: Harness,
}

impl Profile {
    fn graded(self, evidence: Evidence) -> Result<Phase> {
        let phase = match evidence {
            Evidence::TransportAccepted => Phase::TransportAccepted,
            Evidence::Persisted => Phase::Persisted,
            Evidence::Consumed => Phase::Consumed,
        };
        anyhow::ensure!(
            self.proves(phase),
            "{} delivery evidence cannot prove phase {phase:?}",
            self.harness.as_str()
        );
        Ok(phase)
    }

    /// Whether this harness has a concrete observation for `phase`.
    fn proves(self, phase: Phase) -> bool {
        match self.harness {
            // Codex exposes transport acceptance and a typed completed user message, but no
            // storage receipt.
            Harness::Codex => matches!(
                phase,
                Phase::Attempted | Phase::TransportAccepted | Phase::Consumed
            ),
            // OpenCode exposes transport acceptance and durable read-back, but no observation
            // that the scheduler or model consumed the prompt.
            Harness::OpenCode => matches!(
                phase,
                Phase::Attempted | Phase::TransportAccepted | Phase::Persisted
            ),
        }
    }

    fn releases(self, phase: Phase) -> bool {
        matches!(self.harness, Harness::Codex) && phase >= Phase::Consumed
    }
}

/// Durable delivery evidence. Declaration order is the monotone lattice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Persisted before the transport call.
    Attempted,
    /// The transport call returned success.
    TransportAccepted,
    /// The harness durably stores the exact correlated message.
    Persisted,
    /// The model received the message.
    Consumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Correlation {
    pub value: String,
}

impl Correlation {
    pub fn native(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NegativeReceipt {
    /// The harness authoritatively does not hold the correlated message.
    Absent,
    /// The transport refused this attempt.
    Rejected,
}

/// Positive evidence translated by a harness driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    TransportAccepted,
    Persisted,
    Consumed,
}

/// Whether this build observed the evidence behind a phase, or another authority asserted it.
///
/// This is provenance, not authority: no [`Retention`], [`RetryDecision`] or transport decision
/// reads it. What holds a carried-forward attempt is its [`Phase`] measured against the harness
/// [`Profile`] — `Profile::releases` releases only a phase that harness can actually prove, and
/// [`Ledger::seed`] refuses an asserted phase the profile cannot prove at all. The field exists
/// so the fleet can *see* an unobserved phase: it is clause 2 of DELTA-006's Resolution Signal
/// (`st2 doctor` counts it per seat), and it is deleted with the record boundary that produces
/// it. Nothing here names the authority: any party that can bound an attempt this build never
/// watched asserts, and the boundary in `crate::migrations::delivery_state` is one such party.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Attestation {
    /// This build graded the evidence that set the phase through [`Profile::graded`].
    Observed,
    /// Another authority asserted the phase: this build never watched the attempt it describes.
    Asserted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub filename: String,
    pub binding: String,
    pub correlation: Correlation,
    pub phase: Phase,
    /// Whether `phase` is this build's own grading or a claim it accepted from elsewhere.
    pub attestation: Attestation,
    /// The runtime incarnation that made the attempt. Live evidence acknowledges only its own
    /// incarnation; history reconciliation settles attempts from earlier incarnations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<NegativeReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    schema: String,
    harness: String,
    agent: String,
    runtime_id: String,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    Release,
    Hold(HoldReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    AmbiguousAttempt,
    UnreadReceipt,
    NegativeReceipt,
    Quarantined,
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Retry,
    Hold(HoldReason),
}

/// State persisted before a first transport.
#[derive(Debug, Clone)]
pub struct Begin {
    pub filename: String,
    pub binding: String,
    pub correlation: Correlation,
    pub incarnation: Option<String>,
}

pub struct Ledger {
    path: PathBuf,
    profile: Profile,
    record: Record,
    quarantine: Option<String>,
}

impl Ledger {
    /// Open the canonical ledger at `path`.
    ///
    /// `correlate(binding, filename)` must be the derivation used by the transport. Invalid or
    /// unreadable state quarantines delivery rather than preventing the harness process from
    /// starting.
    pub fn open(
        path: &Path,
        profile: Profile,
        agent: &str,
        runtime_id: &str,
        correlate: impl Fn(&str, &str) -> String,
    ) -> Self {
        let mut ledger = Self {
            path: path.to_path_buf(),
            profile,
            record: Record {
                schema: LEDGER_SCHEMA.to_owned(),
                harness: profile.harness.as_str().to_owned(),
                agent: agent.to_owned(),
                runtime_id: runtime_id.to_owned(),
                entries: Vec::new(),
            },
            quarantine: None,
        };
        let outcome = match fs::read(path) {
            Ok(bytes) => ledger.load(&bytes, &correlate),
            // THE RECOVERY SEAM. The only statement in this module that knows any other delivery
            // record has ever existed; deleting `crate::migrations::delivery_state` deletes this
            // arm and nothing else. What it degrades to — `Ok(())`, no entries — is exactly a
            // first run on a fresh seat. DELETION TRIGGER: docs/vrs/.delta/DELTA-006.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => path
                .parent()
                .map_or_else(
                    || Ok(Vec::new()),
                    |state_dir| {
                        crate::migrations::delivery_state::recover(
                            state_dir,
                            profile.harness,
                            agent,
                            &correlate,
                        )
                    },
                )
                .and_then(|recovered| ledger.seed(recovered, &correlate)),
            Err(error) => Err(error).with_context(|| format!("reading delivery ledger {}", path.display())),
        };
        if let Err(error) = outcome {
            ledger.record.entries.clear();
            ledger.quarantine = Some(format!("{error:#}"));
        }
        ledger
    }

    fn load(&mut self, bytes: &[u8], correlate: &impl Fn(&str, &str) -> String) -> Result<()> {
        let mut record: Record = serde_json::from_slice(bytes)
            .with_context(|| format!("reading delivery ledger {}", self.path.display()))?;
        anyhow::ensure!(
            record.schema == LEDGER_SCHEMA,
            "delivery ledger has unsupported schema '{}'",
            record.schema
        );
        anyhow::ensure!(
            Harness::parse(&record.harness)? == self.profile.harness,
            "delivery ledger belongs to harness '{}'",
            record.harness
        );
        anyhow::ensure!(
            record.agent == self.record.agent,
            "delivery ledger belongs to a different agent"
        );
        self.accept(&record.entries, correlate)?;
        record.runtime_id.clone_from(&self.record.runtime_id);
        self.record = record;
        Ok(())
    }

    /// Accept entries this process did not itself create, then make them durable.
    ///
    /// They are validated exactly as bytes from this module's own file would be, so a claim about
    /// evidence the harness cannot produce fails closed instead of being written. Recovery
    /// happens once: the ledger file's existence is what stops it happening twice, and a driver
    /// that never delivered leaves no file behind.
    fn seed(
        &mut self,
        entries: Vec<Entry>,
        correlate: &impl Fn(&str, &str) -> String,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.accept(&entries, correlate)?;
        self.record.entries = entries;
        self.persist()
    }

    /// Every check a set of entries from outside this process must pass.
    fn accept(
        &self,
        entries: &[Entry],
        correlate: &impl Fn(&str, &str) -> String,
    ) -> Result<()> {
        for entry in entries {
            self.validate(entry, correlate)?;
        }
        anyhow::ensure!(
            entries
                .windows(2)
                .all(|pair| pair[0].binding == pair[1].binding),
            "delivery ledger holds entries from more than one binding"
        );
        Ok(())
    }

    fn validate(
        &self,
        entry: &Entry,
        correlate: &impl Fn(&str, &str) -> String,
    ) -> Result<()> {
        anyhow::ensure!(
            message::is_message_filename(&entry.filename) && !entry.binding.is_empty(),
            "delivery ledger entry has an invalid binding or filename"
        );
        anyhow::ensure!(
            entry.correlation.value == correlate(&entry.binding, &entry.filename),
            "delivery ledger entry correlation does not match its binding"
        );
        anyhow::ensure!(
            self.profile.proves(entry.phase),
            "delivery ledger entry records a phase {} cannot prove",
            self.profile.harness.as_str()
        );
        Ok(())
    }

    pub fn quarantined(&self) -> Option<&str> {
        self.quarantine.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.record.entries
    }

    pub fn entry(&self, filename: &str) -> Option<&Entry> {
        self.record
            .entries
            .iter()
            .find(|entry| entry.filename == filename)
    }

    pub fn binding(&self) -> Option<&str> {
        self.record
            .entries
            .first()
            .map(|entry| entry.binding.as_str())
    }

    pub fn correlated(&self, value: &str) -> Vec<String> {
        self.record
            .entries
            .iter()
            .filter(|entry| entry.correlation.value == value)
            .map(|entry| entry.filename.clone())
            .collect()
    }

    /// Persist ownership before the caller transports the message.
    pub fn begin(&mut self, begin: Begin) -> Result<Entry> {
        self.ensure_writable()?;
        anyhow::ensure!(
            message::is_message_filename(&begin.filename) && !begin.binding.is_empty(),
            "delivery attempt has an invalid binding or filename"
        );
        let entry = match self
            .record
            .entries
            .iter()
            .position(|entry| entry.filename == begin.filename)
        {
            Some(index) => {
                let entry = &mut self.record.entries[index];
                entry.binding = begin.binding;
                entry.correlation = begin.correlation;
                entry.incarnation = begin.incarnation;
                entry.phase = entry.phase.max(Phase::Attempted);
                // This build is about to transport it, so the attempt is no longer someone
                // else's claim about the world.
                entry.attestation = Attestation::Observed;
                entry.negative = None;
                entry.clone()
            }
            None => {
                let entry = Entry {
                    filename: begin.filename,
                    binding: begin.binding,
                    correlation: begin.correlation,
                    phase: Phase::Attempted,
                    attestation: Attestation::Observed,
                    incarnation: begin.incarnation,
                    negative: None,
                };
                self.record.entries.push(entry.clone());
                entry
            }
        };
        self.persist()?;
        Ok(entry)
    }

    /// Record positive evidence without allowing a phase downgrade.
    pub fn record(&mut self, filename: &str, evidence: Evidence) -> Result<Option<Phase>> {
        self.ensure_writable()?;
        let phase = self.profile.graded(evidence)?;
        let Some(entry) = self
            .record
            .entries
            .iter_mut()
            .find(|entry| entry.filename == filename)
        else {
            return Ok(None);
        };
        if phase <= entry.phase {
            return Ok(Some(entry.phase));
        }
        entry.phase = phase;
        // This build graded the evidence, so the phase is no longer a carried-forward claim.
        entry.attestation = Attestation::Observed;
        entry.negative = None;
        self.persist()?;
        Ok(Some(phase))
    }

    /// Record an authoritative negative receipt. A settled entry is never reopened.
    pub fn negative(&mut self, filename: &str, receipt: NegativeReceipt) -> Result<Retention> {
        self.ensure_writable()?;
        let retention = self.retention(filename);
        if retention == Retention::Release {
            return Ok(retention);
        }
        let Some(entry) = self
            .record
            .entries
            .iter_mut()
            .find(|entry| entry.filename == filename)
        else {
            return Ok(retention);
        };
        if entry.negative == Some(receipt) {
            return Ok(Retention::Hold(HoldReason::NegativeReceipt));
        }
        entry.negative = Some(receipt);
        // An authoritative absence is itself an observation about this attempt, and it is the
        // only receipt that may re-authorize a transport of a carried-forward one.
        entry.attestation = Attestation::Observed;
        self.persist()?;
        Ok(Retention::Hold(HoldReason::NegativeReceipt))
    }

    pub fn retention(&self, filename: &str) -> Retention {
        if self.quarantine.is_some() {
            return Retention::Hold(HoldReason::Quarantined);
        }
        let Some(entry) = self.entry(filename) else {
            return Retention::Release;
        };
        if entry.negative.is_some() {
            return Retention::Hold(HoldReason::NegativeReceipt);
        }
        if self.profile.releases(entry.phase) {
            return Retention::Release;
        }
        if entry.phase >= Phase::Persisted {
            return Retention::Hold(HoldReason::UnreadReceipt);
        }
        Retention::Hold(HoldReason::AmbiguousAttempt)
    }

    pub fn retry(&self, filename: &str) -> RetryDecision {
        if self.quarantine.is_some() {
            return RetryDecision::Hold(HoldReason::Quarantined);
        }
        let Some(entry) = self.entry(filename) else {
            return RetryDecision::Retry;
        };
        match self.retention(filename) {
            Retention::Release => RetryDecision::Hold(HoldReason::Settled),
            Retention::Hold(HoldReason::NegativeReceipt) => RetryDecision::Retry,
            Retention::Hold(HoldReason::UnreadReceipt) => {
                RetryDecision::Hold(HoldReason::UnreadReceipt)
            }
            Retention::Hold(reason) => {
                debug_assert!(entry.phase <= Phase::TransportAccepted);
                RetryDecision::Hold(reason)
            }
        }
    }

    /// Keep only entries for the selected thread or session.
    pub fn rebind(&mut self, binding: &str) -> Result<()> {
        let before = self.record.entries.len();
        self.record.entries.retain(|entry| entry.binding == binding);
        if self.record.entries.len() != before {
            self.persist()?;
        }
        Ok(())
    }

    /// Reconcile entries against the recipient-owned unread set.
    pub fn prune(&mut self, is_unread: impl Fn(&str) -> bool) -> Result<()> {
        let before = self.record.entries.len();
        self.record
            .entries
            .retain(|entry| is_unread(&entry.filename));
        if self.record.entries.len() != before {
            self.persist()?;
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        anyhow::ensure!(
            self.quarantine.is_none(),
            "delivery ledger is quarantined: {}",
            self.quarantine.as_deref().unwrap_or_default()
        );
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        atomic_json(&self.path, &self.record)
            .with_context(|| format!("writing delivery ledger {}", self.path.display()))
    }
}

/// Build an entry whose phase another authority asserted rather than this build observing it.
///
/// The only constructor the recovery seam may use. The phase it carries is still checked against
/// the harness [`Profile`] by [`Ledger::seed`], so a claim of evidence the harness cannot produce
/// fails closed instead of being written, and the phase itself is what keeps the entry holding:
/// an [`Attestation::Asserted`] entry suppresses a duplicate exactly as far as its phase does,
/// and authorizes a transport only where the profile proves that phase.
pub fn asserted(
    filename: String,
    binding: String,
    correlation: Correlation,
    phase: Phase,
    incarnation: Option<String>,
) -> Entry {
    Entry {
        filename,
        binding,
        correlation,
        phase,
        attestation: Attestation::Asserted,
        incarnation,
        negative: None,
    }
}

/// How many entries in the ledger at `path` carry a phase this build never observed.
///
/// Clause 2 of DELTA-006's Resolution Signal, and the reason [`Attestation`] is serialized at
/// all. Read-only on purpose: [`Ledger::open`] runs the record boundary and may write, and a
/// diagnostic must not change what it measures. A missing ledger counts zero; bytes that will
/// not parse are an error, because "I hold a delivery record I cannot read" is exactly what an
/// operator needs told.
pub(crate) fn asserted_entries(path: &Path) -> Result<usize> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading delivery ledger {}", path.display()));
        }
    };
    let record: Record = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing delivery ledger {}", path.display()))?;
    Ok(record
        .entries
        .iter()
        .filter(|entry| entry.attestation == Attestation::Asserted)
        .count())
}

/// Durable replacement: file bytes reach disk before rename, then the directory entry is synced.
///
/// The temp file is created exclusively at `0600` under a name unique to this process and write,
/// so a stale or adversarial path cannot be followed or truncated and two writes cannot collide.
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static WRITE: AtomicU64 = AtomicU64::new(0);

    let bytes = serde_json::to_vec(value)?;
    let parent = path.parent().context("ledger file has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".delivery-ledger.{}.{}.tmp",
        std::process::id(),
        WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_A: &str = "1786380000000-aaa111.md";
    const FILE_B: &str = "1786380000001-bbb222.md";

    fn correlation(binding: &str, filename: &str) -> String {
        format!("{binding}:{filename}")
    }

    fn open(dir: &Path, harness: Harness) -> Ledger {
        Ledger::open(
            &dir.join(LEDGER_FILE),
            harness.profile(),
            "h.worker",
            "h.worker.runtime",
            correlation,
        )
    }

    fn begin(ledger: &mut Ledger, binding: &str, filename: &str) -> Entry {
        ledger
            .begin(Begin {
                filename: filename.to_owned(),
                binding: binding.to_owned(),
                correlation: Correlation::native(correlation(binding, filename)),
                incarnation: Some("incarnation-1".to_owned()),
            })
            .unwrap()
    }

    #[test]
    fn phase_order_is_the_monotone_evidence_lattice() {
        assert!(Phase::Attempted < Phase::TransportAccepted);
        assert!(Phase::TransportAccepted < Phase::Persisted);
        assert!(Phase::Persisted < Phase::Consumed);
    }

    #[test]
    fn profiles_accept_only_evidence_their_harness_can_produce() {
        let codex = Harness::Codex.profile();
        assert!(codex.graded(Evidence::TransportAccepted).is_ok());
        assert!(codex.graded(Evidence::Persisted).is_err());
        assert!(codex.graded(Evidence::Consumed).is_ok());

        let opencode = Harness::OpenCode.profile();
        assert!(opencode.graded(Evidence::TransportAccepted).is_ok());
        assert!(opencode.graded(Evidence::Persisted).is_ok());
        assert!(opencode.graded(Evidence::Consumed).is_err());
    }

    #[test]
    fn begin_persists_attempted_before_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        let entry = begin(&mut ledger, "thread-main", FILE_A);
        assert_eq!(entry.phase, Phase::Attempted);

        let reopened = open(tmp.path(), Harness::Codex);
        assert_eq!(reopened.entry(FILE_A).unwrap(), &entry);
        assert_eq!(
            reopened.retry(FILE_A),
            RetryDecision::Hold(HoldReason::AmbiguousAttempt)
        );
    }

    /// The safety property the whole record boundary rests on, and where it comes from: a
    /// carried-forward `Attempted` phase is a bound on what already happened, so it holds the
    /// delivery, and no harness profile proves `Attempted`, so it authorizes no transport. The
    /// phase does that work; the attestation only records who saw it. This build's own
    /// observation — here an authoritative absence — is what clears the hold.
    #[test]
    fn an_asserted_phase_suppresses_a_duplicate_and_authorizes_no_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        ledger
            .seed(
                vec![asserted(
                    FILE_A.to_owned(),
                    "thread-main".to_owned(),
                    Correlation::native(correlation("thread-main", FILE_A)),
                    Phase::Attempted,
                    Some("incarnation-0".to_owned()),
                )],
                &correlation,
            )
            .unwrap();
        assert_eq!(
            ledger.retention(FILE_A),
            Retention::Hold(HoldReason::AmbiguousAttempt)
        );
        assert_eq!(
            ledger.retry(FILE_A),
            RetryDecision::Hold(HoldReason::AmbiguousAttempt)
        );

        // The seed is durable, so a restart still holds instead of re-sending.
        let reopened = open(tmp.path(), Harness::Codex);
        assert_eq!(
            reopened.entry(FILE_A).unwrap().attestation,
            Attestation::Asserted
        );
        assert_eq!(
            reopened.retry(FILE_A),
            RetryDecision::Hold(HoldReason::AmbiguousAttempt)
        );

        // An authoritative absence is an observation: it clears the claim and re-authorizes one
        // transport of the same identity.
        let mut ledger = open(tmp.path(), Harness::Codex);
        ledger.negative(FILE_A, NegativeReceipt::Absent).unwrap();
        assert_eq!(
            ledger.entry(FILE_A).unwrap().attestation,
            Attestation::Observed
        );
        assert_eq!(ledger.retry(FILE_A), RetryDecision::Retry);

        // And so is a graded receipt: the entry stops being a carried-forward claim.
        let mut ledger = open(tmp.path(), Harness::Codex);
        ledger.record(FILE_A, Evidence::Consumed).unwrap();
        assert_eq!(
            ledger.entry(FILE_A).unwrap().attestation,
            Attestation::Observed
        );
        assert_eq!(ledger.retention(FILE_A), Retention::Release);
    }

    #[test]
    fn the_persisted_ledger_is_owner_only_and_leaves_no_temp_residue() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        begin(&mut ledger, "thread-main", FILE_A);
        ledger.record(FILE_A, Evidence::TransportAccepted).unwrap();

        let path = tmp.path().join(LEDGER_FILE);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "delivery ledger is world-readable");

        let residue = fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(residue.is_empty(), "temp residue left behind: {residue:?}");
    }

    #[test]
    fn positive_evidence_never_downgrades() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        begin(&mut ledger, "thread-main", FILE_A);
        ledger.record(FILE_A, Evidence::Consumed).unwrap();
        ledger
            .record(FILE_A, Evidence::TransportAccepted)
            .unwrap();
        assert_eq!(ledger.entry(FILE_A).unwrap().phase, Phase::Consumed);
        assert_eq!(ledger.retention(FILE_A), Retention::Release);
    }

    #[test]
    fn negative_receipt_is_the_only_retry_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        begin(&mut ledger, "thread-main", FILE_A);
        ledger.negative(FILE_A, NegativeReceipt::Absent).unwrap();
        assert_eq!(ledger.retry(FILE_A), RetryDecision::Retry);
        begin(&mut ledger, "thread-main", FILE_A);
        assert_eq!(
            ledger.retry(FILE_A),
            RetryDecision::Hold(HoldReason::AmbiguousAttempt)
        );
    }

    #[test]
    fn opencode_persistence_holds_until_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::OpenCode);
        begin(&mut ledger, "ses-main", FILE_A);
        ledger.record(FILE_A, Evidence::Persisted).unwrap();
        assert_eq!(
            ledger.retention(FILE_A),
            Retention::Hold(HoldReason::UnreadReceipt)
        );
        ledger.prune(|_| false).unwrap();
        assert!(ledger.entry(FILE_A).is_none());
    }

    #[test]
    fn rebind_discards_receipts_from_another_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        begin(&mut ledger, "thread-old", FILE_A);
        ledger.rebind("thread-new").unwrap();
        assert!(ledger.entries().is_empty());
        assert_eq!(ledger.retry(FILE_A), RetryDecision::Retry);
    }

    #[test]
    fn correlated_returns_every_matching_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = open(tmp.path(), Harness::Codex);
        let first = begin(&mut ledger, "thread-main", FILE_A);
        let second = begin(&mut ledger, "thread-main", FILE_B);
        assert_eq!(ledger.correlated(&first.correlation.value), [FILE_A]);
        assert_eq!(ledger.correlated(&second.correlation.value), [FILE_B]);
    }

    #[test]
    fn foreign_or_malformed_state_quarantines_without_rewriting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LEDGER_FILE);
        fs::write(&path, b"not json").unwrap();
        let mut malformed = open(tmp.path(), Harness::Codex);
        assert!(malformed.quarantined().is_some());
        assert!(
            malformed
                .begin(Begin {
                    filename: FILE_A.to_owned(),
                    binding: "thread-main".to_owned(),
                    correlation: Correlation::native(correlation("thread-main", FILE_A)),
                    incarnation: None,
                })
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"not json");

        atomic_json(
            &path,
            &Record {
                schema: LEDGER_SCHEMA.to_owned(),
                harness: "opencode".to_owned(),
                agent: "h.worker".to_owned(),
                runtime_id: "runtime".to_owned(),
                entries: Vec::new(),
            },
        )
        .unwrap();
        assert!(open(tmp.path(), Harness::Codex).quarantined().is_some());
    }

    #[test]
    fn every_entry_must_validate_its_own_correlation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LEDGER_FILE);
        atomic_json(
            &path,
            &Record {
                schema: LEDGER_SCHEMA.to_owned(),
                harness: "codex".to_owned(),
                agent: "h.worker".to_owned(),
                runtime_id: "runtime".to_owned(),
                entries: vec![Entry {
                    filename: FILE_A.to_owned(),
                    binding: "thread-main".to_owned(),
                    correlation: Correlation::native("injected"),
                    phase: Phase::Attempted,
                    attestation: Attestation::Observed,
                    incarnation: None,
                    negative: None,
                }],
            },
        )
        .unwrap();
        assert!(open(tmp.path(), Harness::Codex).quarantined().is_some());
    }
}
