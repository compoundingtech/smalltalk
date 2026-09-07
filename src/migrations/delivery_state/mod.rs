//! Translation of the per-driver `delivery-state.json` records into
//! [`crate::delivery_ledger::Entry`].
//!
//! Before `st2.delivery-ledger.v1`, each native transport kept its own single-binding record at
//! `<state-dir>/delivery-state.json` holding one `{binding, filename, correlation, phase in
//! {Attempted, Accepted}}`. The two records had the same field *shape* and different meanings:
//! Codex wrote `Accepted` only from the typed `item/completed{userMessage, clientId}` inside a
//! turn (the model received it), OpenCode from `GET /session/{s}/message/{m}` returning 200 (the
//! server stored it). Nothing on disk said which. That difference — and the field spellings, and
//! the filename, and the schema strings — lives here and nowhere else.
//!
//! One file per legacy version: [`codex_v1`], [`opencode_v1`]. Each owns its wire struct, the
//! meaning of its labels, and a `TryFrom<Adopting<Record>> for Entry`. A future
//! `delivery-state.v2` would be one more file plus one match arm in [`recover`].
//!
//! Two properties make this safe to translate rather than migrate in place:
//!
//! * **Fresh namespace.** The canonical record is a different filename, so nothing is rewritten
//!   and a translation can be re-derived from bytes that are still there.
//! * **Phase, not label.** Every entry this module produces is graded no higher than the
//!   evidence the old record actually carried, and no harness
//!   [`Profile`](crate::delivery_ledger::Profile) proves `Attempted`, so a carried-forward
//!   attempt suppresses a duplicate and authorizes no transport.
//!   [`Ledger::seed`](crate::delivery_ledger::Ledger::seed) re-checks that against the profile
//!   and fails closed, so the safety argument is enforced by the canonical validator, not by
//!   care taken here.
//!   [`crate::delivery_ledger::asserted`] additionally marks each entry
//!   [`crate::delivery_ledger::Attestation::Asserted`], which changes no decision and exists to
//!   make the leftover countable — see the trigger below.
//!
//! # Deletion trigger
//!
//! `docs/vrs/.delta/DELTA-006-delivery-state-v1-arm.md`, whose Resolution Signal is produced by
//! [`resolution_signal`] and printed per seat by `st2 doctor`: no `delivery-state.json` beside a
//! ledger and no ledger entry still carrying an asserted phase, on every admitted host, for
//! seven days. Deleting this arm is this directory, the one seam statement in
//! [`crate::delivery_ledger::Ledger::open`], and the attestation instrumentation the trigger
//! needed. The local half of the trigger is
//! [`tests::deletion_trigger_absent_old_record_makes_this_module_a_no_op`]; the signal itself is
//! pinned by [`tests::the_resolution_signal_counts_each_clause_without_consuming_it`].

mod codex_v1;
mod opencode_v1;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};

use crate::delivery_ledger::{Entry, Harness, LEDGER_FILE};

/// The filename every pre-ledger release wrote. Named here only.
const LEGACY_FILE: &str = "delivery-state.json";

/// A legacy record together with the canonical correlation derivation it has to re-prove.
///
/// The derivation is the same function the transport uses, so a record whose correlation does not
/// match its own binding and filename is provably not this agent's. `TryFrom` cannot carry that
/// context, so it rides in the wrapper.
pub(super) struct Adopting<'a, T> {
    pub record: T,
    pub correlate: &'a dyn Fn(&str, &str) -> String,
}

/// One legacy delivery-state version.
pub(super) trait Version: DeserializeOwned {
    /// The exact `schema` string the release that wrote this record stamped on it.
    const SCHEMA: &'static str;
    fn schema(&self) -> &str;
    fn agent(&self) -> &str;
}

