//! Durable evidence that a provider owes work. Source sequence numbers identify our receipts,
//! never a provider turn or checkpoint. Idle, process exit and session selection settle nothing.
use serde::{Deserialize, Serialize};

pub const MAX_OPEN: usize = 32;
pub const MAX_TERMINALS: usize = 64;
pub const MAX_LEDGER_BYTES: usize = 24 * 1024;
pub const START_BUDGET_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub sequence: u64,
    #[serde(default)]
    pub open: Vec<Obligation>,
    /// A damaged predecessor or exhausted bound cannot be represented as a clean empty ledger.
    #[serde(default)]
    pub unknown: bool,
    /// Source provenance for uncertainty, never a provider turn ID or checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_evidence: Option<UnknownEvidence>,
    /// A turn terminal cannot prove the outcome of an invocation whose result was lost.
    #[serde(default)]
    pub unknown_tool_outcome: bool,
    #[serde(default)]
    pub terminal: Vec<Terminal>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownEvidence {
    pub provider_incarnation: Option<String>,
    pub ownership_sequence: u64,
    pub source_sequence: u64,
    pub observed_at_ms: u64,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    pub source_sequence: u64,
    pub provider_incarnation: String,
    pub ownership_sequence: u64,
    pub runtime_incarnation: Option<String>,
    pub desired_revision: Option<String>,
    pub native_session_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub started_at_ms: u64,
    pub pending_human: bool,
    /// The producer saw a tool invocation with no corresponding result. No recovery path replays it.
    pub tool_outcome_unknown: bool,
    #[serde(default)]
    pub pending_tool_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    pub obligation: Obligation,
    pub outcome: String,
    pub evidence_provider_incarnation: String,
    pub observed_at_ms: u64,
}

/// Positively observed late native tool result, carrying the original exact terminal
/// and the remaining unknown invocation IDs. A clean turn is not such a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub terminal: Terminal,
    pub tool_id: String,
    pub evidence_provider_incarnation: String,
    pub observed_at_ms: u64,
}

/// The current observer's terminal correlation, captured under its ownership lock.
pub struct TerminalSource<'a> {
    pub provider: &'a str,
    pub revision: Option<&'a str>,
    pub session: Option<&'a str>,
    pub turn: Option<&'a str>,
    pub retained_start: Option<u64>,
}

impl Ledger {
    pub fn start(&mut self, mut obligation: Obligation) -> Option<u64> {
        if let Some(existing) = self.open.iter().find(|current| {
            obligation.native_turn_id.is_some()
                && current.provider_incarnation == obligation.provider_incarnation
                && current.native_session_id == obligation.native_session_id
                && current.native_turn_id == obligation.native_turn_id
                && current.desired_revision == obligation.desired_revision
        }) {
            return Some(existing.source_sequence);
        }
        // A terminal's exact native identity is monotone, even if delayed start events arrive.
        if obligation.native_turn_id.is_some()
            && self
                .terminal
                .iter()
                .chain(self.tool_results.iter().map(|result| &result.terminal))
                .any(|terminal| {
                    (terminal.obligation.provider_incarnation == obligation.provider_incarnation
                        || (obligation.desired_revision.is_some()
                            && obligation.native_session_id.is_some()))
                        && terminal.obligation.native_session_id == obligation.native_session_id
                        && terminal.obligation.native_turn_id == obligation.native_turn_id
                        && terminal.obligation.desired_revision == obligation.desired_revision
                })
        {
            return None;
        }
        if self.sequence == u64::MAX {
            self.note_unknown(
                Some(&obligation.provider_incarnation),
                obligation.ownership_sequence,
                obligation.started_at_ms,
                "receipt-sequence-exhausted",
            );
            return None;
        }
        self.sequence += 1;
        if self.open.len() >= MAX_OPEN {
            self.note_unknown(
                Some(&obligation.provider_incarnation),
                obligation.ownership_sequence,
                obligation.started_at_ms,
                "receipt-capacity-exhausted",
            );
            return None;
        }
        obligation.source_sequence = self.sequence;
        self.open.push(obligation);
        while serde_json::to_vec(self).expect("receipt serializes").len() > START_BUDGET_BYTES
            && (!self.terminal.is_empty() || !self.tool_results.is_empty())
        {
            if !self.terminal.is_empty() {
                self.terminal.remove(0);
            } else {
                self.tool_results.remove(0);
            }
        }
        if serde_json::to_vec(self).expect("receipt serializes").len() > START_BUDGET_BYTES {
            let refused = self.open.pop().expect("new receipt");
            self.note_unknown(
                Some(&refused.provider_incarnation),
                refused.ownership_sequence,
                refused.started_at_ms,
                "receipt-byte-bound-exhausted",
            );
            return None;
        }
        Some(self.sequence)
    }

