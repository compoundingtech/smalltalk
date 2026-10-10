//! Test-only native-output reduction model, independent of mail and heartbeats.
//! No callback, provider publication or admitted capability is installed here.
//!
//! The producer must commit this image together with its timeline operation under
//! the exact provider token and ownership sequence. Publication time is never output
//! time. This is observation coverage, not a turn receipt or permission to recover.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use st_drivers::harness_timeline::Operation;

pub const CAPABILITY: &str = "native-output-v1";
pub const SILENCE_MS: u64 = 10 * 60 * 1_000;
const MAX_TOOLS: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Output {
    pub at_unix_ms: u64,
    pub entry_id: String,
    pub sequence: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Snapshot {
    pub capability: String,
    pub driver: String,
    pub component: String,
    pub provider_incarnation: String,
    pub ownership_sequence: u64,
    pub last_output: Option<Output>,
    pub progress_run_since_ms: Option<u64>,
    /// True only after a positively observed start of a fully instrumented native
    /// invocation. Adoption, missing IDs and a capture gap must leave this false.
    pub tool_coverage_complete: bool,
    /// An unanswered call is uncertain; this does not prove a tool is still running.
    pub unresolved_tools: BTreeMap<String, u64>,
}

pub fn component(driver: &str) -> Option<&'static str> {
    match driver {
        "codex" => Some("app-server"),
        "claude" => Some("claude-channel"),
        "pi" => Some("pi-channel"),
        "omp" => Some("omp-channel"),
        "opencode" => Some("native"),
        _ => None,
    }
}

impl Snapshot {
    pub fn new(driver: &str, provider: &str, ownership_sequence: u64) -> Option<Self> {
        if provider.is_empty() || ownership_sequence == 0 {
            return None;
        }
        Some(Self {
            capability: CAPABILITY.into(),
            driver: driver.into(),
            component: component(driver)?.into(),
            provider_incarnation: provider.into(),
            ownership_sequence,
            last_output: None,
            progress_run_since_ms: None,
            tool_coverage_complete: false,
            unresolved_tools: BTreeMap::new(),
        })
    }

    pub fn owns(&self, driver: &str, provider: &str, sequence: u64) -> bool {
        !provider.is_empty()
            && self.capability == CAPABILITY
            && component(driver) == Some(self.component.as_str())
            && self.driver == driver
            && self.provider_incarnation == provider
            && self.ownership_sequence == sequence
            && sequence != 0
    }

    /// A gap retains unanswered calls and old output, but prevents a negative health
    /// inference from absence. It cannot acknowledge or settle any native work.
    pub fn gap(&mut self) {
        self.tool_coverage_complete = false;
        self.progress_run_since_ms = None;
    }