/// Translate the legacy record in `state_dir`, if any, into canonical entries.
///
/// The canonical caller invokes this exactly once — when no ledger file exists — and makes the
/// result durable, so the ledger file's existence is what stops the translation happening twice.
/// Called with the ledger absent and no legacy record present it returns no entries, which is
/// byte-for-byte the behaviour of this module not existing.
pub(crate) fn recover<F>(
    state_dir: &Path,
    harness: Harness,
    agent: &str,
    correlate: &F,
) -> Result<Vec<Entry>>
where
    F: Fn(&str, &str) -> String,
{
    let path = state_dir.join(LEGACY_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading legacy delivery state {}", path.display()));
        }
    };
    let correlate: &dyn Fn(&str, &str) -> String = correlate;
    match harness {
        Harness::Codex => translate::<codex_v1::Record>(&bytes, agent, correlate),
        Harness::OpenCode => translate::<opencode_v1::Record>(&bytes, agent, correlate),
    }
}

/// DELTA-006's Resolution Signal, counted instead of remembered.
///
/// Clause 1 is a pre-ledger record still sitting beside a per-harness state dir; clause 2 is a
/// ledger entry whose phase this fleet asserted rather than observed. Both must read zero, on
/// every admitted host, before this directory and the seam in
/// [`crate::delivery_ledger::Ledger::open`] can go — and a trigger nothing produces resolves on
/// someone remembering, which is a date in disguise. `st2 doctor` prints this per seat so the
/// observation exists.
///
/// Read-only: it opens no ledger and translates nothing, so running the diagnostic cannot make
/// the record it is counting disappear.
pub struct ResolutionSignal {
    pub pre_ledger_records: usize,
    pub asserted_entries: usize,
}

impl ResolutionSignal {
    /// Whether both clauses are clear for the seats measured, i.e. nothing to print.
    pub fn is_clear(&self) -> bool {
        self.pre_ledger_records == 0 && self.asserted_entries == 0
    }
}

/// Measure the signal across one seat's per-harness state directories.
pub fn resolution_signal(state_dirs: &[PathBuf]) -> Result<ResolutionSignal> {
    let mut signal = ResolutionSignal {
        pre_ledger_records: 0,
        asserted_entries: 0,
    };
    for state_dir in state_dirs {
        if state_dir.join(LEGACY_FILE).exists() {
            signal.pre_ledger_records += 1;
        }
        signal.asserted_entries +=
            crate::delivery_ledger::asserted_entries(&state_dir.join(LEDGER_FILE))?;
    }
    Ok(signal)
}

/// Decode, filter by ownership, convert.
///
/// A record naming another schema, another harness's field set, or another agent is *ignored*:
/// it is not this ledger's authority, and refusing to start over someone else's file would
/// deliver nothing at all. Unreadable bytes are ignored for the same reason. But a record that is
/// ours and contradicts itself fails closed, because the canonical caller turns that error into a
/// quarantine and a quarantine is exactly the right answer to "I hold a delivery record I cannot
/// interpret".
///
/// The runtime id is deliberately not compared, for a different reason on each harness. OpenCode's
/// loader never looked at it, so comparing it would drop a record the old binary would have acted
/// on. Codex's loader did compare it — and hard-errored, refusing to start — but a drifted record
/// still describes a real attempt this recipient made, so it is carried forward as an assertion.
/// Held that way, a resume sweep can settle or refuse it BEFORE anything is sent, which is
/// strictly better than ignoring it and opening a second delivery for the same message.
fn translate<V>(
    bytes: &[u8],
    agent: &str,
    correlate: &dyn Fn(&str, &str) -> String,
) -> Result<Vec<Entry>>
where
    V: Version,
    for<'a> Entry: TryFrom<Adopting<'a, V>, Error = anyhow::Error>,
{
    let Ok(record) = serde_json::from_slice::<V>(bytes) else {
        return Ok(Vec::new());
    };
    if record.schema() != V::SCHEMA || record.agent() != agent {
        return Ok(Vec::new());
    }
    Ok(vec![Entry::try_from(Adopting { record, correlate })?])
}

/// Legacy records in the exact shape a pre-ledger release wrote them, for tests that need to
/// stand at the migration boundary.
///
/// A test reaches for these instead of spelling the old field names itself, so the wire shape of
/// a retired format is authored in one place and no other test module has to know it existed.
#[cfg(test)]
pub(crate) mod fixture {
    use serde_json::{Value, json};
    use std::path::Path;

