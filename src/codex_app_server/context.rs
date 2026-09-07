//! The Codex half of the harness-context record.
//!
//! Moved verbatim out of the parent module: Codex's occupancy arithmetic and the producer that
//! turns its token-usage and rate-limit frames into a harness-context reading.

use std::collections::VecDeque;

use anyhow::Result;
use serde_json::Value;

use crate::harness_context;

/// Codex's `BASELINE_TOKENS`, subtracted from BOTH the numerator and the denominator of its
/// displayed occupancy: `codex-rs/protocol/src/protocol.rs:2332` and
/// `codex-rs/tui/src/token_usage.rs:9` at `rust-v0.151.0` carry the same literal with an identical
/// function body, and no configuration override exists. Its doc comment: "should capture tokens
/// that are always present in the context (e.g. system prompt and fixed tool instructions) so that
/// the percentage reflects the portion the user can influence."
pub(super) const CODEX_BASELINE_TOKENS: i64 = 12_000;

/// The seven-day rate-limit window, identified by its duration because
/// `account/rateLimits/updated` names its windows `primary`/`secondary` and nothing else. 10,080
/// minutes = 7 days, and the one captured Codex rate-limit snapshot (rollout, 0.150.1) carries
/// exactly this window as `primary`. See [`CodexContextProducer::observe_rate_limits`] for why the
/// five-hour leg stays `null`.
const CODEX_SEVEN_DAY_WINDOW_MINUTES: i64 = 10_080;

/// How many recent compaction identities the dedupe retains. One compaction reaches this observer
/// as both `item/started` and `item/completed` — and possibly also as the deprecated
/// `thread/compacted` — so counting the edge naively counts one compaction twice or three times. A
/// last-key-only memory would still miscount an interleaving (`started(A)`, `started(B)`,
/// `completed(A)`), which a small ring closes for the same cost.
const CODEX_COMPACTION_MEMORY: usize = 4;