    /// `live_at_ms` is supplied only by a live native callback with an original
    /// observation stamp. Hydration/replay passes None. `positive_progress` is false
    /// for metadata-only messages, empty content and unchanged final/revision flips.
    pub fn observe(
        &mut self,
        operation: &Operation,
        live_at_ms: Option<u64>,
        positive_progress: bool,
    ) {
        if operation.driver != self.driver || operation.incarnation_id != self.provider_incarnation
        {
            self.gap();
            return;
        }
        let Some(at_ms) = live_at_ms.filter(|_| positive_progress) else {
            return;
        };
        let supported = matches!(
            (operation.role.as_str(), operation.entry_type.as_str()),
            ("assistant", "content" | "tool_call") | ("tool", "tool_result")
        );
        if !supported {
            return;
        }
        if operation.entry_type == "content"
            && !operation
                .body
                .get("text")
                .and_then(|text| text.as_str())
                .is_some_and(|text| !text.is_empty())
        {
            return;
        }
        // A repeated result must not become an orphan, and an older operation
        // cannot refresh progress or reconstruct current tool coverage. Producers
        // must also suppress historical nonconsecutive replays before this seam.
        if self.last_output.as_ref().is_some_and(|previous| {
            (
                operation.entry_id.as_str(),
                operation.sequence,
                operation.revision,
            ) == (
                previous.entry_id.as_str(),
                previous.sequence,
                previous.revision,
            ) || at_ms < previous.at_unix_ms
        }) {
            return;
        }
        if matches!(operation.entry_type.as_str(), "tool_call" | "tool_result") {
            let Some(call) = operation
                .body
                .get("call_id")
                .and_then(|id| id.as_str())
                .filter(|id| !id.is_empty() && *id != "unknown")
            else {
                self.gap();
                return;
            };
            if operation.entry_type == "tool_call" {
                // Output while a tool remains unanswered is excluded from healthy.
                // A later result starts a new eligible run rather than inheriting
                // the timestamp from before the exclusion.
                self.progress_run_since_ms = None;
                if self.unresolved_tools.contains_key(call) {
                    // A second positive call with an unanswered ID is ambiguous.
                    // A later result cannot restore the missing coverage.
                    self.gap();
                }
                if !self.unresolved_tools.contains_key(call)
                    && self.unresolved_tools.len() == MAX_TOOLS
                {
                    self.gap();
                    return;
                }
                self.unresolved_tools.entry(call.into()).or_insert(at_ms);
            } else {
                self.progress_run_since_ms = None;
                if self.unresolved_tools.remove(call).is_none() {
                    // An orphan result is output, but cannot prove an empty tool set.
                    self.gap();
                }
            }
        }
        let output = Output {
            at_unix_ms: at_ms,
            entry_id: operation.entry_id.clone(),
            sequence: operation.sequence,
            revision: operation.revision,
        };
        if self.last_output.as_ref().is_none_or(|previous| {
            (output.entry_id.as_str(), output.sequence, output.revision)
                != (
                    previous.entry_id.as_str(),
                    previous.sequence,
                    previous.revision,
                )
                && output.at_unix_ms >= previous.at_unix_ms
        }) {
            if self.progress_run_since_ms.is_none()
                || self.last_output.as_ref().is_none_or(|previous| {
                    output.at_unix_ms.saturating_sub(previous.at_unix_ms) >= SILENCE_MS
                })
            {
                self.progress_run_since_ms = Some(output.at_unix_ms);
            }
            self.last_output = Some(output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn operation(driver: &str, sequence: u64, role: &str, kind: &str) -> Operation {
        Operation {
            operation: "append".into(),
            entry_id: format!("entry-{sequence}"),
            sequence,
            revision: 1,
            role: role.into(),
            entry_type: kind.into(),
            final_entry: true,
            body: json!({"call_id":"call-a", "text":"native output"}),
            driver: driver.into(),
            incarnation_id: "provider-a".into(),
            observed_at_unix_ms: 10,
            source_id: Some(format!("source/{sequence}")),
        }
    }

    #[test]
    fn every_supported_harness_requires_live_positive_output() {
        for driver in ["codex", "claude", "pi", "omp", "opencode"] {
            let mut snapshot = Snapshot::new(driver, "provider-a", 4).unwrap();
            for (role, kind) in [
                ("user", "content"),
                ("assistant", "message"),
                ("system", "usage"),
                ("system", "status"),
            ] {
                snapshot.observe(&operation(driver, 1, role, kind), Some(10), true);
                assert_eq!(snapshot.last_output, None, "{driver}: {role}/{kind}");
            }
            let native = operation(driver, 2, "assistant", "content");
            snapshot.observe(&native, None, true); // transcript catch-up
            snapshot.observe(&native, Some(10), false); // empty envelope/final flip
            assert_eq!(snapshot.last_output, None);
            snapshot.observe(&native, Some(10), true);
            let first = snapshot.clone();
            snapshot.observe(&native, Some(500), true); // publication/replay
            assert_eq!(snapshot, first);
        }
    }

    #[test]
    fn final_tool_call_is_unanswered_until_correlated_result_and_gaps_stay_unknown() {
        let mut snapshot = Snapshot::new("codex", "provider-a", 4).unwrap();
        snapshot.tool_coverage_complete = true;
        snapshot.observe(
            &operation("codex", 1, "assistant", "tool_call"),
            Some(10),
            true,
        );
        assert_eq!(snapshot.unresolved_tools.len(), 1);
        snapshot.gap();
        snapshot.observe(
            &operation("codex", 2, "tool", "tool_result"),
            Some(20),
            true,
        );
        assert!(snapshot.unresolved_tools.is_empty());
        assert!(!snapshot.tool_coverage_complete);
        assert_eq!(snapshot.last_output.unwrap().at_unix_ms, 20);
    }

    #[test]
    fn wrong_provider_or_generation_cannot_reuse_output() {
        let mut snapshot = Snapshot::new("omp", "provider-a", 4).unwrap();
        snapshot.observe(&operation("omp", 1, "assistant", "content"), Some(10), true);
        assert!(!snapshot.owns("omp", "provider-a", 5));
        assert!(!snapshot.owns("omp", "provider-b", 4));
        assert!(!snapshot.owns("pi", "provider-a", 4));
        let successor = Snapshot::new("omp", "provider-b", 5).unwrap();
        assert_eq!(successor.last_output, None);
        assert!(!successor.tool_coverage_complete);
    }

    #[test]
    fn duplicate_results_are_quiet_and_a_gap_needs_a_new_progress_anchor() {
        let mut snapshot = Snapshot::new("codex", "provider-a", 4).unwrap();
        snapshot.tool_coverage_complete = true;
        snapshot.observe(
            &operation("codex", 1, "assistant", "content"),
            Some(10),
            true,
        );
        snapshot.observe(
            &operation("codex", 2, "assistant", "tool_call"),
            Some(20),
            true,
        );
        let result = operation("codex", 3, "tool", "tool_result");
        snapshot.observe(&result, Some(30), true);
        assert_eq!(snapshot.progress_run_since_ms, Some(30));
        let after_result = snapshot.clone();
        snapshot.observe(&result, Some(50), true);
        assert_eq!(snapshot, after_result);
        snapshot.gap();
        assert_eq!(snapshot.progress_run_since_ms, None);
        assert_eq!(snapshot.last_output, after_result.last_output);
        snapshot.observe(
            &operation("codex", 4, "assistant", "content"),
            Some(60),
            true,
        );
        assert_eq!(snapshot.progress_run_since_ms, Some(60));
        assert!(!snapshot.tool_coverage_complete);
    }

    #[test]
    fn reusing_an_unanswered_tool_id_cannot_restore_complete_coverage() {
        let mut snapshot = Snapshot::new("pi", "provider-a", 4).unwrap();
        snapshot.tool_coverage_complete = true;
        snapshot.observe(
            &operation("pi", 1, "assistant", "tool_call"),
            Some(10),
            true,
        );
        snapshot.observe(
            &operation("pi", 2, "assistant", "tool_call"),
            Some(20),
            true,
        );
        assert_eq!(snapshot.unresolved_tools.get("call-a"), Some(&10));
        assert!(!snapshot.tool_coverage_complete);
        snapshot.observe(&operation("pi", 3, "tool", "tool_result"), Some(30), true);
        assert!(snapshot.unresolved_tools.is_empty());
        assert!(!snapshot.tool_coverage_complete);
    }

    #[test]
    fn tool_overflow_retains_bounded_uncertainty_without_recovering_coverage() {
        let mut snapshot = Snapshot::new("omp", "provider-a", 4).unwrap();
        snapshot.tool_coverage_complete = true;
        for sequence in 1..=65 {
            let mut call = operation("omp", sequence, "assistant", "tool_call");
            call.body["call_id"] = serde_json::json!(format!("call-{sequence}"));
            snapshot.observe(&call, Some(sequence), true);
        }
        assert_eq!(snapshot.unresolved_tools.len(), 64);
        assert!(!snapshot.tool_coverage_complete);
        assert_eq!(snapshot.progress_run_since_ms, None);
        assert_eq!(snapshot.last_output.as_ref().unwrap().at_unix_ms, 64);
        let mut result = operation("omp", 66, "tool", "tool_result");
        result.body["call_id"] = serde_json::json!("call-1");
        snapshot.observe(&result, Some(66), true);
        assert_eq!(snapshot.unresolved_tools.len(), 63);
        assert!(!snapshot.tool_coverage_complete);
    }

    #[test]
    fn content_revisions_advance_one_healthy_run_but_a_silence_gap_starts_another() {
        let mut snapshot = Snapshot::new("codex", "provider-a", 4).unwrap();
        let mut first = operation("codex", 2, "assistant", "content");
        first.body = serde_json::json!({"text":""});
        snapshot.observe(&first, Some(10), true);
        assert_eq!(snapshot.last_output, None);
        first.body = serde_json::json!({"text":"first"});
        snapshot.observe(&first, Some(10), true);
        let mut other = operation("codex", 3, "assistant", "content");
        snapshot.observe(&other, Some(20), true);
        first.revision = 2; // an older entry can receive a genuine new content delta
        first.body = serde_json::json!({"text":"first continued"});
        snapshot.observe(&first, Some(30), true);
        assert_eq!(snapshot.last_output.as_ref().unwrap().revision, 2);
        assert_eq!(snapshot.progress_run_since_ms, Some(10));
        other.revision = 2;
        snapshot.observe(&other, Some(30 + SILENCE_MS), true);
        assert_eq!(snapshot.progress_run_since_ms, Some(30 + SILENCE_MS));
    }
}