    pub(crate) fn codex_attempted(
        agent: &str,
        runtime_id: &str,
        incarnation: &str,
        thread_id: &str,
        filename: &str,
        client_id: &str,
    ) -> Value {
        json!({
            "schema": super::codex_v1::SCHEMA,
            "agent": agent,
            "runtimeId": runtime_id,
            "runtimeIncarnation": incarnation,
            "threadId": thread_id,
            "filename": filename,
            "clientId": client_id,
            "phase": "attempted",
        })
    }

    pub(crate) fn opencode_attempted(
        agent: &str,
        runtime_id: &str,
        session_id: &str,
        filename: &str,
        message_id: &str,
    ) -> Value {
        json!({
            "schema": super::opencode_v1::SCHEMA,
            "agent": agent,
            "runtimeId": runtime_id,
            "sessionId": session_id,
            "filename": filename,
            "messageId": message_id,
            "phase": "attempted",
        })
    }

    /// Write `record` where a pre-ledger release would have left it.
    pub(crate) fn place(state_dir: &Path, record: &Value) {
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(
            state_dir.join(super::LEGACY_FILE),
            serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
    }

    pub(crate) fn path(state_dir: &Path) -> std::path::PathBuf {
        state_dir.join(super::LEGACY_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery_ledger::{
        Attestation, HoldReason, LEDGER_SCHEMA, Ledger, Phase, Retention, RetryDecision,
    };

    const FILE_A: &str = "1786380000000-aaa111.md";

    fn correlate(binding: &str, filename: &str) -> String {
        format!("corr:{binding}:{filename}")
    }

    fn open(state_dir: &Path, harness: Harness) -> Ledger {
        Ledger::open(
            &state_dir.join(LEDGER_FILE),
            harness.profile(),
            "h.worker",
            "h.worker",
            correlate,
        )
    }

    /// The local half of the deletion trigger: with no legacy record present this module produces
    /// nothing and causes no write, so removing it cannot change observable behaviour. The fleet
    /// half is the live query in the delta record named in the module doc.
    #[test]
    fn deletion_trigger_absent_old_record_makes_this_module_a_no_op() {
        for harness in [Harness::Codex, Harness::OpenCode] {
            let tmp = tempfile::tempdir().unwrap();
            let state_dir = tmp.path().join("state");
            assert!(
                recover(&state_dir, harness, "h.worker", &correlate)
                    .unwrap()
                    .is_empty()
            );
            let ledger = open(&state_dir, harness);
            assert!(ledger.quarantined().is_none());
            assert!(ledger.entries().is_empty());
            assert!(
                !state_dir.exists(),
                "a driver that never delivered leaves no record behind"
            );
        }
    }

    /// The fleet half of the deletion trigger needs a producer, or it resolves on someone
    /// remembering. Each clause must read nonzero exactly while the thing it names is present,
    /// and measuring must not consume it.
    #[test]
    fn the_resolution_signal_counts_each_clause_without_consuming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        let dirs = [state_dir.clone()];

        assert!(resolution_signal(&dirs).unwrap().is_clear());

        fixture::place(
            &state_dir,
            &fixture::codex_attempted(
                "h.worker",
                "h.worker",
                "incarnation-0",
                "thread-main",
                FILE_A,
                &correlate("thread-main", FILE_A),
            ),
        );
        let signal = resolution_signal(&dirs).unwrap();
        assert_eq!(signal.pre_ledger_records, 1);
        assert_eq!(
            signal.asserted_entries, 0,
            "clause 2 counts translated entries, and nothing has opened the ledger yet"
        );

        // Opening translates: the old record stays (clause 1) and the carried-forward phase is
        // now visible as asserted (clause 2).
        let ledger = open(&state_dir, Harness::Codex);
        assert_eq!(
            ledger.entry(FILE_A).unwrap().attestation,
            Attestation::Asserted
        );
        let signal = resolution_signal(&dirs).unwrap();
        assert_eq!(signal.pre_ledger_records, 1);
        assert_eq!(signal.asserted_entries, 1);
        assert!(!signal.is_clear());

        // Measuring is read-only: both clauses still hold after a second look.
        assert!(state_dir.join(LEGACY_FILE).exists());
        let signal = resolution_signal(&dirs).unwrap();
        assert_eq!(signal.pre_ledger_records, 1);
        assert_eq!(signal.asserted_entries, 1);
    }

    #[test]
    fn accepted_adopts_at_the_evidence_each_harness_actually_proved() {
        // Codex wrote `Accepted` only from the typed completed user message: consumption, its
        // true ceiling, so the delivery is settled and never re-offered.
        let codex = tempfile::tempdir().unwrap();
        let codex_dir = codex.path().join("state");
        let mut record = fixture::codex_attempted(
            "h.worker",
            "h.worker",
            "incarnation-1",
            "thread-main",
            FILE_A,
            &correlate("thread-main", FILE_A),
        );
        record["phase"] = "accepted".into();
        fixture::place(&codex_dir, &record);
        let ledger = open(&codex_dir, Harness::Codex);
        assert_eq!(ledger.entry(FILE_A).unwrap().phase, Phase::Consumed);
        assert_eq!(ledger.retention(FILE_A), Retention::Release);

        // OpenCode wrote it from a storage read-back. Mapping that to consumption would make the
        // stored-but-never-admitted class permanently unretryable, so it adopts as `persisted`
        // and holds.
        let opencode = tempfile::tempdir().unwrap();
        let opencode_dir = opencode.path().join("state");
        let mut record = fixture::opencode_attempted(
            "h.worker",
            "h.worker",
            "ses_target",
            FILE_A,
            &correlate("ses_target", FILE_A),
        );
        record["phase"] = "accepted".into();
        fixture::place(&opencode_dir, &record);
        let ledger = open(&opencode_dir, Harness::OpenCode);
        assert_eq!(ledger.entry(FILE_A).unwrap().phase, Phase::Persisted);
        assert_eq!(
            ledger.retention(FILE_A),
            Retention::Hold(HoldReason::UnreadReceipt)
        );
    }

    #[test]
    fn a_carried_forward_attempt_is_an_assertion_that_authorizes_no_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fixture::place(
            &state_dir,
            &fixture::codex_attempted(
                "h.worker",
                "h.other-runtime",
                "incarnation-0",
                "thread-main",
                FILE_A,
                &correlate("thread-main", FILE_A),
            ),
        );
        let ledger = open(&state_dir, Harness::Codex);
        let entry = ledger.entry(FILE_A).unwrap();
        assert_eq!(entry.attestation, Attestation::Asserted);
        assert_eq!(entry.phase, Phase::Attempted);
        assert_eq!(
            entry.incarnation.as_deref(),
            Some("incarnation-0"),
            "the attempt keeps the incarnation that made it, so no live frame can settle it"
        );
        assert_eq!(
            ledger.retry(FILE_A),
            RetryDecision::Hold(HoldReason::AmbiguousAttempt),
            "a drifted runtime id is carried forward and held, not ignored into a second delivery"
        );
    }

    #[test]
    fn translation_happens_once_and_leaves_the_old_record_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fixture::place(
            &state_dir,
            &fixture::opencode_attempted(
                "h.worker",
                "h.worker",
                "ses_target",
                FILE_A,
                &correlate("ses_target", FILE_A),
            ),
        );
        let mut ledger = open(&state_dir, Harness::OpenCode);
        assert_eq!(ledger.entries().len(), 1);
        // The recipient archives the message; ownership is released and the entry is gone.
        ledger.prune(|_| false).unwrap();

        // The legacy record is still on disk, and it is NOT translated a second time: the ledger
        // file's existence is the once-only fence.
        assert!(fixture::path(&state_dir).is_file());
        let reopened = open(&state_dir, Harness::OpenCode);
        assert!(reopened.quarantined().is_none());
        assert!(
            reopened.entries().is_empty(),
            "a settled delivery is not resurrected by the record it was translated from"
        );
    }