/// Codex's own occupancy arithmetic, mirrored rather than re-derived: the published number is
/// exactly `100 −` the "N% context left" the operator reads in the Codex footer.
///
/// `codex-rs/tui/src/token_usage.rs:43` (and its protocol twin):
///
/// ```text
/// if context_window <= BASELINE_TOKENS { return 0; }
/// effective = context_window - BASELINE_TOKENS
/// used      = (last.total_tokens - BASELINE_TOKENS).max(0)
/// remaining = (effective - used).max(0)
/// ((remaining / effective) * 100).clamp(0,100).round()
/// ```
///
/// Three things this deliberately does NOT do:
///
/// - It does not round the *used* percentage. Rounding `used/effective` and rounding
///   `remaining/effective` disagree on a half — effective 200, used 101 gives 51 one way and 50
///   the other — and only the mirrored order satisfies the spec's "equals `100 −` Codex's
///   displayed '% context left'".
/// - It does not use `total`, which is cumulative session spend. Against the captured window a
///   `total`-based percent reads 100 where the true occupancy is 33.
/// - It does not use `last.inputTokens`, which gives ~36 against the same capture — close enough
///   to look right and wrong by construction.
///
/// The one divergence from the source: where Codex returns `0` remaining for a window at or below
/// the baseline, mirroring blindly would publish "100% used" for a window it cannot normalize. st2
/// withholds instead (HC-R02, HC-R03) — a saturation the harness never displayed is fabricated,
/// not observed.
///
/// The result cannot exceed 100: Codex's `remaining` is floored at zero, so an occupancy above the
/// effective window saturates in the harness's own arithmetic before st2 ever sees it. That is a
/// property of mirroring Codex, not a clamp of st2's — the record still carries what a producer
/// computes, unclamped (HC-R02), and the harnesses that can report an overrun are the ones
/// publishing a float of their own.
pub(super) fn codex_used_percent(window_tokens: Option<i64>, last_total_tokens: i64) -> Option<f64> {
    let window = window_tokens?;
    if window <= CODEX_BASELINE_TOKENS {
        return None;
    }
    let effective = window - CODEX_BASELINE_TOKENS;
    let used = (last_total_tokens - CODEX_BASELINE_TOKENS).max(0);
    let remaining = (effective - used).max(0);
    let remaining_percent = ((remaining as f64 / effective as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round();
    Some(100.0 - remaining_percent)
}

/// One compaction's identity as this observer can name it. The item events carry a stable item id
/// alongside the turn; the deprecated `thread/compacted` notification carries only the turn, so its
/// key collapses with any item key in the same turn rather than counting beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexCompactionKey {
    turn_id: String,
    item_id: Option<String>,
}

impl CodexCompactionKey {
    /// Whether these two names describe the same compaction. Two distinct item ids in one turn are
    /// two compactions; a turn-only name in a turn already counted is the same one under its other
    /// spelling.
    fn same_compaction(&self, other: &Self) -> bool {
        self.turn_id == other.turn_id
            && match (&self.item_id, &other.item_id) {
                (Some(mine), Some(theirs)) => mine == theirs,
                _ => true,
            }
    }
}

/// The Codex half of the harness-context record (HC-R11).
///
/// It owns a [`harness_context::Writer`] beside the harness-state writer, sharing the wrapper's
/// incarnation so both records name the same session as their provenance. It holds no guard of its
/// own: `thread/tokenUsage/updated` arrives once per model response — roughly 10–15 per turn, and
/// replayed to a newly attached connection on resume — and every one of them is handed to
/// [`harness_context::Writer::observe`], whose quantization is the only thing deciding what lands.
/// A second guard here would make the write policy per-harness, which HC-R09 exists to prevent.
///
/// The only state it carries between notifications is what it cannot recover from the next one:
/// the account-scoped rate-limit windows (a separate notification with no reading behind it) and
/// the identities of recently counted compactions.
pub(super) struct CodexContextProducer {
    writer: harness_context::Writer,
    /// Last-known account-scoped windows. `account/rateLimits/updated` is documented as a *sparse
    /// rolling update* whose absent fields do not clear a previously observed value, so the last
    /// known windows ride along with the next reading instead of blanking it.
    rate_limits: harness_context::RateLimits,
    counted_compactions: VecDeque<CodexCompactionKey>,
}

impl CodexContextProducer {
    pub(super) fn new(writer: harness_context::Writer) -> Self {
        Self {
            writer,
            rate_limits: harness_context::RateLimits::default(),
            counted_compactions: VecDeque::new(),
        }
    }

    /// Project one inbound control frame onto the context record, returning whether a write landed.
    ///
    /// Every unknown method, foreign thread, and malformed payload is ignored rather than failed:
    /// this is observability riding a delivery socket, and a frame this producer cannot read must
    /// not disturb the frame the delivery loop can.
    pub(super) fn observe(&mut self, message: &Value, thread_id: &str) -> Result<bool> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        match method {
            "thread/tokenUsage/updated" => {
                if message.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id) {
                    return Ok(false);
                }
                let Some(reading) = self.token_usage_reading(message) else {
                    return Ok(false);
                };
                self.writer.observe(reading)
            }
            // Account-scoped and thread-free (HC-T06): it repeats across every runtime sharing the
            // account, carries no occupancy, and therefore never writes on its own. It is held and
            // published by the next reading.
            "account/rateLimits/updated" => {
                self.observe_rate_limits(message);
                Ok(false)
            }
            "item/started" | "item/completed" => {
                if message.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id)
                    || message.pointer("/params/item/type").and_then(Value::as_str)
                        != Some("contextCompaction")
                {
                    return Ok(false);
                }
                let (Some(turn_id), Some(item_id)) = (
                    message.pointer("/params/turnId").and_then(Value::as_str),
                    message.pointer("/params/item/id").and_then(Value::as_str),
                ) else {
                    return Ok(false);
                };
                self.compacted(CodexCompactionKey {
                    turn_id: turn_id.to_string(),
                    item_id: Some(item_id.to_string()),
                })
            }
            // Deprecated in the protocol in favour of the item ("Deprecated: Use
            // `ContextCompaction` item type instead") and unobserved on 0.150.1. Handled anyway,
            // and deduped against the item, because a harness emitting both must still count one
            // compaction.
            "thread/compacted" => {
                if message.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id) {
                    return Ok(false);
                }
                let Some(turn_id) = message.pointer("/params/turnId").and_then(Value::as_str)
                else {
                    return Ok(false);
                };
                self.compacted(CodexCompactionKey {
                    turn_id: turn_id.to_string(),
                    item_id: None,
                })
            }
            _ => Ok(false),
        }
    }

    /// The reading a `thread/tokenUsage/updated` carries, in Codex's own arithmetic.
    ///
    /// `usedTokens` and `windowTokens` are the harness's raw operands and are published as they
    /// arrive — a window at or below the baseline is still a window the harness reported, even
    /// where it cannot produce a percent. `model` and `costUsd` are `null` because the channel
    /// carries neither: the app-server `Thread` object has `modelProvider` and no model identifier,
    /// and Codex reports no session cost anywhere in the protocol (HC-R16).
    fn token_usage_reading(&self, message: &Value) -> Option<harness_context::Reading> {
        let last_total = message
            .pointer("/params/tokenUsage/last/totalTokens")
            .and_then(Value::as_i64)?;
        let window = message
            .pointer("/params/tokenUsage/modelContextWindow")
            .and_then(Value::as_i64)
            .filter(|window| *window > 0);
        Some(harness_context::Reading {
            used_tokens: u64::try_from(last_total).ok(),
            window_tokens: window.and_then(|window| u64::try_from(window).ok()),
            used_percent: codex_used_percent(window, last_total),
            model: None,
            cost_usd: None,
            // Cumulative lifetime spend and never occupancy (HC-R16): the captured session read
            // 2,235,329 against a 258,400-token window.
            session_total_tokens: message
                .pointer("/params/tokenUsage/total/totalTokens")
                .and_then(Value::as_i64)
                .and_then(|total| u64::try_from(total).ok()),
            rate_limits: self.rate_limits,
        })
    }

    /// Merge a sparse rate-limit update into the last-known windows.
    ///
    /// Codex names its windows `primary` and `secondary` and identifies them only by
    /// `windowDurationMins`, so the join is by duration. Only the seven-day window is carried: the
    /// single captured Codex rate-limit snapshot (0.150.1) contains one window, `primary`, at
    /// 10,080 minutes. No 300-minute window and no `secondary` was ever observed on this harness,
    /// so mapping one onto `fiveHour` would be inference dressed as a measurement — and this
    /// record's whole point is that its numbers were seen. `fiveHour` therefore stays `null` for
    /// Codex until a capture shows the window; admitting it is a one-line change beside the
    /// capture that justifies it.
    fn observe_rate_limits(&mut self, message: &Value) {
        for window in ["primary", "secondary"] {
            let Some(snapshot) = message.pointer(&format!("/params/rateLimits/{window}")) else {
                continue;
            };
            if snapshot.get("windowDurationMins").and_then(Value::as_i64)
                == Some(CODEX_SEVEN_DAY_WINDOW_MINUTES)
                && let Some(used) = snapshot.get("usedPercent").and_then(Value::as_f64)
            {
                self.rate_limits.seven_day = Some(used);
            }
        }
    }

    /// Count one compaction edge unless this compaction was already counted under another of its
    /// spellings. The count is incarnation-scoped: Codex publishes an edge and nothing else, so st2
    /// does the counting and the relaunch claim's record removal resets it (HC-R12, HC-R15). The
    /// trigger is `unknown` because `ContextCompactionThreadItem` carries `id` and `type` and no
    /// reason at all.
    fn compacted(&mut self, key: CodexCompactionKey) -> Result<bool> {
        if self
            .counted_compactions
            .iter()
            .any(|counted| counted.same_compaction(&key))
        {
            return Ok(false);
        }
        self.counted_compactions.push_back(key);
        while self.counted_compactions.len() > CODEX_COMPACTION_MEMORY {
            self.counted_compactions.pop_front();
        }
        self.writer.compacted(harness_context::Compaction::new(
            harness_context::CompactionTrigger::Unknown,
        ))
    }
}
