//! Test-only output model for the atomic spool prototype.
//! No native callback or runtime publisher is installed. Model controls were
//! verified separately in st3; this module supplies types to transaction controls.
//!
//! The producer must commit this image together with its timeline operation under
//! the exact provider token and ownership sequence. Publication time is never output
//! time. This is observation coverage, not a turn receipt or permission to recover.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::harness_timeline::Operation;

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