    /// The once-only fence's known limit, pinned so that changing it is a deliberate edit.
    ///
    /// The fence is "no ledger file exists". After a rollback to a pre-ledger release and a roll
    /// FORWARD again, the ledger file is already there and the record the old binary wrote in
    /// between is never read: the new binary starts with no entry for that filename and its first
    /// transport is not a retry. Rollback to a pre-ledger release is out of support, so this
    /// window closes by policy rather than by code.
    ///
    /// Closing it in code costs a second seam: recovery would have to be consultable per filename
    /// (canonical would ask "can anything contribute an entry for this file?" before authorizing a
    /// first transport) or run on every open with merge semantics — which resurrects an entry the
    /// recipient already archived until the next `prune`. Both are one statement each; neither is
    /// free, and the choice belongs to whoever supports the rollback.
    #[test]
    fn a_rollback_then_roll_forward_does_not_see_the_record_written_in_between() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");

        // A ledger exists (this release ran once) and holds nothing outstanding.
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join(LEDGER_FILE),
            serde_json::to_vec(&serde_json::json!({
                "schema": LEDGER_SCHEMA,
                "harness": "codex",
                "agent": "h.worker",
                "runtimeId": "h.worker",
                "entries": [],
            }))
            .unwrap(),
        )
        .unwrap();

        // The rolled-back release transported a message and wrote its own record.
        fixture::place(
            &state_dir,
            &fixture::codex_attempted(
                "h.worker",
                "h.worker",
                "incarnation-old",
                "thread-main",
                FILE_A,
                &correlate("thread-main", FILE_A),
            ),
        );

        // Rolling forward: the ledger file is present, so the seam does not fire.
        let rolled_forward = open(&state_dir, Harness::Codex);
        assert!(rolled_forward.quarantined().is_none());
        assert!(
            rolled_forward.entry(FILE_A).is_none(),
            "known limit: the once-only fence is the ledger file, not per-filename coverage"
        );
        assert_eq!(
            rolled_forward.retry(FILE_A),
            RetryDecision::Retry,
            "and so the attempt the old binary made is not held"
        );
    }

    #[test]
    fn a_record_belonging_to_someone_else_is_ignored_and_ours_that_lies_fails_closed() {
        // Another agent's record, and another harness's field set: not our authority.
        let other = tempfile::tempdir().unwrap();
        let other_dir = other.path().join("state");
        fixture::place(
            &other_dir,
            &fixture::codex_attempted(
                "h.someone-else",
                "h.someone-else",
                "incarnation-1",
                "thread-main",
                FILE_A,
                &correlate("thread-main", FILE_A),
            ),
        );
        assert!(open(&other_dir, Harness::Codex).entries().is_empty());

        let crossed = tempfile::tempdir().unwrap();
        let crossed_dir = crossed.path().join("state");
        fixture::place(
            &crossed_dir,
            &fixture::opencode_attempted(
                "h.worker",
                "h.worker",
                "ses_target",
                FILE_A,
                &correlate("ses_target", FILE_A),
            ),
        );
        let ledger = open(&crossed_dir, Harness::Codex);
        assert!(ledger.quarantined().is_none());
        assert!(
            ledger.entries().is_empty(),
            "an OpenCode record is not a Codex delivery"
        );

        // Ours, and self-contradicting: the correlation does not re-derive from its own binding.
        let tampered = tempfile::tempdir().unwrap();
        let tampered_dir = tampered.path().join("state");
        fixture::place(
            &tampered_dir,
            &fixture::codex_attempted(
                "h.worker",
                "h.worker",
                "incarnation-1",
                "thread-main",
                FILE_A,
                "forged",
            ),
        );
        let ledger = open(&tampered_dir, Harness::Codex);
        assert!(
            ledger
                .quarantined()
                .is_some_and(|reason| reason.contains("does not match its binding")),
            "the refusal names itself: {:?}",
            ledger.quarantined()
        );
        assert!(
            fixture::path(&tampered_dir).is_file(),
            "a record we refuse to read is not a record we may destroy"
        );
    }

    #[test]
    fn unreadable_or_unlabelled_bytes_are_ignored_rather_than_quarantining_a_working_pump() {
        for body in [
            b"not json at all".to_vec(),
            serde_json::to_vec(&serde_json::json!({"schema": "st2.something-else.v1"})).unwrap(),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let state_dir = tmp.path().join("state");
            fs::create_dir_all(&state_dir).unwrap();
            fs::write(state_dir.join(LEGACY_FILE), &body).unwrap();
            let ledger = open(&state_dir, Harness::Codex);
            assert!(ledger.quarantined().is_none());
            assert!(ledger.entries().is_empty());
        }
    }
}