    pub fn note_unknown(
        &mut self,
        provider: Option<&str>,
        ownership: u64,
        now_ms: u64,
        reason: &str,
    ) {
        self.unknown = true;
        self.unknown_evidence = Some(UnknownEvidence {
            provider_incarnation: provider.map(str::to_owned),
            ownership_sequence: ownership,
            source_sequence: self.sequence,
            observed_at_ms: now_ms,
            reason: reason.into(),
        });
    }

    /// Only exact native identity or the producing observer's retained start receipt can close
    /// an entry. The caller must hold the current provider ownership lock.
    pub fn settle(&mut self, source: TerminalSource<'_>, outcome: &str, now_ms: u64) -> bool {
        let TerminalSource {
            provider,
            revision,
            session,
            turn,
            retained_start,
        } = source;
        if !matches!(outcome, "completed" | "cancelled" | "failed") {
            return false;
        }
        let matches = |entry: &Obligation| {
            entry.desired_revision.as_deref() == revision
                && entry.native_session_id.as_deref() == session
                && if let Some(turn) = turn {
                    entry.native_turn_id.as_deref() == Some(turn)
                        && (entry.provider_incarnation == provider
                            || (revision.is_some() && session.is_some()))
                } else {
                    entry.provider_incarnation == provider
                        && retained_start == Some(entry.source_sequence)
                        && entry.native_turn_id.is_none()
                }
        };
        if !self.open.iter().any(matches) {
            return false;
        }
        let mut settled = false;
        while let Some(position) = self.open.iter().position(matches) {
            if self.terminal.len() >= MAX_TERMINALS {
                self.terminal.remove(0);
            }
            let obligation = self.open.remove(position);
            self.unknown_tool_outcome |= obligation.tool_outcome_unknown;
            self.terminal.push(Terminal {
                obligation,
                outcome: outcome.into(),
                evidence_provider_incarnation: provider.into(),
                observed_at_ms: now_ms,
            });
            settled = true;
        }
        while serde_json::to_vec(self).expect("receipt serializes").len() > MAX_LEDGER_BYTES {
            if self.terminal.len() > 1 {
                self.terminal.remove(0);
            } else if !self.tool_results.is_empty() {
                self.tool_results.remove(0);
            } else {
                break;
            }
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{harness_events, harness_state as hs};

    fn writer(dir: &std::path::Path, provider: &str, runtime: &str) -> hs::Writer {
        let sequence = hs::claim(dir, "agent/fixture", "omp", provider).unwrap();
        hs::Writer::new(dir, "agent/fixture", "omp", Some(runtime.into()))
            .with_ownership(provider, sequence)
            .with_turn_fence(Some(runtime.into()), Some("revision-one".into()))
    }
    fn observed(dir: &std::path::Path) -> hs::Observed {
        hs::read(&hs::harness_state_path(dir), None).unwrap()
    }

    #[test]
    fn late_positive_tool_results_use_original_terminal_and_never_a_clean_turn() {
        let root = tempfile::tempdir().unwrap();
        harness_events::enable(root.path(), "runtime-fixture").unwrap();
        let mut producer = writer(root.path(), "provider-fixture", "runtime-fixture");
        producer.turn_started(Some("native-session"), None).unwrap();
        let receipt = producer.turn_receipt().unwrap();
        for id in ["first", "second"] {
            producer
                .turn_tool(Some("native-session"), None, id, false)
                .unwrap();
        }
        producer
            .turn_terminal(Some("native-session"), None, "cancelled")
            .unwrap();
        producer.turn_started(Some("native-session"), None).unwrap();
        producer
            .turn_terminal(Some("native-session"), None, "completed")
            .unwrap();
        assert!(
            observed(root.path()).turn_obligation.unknown_tool_outcome,
            "clean later terminal cannot settle the original invocation"
        );
        producer.retain_turn_receipt(&receipt).unwrap();
        producer
            .turn_tool(Some("wrong-session"), None, "first", true)
            .unwrap();
        assert!(
            observed(root.path())
                .turn_obligation
                .tool_results
                .is_empty()
        );
        producer
            .turn_tool(Some("native-session"), None, "first", true)
            .unwrap();
        assert!(producer.late_tool_result_recorded(&receipt, "first"));
        let partial = observed(root.path()).turn_obligation;
        assert_eq!(
            partial.tool_results[0].terminal.obligation.pending_tool_ids,
            ["second"]
        );
        assert!(partial.unknown_tool_outcome);
        producer
            .turn_tool(Some("native-session"), None, "second", true)
            .unwrap();
        let complete = observed(root.path()).turn_obligation;
        assert!(
            !complete.tool_results[0]
                .terminal
                .obligation
                .tool_outcome_unknown
        );
        assert!(!complete.unknown_tool_outcome);
        assert_eq!(complete.tool_results[0].terminal.outcome, "cancelled");
        assert_eq!(
            complete.tool_results[0].terminal.obligation.source_sequence,
            receipt["source_sequence"].as_u64().unwrap()
        );
        assert!(producer.terminal_receipt_recorded(&receipt));
    }

    #[test]
    fn unsupported_adapters_expose_scoped_unknown_without_inventing_native_turns() {
        for driver in ["claude", "opencode"] {
            let root = tempfile::tempdir().unwrap();
            let seq = hs::claim(root.path(), "agent/fixture", driver, "old").unwrap();
            let mut old = hs::Writer::new(
                root.path(),
                "agent/fixture",
                driver,
                Some("runtime-old".into()),
            )
            .with_ownership("old", seq);
            old.observe(hs::Observation::new(
                hs::Activity::Active,
                hs::BlockedOn::None,
                hs::InputBuffer::Unknown,
            ))
            .unwrap();
            hs::claim(root.path(), "agent/fixture", driver, "new").unwrap();
            let first = observed(root.path()).turn_obligation;
            assert!(first.open.is_empty());
            assert!(first.unknown);
            let scope = first.unknown_evidence.as_ref().unwrap();
            assert_eq!(scope.provider_incarnation.as_deref(), Some("old"));
            assert_eq!(scope.reason, "native-turn-tracking-unavailable");
            let seq = observed(root.path()).ownership_sequence.unwrap();
            let mut next = hs::Writer::new(
                root.path(),
                "agent/fixture",
                driver,
                Some("runtime-new".into()),
            )
            .with_ownership("new", seq);
            next.observe(hs::Observation::new(
                hs::Activity::Active,
                hs::BlockedOn::None,
                hs::InputBuffer::Unknown,
            ))
            .unwrap();
            hs::claim(root.path(), "agent/fixture", driver, "third").unwrap();
            let second = observed(root.path()).turn_obligation;
            assert!(second.open.is_empty());
            assert_ne!(
                first.unknown_evidence, second.unknown_evidence,
                "another interruption has fresh source provenance"
            );
        }
    }

    #[test]
    fn malformed_native_identity_and_untrackable_tool_are_durable_unknown() {
        let temp = tempfile::tempdir().unwrap();
        harness_events::enable(temp.path(), "runtime-fixture").unwrap();
        let mut producer = writer(temp.path(), "provider-fixture", "runtime-fixture");
        producer
            .turn_started(Some("native-session"), Some(""))
            .unwrap();
        let ledger = observed(temp.path()).turn_obligation;
        assert!(ledger.unknown);
        assert!(ledger.open.is_empty());
        assert!(producer.turn_receipt().is_none());
        producer
            .turn_started(Some("native-session"), Some("native-turn"))
            .unwrap();
        producer
            .turn_tool(Some("native-session"), Some("native-turn"), "", false)
            .unwrap();
        producer
            .turn_terminal(Some("native-session"), Some("native-turn"), "completed")
            .unwrap();
        let ledger = observed(temp.path()).turn_obligation;
        assert!(ledger.unknown_tool_outcome);
        assert!(ledger.open.is_empty());
    }

    #[test]
    fn isolated_provider_fixture_child() {
        let Some(root) = std::env::var_os("ST_OBLIGATION_FIXTURE_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        harness_events::enable(&root, "runtime-fixture").unwrap();
        let mut producer = writer(&root, "fixture-provider", "runtime-fixture");
        producer
            .turn_started(Some("fixture-native-session"), None)
            .unwrap();
        let scenario = std::env::var("ST_OBLIGATION_FIXTURE_SCENARIO").unwrap();
        if matches!(scenario.as_str(), "tool-pending" | "tool-result") {
            producer
                .turn_tool(Some("fixture-native-session"), None, "fixture-tool", false)
                .unwrap();
            if scenario == "tool-result" {
                producer
                    .turn_tool(Some("fixture-native-session"), None, "fixture-tool", true)
                    .unwrap();
            }
        }
        producer
            .observe(hs::Observation::new(
                hs::Activity::Active,
                if scenario == "human" {
                    hs::BlockedOn::Human
                } else {
                    hs::BlockedOn::None
                },
                hs::InputBuffer::Unknown,
            ))
            .unwrap();
        if scenario == "terminal" {
            producer
                .turn_terminal(Some("fixture-native-session"), None, "completed")
                .unwrap();
        }
        std::fs::write(root.join("boundary-committed"), b"ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn killed_isolated_producer_preserves_model_tool_result_tool_pending_and_human_boundaries() {
        use std::time::{Duration, Instant};
        for scenario in ["model", "tool-result", "tool-pending", "human", "terminal"] {
            let temp = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "turn_obligation::tests::isolated_provider_fixture_child",
                    "--nocapture",
                ])
                .env("ST_OBLIGATION_FIXTURE_ROOT", temp.path())
                .env("ST_OBLIGATION_FIXTURE_SCENARIO", scenario)
                .env_remove("ST_AGENT")
                .env_remove("ST3_BIN")
                .env_remove("ST3_ENDPOINT")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !temp.path().join("boundary-committed").exists() && Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let committed = temp.path().join("boundary-committed").exists();
            let _ = child.kill();
            child.wait().unwrap();
            assert!(
                committed,
                "isolated boundary must commit before the crash: {scenario}"
            );
            harness_events::enable(temp.path(), "runtime-successor").unwrap();
            let mut successor = writer(temp.path(), "fixture-successor", "runtime-successor");
            successor
                .observe(hs::Observation::new(
                    hs::Activity::Idle,
                    hs::BlockedOn::None,
                    hs::InputBuffer::Empty,
                ))
                .unwrap();
            let ledger = observed(temp.path()).turn_obligation;
            if scenario == "terminal" {
                assert!(
                    ledger.open.is_empty(),
                    "persisted terminal proof survives SIGKILL without an idle edge"
                );
                assert_eq!(ledger.terminal.len(), 1);
            } else {
                assert_eq!(
                    ledger.open.len(),
                    1,
                    "idle successor cannot erase {scenario}"
                );
                assert_eq!(
                    ledger.open[0].tool_outcome_unknown,
                    scenario == "tool-pending"
                );
                assert_eq!(ledger.open[0].pending_human, scenario == "human");
                assert!(
                    ledger.open[0].native_turn_id.is_none(),
                    "OMP receipt must not become an invented native turn ID"
                );
            }
        }
    }

    #[test]
    fn provider_replacement_and_idle_preserve_pending_tool_and_human_debt() {
        let temp = tempfile::tempdir().unwrap();
        let mut old = writer(temp.path(), "old", "runtime-old");
        old.turn_started(Some("native-session"), None).unwrap();
        let receipt = old.turn_receipt().unwrap();
        old.turn_tool(Some("native-session"), None, "side-effecting-tool", false)
            .unwrap();
        old.observe(hs::Observation::new(
            hs::Activity::Active,
            hs::BlockedOn::Human,
            hs::InputBuffer::Unknown,
        ))
        .unwrap();
        drop(old);
        let mut successor = writer(temp.path(), "new", "runtime-new");
        successor
            .observe(hs::Observation::new(
                hs::Activity::Idle,
                hs::BlockedOn::None,
                hs::InputBuffer::Empty,
            ))
            .unwrap();
        let debt = observed(temp.path()).turn_obligation;
        assert_eq!(debt.open.len(), 1);
        assert!(debt.open[0].pending_human);
        assert!(debt.open[0].tool_outcome_unknown);
        assert_eq!(
            debt.open[0].runtime_incarnation.as_deref(),
            Some("runtime-old")
        );
        successor
            .turn_terminal_receipt(Some("native-session"), "completed", &receipt)
            .unwrap();
        assert_eq!(
            observed(temp.path()).turn_obligation.open.len(),
            1,
            "successor cannot consume another provider's source receipt"
        );
    }

    #[test]
    fn channel_replacement_can_close_its_retained_receipt_but_not_new_input() {
        let temp = tempfile::tempdir().unwrap();
        let mut hook = writer(temp.path(), "provider", "runtime-one");
        hook.turn_started(Some("native-session"), None).unwrap();
        let first = hook.turn_receipt().unwrap();
        let ownership = first["ownership_sequence"].as_u64().unwrap();
        drop(hook);
        let mut replacement = hs::Writer::new(
            temp.path(),
            "agent/fixture",
            "omp",
            Some("runtime-one".into()),
        )
        .with_ownership("provider", ownership)
        .with_turn_fence(Some("runtime-one".into()), Some("revision-one".into()));
        replacement
            .turn_started(Some("native-session"), None)
            .unwrap();
        replacement
            .turn_terminal_receipt(Some("native-session"), "cancelled", &first)
            .unwrap();
        assert!(replacement.terminal_receipt_recorded(&first));
        let debt = observed(temp.path()).turn_obligation;
        assert_eq!(debt.open.len(), 1);
        assert_ne!(
            debt.open[0].source_sequence,
            first["source_sequence"].as_u64().unwrap()
        );
        assert_eq!(debt.terminal[0].outcome, "cancelled");
    }

    #[test]
    fn durable_outbox_keeps_clean_terminal_before_any_idle_publication() {
        let temp = tempfile::tempdir().unwrap();
        harness_events::enable(temp.path(), "runtime-old").unwrap();
        let mut old = writer(temp.path(), "old", "runtime-old");
        old.turn_started(Some("native-session"), Some("native-turn"))
            .unwrap();
        old.turn_terminal(Some("native-session"), Some("native-turn"), "completed")
            .unwrap();
        drop(old);
        // This is provider/channel reconstruction over durable storage, not a history query.
        let _successor = writer(temp.path(), "new", "runtime-new");
        let debt = observed(temp.path()).turn_obligation;
        assert!(debt.open.is_empty());
        assert_eq!(
            debt.terminal[0].obligation.native_turn_id.as_deref(),
            Some("native-turn")
        );
        assert_eq!(debt.terminal[0].outcome, "completed");
    }
    fn obligation(provider: &str, turn: Option<&str>) -> Obligation {
        Obligation {
            source_sequence: 0,
            provider_incarnation: provider.into(),
            ownership_sequence: 1,
            runtime_incarnation: Some(format!("runtime-{provider}")),
            desired_revision: Some("revision-one".into()),
            native_session_id: Some("native-session".into()),
            native_turn_id: turn.map(str::to_owned),
            started_at_ms: 1,
            pending_human: false,
            tool_outcome_unknown: false,
            pending_tool_ids: Vec::new(),
        }
    }
    #[test]
    fn exact_terminal_survives_loss_and_fences_other_input_and_revision() {
        let mut ledger = Ledger::default();
        ledger.start(obligation("old", Some("turn-one")));
        let bytes = serde_json::to_vec(&ledger).unwrap();
        let mut ledger: Ledger = serde_json::from_slice(&bytes).unwrap();
        ledger.start(obligation("new", Some("turn-two")));
        assert!(!ledger.settle(
            TerminalSource {
                provider: "new",
                revision: Some("revision-two"),
                session: Some("native-session"),
                turn: Some("turn-one"),
                retained_start: None,
            },
            "completed",
            2
        ));
        assert!(!ledger.settle(
            TerminalSource {
                provider: "new",
                revision: Some("revision-one"),
                session: Some("another-session"),
                turn: Some("turn-one"),
                retained_start: None,
            },
            "completed",
            2
        ));
        assert!(ledger.settle(
            TerminalSource {
                provider: "new",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: Some("turn-two"),
                retained_start: None,
            },
            "completed",
            2
        ));
        assert_eq!(ledger.open[0].native_turn_id.as_deref(), Some("turn-one"));
        assert!(ledger.settle(
            TerminalSource {
                provider: "new",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: Some("turn-one"),
                retained_start: None,
            },
            "completed",
            3
        ));
        assert!(ledger.start(obligation("old", Some("turn-one"))).is_none());
    }
    #[test]
    fn unidentified_successor_cannot_close_a_predecessor_receipt() {
        let mut ledger = Ledger::default();
        let source = ledger.start(obligation("old", None));
        assert!(!ledger.settle(
            TerminalSource {
                provider: "new",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: None,
                retained_start: source,
            },
            "completed",
            2
        ));
        assert!(!ledger.settle(
            TerminalSource {
                provider: "old",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: None,
                retained_start: None,
            },
            "completed",
            2
        ));
        assert!(!ledger.settle(
            TerminalSource {
                provider: "old",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: None,
                retained_start: source,
            },
            "exit 0",
            2
        ));
        assert!(ledger.settle(
            TerminalSource {
                provider: "old",
                revision: Some("revision-one"),
                session: Some("native-session"),
                turn: None,
                retained_start: source,
            },
            "cancelled",
            2
        ));
    }
    #[test]
    fn an_exhausted_bound_preserves_unknown_and_all_unsettled_work() {
        let mut ledger = Ledger::default();
        for _ in 0..MAX_OPEN {
            ledger.start(obligation("old", None));
        }
        assert!(ledger.start(obligation("new", None)).is_none());
        assert!(ledger.unknown);
        assert_eq!(ledger.open.len(), MAX_OPEN);
    }
}
