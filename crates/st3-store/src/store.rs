// Literal recipient predicates shared with the private point qualification reader.
// Callers choose their own indexed subject/candidate traversal, not a new eligibility domain.
macro_rules! message_sent_recipient_match {
    () => {
        "json_extract(body, '$.fields.to') IN (?1, ?2)"
    };
}
macro_rules! message_desired_recipient_match {
    () => {
        "json_extract(child.value, '$.name')='to' AND json_extract(child.value, '$.arguments[0]') IN (?1, ?2)"
    };
}

// Store methods by domain (the crate plan's domain rules); core stays in this file.
pub mod attention;
pub mod delivery;
pub mod evidence;
pub mod github;
pub mod missions;
pub mod seats;
pub mod trace_principal;
#[cfg(test)]
pub mod accountable_tests;
pub mod custom;
pub mod assistant_write;
#[cfg(feature = "test-support")]
pub mod assistant_write_gate;
pub mod declarations;
pub mod retirements;
pub mod directive_notes;
pub mod observer_subscription_detail;
pub mod glass_heads;
pub mod arrangements;
#[cfg(test)]
pub mod arrangements_tests;
#[cfg(test)]
pub mod authored_pull_requests_tests;
#[cfg(test)]
pub mod schedule_active_run_tests;
pub mod glasses;
pub mod mailbox_wakes;
pub mod mailbox_changes;
pub mod mailbox_lease;
#[cfg(test)]
pub mod message_send_tests;
// Qualification only: no production caller or Source registration.
#[cfg(test)]
pub mod message_point_qualification;
#[cfg(test)]
pub mod message_thread_legacy_rank_dependency_tests;
#[cfg(test)]
pub mod actual_state_sorter_tests;
#[cfg(test)]
pub mod writer_bound_tests;
pub mod owned_sets;
#[cfg(test)]
pub mod owned_sets_tests;
#[cfg(test)]
pub mod usage_fold_tests;
pub mod resources;
pub mod resource_preparation;
pub mod observer_actuals;
#[cfg(test)]
pub mod resource_preparation_fence_tests;
pub mod github_workflow_failures;
pub mod git_changes;
pub mod message_subscriptions;
pub mod rollouts;
pub mod publication;
pub mod publication_facts;
pub mod seat_status;
pub mod stop_deferral;
pub mod card_fold;
#[cfg(test)]
pub mod card_fold_tests;
#[cfg(test)]
pub mod roster_controls;
#[cfg(test)]
pub mod roster_fresh_proof_tests;
#[cfg(test)]
pub mod agents_window_deadline_tests;
pub mod step_labels;
pub mod operation_audit;
pub mod mission_provenance;
pub mod agent_queue_values;
pub mod backup_format;
pub mod message_bodies;
pub mod projectors;
#[cfg(test)]
pub mod projectors_tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::fs;
use std::ops::Deref;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::model::{
    ApplyResponse, AttentionActionView, AttentionClosing, AttentionItemView, AttentionRequest,
    AttentionRequestView, AttentionResolveRequest, AttentionWithdrawRequest, Capability,
    ClaimInput, ClaimRecord, ClaimsPage, ClaimsSummary, ContextUsage, DependencySpec,
    DesiredSubject, EventRecord, FaultView, HumanReviewView, IntentInput, LoopRoundView,
    LoopRunView, MAX_EVAL_TIMEOUT_MS, SESSION_CLAIMS, MessageView, MissionDefinitionView,
    MissionInputKind, MissionOutputView, MissionResponse, MissionRevisionOperation,
    MissionRunDeclaration, MissionRunInput, MissionRunOutcomeView, MissionRunReportView,
    MissionRunRequest, MissionRunView, MissionSpec, MissionState, NormalizedIntent,
    OperationalAnnotation, OperationalRepairItem, OperationalRepairPlan, OperationalRepairResult,
    PlannedAction, PlannerSpec, PlanningCandidateView, PlanningPreviewView,
    PlanningSessionDeclaration, PlanningSessionView, PlanningVariantView, ReplicaBatch,
    ReplicaRepairDeclaration, ResourceObservationOutcome, ResourceRefreshOperation,
    RevisionCutover, RevisionProposalView, RevisionSubmissionView, RunGenerationView,
    RuntimeResetOperation, St3Error, StatusResponse, StepRunView, SubjectChange, SubjectStatus,
    SubscriptionConditionSpec, SubscriptionRequestDecision, SubscriptionRequestView,
    SubscriptionSpec, UsageSummary, WorkRequest, WorkSelector, WorkWakeView,
};
#[cfg(test)]
use crate::model::{ReplicaEnvelope, ReplicaEnvelopeId, ReplicationInventory, ReplicationReceipt};
#[cfg(any(test, feature = "test-support"))]
use crate::model::ReplicationExchange;
#[cfg(any(test, feature = "test-support"))]
use smallclaims::replication::{ReplicaRange, ReplicationBatch, ReplicationResponse};
use crate::model::{SeatQueueMoveRequest, SeatQueueMoveView, SeatQueueRunView, SeatQueueView};
use crate::seat_queue::{self, Placement, QueueJoin, QueueMove, SeatStep};
#[cfg(test)]
use smallclaims::claim::ReplicaEnvelopePayload;
use smallclaims::error::internal;
use smallclaims::hash::{
    batch_header_hash, canonical_hash, canonical_json_text, canonical_json_value,
    canonical_serialized_json_text, claim_hash,
};
#[cfg(any(test, feature = "test-support"))]
use smallclaims::hash::{claim_id_is_content_hash, verify_replica_batch_header};
#[cfg(test)]
use smallclaims::hash::{replica_envelope_hash, replica_record_ref};
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::Ordering;

// The graph half of the store: the claim log, envelopes, replication, checkpoints,
// documents and blobs. Everything it exports is in scope here as it was before the carve.
#[allow(unused_imports)]
use smallclaims::sqlite::*;
pub use smallclaims::store::Store as GraphStore;
#[allow(unused_imports)]
use smallclaims::store::runtime::LegacyDigestTable;
#[allow(unused_imports)]
use smallclaims::store::*;
pub use smallclaims::store::{
    CHECKPOINT_ATTENTION_AFTER_MS, CHECKPOINT_EXCUSED, CHECKPOINT_MANIFEST_PAGE_LIMIT,
    CHECKPOINT_PROTOCOL, CHECKPOINT_SEALED, CHECKPOINT_VERIFIED, Certificate, CheckpointAction,
    CheckpointClaim, CheckpointContext, CheckpointExcuseRequest, CheckpointManifest,
    CheckpointManifestCursor, CheckpointManifestNeed, CheckpointManifestPage,
    CheckpointManifestRequest, CheckpointPlanRequest, CheckpointPlanView, CheckpointProof,
    CheckpointResumeRequest, CheckpointStatusView, ClaimTombstone, CreatedFleetInvite, DropCount,
    DropPlan, EnvelopeKey, EnvelopeTombstone, FleetAdmissionResidue, FleetInviteView,
    FleetRedemption, FleetRemoval, PendingCheckpointView, ReplicationAdmission, SealTerms,
    SealedSet, TRIM_CHUNK_ENVELOPES, TrimFault, VerifiedTerms, certificates, checkpoint_build,
    checkpoint_cut, checkpoint_name, checkpoint_participants, chosen_certificate,
    configure_projection_writer, drop_digest, excused_writers, first_verifications, newest_due_cut,
    newest_seals, stable_checkpoints, valid_fleet_node_name, verify_checkpoint_manifest,
};


pub mod accounts;
pub mod runtime_starts;
pub use runtime_starts::{RuntimeStartAdmission, RuntimeStartOperation, RuntimeStartLock};
pub mod roster_admission;
pub use roster_admission::AdmissionGuard;
pub mod adhoc_work;
pub mod attention_snapshot;
pub use attention_snapshot::{
    CLAUDE_PROMPT_ANSWER_WAIT_MS, NativePromptState, PromptAnswer, native_prompt_gone_key,
    plan_approval, screen_prompt_delivered_key, screen_prompt_gone_key,
    screen_prompt_undelivered_key,
};
pub mod attention_list;
pub mod owner_lists;
pub mod summary_list;
// Registration stays opt-in until the shared installer certifies every source family.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "prepared attention operators await certified shared-runtime integration"
    )
)]
pub mod attention_ivm;
pub mod backup;
pub mod checkpoint_rules;
pub mod harness_questions;
pub mod checkpoint_status;
pub mod mission_eligibility;
pub use mission_eligibility::MISSING_AGENT_CONDITION;
pub mod revision_seats;
pub mod delegation;
pub mod limits;
pub mod database_size;
pub mod projection_divergence;
pub use database_size::DatabaseSize;
pub mod person_work;
pub use person_work::AskImages;
pub mod reconcile_work_context;
pub mod work_summaries_ivm;
pub mod subagents;
pub mod watches;
pub mod idle_holds;
pub mod idle_mail;
pub use idle_holds::WORK_NUDGED_KIND;
pub use checkpoint_rules::{RULES_VERSION, plan_drops, rules_digest};
pub use limits::{AccountLimit, LIMITS_ACTOR, LIMITS_CATCH_UP_PAGE, LimitsOutcome, LimitsPolicy};
pub use limits::ACCOUNT_READING_WINDOW_MS;
pub use subagents::{
    EndedSubagent, SUBAGENT_KINDS, SUBAGENT_LEASE_MS, SubagentSweep, SubagentView,
};
#[cfg(test)]
pub mod checkpoint_agreement_tests;
#[cfg(any(test, feature = "test-support"))]
pub mod terminal_gate_evidence;
#[cfg(test)]
pub mod checkpoint_tests;
#[cfg(test)]
pub mod checkpoint_capture_epoch_tests;
#[cfg(test)]
pub mod checkpoint_replication_capture_tests;
#[cfg(test)]
pub mod convergence;
#[cfg(test)]
pub mod document_index_tests;
pub mod lanes;
pub mod operations;
pub mod mission_ivm;
pub mod client_summary;
pub mod unread_mail;
pub mod agent_messages;
pub mod coordination;
pub mod agent_view;
pub mod conversation_reads;
pub mod usage_period;
pub mod runtime;
pub mod published_views;
pub mod published_list;
pub mod mission_list;
pub mod work_list;
#[cfg(test)]
pub mod tombstones_tests;
#[cfg(test)]
pub mod step_wait_tests;
#[cfg(test)]
pub mod work_renew_tests;
pub use runtime::SmalltalkRuntime;
#[cfg(any(test, feature = "test-support"))]
pub use smallclaims::sqlite::STATEMENTS_RUN;

/// The graph's checkpoints with smalltalk's checkpoint rules beside them.
pub mod checkpoint {
    pub use super::checkpoint_rules::*;
    pub use smallclaims::store::checkpoint::*;

    /// Prove a drop plan on a copy of the store with smalltalk's projections and readers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn prove_on_copy(
        copy: &std::path::Path,
        sealed: &SealedSet,
        plan: &DropPlan,
    ) -> anyhow::Result<CheckpointProof> {
        smallclaims::store::checkpoint::prove_on_copy(
            &super::SmalltalkRuntime::default(),
            copy,
            sealed,
            plan,
        )
    }
}

/// The graph's projection digests over smalltalk's shared tables.
pub mod projection_digest {
    pub use smallclaims::store::projection_digest::*;

    /// The shared tables the projection digest covers.
    #[cfg(any(test, feature = "test-support"))]
    pub const TABLES: &[(&str, &[&str])] = super::PROJECTION_DIGEST_TABLES;

    /// Every shared table's digest, computed from the rows themselves.
    #[cfg(any(test, feature = "test-support"))]
    pub fn oracle(
        connection: &rusqlite::Connection,
    ) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
        smallclaims::store::projection_digest::oracle(connection, super::PROJECTION_DIGEST_TABLES)
    }
}

type CumulativeUsage = (u64, u64, u64, u64, Option<f64>, Option<String>);

/// One incarnation's spend as its usage claims fold, in canonical claim order.
#[derive(Clone, Default)]
struct UsageSpend {
    cumulative: Option<CumulativeUsage>,
    rollups: BTreeMap<String, (u64, u64, u64, u64, u64)>,
    response_total: u64,
    response_input: u64,
    response_output: u64,
    response_cached: u64,
    response_cost: f64,
    response_has_cost: bool,
    response_currency: Option<String>,
}

/// An agent's usage summary as a fold over its `harness.usage` claims in canonical order, kept
/// between reads so a later read folds only the claims that sort after it.
#[derive(Clone, Default)]
pub struct UsageFold {
    spend: BTreeMap<String, UsageSpend>,
    context: Option<(u64, ContextUsage)>,
    saw: bool,
}

/// An agent's usage fold over every usage claim at or before `through`, with the accepted time
/// of the last one it folded.
pub struct CachedUsageFold {
    through: u64,
    last_accepted: String,
    fold: UsageFold,
}

/// How much usage fold state a store keeps, counted in incarnations and rollup series. Past it
/// the kept folds start over; a single fold heavier than a sixteenth of it is never kept.
const USAGE_FOLD_WEIGHT: usize = 65_536;

/// The usage folds a store keeps, by agent, and their total weight.
#[derive(Default)]
pub struct UsageFolds {
    folds: HashMap<String, Arc<CachedUsageFold>>,
    weight: usize,
}

impl UsageFolds {
    fn get(&self, subject: &str) -> Option<&Arc<CachedUsageFold>> {
        self.folds.get(subject)
    }

    /// Keep `cached` unless a newer fold is kept already or it is too heavy to keep.
    fn keep(&mut self, subject: String, cached: CachedUsageFold) {
        let weight = cached.fold.weight();
        if weight > USAGE_FOLD_WEIGHT / 16
            || self.folds.get(&subject).is_some_and(|kept| kept.through > cached.through)
        {
            return;
        }
        if let Some(previous) = self.folds.remove(&subject) {
            self.weight -= previous.fold.weight();
        }
        if self.weight + weight > USAGE_FOLD_WEIGHT {
            *self = Self::default();
        }
        self.weight += weight;
        self.folds.insert(subject, Arc::new(cached));
    }
}

impl UsageFold {
    /// Fold the next usage claim in canonical order.
    fn apply(&mut self, store_index: u64, body: &str, accepted_at: &str, incarnation: Option<&str>) -> Result<()> {
        let body: Value = serde_json::from_str(body)?;
        let fields = body.get("fields").unwrap_or(&body);
        let claim_incarnation = fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if incarnation.is_some_and(|expected| expected != claim_incarnation) {
            return Ok(());
        }
        self.saw = true;
        match fields.get("semantics").and_then(Value::as_str) {
            Some("context_occupancy") => {
                self.context = Some((
                    store_index,
                    ContextUsage {
                        used_tokens: fields.get("context_used_tokens").and_then(Value::as_u64),
                        window_tokens: fields
                            .get("context_window_tokens")
                            .and_then(Value::as_u64),
                        used_percent: fields
                            .get("context_used_percent")
                            .and_then(Value::as_f64),
                        model: fields
                            .get("model")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        compactions: fields
                            .get("compactions")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        last_compaction_ms: fields
                            .get("last_compaction_ms")
                            .and_then(Value::as_u64),
                        last_compaction_trigger: fields
                            .get("last_compaction_trigger")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        observed_at_unix_ms: accepted_at.parse().unwrap_or_default(),
                    },
                ));
            }
            Some("session_cumulative") => {
                let Some(total) = fields.get("total_tokens").and_then(Value::as_u64) else {
                    return Ok(());
                };
                let candidate = (
                    total,
                    fields
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    fields
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    fields
                        .get("cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    fields.get("cost").and_then(Value::as_f64),
                    fields
                        .get("currency")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
                let group = self.spend.entry(claim_incarnation.to_owned()).or_default();
                if group
                    .cumulative
                    .as_ref()
                    .is_none_or(|current| candidate.0 >= current.0)
                {
                    group.cumulative = Some(candidate);
                }
            }
            Some("response_rollup") => {
                let group = self.spend.entry(claim_incarnation.to_owned()).or_default();
                let key = ["model", "account", "owner_run", "owner_step", "host"]
                    .iter()
                    .map(|key| fields.get(*key).and_then(Value::as_str).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\0");
                group.rollups.insert(
                    key,
                    (
                        fields
                            .get("total_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        fields
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        fields
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        fields
                            .get("cache_write_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        fields
                            .get("cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    ),
                );
            }
            Some("response") => {
                let Some(total) = fields.get("total_tokens").and_then(Value::as_u64) else {
                    return Ok(());
                };
                let group = self.spend.entry(claim_incarnation.to_owned()).or_default();
                group.response_total = group.response_total.saturating_add(total);
                group.response_input = group.response_input.saturating_add(
                    fields
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                group.response_output = group.response_output.saturating_add(
                    fields
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                group.response_cached = group.response_cached.saturating_add(
                    fields
                        .get("cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                if let Some(cost) = fields.get("cost").and_then(Value::as_f64) {
                    group.response_cost += cost;
                    group.response_has_cost = true;
                }
                if let Some(currency) = fields.get("currency").and_then(Value::as_str) {
                    group.response_currency = Some(currency.to_owned());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The state this fold keeps: one per incarnation and one per rollup series.
    fn weight(&self) -> usize {
        1 + self.spend.values().map(|group| 1 + group.rollups.len()).sum::<usize>()
    }

    fn summary(&self) -> Option<UsageSummary> {
        if !self.saw {
            return None;
        }
        let mut summary = UsageSummary {
            aggregation: "cumulative-per-incarnation-else-response-deltas".into(),
            context: self.context.clone().map(|(_, value)| value),
            ..UsageSummary::default()
        };
        let mut cost = 0.0;
        let mut has_cost = false;
        for group in self.spend.values().cloned() {
            summary.incarnation_count += 1;
            if !group.rollups.is_empty() {
                for (total, input, output, writes, reads) in group.rollups.into_values() {
                    summary.total_tokens = summary.total_tokens.saturating_add(total);
                    summary.input_tokens = summary.input_tokens.saturating_add(input);
                    summary.output_tokens = summary.output_tokens.saturating_add(output);
                    summary.cache_write_tokens = summary.cache_write_tokens.saturating_add(writes);
                    summary.cached_tokens = summary.cached_tokens.saturating_add(reads);
                }
            } else if let Some((total, input, output, cached, group_cost, currency)) =
                group.cumulative
            {
                summary.total_tokens = summary.total_tokens.saturating_add(total);
                summary.input_tokens = summary.input_tokens.saturating_add(input);
                summary.output_tokens = summary.output_tokens.saturating_add(output);
                summary.cached_tokens = summary.cached_tokens.saturating_add(cached);
                if let Some(value) = group_cost {
                    cost += value;
                    has_cost = true;
                }
                summary.currency = summary.currency.or(currency);
            } else {
                summary.total_tokens = summary.total_tokens.saturating_add(group.response_total);
                summary.input_tokens = summary.input_tokens.saturating_add(group.response_input);
                summary.output_tokens = summary.output_tokens.saturating_add(group.response_output);
                summary.cached_tokens = summary.cached_tokens.saturating_add(group.response_cached);
                if group.response_has_cost {
                    cost += group.response_cost;
                    has_cost = true;
                }
                summary.currency = summary.currency.or(group.response_currency);
            }
        }
        summary.cost = has_cost.then_some(cost);
        Some(summary)
    }
}


#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentWorkQueue {
    pub current_work_ids: Vec<String>,
    pub active_work_count: u64,
    pub next_work_id: Option<String>,
    pub upcoming_work_ids: Vec<String>,
    pub queued_work_count: u64,
}

/// The longest one `work wait` stops the execution clock. A wait past it must be declared again,
/// so a seat that stopped answering cannot hide behind a wait.
pub const MAX_STEP_WAIT_MS: u64 = 12 * 60 * 60 * 1000;

/// A wait span as hours and minutes, for the budget error and the step view.
pub fn format_wait_span(milliseconds: u128) -> String {
    let minutes = milliseconds / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes:02}m"),
    }
}

/// smalltalk's projection tables, and the indexes its folds read the claim log through. The
/// graph creates its own tables first; see `smallclaims::store::SCHEMA`.
const SCHEMA: &str = r#"
-- An unmanaged member's staging check needs tagged claims, not its runtime history.
CREATE INDEX IF NOT EXISTS claims_owned_set_subject_index ON claims(subject)
WHERE json_extract(body,'$.owned_set') IS NOT NULL;
CREATE INDEX IF NOT EXISTS claims_terminal_history_index ON claims(store_index)
WHERE kind IN ('mission-run.state','step-run.state','work.failed')
  AND json_extract(body,'$.fields.status') IN ('failed','cancelled','completed');
CREATE INDEX IF NOT EXISTS claims_terminal_capability_hash_index
ON claims(json_extract(body, '$.fields.capability_hash'), store_index)
WHERE kind='custom.client.terminal-attached';
CREATE INDEX IF NOT EXISTS claims_message_to_index
ON claims(json_extract(body, '$.fields.to'), subject)
WHERE kind='message.sent';
CREATE INDEX IF NOT EXISTS claims_run_report_to_index
ON claims(json_extract(body, '$.fields.report_to'), subject)
WHERE kind IN ('mission-run.created','mission-run.report-to');
CREATE INDEX IF NOT EXISTS claims_message_from_index
ON claims(json_extract(body, '$.fields.from'), store_index)
WHERE kind='message.sent';
-- Legacy sends can put endpoint fields at the body root. They must remain readable without
-- making every conversation parse the entire fleet's message payloads to find them.
CREATE INDEX IF NOT EXISTS claims_message_legacy_to_index
ON claims(json_extract(body, '$.to'), store_index)
WHERE kind='message.sent' AND json_type(body, '$.fields') IS NULL;
CREATE INDEX IF NOT EXISTS claims_message_legacy_from_index
ON claims(json_extract(body, '$.from'), store_index)
WHERE kind='message.sent' AND json_type(body, '$.fields') IS NULL;
CREATE INDEX IF NOT EXISTS claims_resume_host_index
ON claims(json_extract(body, '$.fields.host'), subject)
WHERE kind='runtime.action.requested' AND json_extract(body,'$.fields.action')='resume';
-- Last activity is the greatest numeric acceptance time, independent of local arrival.
CREATE INDEX IF NOT EXISTS claims_actor_activity_time_index
ON claims(actor, length(accepted_at_unix_ms), accepted_at_unix_ms, store_index)
WHERE kind IN ('work.progress', 'work.submitted');
-- The acceptance-time index replaces the only reader of the arrival-ordered one.
DROP INDEX IF EXISTS claims_actor_progress_index;
CREATE INDEX IF NOT EXISTS claims_message_from_activity_time_index
ON claims(json_extract(body, '$.fields.from'), length(accepted_at_unix_ms), accepted_at_unix_ms, store_index)
WHERE kind='message.sent';
CREATE INDEX IF NOT EXISTS claims_message_to_activity_time_index
ON claims(json_extract(body, '$.fields.to'), length(accepted_at_unix_ms), accepted_at_unix_ms, store_index)
WHERE kind='message.sent';
CREATE INDEX IF NOT EXISTS claims_timeline_activity_time_index
ON claims(subject, json_extract(body, '$.fields.incarnation_id'), length(accepted_at_unix_ms), accepted_at_unix_ms, store_index)
WHERE kind='harness.timeline'
  AND json_extract(body, '$.fields.entry_type') IN ('message','content','tool_call','tool_result');
CREATE INDEX IF NOT EXISTS claims_actor_work_activity_index
ON claims(actor, json_extract(body, '$.fields.claim_incarnation'), store_index)
WHERE kind IN ('work.claimed', 'work.progress');
CREATE INDEX IF NOT EXISTS claims_subagent_appeared_index
ON claims(subject, json_extract(body, '$.fields.subagent_id'))
WHERE kind='subagent.appeared';
-- Include the numeric lease so MAX for one live pair seeks its final index entry instead
-- of folding all its renewals. A new name upgrades existing stores at schema initialization.
CREATE INDEX IF NOT EXISTS claims_subagent_renewed_max_lease_index
ON claims(subject, json_extract(body, '$.fields.subagent_id'),
          CAST(json_extract(body, '$.fields.lease_expires_at_unix_ms') AS INTEGER))
WHERE kind='subagent.renewed';
DROP INDEX IF EXISTS claims_subagent_renewed_index;
CREATE INDEX IF NOT EXISTS claims_subagent_ended_index
ON claims(subject, json_extract(body, '$.fields.subagent_id'))
WHERE kind='subagent.ended';
CREATE INDEX IF NOT EXISTS claims_subagent_appeared_lease_index
ON claims(CAST(json_extract(body, '$.fields.lease_expires_at_unix_ms') AS INTEGER))
WHERE kind='subagent.appeared';
CREATE INDEX IF NOT EXISTS claims_subagent_renewed_lease_index
ON claims(CAST(json_extract(body, '$.fields.lease_expires_at_unix_ms') AS INTEGER))
WHERE kind='subagent.renewed';
CREATE INDEX IF NOT EXISTS claims_timeline_incarnation_index
ON claims(subject, kind, json_extract(body, '$.fields.incarnation_id'), store_index)
WHERE kind='harness.timeline';
CREATE INDEX IF NOT EXISTS claims_subscription_finished_request_index
ON claims(subject, json_extract(body, '$.fields.request'))
WHERE kind IN (
    'subscription.mission-started',
    'subscription.mission-failed',
    'subscription.mission-request-cancelled'
);
CREATE INDEX IF NOT EXISTS claims_subscription_delivery_key_index
ON claims(json_extract(body, '$.fields.delivery_key'))
WHERE kind='subscription.mission-requested';
CREATE INDEX IF NOT EXISTS claims_subscription_started_run_index
ON claims(json_extract(body, '$.fields.mission_run'))
WHERE kind='subscription.mission-started';
CREATE INDEX IF NOT EXISTS claims_subscription_mission_resource_index
ON claims(json_extract(body, '$.fields.mission'), json_extract(body, '$.fields.resource'))
WHERE kind='subscription.mission-requested';
-- Authored PR ownership is looked up by URL or repository/number. Keep ordinary observed
-- resources out of both indexes; the lookup unions two seeks before canonical ordering.
CREATE INDEX IF NOT EXISTS claims_authored_pull_request_url_index
ON claims(json_extract(body, '$.fields.facts.url'))
WHERE kind='resource.observed' AND subject LIKE 'resource/mission-run/%/pull-request';
CREATE INDEX IF NOT EXISTS claims_authored_pull_request_number_index
ON claims(json_extract(body, '$.fields.facts.repository'), json_extract(body, '$.fields.facts.number'))
WHERE kind='resource.observed' AND subject LIKE 'resource/mission-run/%/pull-request';
CREATE INDEX IF NOT EXISTS claims_subscription_deferred_request_index
ON claims(subject, json_extract(body, '$.fields.request'), store_index)
WHERE kind='subscription.mission-deferred';
CREATE INDEX IF NOT EXISTS claims_subscription_deferred_deadline_index
ON claims(CAST(json_extract(body, '$.fields.not_before_unix_ms') AS INTEGER))
WHERE kind='subscription.mission-deferred';
CREATE INDEX IF NOT EXISTS claims_schedule_started_request_index
ON claims(subject, json_extract(body, '$.fields.request'))
WHERE kind='schedule.work-started';
-- The newest message to a recipient, found by one seek instead of a scan of every message.
CREATE INDEX IF NOT EXISTS claims_message_to_order_index
ON claims(json_extract(body, '$.fields.to'), store_index)
WHERE kind='message.sent';
-- Claims that can place a runtime subject in a current view: a declaration, or a live status.
-- The predicate is CURRENT_VIEW_CLAIM, which queries repeat so the planner uses this index.
CREATE INDEX IF NOT EXISTS claims_current_view_index
ON claims(subject, store_index)
WHERE kind='intent.desired'
   OR json_extract(body, '$.fields.status') NOT IN ('stopped', 'absent', 'exited')
   OR json_extract(body, '$.status') NOT IN ('stopped', 'absent', 'exited');
-- The gate requests a person reviews, a small part of every gate request a fleet makes.
CREATE INDEX IF NOT EXISTS claims_human_gate_request_index
ON claims(json_extract(body, '$.fields.reviewer'), store_index)
WHERE kind='gate.requested' AND json_extract(body, '$.fields.reviewer') IS NOT NULL;
-- The subjects that ever observed a runtime, found one seek per subject (RUNTIME_SUBJECTS)
-- instead of reading every runtime observation. It holds runtime observations alone, so queries
-- of other kinds plan as they did.
CREATE INDEX IF NOT EXISTS claims_runtime_subject_index ON claims(subject, store_index)
WHERE kind='runtime.observed';
-- An already-open raw stream revalidates the original paired actor, never a full graph scan.
CREATE INDEX IF NOT EXISTS claims_client_pairing_actor_index
ON claims(json_extract(body, '$.fields.session_actor'))
WHERE kind='custom.client.pairing-completed';
CREATE INDEX IF NOT EXISTS claims_assistant_v1_credential_index
ON claims(json_extract(body, '$.fields.credential_hash'), store_index)
WHERE kind='custom.client.assistant.v1.granted';
-- A runtime incarnation's claims about a subject by acceptance time, so when a session began
-- and last changed are two seeks instead of a read of every claim the subject has. The
-- expression is INCARNATION_OF_CLAIM, which queries repeat so the planner uses this index.
CREATE INDEX IF NOT EXISTS claims_incarnation_accepted_index
ON claims(
    subject,
    json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END),
    length(accepted_at_unix_ms),
    accepted_at_unix_ms
)
WHERE json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
    THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END) IS NOT NULL;

-- Harness folds must not walk the same incarnation's accumulated legacy timeline rows.
CREATE INDEX IF NOT EXISTS claims_harness_incarnation_accepted_index
ON claims(
    subject,
    json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END),
    length(accepted_at_unix_ms),
    accepted_at_unix_ms
)
WHERE kind='harness.observed'
    AND json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END) IS NOT NULL;

-- Native decision releases stay attached to the exact driver episode across
-- arbitrarily many heartbeats; looking one up must not scan the seat's history.
CREATE INDEX IF NOT EXISTS claims_harness_episode_index
ON claims(subject, json_extract(body, '$.fields.ownership_sequence'),
    json_extract(body, '$.fields.transition_sequence'),
    length(accepted_at_unix_ms), accepted_at_unix_ms)
WHERE kind='harness.observed';

-- A legacy observation without a text incarnation remains eligible for the current
-- runtime's optional-field fold. Keep those rare rows separate: checking their JSON
-- predicate against every named heartbeat made attached agent windows scan whole epochs.
CREATE INDEX IF NOT EXISTS claims_harness_unnamed_accepted_index
ON claims(subject, length(accepted_at_unix_ms), accepted_at_unix_ms)
WHERE kind='harness.observed'
    AND (json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END) IS NULL
        OR typeof(json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END))!='text');

-- Attachment checks must not walk a quiet seat's accumulated hook and work history.
-- Only phase transitions publish these diagnostics, so a current-runtime lookup stays small.
CREATE INDEX IF NOT EXISTS claims_claude_attachment_index
ON claims(
    subject,
    json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END),
    store_index
)
WHERE kind='harness.diagnostic'
    AND json_extract(body, '$.fields.code') IN ('claude-channel-unattached','claude-channel-attached');

-- Mailbox admission needs the newest state for one incarnation, never optional display fields
-- from its entire history. Include legacy reports without an incarnation in a separate seek.
CREATE INDEX IF NOT EXISTS claims_harness_state_incarnation_accepted_index
ON claims(
    subject,
    CASE WHEN json_type(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END)='text'
        THEN json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END) END,
    length(accepted_at_unix_ms), accepted_at_unix_ms
)
WHERE kind='harness.observed'
    AND json_type(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.state' ELSE '$.fields.state' END)='text';

-- Older drivers have no credential axis. A partial index prevents an unknown
-- credential state from scanning every activity observation in a long-lived incarnation.
CREATE INDEX IF NOT EXISTS claims_harness_auth_incarnation_index
ON claims(subject, json_extract(body, '$.fields.incarnation_id'),
    length(accepted_at_unix_ms), accepted_at_unix_ms)
WHERE kind='harness.observed'
    AND json_type(body, '$.fields.provider_auth') IN ('true','false');

CREATE INDEX IF NOT EXISTS claims_harness_auth_diagnostic_index
ON claims(subject, json_extract(body, '$.fields.incarnation_id'),
    length(accepted_at_unix_ms), accepted_at_unix_ms)
WHERE kind='harness.diagnostic'
    AND json_extract(body, '$.fields.code') IN ('provider-auth-expired','provider-auth-restored');

-- Attention needs full runtime/owner checks only for seats with positive login evidence.
-- Keep legacy flat observations in this index as well as native credential reports.
CREATE INDEX IF NOT EXISTS claims_harness_login_candidate_index ON claims(subject)
WHERE (kind='harness.observed' AND (
    json_type(body, '$.fields.provider_auth')='false'
    OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
    OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
    OR (kind='harness.diagnostic'
        AND json_extract(body, '$.fields.code')='provider-auth-expired');

CREATE TABLE IF NOT EXISTS desired (
    subject TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    revision TEXT NOT NULL,
    claim_id TEXT NOT NULL REFERENCES claims(id),
    body TEXT NOT NULL,
    member TEXT,
    owner_run TEXT,
    owner_generation TEXT,
    owner_step TEXT
);
CREATE INDEX IF NOT EXISTS desired_owner_step_index ON desired(owner_step, subject);
CREATE INDEX IF NOT EXISTS desired_owner_run_index ON desired(owner_run, subject);
-- Deleting a claim checks these references (foreign keys are on); see
-- `operations_canonical_claim_index`.
CREATE INDEX IF NOT EXISTS desired_claim_index ON desired(claim_id);
CREATE INDEX IF NOT EXISTS desired_agent_subject_index ON desired(subject) WHERE kind='agent';
CREATE INDEX IF NOT EXISTS desired_agent_host_index ON desired(json_extract(member, '$.host'), subject) WHERE kind='agent';
-- Filtered mail candidate discovery must not walk unrelated desired resources.
CREATE INDEX IF NOT EXISTS desired_message_subject_index ON desired(subject) WHERE kind='message';

-- Watch subjects encode OWNER/REPO/NUMBER followed by the recipient (which may contain slashes).
-- Seek that suffix so an idle seat never scans the fleet's declarations.
CREATE INDEX IF NOT EXISTS desired_watch_agent_index ON desired(substr(substr(substr(substr(subject, 20), instr(substr(subject, 20), '/') + 1), instr(substr(substr(subject, 20), instr(substr(subject, 20), '/') + 1), '/') + 1), instr(substr(substr(substr(subject, 20), instr(substr(subject, 20), '/') + 1), instr(substr(substr(subject, 20), instr(substr(subject, 20), '/') + 1), '/') + 1), '/') + 1))
WHERE subject >= 'subscription/watch/' AND subject < 'subscription/watch0';

-- A replicated projection finds a mission run tree's runs, generations and proposals from the
-- claims that create them, without reading every such claim.
CREATE INDEX IF NOT EXISTS claims_run_root_index ON claims(json_extract(body, '$.fields.root_mission_run'))
WHERE kind='mission-run.created';
CREATE INDEX IF NOT EXISTS claims_run_generation_index ON claims(json_extract(body, '$.fields.current_generation'))
WHERE kind='mission-run.created';
CREATE INDEX IF NOT EXISTS claims_generation_run_index ON claims(json_extract(body, '$.fields.run'))
WHERE kind='run-generation.created';
CREATE INDEX IF NOT EXISTS claims_proposal_run_index ON claims(json_extract(body, '$.fields.run'))
WHERE kind='revision-proposal.created';
CREATE INDEX IF NOT EXISTS claims_run_revision_index ON claims(json_extract(body, '$.fields.revision'))
WHERE kind='mission-run.created';
CREATE INDEX IF NOT EXISTS claims_generation_revision_index ON claims(json_extract(body, '$.fields.revision'))
WHERE kind='run-generation.created';
CREATE INDEX IF NOT EXISTS claims_proposal_revision_index ON claims(json_extract(body, '$.fields.candidate_revision'))
WHERE kind='revision-proposal.created';
-- A standalone person ask creates its run and generation in the asking claim itself.
CREATE INDEX IF NOT EXISTS claims_person_ask_generation_index ON claims(json_extract(body, '$.fields.generation'))
WHERE kind='work.person-asked';
CREATE INDEX IF NOT EXISTS claims_person_ask_run_index ON claims(json_extract(body, '$.fields.run'))
WHERE kind='work.person-asked';
CREATE INDEX IF NOT EXISTS claims_person_ask_owner_index ON claims(json_extract(body, '$.fields.owner_run'))
WHERE kind='work.person-asked';
CREATE INDEX IF NOT EXISTS claims_person_ask_origin_step_index ON claims(json_extract(body, '$.fields.origin_step'))
WHERE kind='work.person-asked';

CREATE TABLE IF NOT EXISTS mission_run_requests (
    operation_id TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS mission_run_request_expiry AFTER DELETE ON idempotency
BEGIN
    DELETE FROM mission_run_requests WHERE operation_id=OLD.operation_id;
END;

-- Every message this node holds claims for: its first claim's store index, and whether a claim
-- closed it. The claim log alone decides it, through the triggers below, so listing the open
-- messages reads the open messages rather than every lifecycle claim of every message.
CREATE TABLE IF NOT EXISTS message_index (
    subject TEXT PRIMARY KEY,
    created_index INTEGER NOT NULL,
    closed INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS message_index_open ON message_index(closed, created_index);
CREATE TRIGGER IF NOT EXISTS message_index_claim_insert AFTER INSERT ON claims
WHEN NEW.subject LIKE 'message/%'
BEGIN
    INSERT INTO message_index(subject, created_index, closed)
    VALUES (NEW.subject, NEW.store_index, NEW.kind = 'message.closed')
    ON CONFLICT(subject) DO UPDATE SET
        created_index = MIN(created_index, excluded.created_index),
        closed = MAX(closed, excluded.closed);
END;
CREATE TRIGGER IF NOT EXISTS message_index_claim_delete AFTER DELETE ON claims
WHEN OLD.subject LIKE 'message/%'
BEGIN
    DELETE FROM message_index WHERE subject = OLD.subject;
    INSERT INTO message_index(subject, created_index, closed)
    SELECT subject, MIN(store_index), MAX(kind = 'message.closed')
    FROM claims WHERE subject = OLD.subject GROUP BY subject;
END;

CREATE TABLE IF NOT EXISTS capabilities (
    secret_hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    incarnation_id TEXT,
    expires_at_unix_ms TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS mission_revisions (
    mission_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL,
    body TEXT NOT NULL,
    claim_id TEXT NOT NULL REFERENCES claims(id),
    created_index INTEGER NOT NULL,
    PRIMARY KEY(mission_id, revision)
);
CREATE INDEX IF NOT EXISTS mission_revisions_claim_index ON mission_revisions(claim_id);

CREATE TABLE IF NOT EXISTS mission_definitions (
    mission_id TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    state TEXT NOT NULL,
    claim_id TEXT NOT NULL REFERENCES claims(id)
);
CREATE INDEX IF NOT EXISTS mission_definitions_claim_index ON mission_definitions(claim_id);

CREATE TABLE IF NOT EXISTS mission_runs (
    id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    initial_revision TEXT NOT NULL,
    current_generation_id TEXT NOT NULL,
    root_revision TEXT NOT NULL,
    root_run_id TEXT NOT NULL,
    parent_step_run TEXT,
    workspace TEXT NOT NULL,
    requester TEXT NOT NULL,
    inputs TEXT NOT NULL,
    mode TEXT NOT NULL,
    status TEXT NOT NULL,
    phase TEXT NOT NULL,
    created_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mission_runs_mission_index ON mission_runs(mission_id, created_at_unix_ms);
CREATE INDEX IF NOT EXISTS mission_runs_root_page_index
ON mission_runs(root_run_id, created_at_unix_ms, id);
-- The runs that have not finished, a few of every run a fleet has made. The predicate is
-- OPEN_MISSION_RUN, which queries repeat so the planner uses this index.
CREATE INDEX IF NOT EXISTS mission_runs_open_index ON mission_runs(created_at_unix_ms, id)
WHERE status NOT IN ('completed','failed','cancelled');

CREATE TABLE IF NOT EXISTS mission_run_deadlines (
    run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
    timeout_ms INTEGER NOT NULL,
    deadline_at_unix_ms TEXT
);

CREATE TABLE IF NOT EXISTS mission_run_after (
    run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
    after_run TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS run_generations (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES mission_runs(id),
    revision TEXT NOT NULL,
    predecessor_id TEXT,
    status TEXT NOT NULL,
    actor TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL,
    UNIQUE(run_id, id)
);
CREATE INDEX IF NOT EXISTS run_generations_run_index ON run_generations(run_id, created_at_unix_ms);

CREATE TABLE IF NOT EXISTS step_runs (
    subject TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES mission_runs(id),
    generation_id TEXT NOT NULL REFERENCES run_generations(id),
    step_path TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    assignee TEXT,
    available_to TEXT NOT NULL DEFAULT '[]',
    agentless INTEGER NOT NULL DEFAULT 1,
    title TEXT,
    goals TEXT NOT NULL,
    worker_reported INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_incarnation TEXT,
    lease_expires_at_unix_ms TEXT,
    blocked_reason TEXT,
    not_before_unix_ms TEXT,
    activated_at_unix_ms TEXT,
    readiness_epoch INTEGER NOT NULL DEFAULT 0,
    created_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL,
    constraints TEXT NOT NULL DEFAULT '[]',
    UNIQUE(generation_id, step_path)
);
CREATE INDEX IF NOT EXISTS step_runs_run_index ON step_runs(run_id, generation_id, step_path);
-- The recursive descendant-runs query joins runs to the steps that started them. Without these two
-- it scanned every mission run, then built a throwaway index on parent_step_run for each level:
-- 3.6 to 4.7 ms a call, 1,213 calls in one run-tree rebuild on a real store.
CREATE INDEX IF NOT EXISTS step_runs_generation_index ON step_runs(generation_id);
CREATE INDEX IF NOT EXISTS mission_runs_parent_step_index ON mission_runs(parent_step_run)
WHERE parent_step_run IS NOT NULL;
CREATE INDEX IF NOT EXISTS step_runs_assignee_index ON step_runs(assignee, status);
-- The steps a lease holds, a handful of all the steps a fleet has ever run.
CREATE INDEX IF NOT EXISTS step_runs_lease_index ON step_runs(lease_owner)
WHERE lease_owner IS NOT NULL;
-- The few steps offered to several seats, so a seat's steps are found without reading every step.
CREATE INDEX IF NOT EXISTS step_runs_available_index ON step_runs(run_id)
WHERE available_to!='[]';
-- The steps that have not finished, a few of every step a fleet has run.
CREATE INDEX IF NOT EXISTS step_runs_open_index ON step_runs(created_at_unix_ms, step_path)
WHERE status NOT IN ('completed','failed','cancelled');
-- This daemon's native subscription ownership is transport state, outside shared projections.
CREATE TABLE IF NOT EXISTS local_mailbox_owners (
    subject TEXT NOT NULL,
    component TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    PRIMARY KEY(subject, component)
);
CREATE TABLE IF NOT EXISTS local_mailbox_bindings (
    token TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    component TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    epoch INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS local_mailbox_argv_bindings (
    token TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    component TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    member TEXT NOT NULL
);
-- Authenticated process/session custody, independent of the connection binding.
CREATE TABLE IF NOT EXISTS local_mailbox_leases (
    subject TEXT NOT NULL,
    component TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    pid INTEGER NOT NULL,
    process_token TEXT NOT NULL,
    token TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(subject, component)
);
CREATE TABLE IF NOT EXISTS local_work_lease_renewals (
    subject TEXT PRIMARY KEY,
    attempt INTEGER NOT NULL,
    lease_owner TEXT NOT NULL,
    lease_incarnation TEXT NOT NULL,
    lease_expires_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL
);
-- Observations of `local` retention. Rows never enter a batch, an envelope or a digest and
-- never replicate. `after_store_index` is the newest claim when the row was written, so a
-- row sorts after that claim and before the next one.
CREATE TABLE IF NOT EXISTS local_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    after_store_index INTEGER NOT NULL,
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT,
    body TEXT NOT NULL,
    dedupe_key TEXT,
    request_digest TEXT,
    observed_at_unix_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS local_observations_subject_kind_index
ON local_observations(subject, kind, id);
-- "Anything newer for this subject": a mailbox stream asks it on every graph change, and the
-- subject-and-kind index made it read every observation the seat ever had.
CREATE INDEX IF NOT EXISTS local_observations_subject_id_index
ON local_observations(subject, id);
CREATE INDEX IF NOT EXISTS local_observations_after_store_index
ON local_observations(after_store_index);
CREATE INDEX IF NOT EXISTS local_observations_time_index
ON local_observations(observed_at_unix_ms);
CREATE UNIQUE INDEX IF NOT EXISTS local_observations_dedupe_index
ON local_observations(dedupe_key) WHERE dedupe_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_observations_timeline_index
ON local_observations(subject, json_extract(body, '$.fields.incarnation_id'), id)
WHERE kind='harness.timeline';
-- The roster cache's local frontier: the newest agent timeline row a graph cut can see. Only
-- an agent's timeline rows can change a roster card, so this partial covering index bounds the
-- frontier lookup by newer agent timeline rows alone; heartbeats, telemetry and every other
-- subject's observations never enter it, and the scan reads no table row.
CREATE INDEX IF NOT EXISTS local_observations_roster_frontier_index
ON local_observations(id, after_store_index)
WHERE kind='harness.timeline' AND subject LIKE 'agent/%';
-- A mission capacity retry is scheduling state owned by this reconciler. Replicating every
-- backoff attempt makes every peer reconcile even though only this node can retry it.
CREATE TABLE IF NOT EXISTS local_subscription_mission_deferrals (
    subject TEXT NOT NULL,
    request TEXT NOT NULL,
    not_before_unix_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    PRIMARY KEY(subject, request)
);
CREATE INDEX IF NOT EXISTS local_subscription_mission_deferrals_deadline_index
ON local_subscription_mission_deferrals(not_before_unix_ms);
-- Response spend accumulated per agent incarnation, model, paying account, owning mission run and
-- step, and host. Each key replicates as a cumulative `harness.usage` rollup. The account joined
-- the key after the first release, which kept these totals in `local_usage_totals`; that table's
-- keys already replicated, so its totals end where these begin and nothing counts twice.
DROP TABLE IF EXISTS local_usage_totals;
CREATE TABLE IF NOT EXISTS local_usage_spend (
    subject TEXT NOT NULL,
    incarnation_id TEXT NOT NULL,
    model TEXT NOT NULL,
    account TEXT NOT NULL,
    owner_run TEXT NOT NULL,
    owner_step TEXT NOT NULL,
    host TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    cost_microusd INTEGER NOT NULL DEFAULT 0,
    reported_cost_microusd INTEGER NOT NULL DEFAULT 0,
    unpriced_tokens INTEGER NOT NULL DEFAULT 0,
    observed_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(subject, incarnation_id, model, account, owner_run, owner_step, host)
);
-- Provenance shares the existing cumulative spend slot; individual responses remain local.
CREATE TABLE IF NOT EXISTS local_usage_provenance (
    subject TEXT NOT NULL,
    slot TEXT NOT NULL,
    pricing TEXT NOT NULL,
    PRIMARY KEY(subject, slot)
);
-- The seats this node's limits policy stopped, once per account and weekly window, so a seat a
-- person starts again stays up until the window resets.
CREATE TABLE IF NOT EXISTS local_limit_stops (
    account TEXT NOT NULL,
    episode TEXT NOT NULL,
    seat TEXT NOT NULL,
    stopped_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(account, episode, seat)
);
-- The account each pooled seat this node starts runs on, chosen when the seat starts and kept
-- until a restart moves it to another account in its pool. Local: only the node that runs a seat
-- decides which of its owner's logins that seat uses.
CREATE TABLE IF NOT EXISTS local_seat_accounts (
    seat TEXT PRIMARY KEY,
    account TEXT NOT NULL,
    chosen_at_unix_ms INTEGER NOT NULL
);
-- Each response already counted, with when the harness recorded it. A response is remembered
-- for `USAGE_RESPONSE_HORIZON_MS`, longer than any harness keeps a response in the record st
-- replays, and an older response is never counted.
CREATE TABLE IF NOT EXISTS local_usage_responses (
    subject TEXT NOT NULL,
    source_id TEXT NOT NULL,
    observed_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(subject, source_id)
);
CREATE INDEX IF NOT EXISTS local_usage_responses_age_index
ON local_usage_responses(observed_at_unix_ms);
-- The last replicated observation of each `latest` slot this node wrote, and the newest
-- local observation of the slot that no replicated claim carries yet.
CREATE TABLE IF NOT EXISTS local_latest_slots (
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    slot TEXT NOT NULL,
    published_at_unix_ms INTEGER NOT NULL,
    published_fields TEXT NOT NULL,
    pending_local_id INTEGER,
    PRIMARY KEY (subject, kind, slot)
);
CREATE TABLE IF NOT EXISTS revision_proposals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES mission_runs(id),
    source_generation_id TEXT NOT NULL REFERENCES run_generations(id),
    candidate_revision TEXT NOT NULL,
    actor TEXT NOT NULL,
    reason TEXT NOT NULL,
    status TEXT NOT NULL,
    cutover TEXT NOT NULL,
    compatible_steps TEXT NOT NULL,
    reviewers TEXT NOT NULL,
    approvals TEXT NOT NULL,
    preview_hash TEXT,
    successor_generation_id TEXT,
    created_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS revision_proposals_run_index ON revision_proposals(run_id);
CREATE INDEX IF NOT EXISTS revision_proposals_generation_index
ON revision_proposals(source_generation_id);
CREATE TABLE IF NOT EXISTS planning_sessions (
    id TEXT PRIMARY KEY,
    mission_id TEXT NOT NULL,
    request_ref TEXT NOT NULL,
    workspace TEXT NOT NULL,
    requester TEXT NOT NULL,
    planner TEXT NOT NULL,
    planner_spec_json TEXT NOT NULL DEFAULT '{"provider":"codex"}',
    status TEXT NOT NULL,
    target_run_id TEXT,
    source_generation_id TEXT,
    published_revision TEXT,
    created_at_unix_ms TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS planning_candidates (
    session_id TEXT NOT NULL REFERENCES planning_sessions(id),
    variant TEXT NOT NULL,
    revision INTEGER NOT NULL,
    markdown_ref TEXT NOT NULL,
    kdl_ref TEXT NOT NULL,
    mission_revision TEXT NOT NULL,
    submitted_at_unix_ms TEXT NOT NULL,
    PRIMARY KEY(session_id, variant, revision)
);
CREATE TABLE IF NOT EXISTS planning_previews (
    session_id TEXT NOT NULL REFERENCES planning_sessions(id),
    variant TEXT NOT NULL,
    candidate_revision INTEGER NOT NULL,
    hash TEXT NOT NULL,
    store_index INTEGER NOT NULL,
    graph TEXT NOT NULL,
    diff TEXT NOT NULL,
    mission_response TEXT NOT NULL,
    created_at_unix_ms TEXT NOT NULL,
    PRIMARY KEY(session_id, variant)
);
"#;

/// How many threads one large status projection splits across.
const STATUS_WORKERS: usize = 4;

/// The claims that say what a subject actually is: none of its declarations, harness reports,
/// readiness deadlines or reconcile faults. A seat's harness reports grow with every turn it
/// takes, to tens of thousands, so `harness.*` is written as the two `kind` ranges around it and
/// the query names `claims_subject_kind_index`: SQLite then seeks past a subject's harness
/// reports instead of reading each one. Unlike `NOT LIKE`, the ranges are case-sensitive, as
/// `str::starts_with` is.
const ACTUAL_STATE_CLAIM: &str = "(claims.kind<'harness.' OR claims.kind>='harness/')
     AND claims.kind NOT IN ('intent.desired', 'runtime.readiness-deadline-reached',
                             'reconcile.fault')";

// Repaired originals are not live actual-state sources, even if a node retains their
// claim rows or has trimmed the admission receipt. The digest's repaired-ID set survives
// receipt cleanup and is maintained transactionally by repair claims. Keep this separate
// from ACTUAL_STATE_CLAIM, which is also a SQLite partial-index predicate.
const UNREPAIRED_CLAIM: &str = "NOT EXISTS (
    SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id
)";

// Both discovery and index installation share the actual fold's open-ended kind predicate.
// Custom actual-state kinds can also supply a runtime id; a fixed kind allowlist is incomplete.
const RUNTIME_ID_FIELD: &str = "json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
    THEN '$.runtime_id' ELSE '$.fields.runtime_id' END)";
static RUNTIME_ID_INDEX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| format!(
    "CREATE INDEX IF NOT EXISTS claims_runtime_id_subject_index
     ON claims({RUNTIME_ID_FIELD}, subject, store_index)
     WHERE {ACTUAL_STATE_CLAIM} AND typeof({RUNTIME_ID_FIELD})='text'"
));

/// The predicate of `claims_current_view_index`, word for word, so SQLite can use the index.
const CURRENT_VIEW_CLAIM: &str = "(kind='intent.desired'
   OR json_extract(body, '$.fields.status') NOT IN ('stopped', 'absent', 'exited')
   OR json_extract(body, '$.status') NOT IN ('stopped', 'absent', 'exited'))";

/// The subjects with a runtime observation at or before store index `?1`, in subject order. Each
/// step seeks past the subject before it in `claims_runtime_subject_index`, so the query reads
/// about one index entry per subject, not every runtime observation.
const RUNTIME_SUBJECTS: &str = "WITH RECURSIVE runtime_subjects(subject) AS (
         SELECT (SELECT subject FROM claims INDEXED BY claims_runtime_subject_index
                 WHERE kind='runtime.observed' AND store_index<=?1 ORDER BY subject LIMIT 1)
         UNION ALL
         SELECT (SELECT subject FROM claims INDEXED BY claims_runtime_subject_index
                 WHERE kind='runtime.observed' AND subject>runtime_subjects.subject
                   AND store_index<=?1
                 ORDER BY subject LIMIT 1)
         FROM runtime_subjects WHERE runtime_subjects.subject IS NOT NULL
     )
     SELECT subject FROM runtime_subjects WHERE subject IS NOT NULL";

/// The subjects from `?2` up to but not including `?3` with a claim at or before store index
/// `?1`, in subject order, by one seek per subject in `claims_subject_index`.
pub const RANGE_SUBJECTS: &str = "WITH RECURSIVE range_subjects(subject) AS (
         SELECT (SELECT subject FROM claims INDEXED BY claims_subject_index
                 WHERE subject>=?2 AND subject<?3 AND store_index<=?1 ORDER BY subject LIMIT 1)
         UNION ALL
         SELECT (SELECT subject FROM claims INDEXED BY claims_subject_index
                 WHERE subject>range_subjects.subject AND subject<?3 AND store_index<=?1
                 ORDER BY subject LIMIT 1)
         FROM range_subjects WHERE range_subjects.subject IS NOT NULL
     )
     SELECT subject FROM range_subjects WHERE subject IS NOT NULL";

/// The least string above every string that begins with `prefix`, when `prefix` is plain ASCII
/// without GLOB wildcards, so `subject GLOB 'prefix*'` holds exactly when
/// `prefix <= subject < bound`.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    if !prefix.is_ascii() || prefix.contains(['*', '?', '[']) {
        return None;
    }
    let mut bytes = prefix.as_bytes().to_vec();
    let last = bytes.last_mut().filter(|last| **last < 0x7f)?;
    *last += 1;
    String::from_utf8(bytes).ok()
}

/// Whether a run `?2`, its generation `?3`, or a step of that generation (subjects from `?4` up
/// to `?5`) has a claim at or before store index `?1`. Three seeks in `claims_subject_index`;
/// the steps are a subject range, as a `LIKE` pattern cannot use the index.
const RUN_HAS_PRIOR_CLAIMS: &str =
    "SELECT EXISTS(SELECT 1 FROM claims WHERE store_index<=?1 AND subject=?2)
         OR EXISTS(SELECT 1 FROM claims WHERE store_index<=?1 AND subject=?3)
         OR EXISTS(SELECT 1 FROM claims WHERE store_index<=?1 AND subject>=?4 AND subject<?5)";

/// The runtime incarnation a claim names, read as a view of the claim does: from its `fields`,
/// or from the body itself when it has none. The expression of
/// `claims_incarnation_accepted_index`, word for word, so SQLite can use the index.
const INCARNATION_OF_CLAIM: &str =
    "json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END)";

/// The predicate of `mission_runs_open_index`, word for word, so SQLite can use the index.
const OPEN_MISSION_RUN: &str = "mission_runs.status NOT IN ('completed','failed','cancelled')";

/// The first (`ASC`) or last (`DESC`) acceptance time of the claims about subject `?1` that
/// name incarnation `?2`, at or before store index `?3`: one seek into the incarnation's claims.
fn incarnation_claim_time_query(order: &str) -> String {
    format!(
        "SELECT accepted_at_unix_ms
         FROM claims INDEXED BY claims_incarnation_accepted_index
         WHERE subject=?1 AND {INCARNATION_OF_CLAIM}=?2 AND store_index<=?3
         ORDER BY length(accepted_at_unix_ms) {order}, accepted_at_unix_ms {order}
         LIMIT 1"
    )
}

/// The newest claim at or before a snapshot that an agent status can depend on.
const AGENT_STATUS_INDEX_QUERY: &str = "SELECT store_index FROM claims WHERE store_index<=?1
     AND (+subject GLOB 'agent/*' OR +subject GLOB 'mission-run/*'
          OR +subject GLOB 'run-generation/*')
     ORDER BY store_index DESC LIMIT 1";

/// Whether `subject` names a runtime, whose stopped or undeclared state puts it in history.
fn runtime_subject(subject: &str) -> bool {
    subject.starts_with("agent/")
        || subject.starts_with("exec/")
        || subject.starts_with("pty/")
        || subject.starts_with("gate-operation/")
}

/// Status reductions kept per subject between snapshots. `through` is the newest snapshot whose
/// arrivals were applied: a subject that gained a claim, acted in one, or whose owning run or
/// generation gained one since an answer was read loses it. `conflicts` counts conflicting
/// operations, which change a subject's status without a claim of its own.
#[derive(Default)]
pub struct SubjectCache {
    through: u64,
    conflicts: u64,
    views: HashMap<String, ViewEntry>,
    statuses: HashMap<String, StatusEntry>,
    summary_statuses: HashMap<String, StatusEntry>,
    runtime_statuses: HashMap<String, StatusEntry>,
    // Summary and full statuses never share entries: their provenance/harness fields differ.
    card_statuses: HashMap<String, StatusEntry>,
}

const AGENT_CARD_STATUS_LIMIT: usize = 4096;

/// One subject's reduction, read at snapshot `read_at`, so it holds from there on. The claims
/// mode is part of the answer: a summary reduction and a full one are cached side by side.
struct StatusEntry {
    read_at: u64,
    owners: Vec<String>,
    claims: ClaimsReduction,
    status: SubjectStatus,
    action: Option<PlannedAction>,
}

/// One runtime's answer. It depends on the runtime's own claims and, for a declared runtime,
/// on its owning run's and generation's, so it holds at every snapshot from the newest of
/// those claims, `head`, while none of `owners` or the runtime itself gains another.
pub struct ViewEntry {
    head: u64,
    history: bool,
    /// Whether the answer used a declaration, and so the snapshot's owner state.
    declared: bool,
    owners: Vec<String>,
}

fn subject_head_at(connection: &Connection, subject: &str, store_index: u64) -> Result<u64> {
    if subject.starts_with("arrangement/") {
        let index: u64 = connection.query_row("SELECT changed_index FROM arrangements WHERE subject=?1", [subject], |row| row.get(0)).optional()?.unwrap_or(0);
        anyhow::ensure!(index <= store_index, "arrangement snapshot frontier is stale");
        return Ok(index);
    }
    connection
        .prepare_cached(
            "SELECT COALESCE(MAX(store_index), 0) FROM claims WHERE subject=?1 AND store_index<=?2",
        )?
        .query_row(params![subject, store_index], |row| row.get::<_, u64>(0))
        .map_err(Into::into)
}

/// Whether a current view at `store_index` leaves out runtime `subject`: exactly when its
/// reduction there would place it in history. `owners` says whether a declared runtime's owning
/// run and generation may decide it; the snapshot reduction reads them from their claims. The
/// roster reads many runtimes at once ([`Store::runtime_view_entries`]); this one-runtime read
/// is what its parity tests compare it with.
#[cfg(any(test, feature = "test-support"))]
pub fn runtime_view_entry(
    connection: &Connection,
    subject: &str,
    store_index: u64,
    owners: bool,
) -> Result<ViewEntry> {
    let head = subject_head_at(connection, subject, store_index)?;
    // A folded status can be live only when some claim carries a live status, and a declaration
    // is one of these claims, so a runtime with none of them is stopped or undeclared: history.
    let candidate = connection
        .prepare_cached(&format!(
            "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_current_view_index
                           WHERE subject=?1 AND store_index<=?2 AND {CURRENT_VIEW_CLAIM})"
        ))?
        .query_row(params![subject, store_index], |row| row.get::<_, bool>(0))?;
    runtime_view_entry_from(
        connection,
        subject,
        store_index,
        owners,
        head,
        candidate,
        &mut || desired_row_at(connection, subject, Some(store_index)),
        &mut |of| latest_actual_at(connection, of, Some(store_index)),
        &mut |owner| subject_head_at(connection, owner, store_index),
    )
}

/// [`runtime_view_entry`] from a runtime's head and whether it has a current-view claim. Its
/// declaration, the actual states of it and its owners, and its owners' heads are read through
/// the closures, only as the answer needs them.
#[allow(clippy::too_many_arguments)]
fn runtime_view_entry_from(
    connection: &Connection,
    subject: &str,
    store_index: u64,
    owners: bool,
    head: u64,
    candidate: bool,
    desired: &mut dyn FnMut() -> Result<Option<DesiredRow>>,
    actual: &mut dyn FnMut(&str) -> Result<Option<Value>>,
    owner_head: &mut dyn FnMut(&str) -> Result<u64>,
) -> Result<ViewEntry> {
    let entry = |history| ViewEntry {
        head,
        history,
        declared: false,
        owners: Vec::new(),
    };
    if !candidate {
        return Ok(entry(true));
    }
    let status = |actual: Option<Value>| -> Option<String> {
        actual
            .as_ref()
            .and_then(|value| value.get("fields").unwrap_or(value).get("status"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let stopped = |status: Option<&str>| matches!(status, Some("stopped" | "absent" | "exited"));
    let Some(desired) = desired()? else {
        // Undeclared: current only while its status is live.
        let status = status(actual(subject)?);
        return Ok(entry(status.is_none() || stopped(status.as_deref())));
    };
    if !owners {
        return Ok(ViewEntry {
            declared: true,
            ..entry(false)
        });
    }
    let annotation = operational_annotation(
        connection,
        subject,
        Some(&desired.kind),
        true,
        desired.owner_run.as_deref(),
        desired.owner_generation.as_deref(),
        None,
        Some(store_index),
        actual,
    )?;
    // A stop declaration leaves a stopped runtime in history; no other declaration reads status.
    let history = annotation.layer != "current"
        || (desired.kind == "stop" && stopped(status(actual(subject)?).as_deref()));
    let owners = desired
        .owner_run
        .iter()
        .chain(desired.owner_generation.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut head = head;
    for owner in &owners {
        head = head.max(owner_head(owner)?);
    }
    Ok(ViewEntry {
        head,
        history,
        declared: true,
        owners,
    })
}

/// A single-origin runtime-only history cannot have rival runtime authority. Runtime observations
/// append fields, so an omitted field retains its newest earlier value; explicit null replaces it.
/// Other actual kinds and origins use the general fold and causal source selection instead.
fn runtime_only_authority(
    connection: &Connection,
    subject: &str,
    through: u64,
    fields: &[&str],
) -> Result<Option<(Value, String)>> {
    let mut sources = connection.prepare_cached(&format!(
        "SELECT DISTINCT claims.kind, claims.origin FROM claims INDEXED BY claims_subject_kind_index
         WHERE claims.subject=?1 AND claims.store_index<=?2 AND {ACTUAL_STATE_CLAIM}
           AND {UNREPAIRED_CLAIM} LIMIT 2"
    ))?;
    let mut rows = sources.query(params![subject, through])?;
    let Some(row) = rows.next()? else { return Ok(None); };
    if row.get::<_, String>(0)? != "runtime.observed" {
        return Ok(None);
    }
    let origin = row.get::<_, String>(1)?;
    if rows.next()?.is_some() {
        return Ok(None);
    }
    let body: String = connection.prepare_cached(&format!(
        "{} LIMIT 1", newest_claims_of_kind_query("claims.body", "runtime.observed"),
    ))?.query_row(params![subject, through], |row| row.get(0))?;
    let latest: Value = serde_json::from_str(&body)?;
    let latest = latest.get("fields").unwrap_or(&latest);
    let mut older = connection.prepare_cached(&format!(
        "SELECT claims.body FROM claims INDEXED BY claims_subject_kind_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='runtime.observed' AND claims.store_index<=?4
           AND {UNREPAIRED_CLAIM}
           AND json_type(claims.body, CASE WHEN json_type(claims.body, '$.fields') IS NULL
                         THEN ?2 ELSE ?3 END) IS NOT NULL
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    ))?;
    let mut actual = serde_json::Map::new();
    for &field in fields {
        let root_path = format!("$.{field}");
        let fields_path = format!("$.fields.{field}");
        if let Some(value) = latest.get(field) {
            actual.insert(field.into(), value.clone());
        } else {
            let body: Option<String> = older.query_row(
                params![subject, root_path, fields_path, through],
                |row| row.get(0),
            ).optional()?;
            if let Some(body) = body {
                let value: Value = serde_json::from_str(&body)?;
                if let Some(value) = value.get("fields").unwrap_or(&value).get(field) {
                    actual.insert(field.into(), value.clone());
                }
            }
        }
    }
    Ok(Some((Value::Object(actual), origin)))
}

/// Which claims — and, for agent cards, which provenance — a status reduction reads.
/// `Full` carries the whole canonical id vector in `SubjectStatus::claims`, which costs a
/// claim-by-claim pass over the subject's history: only reductions a public response
/// serializes, and the apply/planning fences that compare vectors, need it. `Summary`
/// leaves `claims` empty and fills `claims_summary` instead, resolving canonical tie keys
/// only for the accepted-time groups the summary's answers can come from. `AgentCard`
/// also skips the harness fold and desired conflicts, which cards replace with observed
/// evidence; provenance is only the last canonical claim, and only without a declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimsReduction {
    Full,
    Summary,
    /// Runtime resources do not consume claim ids, counts, or session boundaries.
    Runtime,
    AgentCard,
}
/// One subject's status at `at_index`, and the action it asks of its host when it is current and
/// differs from what is declared. With `owner_filter`, a subject another run owns is skipped.
fn subject_status_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    owner_filter: Option<&str>,
    claims: ClaimsReduction,
) -> Result<Option<(SubjectStatus, Option<PlannedAction>)>> {
    let _profile = crate::profile::span(match claims {
        ClaimsReduction::Full => "status/reduction-full",
        ClaimsReduction::Summary => "status/reduction-summary",
        ClaimsReduction::Runtime => "status/reduction-runtime",
        ClaimsReduction::AgentCard => "status/reduction-agent-card",
    });
    #[cfg(any(test, feature = "test-support"))]
    SUBJECT_REDUCTIONS.with(|reductions| reductions.set(reductions.get() + 1));
    if subject.starts_with("arrangement/") {
        if owner_filter.is_some() { return Ok(None); }
        let actual = arrangements::arrangement_at(connection, subject, at_index.unwrap_or(i64::MAX as u64))?;
        let revision = actual.as_ref().and_then(|v| v["revision"].as_str()).map(str::to_owned);
        let live = actual.is_some();
        return Ok(Some((SubjectStatus {
            subject: subject.into(), kind: Some("arrangement".into()),
            desired_token: None, desired_revision: None, desired: None,
            actual, actual_claim: revision.clone(), actual_origin: None,
            harness: None, conflicts: vec![], claims: revision.clone().into_iter().collect(),
            claims_summary: ClaimsSummary {
                first: revision.clone(),
                latest: revision.clone(),
                count: usize::from(revision.is_some()),
                session_boundary: None,
            },
            owner_run: None, gap: None, reachability: if live { "reachable" } else { "unknown" }.into(),
            reason: None, under: vec![],
            projection: OperationalAnnotation { layer: if live { "current" } else { "history" }.into(), actionable: false,
                reasons: if live { vec![] } else { vec!["retired-or-uncreated".into()] }, owner_generation: None, runtime_incarnation: None },
        }, None)));
    }
    let desired = desired_row_at(connection, subject, at_index)?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(after_read) = STATUS_AFTER_DESIRED_READ.with(|slot| slot.borrow_mut().take()) {
        after_read();
    }
    let member = desired
        .as_ref()
        .and_then(|row| row.member.as_deref())
        .and_then(|value| serde_json::from_str::<crate::model::MemberSpec>(value).ok());
    let actual = latest_actual_at(connection, subject, at_index)?;
    let (actual_claim, actual_origin, actual_origin_conflict) = selected_actual_source_at(
        connection,
        subject,
        at_index,
        member.as_ref().map(|member| member.host.as_str()),
    )?;
    let (harness, claims, claims_summary, conflicts) = match claims {
        ClaimsReduction::Full => (
            current_harness_at(connection, subject, at_index)?,
            claim_ids_at(connection, subject, at_index)?,
            ClaimsSummary::default(),
            desired_conflicts_at(
                connection,
                subject,
                desired.as_ref().map(|row| row.claim_id.as_str()),
                at_index,
            )?,
        ),
        ClaimsReduction::Summary => (
            current_harness_at(connection, subject, at_index)?,
            Vec::new(),
            claims_summary_at(connection, subject, at_index)?,
            desired_conflicts_at(
                connection,
                subject,
                desired.as_ref().map(|row| row.claim_id.as_str()),
                at_index,
            )?,
        ),
        ClaimsReduction::Runtime => (
            current_harness_at(connection, subject, at_index)?,
            Vec::new(),
            ClaimsSummary::default(),
            desired_conflicts_at(connection, subject, desired.as_ref().map(|row| row.claim_id.as_str()), at_index)?,
        ),
        ClaimsReduction::AgentCard => {
            let latest = if desired.is_some() {
                None
            } else {
                connection.prepare_cached(&canonical_sql(
                    "SELECT id FROM claims INDEXED BY claims_subject_accepted_index
                     WHERE subject=?1 AND store_index<=?2
                     ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                    ))?
                    .query_row(
                        params![subject, at_index.unwrap_or(i64::MAX as u64)],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
            };
            (
                None,
                latest.clone().into_iter().collect(),
                ClaimsSummary {
                    latest,
                    ..Default::default()
                },
                Vec::new(),
            )
        }
    };
    if owner_filter.is_some_and(|run| {
        desired.as_ref().and_then(|row| row.owner_run.as_deref()) != Some(run)
    }) {
        return Ok(None);
    }
    let unknown_claim = has_unknown_claim_at(connection, subject, at_index)?;
    let inputs = SubjectStatusInputs {
        desired,
        member,
        actual,
        actual_source: (actual_claim, actual_origin, actual_origin_conflict),
        harness,
        claims,
        claims_summary,
        conflicts,
        unknown_claim,
    };
    assemble_subject_status(
        connection, subject, at_index, inputs,
        &mut |owner| latest_actual_at(connection, owner, at_index),
    ).map(Some)
}

/// What one subject's status reduction reads at a snapshot: read for one subject by
/// [`subject_status_at`], or for many at once by the agent card fold.
struct SubjectStatusInputs {
    desired: Option<DesiredRow>,
    member: Option<crate::model::MemberSpec>,
    actual: Option<Value>,
    /// The selected actual claim, its origin, and whether rival origins leave it indeterminate.
    actual_source: (Option<String>, Option<String>, bool),
    harness: Option<crate::model::CurrentHarnessView>,
    claims: Vec<String>,
    claims_summary: ClaimsSummary,
    conflicts: Vec<String>,
    unknown_claim: Option<String>,
}

/// The status and planned action [`SubjectStatusInputs`] reduce to. A snapshot's owning run
/// and generation states are read through `owner_actual`.
fn assemble_subject_status(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    inputs: SubjectStatusInputs,
    owner_actual: &mut dyn FnMut(&str) -> Result<Option<Value>>,
) -> Result<(SubjectStatus, Option<PlannedAction>)> {
    let SubjectStatusInputs {
        desired,
        member,
        actual,
        actual_source: (actual_claim, actual_origin, actual_origin_conflict),
        harness,
        claims,
        claims_summary,
        conflicts,
        unknown_claim,
    } = inputs;
    let kind = desired.as_ref().map(|row| row.kind.clone());
    let owner_run = desired.as_ref().and_then(|row| row.owner_run.clone());
    let owner_generation = desired
        .as_ref()
        .and_then(|row| row.owner_generation.clone());
    let status = actual
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str);
    let reachability = if unknown_claim.is_some() || actual_origin_conflict {
        "indeterminate".to_owned()
    } else {
        actual
            .as_ref()
            .and_then(|value| value.get("reachability"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                if actual.is_some() {
                    "reachable"
                } else if desired.is_some() {
                    "indeterminate"
                } else {
                    // Nothing declares or observes this subject, so nothing says it
                    // can be reached.
                    "unknown"
                }
            })
            .to_owned()
    };
    let gap = match (kind.as_deref(), member.as_ref(), status) {
        (Some("stop"), _, Some("stopped" | "absent" | "exited")) => None,
        (Some("stop"), _, _) => Some("the desired state is stopped".to_owned()),
        (_, Some(_), Some("running" | "ready" | "working" | "idle")) => None,
        (_, Some(member), Some("exited")) if member.restart == crate::model::RestartType::Never => {
            None
        }
        (_, Some(_), Some(value)) => Some(format!("the member is {value}")),
        (_, Some(_), None) => Some("the desired member has no actual state".to_owned()),
        _ => None,
    };
    let projection = operational_annotation(
        connection,
        subject,
        kind.as_deref(),
        desired.is_some(),
        owner_run.as_deref(),
        owner_generation.as_deref(),
        actual.as_ref(),
        at_index,
        owner_actual,
    )?;
    let action = (projection.layer == "current")
        .then_some(gap.as_ref())
        .flatten()
        .map(|reason| PlannedAction {
            subject: subject.to_owned(),
            action: if matches!(kind.as_deref(), Some("stop")) {
                "stop"
            } else {
                "reconcile"
            }
            .into(),
            reason: reason.clone(),
        });
    let reason = actual_origin_conflict
        .then(|| "concurrent runtime observations have indeterminate authority".to_owned())
        .or_else(|| {
            unknown_claim
                .map(|kind| format!("claim kind `{kind}` is not registered"))
                .or_else(|| {
                    actual
                        .as_ref()
                        .and_then(|value| value.get("reason"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        });
    let desired_value = desired
        .as_ref()
        .map(|row| serde_json::from_str(&row.body))
        .transpose()?;
    let under = desired_value
        .as_ref()
        .map(crate::graph::agent_under)
        .unwrap_or_default();
    Ok((
        SubjectStatus {
            subject: subject.to_owned(),
            kind,
            desired_token: desired.as_ref().map(|row| row.claim_id.clone()),
            desired_revision: desired.as_ref().map(|row| row.revision.clone()),
            desired: desired_value,
            actual,
            actual_claim,
            actual_origin,
            harness,
            conflicts,
            claims,
            claims_summary,
            owner_run,
            gap,
            reachability,
            reason,
            under,
            projection,
        },
        action,
    ))
}

/// A step as a person reads it in a list: which mission, which step, and what it is for.
#[derive(Clone, Debug)]
pub struct StepLabel {
    pub run: String,
    pub mission: String,
    pub path: String,
    pub title: Option<String>,
    pub goal: Option<String>,
    pub status: String,
    pub updated_at_unix_ms: u128,
}

/// A kept agent status: the snapshot it answers, the agent projection index it was reduced at,
/// whether it includes history, which claims it carries, and the status.
type AgentStatusEntry = (u64, u64, bool, ClaimsReduction, Arc<StatusResponse>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionGateRunner {
    pub subject: String,
    pub host: String,
    pub owner_run: String,
    pub owner_step: Option<String>,
    pub retired: bool,
}

/// The claims of one idempotency key used for different requests, each as `(subject, writer)`.
pub type IdempotencyConflict = Vec<(String, String)>;

/// smalltalk's store: the graph store, which it derefs to, and smalltalk's runtime, which
/// keeps the caches of its projections and plugs its claim kinds into the graph.
pub struct Store {
    graph: GraphStore,
    smalltalk: Arc<SmalltalkRuntime>,
}

impl std::borrow::Borrow<GraphStore> for Store {
    fn borrow(&self) -> &GraphStore {
        &self.graph
    }
}

impl Deref for Store {
    type Target = GraphStore;

    fn deref(&self) -> &GraphStore {
        &self.graph
    }
}

// Owner joins compare the run ID with the reference's suffix, so SQLite looks each owner up by
// primary key instead of scanning every run for each desired row on every reconcile pass.
const TERMINAL_OWNED_RUNTIME_SUBJECTS: &str = "SELECT desired.subject
     FROM desired
     JOIN mission_runs owner
       ON substr(desired.owner_run, 1, 12)='mission-run/'
      AND owner.id=substr(desired.owner_run, 13)
     JOIN mission_runs root ON root.id=owner.root_run_id
     LEFT JOIN run_generations generation
       ON substr(desired.owner_generation, 1, 15)='run-generation/'
      AND generation.id=substr(desired.owner_generation, 16)
     LEFT JOIN step_runs step ON step.subject=desired.owner_step
     WHERE desired.member IS NOT NULL
       AND (
         owner.status IN ('completed','failed','cancelled')
         OR owner.phase='terminal'
         OR root.status IN ('completed','failed','cancelled')
         OR root.phase='terminal'
         OR generation.status IN ('completed','failed','cancelled')
         OR step.status='cancelled'
         OR (owner.phase='final-cancelled' AND desired.kind IN ('exec','pty')
             AND (desired.owner_step IS NULL OR step.status IN ('completed','failed')))
       )
     ORDER BY desired.subject";
const RETIRED_OWNED_INTAKE_SUBJECTS: &str = "SELECT desired.subject
     FROM desired
     JOIN mission_runs owner
       ON substr(desired.owner_run, 1, 12)='mission-run/'
      AND owner.id=substr(desired.owner_run, 13)
     JOIN mission_runs root ON root.id=owner.root_run_id
     WHERE desired.kind IN ('observer','subscription','schedule')
       AND (
         owner.status IN ('completed','failed','cancelled')
         OR owner.phase='terminal'
         OR root.status IN ('completed','failed','cancelled')
         OR root.phase='terminal'
         OR (
           desired.owner_generation IS NOT NULL
           AND desired.owner_generation != ('run-generation/' || owner.current_generation_id)
         )
       )
     ORDER BY desired.subject";
const ACTUAL_CACHE_LIMIT: usize = 4096;

pub struct MessageCacheEntry {
    latest_claim_index: u64,
    desired_claim_id: Option<String>,
    view: MessageView,
}

#[cfg(any(test, feature = "test-support"))]
impl Store {
    /// Hold every read connection the pool has, as a burst of long reads does, so a read
    /// inside has to open another.
    pub fn hold_read_connections_for_test(&self, hold: impl FnOnce()) {
        let idle_count = self
            .readers
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        let guards: Vec<_> = (0..idle_count).map(|_| self.readers.get()).collect();
        hold();
        drop(guards);
    }

    pub fn hold_writer_for_test(&self, hold: impl FnOnce()) {
        let _writer = self.connection.write();
        hold();
    }

    /// Record local observations as `append_claim` does, in one transaction: one commit, and
    /// so one sync to disk, instead of one for each observation.
    pub fn append_local_observations_for_test(&self, inputs: &[ClaimInput]) {
        let mut connection = self.connection.write();
        let transaction = connection.transaction().unwrap();
        for input in inputs {
            self.validate_claim_input(input).unwrap();
            assert!(
                local_retention(&input.kind),
                "{} is not a local observation",
                input.kind
            );
            validate_local_observation(input).unwrap();
            insert_local_observation_tx(&transaction, &self.origin, input, now_ms()).unwrap();
        }
        transaction.commit().unwrap();
    }

    pub fn hold_write_transaction_for_test(&self, hold: impl FnOnce()) {
        let mut writer = self.connection.write();
        let transaction = writer.transaction().unwrap();
        transaction
            .execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('test-write-stall', '1')",
                [],
            )
            .unwrap();
        hold();
        transaction.rollback().unwrap();
    }
}

pub struct ChildMissionContext {
    root_revision: String,
    root_run_id: String,
    parent_step_run: String,
    default_selector: Option<WorkSelector>,
}

fn stale_ref_request_tx(connection: &Connection, resource: &str, discovery: &str) -> Result<Option<String>> {
    let Some(requested) = claim_by_id_tx(connection, discovery)?.filter(|claim|
        claim.subject == resource && claim.body.pointer("/fields/kind").and_then(Value::as_str) == Some("vcs.ref")) else { return Ok(None); };
    let Some(current) = latest_actual(connection, resource)?.and_then(|actual| actual.get("facts").cloned()) else { return Ok(None); };
    let requested_head = requested.body.pointer("/fields/facts/head").and_then(Value::as_str);
    if let Some((requested, current)) = requested_head.zip(current.get("head").and_then(Value::as_str))
        && requested != current { return Ok(Some(format!("ref {resource} moved from head {requested} to {current}"))); }
    Ok(None)
}

const SUBSCRIPTION_STARTED_RUN_QUERY: &str =
    "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_subscription_started_run_index
     WHERE kind='subscription.mission-started'
     AND json_extract(body, '$.fields.mission_run')=?1)";

fn subscription_started_run_tx(connection: &Connection, run: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached(SUBSCRIPTION_STARTED_RUN_QUERY)?
        .query_row([run], |row| row.get(0))?)
}

fn stale_pull_request_request_tx(
    connection: &Connection,
    resource: &str,
    discovery: &str,
) -> Result<Option<String>> {
    smallclaims::touched::note_read(|| resource.to_owned());
    let Some(requested) = claim_by_id_tx(connection, discovery)?.filter(|claim| {
        claim.subject == resource
            && claim.body.pointer("/fields/kind").and_then(Value::as_str)
                == Some("vcs.pull-request")
    }) else {
        return Ok(None);
    };
    let Some(current) =
        latest_actual(connection, resource)?.and_then(|actual| actual.get("facts").cloned())
    else {
        return Ok(None);
    };
    let number = current
        .get("number")
        .and_then(Value::as_u64)
        .map_or_else(|| resource.to_owned(), |number| format!("#{number}"));
    if let Some(state) = current
        .get("state")
        .and_then(Value::as_str)
        .filter(|state| *state != "open")
    {
        return Ok(Some(format!("pull request {number} is {state}")));
    }
    if current.get("draft").and_then(Value::as_bool) == Some(true) {
        return Ok(Some(format!("pull request {number} is a draft again")));
    }
    let short = |head: &str| head.chars().take(12).collect::<String>();
    let requested_head = match requested
        .body
        .pointer("/fields/facts/head_sha")
        .and_then(Value::as_str)
    {
        Some(head) => Some(head.to_owned()),
        None => listed_head_tx(connection, Some(&requested.body))?,
    };
    let current_head = current.get("head_sha").and_then(Value::as_str);
    if let (Some(requested_head), Some(current_head)) = (requested_head.as_deref(), current_head)
        && requested_head != current_head
    {
        return Ok(Some(format!(
            "pull request {number} moved from head {} to {}",
            short(requested_head),
            short(current_head)
        )));
    }
    Ok(None)
}

fn migrate_schema(connection: &Connection) -> Result<()> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0
        || version == 13
        || version == 14
        || version == 15
        || version == 16
        || version == 17
        || version == 18
        || version == smallclaims::store::CURRENT_SCHEMA_VERSION
    {
        return Ok(());
    }
    if version == 12 {
        let has_planner_spec: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('planning_sessions') WHERE name='planner_spec_json')",
            [],
            |row| row.get(0),
        )?;
        if !has_planner_spec {
            connection.execute_batch(
                "ALTER TABLE planning_sessions ADD COLUMN planner_spec_json TEXT NOT NULL DEFAULT '{\"provider\":\"codex\"}';",
            )?;
        }
        connection.execute_batch("PRAGMA user_version = 13;")?;
        return Ok(());
    }
    if version == 10 {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS mission_run_deadlines (
                 run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
                 timeout_ms INTEGER NOT NULL,
                 deadline_at_unix_ms TEXT NOT NULL
             );
             PRAGMA user_version = 11;",
        )?;
    }
    connection.execute_batch(
        "PRAGMA foreign_keys = OFF;
         PRAGMA legacy_alter_table = ON;
         BEGIN IMMEDIATE;
         ALTER TABLE batches RENAME TO batches_schema_11;
         CREATE TABLE batches (
             id TEXT PRIMARY KEY,
             origin TEXT NOT NULL,
             replica_sequence INTEGER NOT NULL,
             previous_hash TEXT,
             hash TEXT NOT NULL,
             accepted_at_unix_ms TEXT NOT NULL
         );
         INSERT INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms)
         SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms
         FROM batches_schema_11;
         DROP TABLE batches_schema_11;
         DROP INDEX IF EXISTS revision_proposals_one_pending;
         COMMIT;
         PRAGMA legacy_alter_table = OFF;
         PRAGMA foreign_keys = ON;",
    )?;
    let failures: u64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        failures == 0,
        "the schema migration broke {failures} foreign keys"
    );
    let planning_table_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='planning_sessions')",
        [],
        |row| row.get(0),
    )?;
    let has_planner_spec: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('planning_sessions') WHERE name='planner_spec_json')",
        [],
        |row| row.get(0),
    )?;
    if planning_table_exists && !has_planner_spec {
        connection.execute_batch(
            "ALTER TABLE planning_sessions ADD COLUMN planner_spec_json TEXT NOT NULL DEFAULT '{\"provider\":\"codex\"}';",
        )?;
    }
    Ok(())
}

/// Older stores armed chained runs at creation. Preserve their budget, remove the clock
/// for runs never admitted, and derive admitted clocks from the synthetic step's accepted
/// completion claims. These are indexed reads for after-run steps, without a history replay.
fn migrate_mission_run_deadlines(connection: &Connection) -> Result<()> {
    let needs_migration: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mission_run_deadlines')
         WHERE name='deadline_at_unix_ms' AND \"notnull\"=1)",
        [],
        |row| row.get(0),
    )?;
    if !needs_migration {
        return Ok(());
    }
    let transaction = connection.unchecked_transaction()?;
    let triggers = transaction
        .prepare("SELECT name, sql FROM sqlite_master WHERE type='trigger' AND tbl_name='mission_run_deadlines'")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let deadlines = transaction
        .prepare(
            "SELECT deadlines.run_id, deadlines.timeout_ms, deadlines.deadline_at_unix_ms,
                    mission_run_after.run_id IS NOT NULL
             FROM mission_run_deadlines AS deadlines
             LEFT JOIN mission_run_after ON mission_run_after.run_id=deadlines.run_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut admitted = Vec::with_capacity(deadlines.len());
    for (run, timeout, old_deadline, after) in deadlines {
        let deadline = if after {
            let stamps = transaction
                .prepare_cached(
                    "SELECT claims.accepted_at_unix_ms FROM step_runs INDEXED BY step_runs_run_index
                     CROSS JOIN claims INDEXED BY claims_subject_kind_accepted_index
                     WHERE step_runs.run_id=?1 AND step_runs.step_path=?2
                       AND claims.subject=step_runs.subject AND claims.kind='step-run.state'
                       AND json_extract(claims.body, '$.fields.status')='completed'",
                )?
                .query_map(params![run, crate::mission::AFTER_RUN_STEP], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            stamps
                .into_iter()
                .map(|stamp| stamp.parse::<u128>().map_err(|error| anyhow::anyhow!(
                    "invalid accepted after-run completion timestamp for mission-run/{run}: {stamp:?}: {error}"
                )))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min()
                .map(|admitted| admitted.saturating_add(u128::from(timeout)).to_string())
        } else {
            Some(old_deadline)
        };
        admitted.push((run, timeout, deadline));
    }
    transaction.execute_batch(
        "DROP TABLE mission_run_deadlines;
         CREATE TABLE mission_run_deadlines (
             run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
             timeout_ms INTEGER NOT NULL,
             deadline_at_unix_ms TEXT
         );",
    )?;
    // Restore this table's mutation/digest triggers before inserting its corrected rows.
    // Keeping the registry intact avoids rehashing every unrelated projection on reopen.
    if triggers
        .iter()
        .any(|(name, _)| name == "projection_digest_mission_run_deadlines_insert")
    {
        transaction.execute(
            "UPDATE projection_digest_state SET row_count=0, accumulator=zeroblob(64)
             WHERE table_name='mission_run_deadlines'",
            [],
        )?;
    }
    for (_, sql) in triggers {
        transaction.execute_batch(&sql)?;
    }
    for (run, timeout, deadline) in admitted {
        transaction.execute(
            "INSERT INTO mission_run_deadlines(run_id, timeout_ms, deadline_at_unix_ms)
             VALUES (?1, ?2, ?3)",
            params![run, timeout, deadline],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// One repository item whose recorded facts changed, with the facts that changed. `deliver` says
/// whether the change asks a subscription for a review or a triage.
pub struct DiscoveredItem {
    subject: String,
    kind: &'static str,
    facts: Value,
    changed_fields: Vec<String>,
    /// The data types the change belongs to: the item's collection, `comments`, `reactions` or
    /// `mentions`.
    data_types: BTreeSet<String>,
    deliver: bool,
}

/// The data types a change to an item's facts belongs to. A comment, a reaction and a mention
/// each have their own; every other fact belongs to the item's collection.
fn item_data_types(
    collection: &str,
    prior: Option<&Value>,
    facts: &Value,
    changed_fields: &[String],
) -> BTreeSet<String> {
    let mut data_types = BTreeSet::new();
    for field in changed_fields {
        match field.as_str() {
            "comments" => data_types.insert("comments".to_owned()),
            "reactions" => data_types.insert("reactions".to_owned()),
            "mentions" => data_types.insert("mentions".to_owned()),
            "recent_comments" => data_types.insert("comments".to_owned()),
            "last_comment" => {
                let reactions = |value: Option<&Value>| {
                    value
                        .and_then(|value| value.pointer("/last_comment/reactions"))
                        .cloned()
                };
                if reactions(prior) != reactions(Some(facts)) {
                    data_types.insert("reactions".to_owned());
                }
                let without_reactions = |value: Option<&Value>| {
                    let mut comment = value.and_then(|value| value.get("last_comment")).cloned();
                    if let Some(Value::Object(comment)) = comment.as_mut() {
                        comment.remove("reactions");
                    }
                    comment
                };
                if without_reactions(prior) != without_reactions(Some(facts)) {
                    data_types.insert("comments".to_owned())
                } else {
                    false
                }
            }
            _ => data_types.insert(collection.to_owned()),
        };
    }
    data_types
}

/// A pull request is reviewed once for each head it is ready at: when it first appears open and
/// ready, when a draft becomes ready, and when a new head replaces one that was known. Nothing
/// else starts a review. A closure, a merge, a reopening at the same head, a title edit, and a
/// field that an older build did not record never do, because st would review a head it already
/// saw or one nobody can act on. A known item with no known head proves nothing, so a head first
/// recorded for it is a baseline.
fn pull_request_needs_review(prior: Option<&Value>, item: &Value) -> bool {
    // An item resource names the head `head_sha`; a listing names it `head`.
    let head_of = |item: &Value| {
        item.get("head_sha")
            .or_else(|| item.get("head"))
            .filter(|head| !head.is_null())
            .cloned()
    };
    let head = head_of(item);
    let open = item
        .get("state")
        .and_then(Value::as_str)
        .is_none_or(|state| state == "open");
    let ready = item.get("draft").and_then(Value::as_bool) != Some(true);
    if head.is_none() || !open || !ready {
        return false;
    }
    let Some(prior) = prior else {
        return true;
    };
    let prior_head = head_of(prior);
    prior.get("draft").and_then(Value::as_bool) == Some(true)
        || prior_head.is_some_and(|prior_head| Some(prior_head) != head)
}

/// The repository collections whose items become their own resources, with each item's subject
/// segment and resource kind.
const REPOSITORY_COLLECTIONS: [(&str, &str, &str); 2] = [
    ("pull_requests", "pull-request", "vcs.pull-request"),
    ("issues", "issue", "vcs.issue"),
];

/// The most mentions one item resource keeps, newest first.
const ITEM_MENTIONS: usize = 50;

/// What an item resource records after one sighting: its prior facts with every fact the
/// sighting read laid over them. A listing names the head commit `head`; the resource calls it
/// `head_sha`. A fact the sighting read as null is gone, except an unknown head. Only an open
/// pull request is known to be unmerged: one that closes keeps `merged` only when the sighting
/// could tell a merge from a closure. An edit to an older comment does not replace the last
/// comment, and mentions add to the ones known, keeping the newest of each login.
fn item_facts(
    repository: &str,
    field: &str,
    number: u64,
    prior: Option<&Value>,
    seen: &Value,
) -> serde_json::Map<String, Value> {
    let mut facts = prior
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, value) in seen.as_object().into_iter().flatten() {
        let name = match name.as_str() {
            "new" => continue,
            "head" => "head_sha",
            name => name,
        };
        match (name, facts.get(name)) {
            (_, _) if value.is_null() => {
                if name != "head_sha" {
                    facts.remove(name);
                }
            }
            ("last_comment", Some(known)) if !newer_comment(value, known) => {}
            // Each comment and review once, by kind and ID; the newest are kept.
            ("recent_comments", known) => {
                let merged = crate::resource::merge_recent_comments(
                    known.and_then(Value::as_array).into_iter().flatten(),
                    value.as_array().into_iter().flatten(),
                    Some(crate::resource::RECENT_COMMENTS),
                );
                facts.insert(name.into(), Value::Array(merged));
            }
            ("mentions", Some(Value::Array(known))) => {
                let mut mentions = BTreeMap::<String, Value>::new();
                for mention in known.iter().chain(value.as_array().into_iter().flatten()) {
                    let Some(login) = mention.get("login").and_then(Value::as_str) else {
                        continue;
                    };
                    let at = |mention: &Value| {
                        mention.get("at").and_then(Value::as_str).map(str::to_owned)
                    };
                    if mentions
                        .get(login)
                        .is_none_or(|kept| at(mention) >= at(kept))
                    {
                        mentions.insert(login.to_owned(), mention.clone());
                    }
                }
                let mut mentions = mentions.into_values().collect::<Vec<_>>();
                mentions.sort_by_key(|mention| {
                    std::cmp::Reverse(mention.get("at").and_then(Value::as_str).map(str::to_owned))
                });
                mentions.truncate(ITEM_MENTIONS);
                mentions.sort_by_key(|mention| {
                    mention
                        .get("login")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
                facts.insert(name.into(), Value::Array(mentions));
            }
            _ => {
                facts.insert(name.into(), value.clone());
            }
        }
    }
    facts.insert("repository".into(), Value::String(repository.into()));
    facts.insert("number".into(), Value::from(number));
    let state = facts
        .entry("state")
        .or_insert_with(|| Value::String("open".into()))
        .clone();
    if field == "pull_requests" {
        facts.entry("draft").or_insert_with(|| Value::Bool(false));
        if state == "open" {
            facts.insert("merged".into(), Value::Bool(false));
        } else if seen.get("merged").is_none() && seen.get("state").is_some() {
            facts.remove("merged");
        }
    }
    facts
}

use crate::resource::newer_comment;

/// The items of one repository collection whose recorded facts change with this observation.
/// `current` names only the items the observation saw, each with only the facts it read; an
/// item it did not see keeps its facts. `recorded` reads an item resource's latest facts.
///
/// An item resource that was never recorded falls back to the item in the previous repository
/// facts, where builds before item resources kept every item, and a recorded resource takes
/// what it lacks from there; such an item is recorded once even when nothing changed, so its
/// resource holds every fact from then on. An item known to neither is new,
/// unless the observation says it existed before the observer last looked (`"new": false`) or
/// this is the repository's baseline. Only a new item, or a pull request at a new ready head,
/// asks a subscription for a review or a triage.
fn discovered_collection_items(
    repository: &str,
    field: &str,
    previous: Option<&Value>,
    current: &Value,
    recorded: &mut dyn FnMut(&str) -> Option<Value>,
) -> Vec<DiscoveredItem> {
    let Some((_, segment, kind)) = REPOSITORY_COLLECTIONS
        .iter()
        .copied()
        .find(|(name, _, _)| *name == field)
    else {
        return Vec::new();
    };
    // The first observation is the baseline. The first listing that names its repository ID is
    // also the first complete listing: the earlier facts came from a first-page read, so the items
    // this listing adds were missed, not opened. Either way its items become known without a
    // delivery.
    let baseline = previous.is_none()
        || (current.get("repository_id").is_some()
            && previous
                .and_then(|value| value.get("repository_id"))
                .is_none());
    let Some(current_items) = current.get(field).and_then(Value::as_array) else {
        return Vec::new();
    };
    let legacy_items = previous
        .and_then(|value| value.get(field))
        .and_then(Value::as_array);
    current_items
        .iter()
        .filter_map(|item| {
            let number = item.get("number")?.as_u64()?;
            let subject = format!("{repository}/{segment}/{number}");
            let resource = recorded(&subject);
            let legacy = legacy_items
                .and_then(|items| {
                    items
                        .iter()
                        .find(|old| old.get("number").and_then(Value::as_u64) == Some(number))
                })
                .map(|old| Value::Object(item_facts(repository, field, number, None, old)));
            // A resource recorded before items carried `head_sha` takes its head from the listing.
            let prior = match (&resource, legacy) {
                (Some(resource), Some(Value::Object(mut legacy))) => {
                    legacy.extend(resource.as_object().cloned().unwrap_or_default());
                    Some(Value::Object(legacy))
                }
                (Some(resource), _) => Some(resource.clone()),
                (None, legacy) => legacy,
            };
            let prior = prior.as_ref();
            let facts = Value::Object(item_facts(repository, field, number, prior, item));
            if resource.as_ref() == Some(&facts) {
                return None;
            }
            // Creation receipts establish identity and attribution, not observer state.
            let observed_prior = prior.filter(|prior| prior.get("state").is_some());
            let new_item =
                observed_prior.is_none() && !baseline && item.get("new") != Some(&Value::Bool(false));
            let deliver = match (field, observed_prior) {
                ("pull_requests", Some(prior)) => pull_request_needs_review(Some(prior), &facts),
                (_, Some(_)) => false,
                ("pull_requests", None) => new_item && pull_request_needs_review(None, &facts),
                _ => new_item,
            };
            let changed_fields = facts
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(name, value)| {
                    prior.and_then(|prior| prior.get(name.as_str())) != Some(*value)
                })
                .map(|(name, _)| name.clone())
                .chain(
                    prior
                        .and_then(Value::as_object)
                        .into_iter()
                        .flat_map(|prior| {
                            prior
                                .keys()
                                .filter(|name| facts.get(name.as_str()).is_none())
                                .cloned()
                        }),
                )
                .collect::<Vec<_>>();
            Some(DiscoveredItem {
                subject,
                kind,
                data_types: item_data_types(field, prior, &facts, &changed_fields),
                changed_fields,
                facts,
                deliver,
            })
        })
        .collect()
}

/// Item claims written before items carried `head_sha` still cite the repository listing they
/// came from, and that listing names the head.
fn listed_head_tx(
    connection: &Connection,
    item: Option<&Value>,
) -> rusqlite::Result<Option<String>> {
    let Some(item) = item else {
        return Ok(None);
    };
    let (Some(number), Some(listing)) = (
        item.pointer("/fields/facts/number").and_then(Value::as_u64),
        item.pointer("/evidence/0").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let listing = connection
        .query_row("SELECT body FROM claims WHERE id=?1", [listing], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|body| serde_json::from_str::<Value>(&body).ok());
    Ok(listing
        .as_ref()
        .and_then(|value| value.pointer("/fields/facts/pull_requests"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("number").and_then(Value::as_u64) == Some(number))
        })
        .and_then(|item| item.get("head"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// How a watch ended, when it did: the `subscription.watch-ended` claim for the watch that began
/// at its `since`. Watching again begins a new watch.
pub fn watch_ended_tx(
    connection: &Connection,
    subject: &str,
    watch: &crate::model::WatchSpec,
) -> Result<Option<Value>, St3Error> {
    smallclaims::touched::note_read(|| subject.to_owned());
    let since = watch.since_unix_ms.to_string();
    let mut statement = connection
        .prepare_cached(
            "SELECT body FROM claims WHERE subject=?1 AND kind='subscription.watch-ended'",
        )
        .map_err(internal)?;
    let bodies = statement
        .query_map([subject], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(bodies
        .into_iter()
        .filter_map(|body| serde_json::from_str::<Value>(&body).ok())
        .map(|body| body["fields"].clone())
        .find(|fields| fields.get("since_unix_ms").and_then(Value::as_str) == Some(since.as_str())))
}

/// End a watch: why, and the final wake that told its seat, if one did. Ending it twice records
/// nothing more.
pub fn end_watch_tx(
    transaction: &rusqlite::Transaction<'_>,
    origin: &str,
    batch_id: Option<&str>,
    subject: &str,
    watch: &crate::model::WatchSpec,
    reason: &str,
    message: Option<&str>,
) -> Result<bool, St3Error> {
    if watch_ended_tx(transaction, subject, watch)?.is_some() {
        return Ok(false);
    }
    let mut fields = json!({
        "reason": reason,
        "since_unix_ms": watch.since_unix_ms.to_string(),
    });
    if let Some(message) = message {
        fields["message"] = Value::String(message.to_owned());
    }
    append_claim_tx(
        transaction,
        origin,
        subject,
        "subscription.watch-ended",
        None,
        &json!({"fields": fields}),
        &[],
        batch_id,
    )
    .map_err(claim_append_error)?;
    Ok(true)
}

/// The seat that registered the comment or review `kind` `id` of a repository.
pub fn github_post_agent_tx(
    connection: &Connection,
    locator: &str,
    kind: &str,
    id: u64,
) -> Result<Option<String>, St3Error> {
    let subject = crate::github_watch::github_post_subject(locator, kind, id);
    let body = connection
        .prepare_cached("SELECT body FROM claims WHERE subject=?1 AND kind='github.posted' LIMIT 1")
        .map_err(internal)?
        .query_row([&subject], |row| row.get::<_, String>(0))
        .optional()
        .map_err(internal)?;
    Ok(body
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .and_then(|body| body["fields"]["agent"].as_str().map(str::to_owned)))
}

/// The mission runs that published a `resource/mission-run/RUN/pull-request` naming this pull
/// request, by URL or by repository and number, at the same head when both name one. An
/// authoring mission publishes that resource when it opens the pull request.
const AUTHORING_PULL_REQUEST_RUNS_QUERY: &str =
    "SELECT subject, body FROM claims WHERE id IN (
         SELECT id FROM claims WHERE kind='resource.observed'
           AND subject LIKE 'resource/mission-run/%/pull-request'
           AND json_extract(body, '$.fields.facts.url')=?1
         UNION
         SELECT id FROM claims WHERE kind='resource.observed'
           AND subject LIKE 'resource/mission-run/%/pull-request'
           AND json_extract(body, '$.fields.facts.repository')=?2
           AND json_extract(body, '$.fields.facts.number')=?3
     ) ORDER BY CANONICAL_ASC(claims)";

fn authoring_pull_request_runs_tx(
    connection: &Connection,
    facts: &Value,
) -> rusqlite::Result<Vec<String>> {
    let url = facts.get("url").and_then(Value::as_str);
    let repository = facts.get("repository").and_then(Value::as_str);
    let number = facts.get("number").and_then(Value::as_u64);
    if url.is_none() && (repository.is_none() || number.is_none()) {
        return Ok(Vec::new());
    }
    let head = facts.get("head_sha").and_then(Value::as_str);
    // An OR can scan the whole partial index. UNION locates matching claim IDs with one
    // expression-index seek per identity, deduplicates dual matches, then orders only those
    // claims with the same canonical order as the original query.
    let mut statement =
        connection.prepare_cached(&canonical_sql(AUTHORING_PULL_REQUEST_RUNS_QUERY))?;
    let candidates = statement
        .query_map(params![url, repository, number], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut runs = Vec::new();
    for (resource, body) in candidates {
        let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let authored_head = value
            .pointer("/fields/facts/head_sha")
            .and_then(Value::as_str);
        if head.is_some() && authored_head.is_some() && head != authored_head {
            continue;
        }
        let Some(run) = resource
            .strip_prefix("resource/")
            .and_then(|name| name.strip_suffix("/pull-request"))
        else {
            continue;
        };
        if !runs.iter().any(|known| known == run) {
            runs.push(run.to_owned());
        }
    }
    Ok(runs)
}

/// Fill the message index once, from the claims a store held before the index existed. The
/// triggers keep it after that.
fn backfill_message_index(connection: &Connection) -> Result<()> {
    let filled: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='message_index')",
        [],
        |row| row.get(0),
    )?;
    if filled {
        return Ok(());
    }
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT OR REPLACE INTO message_index(subject, created_index, closed)
         SELECT subject, MIN(store_index), MAX(kind = 'message.closed')
         FROM claims WHERE subject LIKE 'message/%'
         GROUP BY subject;
         INSERT INTO meta(key, value) VALUES ('message_index', '1');
         COMMIT;",
    )?;
    Ok(())
}

/// The runtime smalltalk opens the graph with, for code that opens the graph store itself.
pub fn runtime() -> Arc<dyn smallclaims::Runtime> {
    Arc::new(SmalltalkRuntime::default())
}

/// The local rows a roster card can read: timeline entries an agent subject made, which date its
/// activity and its managed session. Heartbeat and telemetry observations never change a card —
/// the harness fold, working episodes, usage summaries and todos all read replicated claims —
/// and neither do `Latest` observations, which reach cards only through the claims written when
/// their state changes. So the frontier is the newest agent timeline row at or before the cut,
/// an exact `MAX(id)`: it never assumes observation ids and graph cuts rise together, which
/// repair and trim can break.
const ROSTER_LOCAL_FRONTIER: &str = "SELECT COALESCE((SELECT id FROM local_observations
 WHERE kind='harness.timeline' AND subject LIKE 'agent/%' AND after_store_index<=?1
 ORDER BY id DESC LIMIT 1), 0)";

/// One frontier read, inside the caller's SQLite snapshot when it holds one: the reader pool
/// lends pinned reads the same connection. The partial frontier index keeps an old cut from
/// walking newer heartbeat rows.
fn roster_local_frontier(connection: &Connection, index: u64) -> Result<u64> {
    Ok(connection.query_row(ROSTER_LOCAL_FRONTIER, [index], |row| row.get::<_, u64>(0))?)
}

#[derive(Default)]
pub struct AgentResourcesDelta {
    subjects: BTreeSet<String>,
    queues: bool,
    membership: bool,
}

pub enum AgentResourcesChange {
    Delta(AgentResourcesDelta),
    Invalidated { reason: &'static str, detail: String },
}

/// The exact cache-hit contract shared by the building path and the read-only warm pin: one
/// graph cut, one roster-relevant local frontier, one history mode, a queue lease window that
/// has not ended, and coverage of every requested subject.
fn agent_resources_entry_hits(
    entry: &runtime::AgentResourcesEntry,
    now: u128,
    index: u64,
    local: u64,
    history: bool,
    selected: Option<&BTreeSet<String>>,
) -> bool {
    entry.index == index
        && entry.local == local
        && entry.history == history
        && entry.valid_until_unix_ms.is_none_or(|expiry| now < expiry)
        && match selected {
            None => entry.covered.is_none(),
            Some(names) => entry
                .covered
                .as_ref()
                .is_none_or(|covered| names.is_subset(covered)),
        }
}

/// The runs that wait on a person, and the open human gates among them.
pub type HumanAttention = (BTreeSet<String>, Vec<st3_ui_model::missions::Gate>);

impl Store {
    /// The explicitly registered keyed views for this graph. This getter performs no reads
    /// or installation; availability and source coverage must be checked in a snapshot.
    pub fn ivm_views(&self) -> Option<Arc<smallclaims::ivm::Views>> {
        self.smalltalk.ivm_views.clone()
    }

    pub fn open(path: &Path, origin: impl Into<String>) -> Result<Self> {
        let smalltalk = Arc::new(SmalltalkRuntime::default());
        let graph = GraphStore::open(path, origin, smalltalk.clone())?;
        publication_facts::create_schema(&graph.connection.write())?;
        graph.connection.install_transaction_maintenance(publication_facts::flush)?;
        publication_facts::install_cache_observer(&graph, &smalltalk);
        Ok(Self { graph, smalltalk })
    }

    /// Open explicitly registered shadow views beside Smalltalk's production admission and
    /// projections. Source adapters own installation and certification before any read switch.
    pub fn open_with_ivm_views(
        path: &Path,
        origin: impl Into<String>,
        views: Arc<smallclaims::ivm::Views>,
    ) -> Result<Self> {
        let smalltalk = Arc::new(SmalltalkRuntime::with_ivm_views(views));
        let graph = GraphStore::open(path, origin, smalltalk.clone())?;
        publication_facts::create_schema(&graph.connection.write())?;
        graph.connection.install_transaction_maintenance(publication_facts::flush)?;
        publication_facts::install_cache_observer(&graph, &smalltalk);
        Ok(Self { graph, smalltalk })
    }

    /// Open a shared-memory store for sequential fixtures and short-lived tools.
    /// Concurrent server tests should use [`Self::open`]: shared-cache read/write
    /// contention returns `SQLITE_LOCKED` immediately instead of waiting.
    pub fn open_memory(origin: impl Into<String>) -> Result<Self> {
        let smalltalk = Arc::new(SmalltalkRuntime::default());
        let graph = GraphStore::open_memory(origin, smalltalk.clone())?;
        publication_facts::create_schema(&graph.connection.write())?;
        graph.connection.install_transaction_maintenance(publication_facts::flush)?;
        publication_facts::install_cache_observer(&graph, &smalltalk);
        Ok(Self { graph, smalltalk })
    }

    /// Simulate a different build's registry on this node.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_claim_registry(&self, registry: st3_schema::Registry) {
        assert!(
            self.smalltalk.claim_registry.set(registry).is_ok(),
            "the registry is set once"
        );
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn append_latest_observation(
        &self,
        input: &ClaimInput,
        now: u128,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        append_latest_observation(&self.graph, input, now)
    }


    /// When this node committed its first claim after store index `after`, through `through`,
    /// in Unix ms; `None` when every claim between came from another node, or there is none.
    pub fn first_local_commit_after(
        &self,
        after: u64,
        through: u64,
    ) -> Result<Option<u128>> {
        let accepted: Option<String> = self
            .readers
            .get()
            .prepare_cached(
                "SELECT accepted_at_unix_ms FROM claims WHERE store_index>?1 AND store_index<=?2
                 AND origin=?3 ORDER BY store_index LIMIT 1",
            )?
            .query_row(params![after as i64, through as i64, self.origin()], |row| row.get(0))
            .optional()?;
        Ok(accepted.and_then(|accepted| accepted.parse().ok()))
    }

    /// Rebuild the operation projection when it no longer matches the claim log, and say
    /// whether it had to. A start no longer rebuilds it; the daemon checks off the request path
    /// once it serves.
    pub fn repair_operation_projection_drift(&self) -> Result<bool> {
        // Finding the drift reads every operation, on a read connection. Repairing it writes
        // only the drifted rows: rebuilding the whole table held the only writer for a minute
        // and more on a populated store, stalling every write behind it.
        let drift = self.operation_projection_drift()?;
        self.repair_operation_projection_drift_from_audit(&drift)
    }

    /// Repair only keys found by an earlier audit, re-deriving them from current claims.
    /// A concurrent append cannot make an old finding overwrite the new canonical row.
    /// At most 16 keys share a writer transaction; release the writer between batches.
    /// This bounds the batch, not the work of one key with arbitrarily many source claims.
    pub fn repair_operation_projection_drift_from_audit(&self, drift: &[String]) -> Result<bool> {
        if drift.is_empty() {
            return Ok(false);
        }
        let mut changed = 0;
        for batch in drift.chunks(16) {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            changed += repair_operations_tx(&transaction, batch)?;
            transaction.commit()?;
        }
        Ok(changed != 0)
    }

    /// Idempotency keys that two requests with different content used, which only members
    /// that accepted them apart, as during a partition, can produce: how many there are, and up
    /// to `limit` of them with the subject and writer of each claim. Both claims stand.
    pub fn idempotency_conflicts(&self, limit: usize) -> Result<(u64, Vec<IdempotencyConflict>)> {
        let connection = self.readers.get();
        let count: u64 = connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE state='conflict'",
            [],
            |row| row.get(0),
        )?;
        let operations = connection
            .prepare_cached(
                "SELECT id FROM operations INDEXED BY operations_conflict_index
                 WHERE state='conflict' ORDER BY id LIMIT ?1",
            )?
            .query_map([limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut claims = connection.prepare_cached(
            "SELECT subject, origin FROM claims
             WHERE json_extract(body, '$._operation.id')=?1 ORDER BY subject, origin",
        )?;
        let mut conflicts = Vec::new();
        for operation in operations {
            conflicts.push(
                claims
                    .query_map([&operation], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok((count, conflicts))
    }

    pub fn operation_projection_drift(&self) -> Result<Vec<String>> {
        self.operation_audit(&mut |_| {})
    }

    pub fn rebuild_claim_projections(&self) -> Result<()> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        rebuild_operations_tx(&transaction)?;
        projectors::run_hooks(projectors::REBUILD, &transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn cached_idempotency_response<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>> {
        smallclaims::store::idempotency::cached_response(&self.readers.get(), key)?
            .map(|response| serde_json::from_str(&response))
            .transpose()
            .map_err(Into::into)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn cache_idempotency_response<T: Serialize>(
        &self,
        key: &str,
        response: &T,
    ) -> Result<()> {
        let response = serde_json::to_string(response)?;
        self.connection
            .batched(|transaction| {
                transaction.execute(
                    "INSERT OR IGNORE INTO idempotency(operation_id, response,replay_safe) VALUES (?1, ?2,1)",
                    params![opaque_cache_key(key), response],
                )
            })
            .map_err(anyhow::Error::msg)??;
        Ok(())
    }

    pub fn apply_drained_revision(
        &self,
        run: &str,
    ) -> Result<Option<RevisionSubmissionView>, St3Error> {
        let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
        let proposal = {
            let connection = self.connection.write();
            let active: u32 = connection
                .query_row(
                    "SELECT COUNT(*) FROM step_runs
                     WHERE generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)
                       AND status IN ('claimed','working','verifying')",
                    [run_id],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            if active != 0 {
                return Ok(None);
            }
            let current_generation: String = connection
                .query_row(
                    "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                    [run_id],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            for descendant in
                descendant_mission_run_ids_tx(&connection, &current_generation).map_err(internal)?
            {
                let active: u32 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM step_runs
                         WHERE run_id=?1
                           AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)
                           AND status IN ('claimed','working','verifying')",
                        [descendant],
                        |row| row.get(0),
                    )
                    .map_err(internal)?;
                if active != 0 {
                    return Ok(None);
                }
            }
            let proposal_id = connection
                .query_row(
                    &canonical_sql(
                        "SELECT p.id FROM revision_proposals p
                     JOIN mission_runs r ON r.id=p.run_id
                     JOIN claims created ON created.subject='revision-proposal/' || p.id
                       AND created.kind='revision-proposal.created'
                     WHERE p.run_id=?1 AND p.source_generation_id=r.current_generation_id
                       AND p.status='draining'
                     ORDER BY CANONICAL_ASC(created) LIMIT 1",
                    ),
                    [run_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?;
            proposal_id
                .map(|id| revision_proposal_view_tx(&connection, &id))
                .transpose()
                .map_err(internal)?
        };
        let Some(proposal) = proposal else {
            return Ok(None);
        };
        let run = self.finish_proposal_cutover(
            &proposal.id,
            &proposal.actor,
            &format!("revision-proposal:{}:drained", proposal.id),
        )?;
        let proposal = self
            .revision_proposal(&proposal.id)
            .map_err(internal)?
            .expect("the applied proposal exists");
        Ok(Some(RevisionSubmissionView {
            provenance: self
                .mission_provenance(&run.mission, &proposal.candidate_revision)
                .map_err(internal)?,
            status: "applied".into(),
            mission_run: run,
            proposal: Some(proposal),
        }))
    }

    /// What changed after store index `index` and local observation `local`, and the indexes the
    /// answer reaches: each new claim and local observation as subject, kind, actor and body.
    pub fn changes_since(&self, index: u64, local: i64) -> Result<ChangeFeed> {
        let connection = self.readers.get();
        let (to_index, to_local): (u64, i64) = connection.query_row(
            "SELECT (SELECT COALESCE(MAX(store_index), 0) FROM claims),
                    (SELECT COALESCE(MAX(id), 0) FROM local_observations)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut changes = connection
            .prepare_cached(
                "SELECT subject, kind, actor, body FROM claims
                 WHERE store_index>?1 AND store_index<=?2",
            )?
            .query_map(params![index, to_index], change_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        changes.extend(
            connection
                .prepare_cached(
                    "SELECT subject, kind, actor, body FROM local_observations
                     WHERE id>?1 AND id<=?2",
                )?
                .query_map(params![local, to_local], change_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        Ok(ChangeFeed {
            index: to_index,
            local: to_local,
            changes,
        })
    }

    /// The faults this host recorded that have not recovered, keyed by subject and scope.
    pub fn open_reconcile_faults(
        &self,
        origin: &str,
    ) -> Result<BTreeMap<(String, String), String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT subject, body FROM claims
             WHERE kind='reconcile.fault' AND origin=?1
             ORDER BY CANONICAL_ASC(claims)",
        ))?;
        let rows = statement
            .query_map([origin], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut faults = BTreeMap::new();
        for (subject, body) in rows {
            let body: Value = serde_json::from_str(&body)?;
            let fields = body.get("fields").unwrap_or(&body);
            let Some(scope) = fields.get("scope").and_then(Value::as_str) else {
                continue;
            };
            let key = (subject, scope.to_owned());
            if fields.get("status").and_then(Value::as_str) == Some("faulted") {
                let reason = fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                faults.insert(key, reason);
            } else {
                faults.remove(&key);
            }
        }
        Ok(faults)
    }

    /// The latest fault claim on `subject` in `scope`, while that fault is open.
    pub fn open_reconcile_fault_claim(
        &self,
        subject: &str,
        scope: &str,
    ) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        let claim = connection
            .query_row(
                &canonical_sql(
                    "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                        predecessors, accepted_at_unix_ms
                 FROM claims WHERE subject=?1 AND kind='reconcile.fault'
                   AND origin=?2 AND json_extract(body, '$.fields.scope')=?3
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                ),
                params![subject, self.origin, scope],
                claim_from_row,
            )
            .optional()?;
        Ok(claim.filter(|claim| {
            claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("faulted")
        }))
    }

    /// The reason for an open fault on `subject` in `scope`, if the latest record is a fault.
    pub fn reconcile_fault(&self, subject: &str, scope: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        let body: Option<String> = connection
            .query_row(
                &canonical_sql(
                    "SELECT body FROM claims WHERE subject=?1 AND kind='reconcile.fault'
                   AND json_extract(body, '$.fields.scope')=?2
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                ),
                params![subject, scope],
                |row| row.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(None);
        };
        let body: Value = serde_json::from_str(&body)?;
        let fields = body.get("fields").unwrap_or(&body);
        Ok(
            (fields.get("status").and_then(Value::as_str) == Some("faulted")).then(|| {
                fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            }),
        )
    }

    /// Latest preparation outcome for one message and the recipient's current incarnation.
    /// A delayed report from a retired channel cannot mask a current failure or recovery.
    pub fn claude_preparation_diagnostic(
        &self,
        seat: &str,
        message: &str,
        incarnation: &str,
    ) -> Result<Option<(Value, u128)>> {
        let connection = self.readers.get();
        let row = connection.prepare_cached(&canonical_sql(
            "SELECT body, accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND actor=?1 AND kind='harness.diagnostic'
               AND json_extract(body, '$.fields.code')='claude-message-unforwarded'
               AND json_extract(body, '$.fields.incarnation_id')=?3
               AND EXISTS (SELECT 1 FROM claims AS message_claim
                 WHERE message_claim.id=json_extract(claims.body, '$.evidence[0]')
                   AND message_claim.subject=?2)
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        ))?.query_row(params![seat, message, incarnation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).optional()?;
        row.map(|(body, at)| Ok((serde_json::from_str(&body)?, at.parse()?))).transpose()
    }

    /// When a loop first executed: a worker's first claim, or the creation of a round
    /// with no claimable steps. Presentation details and other loops' rounds do not
    /// affect this clock, including when an earlier round is already terminal.
    pub fn loop_first_execution_at(&self, root: &str, parent_step: &str) -> Result<Option<u128>> {
        let root = root.strip_prefix("mission-run/").unwrap_or(root);
        smallclaims::touched::note_read(|| format!("children-of-step:{parent_step}"));
        let connection = self.readers.get();
        let mut children = connection.prepare_cached(
            "SELECT id, current_generation_id, created_at_unix_ms FROM mission_runs
             WHERE root_run_id=?1 AND parent_step_run=?2",
        )?;
        let mut steps = connection.prepare_cached(
            "SELECT subject, agentless FROM step_runs WHERE run_id=?1 AND generation_id=?2",
        )?;
        let mut claims = connection.prepare_cached(
            "SELECT accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='work.claimed'",
        )?;
        let mut started: Option<u128> = None;
        let children = children.query_map(params![root, parent_step], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for child in children {
            let (id, generation, created) = child?;
            smallclaims::touched::note_read(|| format!("mission-run/{id}"));
            smallclaims::touched::note_read(|| format!("run-generation/{generation}"));
            smallclaims::touched::note_read(|| format!("generations:mission-run/{id}"));
            let steps = steps
                .query_map(params![id, generation], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (subject, _) in &steps {
                smallclaims::touched::note_read(|| subject.clone());
            }
            if steps.iter().all(|(_, agentless)| *agentless) {
                let created = created.parse::<u128>()?;
                started = Some(started.map_or(created, |at| at.min(created)));
            } else {
                for (subject, _) in steps {
                    for accepted in claims.query_map([subject], |row| row.get::<_, String>(0))? {
                        let accepted = accepted?.parse::<u128>()?;
                        started = Some(started.map_or(accepted, |at| at.min(accepted)));
                    }
                }
            }
        }
        Ok(started)
    }

    pub fn fresh_context_ready(
        &self,
        step: &StepRunView,
        actor: &str,
        incarnation: &str,
    ) -> Result<bool> {
        let connection = self.readers.get();
        fresh_context_ready_tx(&connection, step, actor, incarnation).map_err(Into::into)
    }

    pub fn apply_internal(
        &self,
        intent: &NormalizedIntent,
        key: &str,
    ) -> Result<ApplyResponse, St3Error> {
        self.apply_internal_with_declared_agents(intent, key, &BTreeSet::new())
    }

    /// Every reference in a publication that resolves neither in the publication nor here: a
    /// pinned mission revision that is not stored, or a mission, agent or observer that nothing
    /// declares. The publication routes refuse them, and their previews list them as blockers.
    pub fn unresolved_references(
        &self,
        intent: &NormalizedIntent,
    ) -> Result<Vec<String>, St3Error> {
        let connection = self.readers.get();
        crate::references::publication(
            intent,
            &ReferenceGraph {
                connection: &connection,
            },
            &self.origin,
        )
    }

    /// Every reference in the ready missions, current declarations and active runs of the graph
    /// that no longer resolves, for `st doctor`.
    pub fn unresolved_graph_references(&self) -> Result<Vec<String>, St3Error> {
        unresolved_graph_references(&self.readers.get(), &self.origin)
    }

    pub fn apply(
        &self,
        intent: &NormalizedIntent,
        expected: &BTreeMap<String, Vec<String>>,
        idempotency_key: &str,
    ) -> Result<ApplyResponse, St3Error> {
        self.apply_as(intent, expected, idempotency_key, None)
    }

    pub fn apply_as(
        &self,
        intent: &NormalizedIntent,
        expected: &BTreeMap<String, Vec<String>>,
        idempotency_key: &str,
        actor: Option<&str>,
    ) -> Result<ApplyResponse, St3Error> {
        self.apply_as_impl(
            intent,
            expected,
            idempotency_key,
            actor,
            None,
            publication::CommitChecks::default(),
        )
    }

    /// Internal commit-time guard. Runs after subject CAS checks, before publication writes,
    /// using the writer's current runtime and incarnation rather than preview evidence.
    pub fn apply_as_checked(
        &self,
        intent: &NormalizedIntent,
        expected: &BTreeMap<String, Vec<String>>,
        key: &str,
        actor: Option<&str>,
        guard: &publication::Guard,
    ) -> Result<ApplyResponse, St3Error> {
        self.apply_as_impl(
            intent,
            expected,
            key,
            actor,
            None,
            publication::CommitChecks {
                owned: None,
                guard: Some(guard),
            },
        )
    }

    /// Atomically commit the declaration and its caller-facing completed response. Exact retries
    /// also repair the old split-response window without publishing another declaration.
    pub fn apply_with_local_receipt<T: Serialize>(
        &self,
        intent: &NormalizedIntent,
        expected: &BTreeMap<String, Vec<String>>,
        key: &str,
        response_key: &str,
        response: &T,
    ) -> Result<ApplyResponse, St3Error> {
        let value = serde_json::to_value(response).map_err(internal)?;
        self.apply_as_impl(
            intent,
            expected,
            key,
            None,
            Some((response_key, &value)),
            publication::CommitChecks::default(),
        )
    }

    fn apply_as_impl(
        &self,
        intent: &NormalizedIntent,
        expected: &BTreeMap<String, Vec<String>>,
        idempotency_key: &str,
        actor: Option<&str>,
        local_receipt: Option<(&str, &Value)>,
        checks: publication::CommitChecks<'_>,
    ) -> Result<ApplyResponse, St3Error> {
        let publication::CommitChecks { owned, guard } = checks;
        self.connection
            .batched(|transaction| -> Result<ApplyResponse, St3Error> {
                message_subscriptions::validate_publisher(transaction, intent, actor)?;
                if let Some(options) = owned {
                    let digest = canonical_hash(&(intent, options, actor)).map_err(internal)?;
                    let cache_key = opaque_cache_key(&format!("owned-set-request:{idempotency_key}"));
                    let prior: Option<String> = transaction.query_row("SELECT response FROM idempotency WHERE operation_id=?1", [&cache_key], |row|row.get(0)).optional().map_err(internal)?;
                    if prior.as_deref().is_some_and(|prior|prior != digest) {
                        return Err(St3Error::new("idempotency-mismatch", "owned-set request key was used with different input"));
                    }
                    transaction.execute("INSERT OR IGNORE INTO idempotency(operation_id,response) VALUES (?1,?2)", params![cache_key,digest]).map_err(internal)?;
                }
                if let Some(response) = smallclaims::store::idempotency::cached_response(transaction, idempotency_key)?
                {
                    return cache_local_apply_receipt_tx(transaction,
                        serde_json::from_str(&response).map_err(internal)?,local_receipt,idempotency_key);
                }
                crate::provenance::validate_publication(transaction, intent)?;
                message_subscriptions::validate(transaction, intent, current_index_tx(transaction).map_err(internal)?)?;
                let owned_plan = owned.map(|options| owned_sets::plan_tx(transaction, intent, options)).transpose()?;
                let mut one_shot_sets = BTreeMap::new();
                if let (Some(plan), Some(options)) = (&owned_plan, owned) {
                    owned_sets::validate_apply(plan, options)?;
                    if plan.preview.noop {
                        return cache_local_apply_receipt_tx(transaction,ApplyResponse {
                            changed: false, store_index: current_index_tx(transaction).map_err(internal)?,
                            batch_id: None, claim_ids: Vec::new(), subject_tokens: BTreeMap::new(),
                            reconcile_subjects: Vec::new(), resolved_kdl: String::new(), operations: Vec::new(),
                        },local_receipt,idempotency_key);
                    }
                } else {
                    for subject in intent.subjects.keys().chain(intent.missions.values().map(|m| &m.subject)) {
                        if actor == Some("daemon/runtime")
                            && let Some(desired) = intent.subjects.get(subject).filter(|d| d.kind == "stop")
                            && let Some(set) = owned_sets::one_shot_retirement_owner(transaction, desired, &self.origin)?
                        {
                            one_shot_sets.insert(subject.clone(), set);
                            continue;
                        }
                        owned_sets::refuse_unmanaged(transaction, subject)?;
                    }
                }
                let intent = owned_plan.as_ref().map_or(intent, |p| &p.intent);
                let expected = owned_plan.as_ref().map_or(expected, |p| &p.preview.expected_subjects);
                for desired in intent.subjects.values() {
                    retirements::refuse_declaration(transaction, desired)?;
                    crate::terminal_binding::validate_declaration(desired, actor)?;
                    st3_schema::accountable::validate_claim(&desired.subject, "intent.desired",
                        &serde_json::to_value(desired).map_err(internal)?, actor)
                        .map_err(|e| St3Error::new(e.code, e.message))?;
                    if let Some(binding) = desired.member.as_ref().and_then(|m| m.terminal_binding.as_ref()) {
                        let terminal = current_desired_row_tx(transaction, &binding.subject).map_err(internal)?
                            .filter(|row| row.kind == "pty")
                            .and_then(|row| row.member)
                            .and_then(|member| serde_json::from_str::<crate::model::MemberSpec>(&member).ok())
                            .ok_or_else(|| St3Error::new("invalid-terminal-binding", "the terminal must already be declared"))?;
                        let member = desired.member.as_ref().expect("binding member");
                        if terminal.host != member.host || terminal.runtime_id != member.runtime_id {
                            return Err(St3Error::new("invalid-terminal-binding", "the bound seat must use its terminal's host and runtime"));
                        }
                    }
                }
                validate_documents(transaction, &intent.document_refs)?;
                for desired in intent
                    .subjects
                    .values()
                    .filter(|subject| subject.kind == "subscription")
                {
                    let Some(spec) = crate::graph::subscription_spec(&desired.desired) else {
                        continue;
                    };
                    if spec.stopped || spec.delivery != "mission" {
                        continue;
                    }
                    let (Some(mission_id), Some(revision), Some(input)) = (
                        spec.mission.as_deref(),
                        spec.revision.as_deref(),
                        spec.resource_input.as_deref(),
                    ) else {
                        continue;
                    };
                    let local = transaction
                        .query_row(
                            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                            params![mission_id, revision],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .map_err(internal)?
                        .map(|body| serde_json::from_str::<MissionSpec>(&body).map_err(internal))
                        .transpose()?;
                    let published = intent
                        .missions
                        .get(mission_id)
                        .filter(|mission| mission.revision == revision)
                        .cloned()
                        .or(local);
                    let Some(mission) = published else {
                        return Err(St3Error::new(
                            "missing-mission",
                            format!(
                                "subscription `{}` references unpublished mission/{}@{revision}",
                                desired.subject, mission_id
                            ),
                        ));
                    };
                    if mission
                        .inputs
                        .get(input)
                        .is_none_or(|declaration| declaration.kind != MissionInputKind::Resource)
                    {
                        return Err(St3Error::new(
                            "invalid-subscription-resource-input",
                            format!(
                                "subscription `{}` uses undeclared resource input `{input}` in mission/{}@{revision}",
                                desired.subject, mission_id
                            ),
                        ));
                    }
                }
                for subject in intent.subjects.keys() {
                    let actual = intent_leaves_tx(transaction, subject).map_err(internal)?;
                    let expected = expected.get(subject).ok_or_else(|| {
                        St3Error::new(
                            "missing-subject-token",
                            format!("apply omitted the subject token for `{subject}`"),
                        )
                        .with_detail("subject", subject.clone())
                        .with_detail("current_heads", json!(actual.clone()))
                    })?;
                    if actual != *expected {
                        return Err(St3Error::new(
                            "stale-subject",
                            format!("the desired state for `{subject}` changed after planning"),
                        )
                        .with_detail("subject", subject.clone())
                        .with_detail("expected_heads", json!(expected))
                        .with_detail("current_heads", json!(actual)));
                    }
                }
                for mission in intent.missions.values() {
                    let actual =
                        mission_definition_token_tx(transaction, &mission.id).map_err(internal)?;
                    let expected = expected.get(&mission.subject).ok_or_else(|| {
                        St3Error::new(
                            "missing-subject-token",
                            format!("apply omitted the subject token for `{}`", mission.subject),
                        )
                        .with_detail("subject", mission.subject.clone())
                        .with_detail("current_heads", json!(actual.clone()))
                    })?;
                    if actual != *expected {
                        return Err(St3Error::new(
                            "stale-subject",
                            format!(
                                "the published mission for `{}` changed after planning",
                                mission.subject
                            ),
                        )
                        .with_detail("subject", mission.subject.clone())
                        .with_detail("expected_heads", json!(expected))
                        .with_detail("current_heads", json!(actual)));
                    }
                }
                for declaration in intent.mission_runs.values() {
                    let actual = latest_claim_id_tx(transaction, &declaration.subject)
                        .map_err(internal)?
                        .into_iter()
                        .collect::<Vec<_>>();
                    let expected = expected.get(&declaration.subject).ok_or_else(|| {
                        St3Error::new(
                            "missing-subject-token",
                            format!(
                                "apply omitted the subject token for `{}`",
                                declaration.subject
                            ),
                        )
                    })?;
                    if actual != *expected {
                        return Err(St3Error::new(
                            "stale-subject",
                            format!(
                                "mission run `{}` changed after planning",
                                declaration.subject
                            ),
                        ));
                    }
                }
                for declaration in intent.planning_sessions.values() {
                    let actual = latest_claim_id_tx(transaction, &declaration.subject)
                        .map_err(internal)?
                        .into_iter()
                        .collect::<Vec<_>>();
                    let expected = expected.get(&declaration.subject).ok_or_else(|| {
                        St3Error::new(
                            "missing-subject-token",
                            format!(
                                "apply omitted the subject token for `{}`",
                                declaration.subject
                            ),
                        )
                    })?;
                    if actual != *expected {
                        return Err(St3Error::new(
                            "stale-subject",
                            format!("launch `{}` changed after planning", declaration.subject),
                        ));
                    }
                }
                for repair in &intent.replica_repairs {
                    let subject = format!(
                        "repair/{}",
                        repair
                            .record_ref
                            .strip_prefix("record/")
                            .unwrap_or(&repair.record_ref)
                    );
                    let actual = claim_ids_at(transaction, &subject, None).map_err(internal)?;
                    let expected = expected.get(&subject).ok_or_else(|| {
                        St3Error::new(
                            "missing-subject-token",
                            format!("apply omitted the subject token for `{subject}`"),
                        )
                    })?;
                    if &actual != expected {
                        return Err(St3Error::new(
                            "stale-subject",
                            format!("repair `{subject}` changed after planning"),
                        ));
                    }
                }
                let desired_changed = intent.subjects.iter().any(|(subject, desired)| {
                    current_desired_row_tx(transaction, subject)
                        .map(|current| {
                            current
                                .as_ref()
                                .is_none_or(|row| row.revision != desired_revision(desired))
                        })
                        .unwrap_or(true)
                });
                let missions_changed = intent.missions.values().any(|mission| {
                    transaction
                        .query_row(
                            "SELECT revision FROM mission_definitions WHERE mission_id=?1",
                            [&mission.id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .map(|current| current.as_deref() != Some(mission.revision.as_str()))
                        .unwrap_or(true)
                });
                let mut operations_changed = false;
                for declaration in intent.mission_runs.values() {
                    let run_id = declaration
                        .subject
                        .strip_prefix("mission-run/")
                        .unwrap_or(&declaration.subject);
                    if declaration.creation.is_some()
                        && transaction
                            .query_row("SELECT 1 FROM mission_runs WHERE id=?1", [run_id], |_| {
                                Ok(())
                            })
                            .optional()
                            .map_err(internal)?
                            .is_none()
                    {
                        operations_changed = true;
                    }
                    for revision in declaration.revisions.values() {
                        operations_changed |= publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "revision",
                            &revision.id,
                            revision,
                        )?;
                    }
                    for reset in declaration.resets.values() {
                        operations_changed |= publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "reset",
                            &reset.id,
                            reset,
                        )?;
                    }
                    for cancellation in declaration.cancellations.values() {
                        operations_changed |= publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "cancellation",
                            &cancellation.id,
                            cancellation,
                        )?;
                    }
                }
                for declaration in intent.planning_sessions.values() {
                    let id = declaration
                        .subject
                        .strip_prefix("planning-session/")
                        .unwrap_or(&declaration.subject);
                    if declaration.creation.is_some()
                        && transaction
                            .query_row("SELECT 1 FROM planning_sessions WHERE id=?1", [id], |_| {
                                Ok(())
                            })
                            .optional()
                            .map_err(internal)?
                            .is_none()
                    {
                        operations_changed = true;
                    }
                    for feedback in declaration.feedback.values() {
                        operations_changed |= publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "feedback",
                            &feedback.id,
                            feedback,
                        )?;
                    }
                    for cancellation in declaration.cancellations.values() {
                        operations_changed |= publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "cancellation",
                            &cancellation.id,
                            cancellation,
                        )?;
                    }
                }
                for refresh in &intent.resource_refreshes {
                    operations_changed |= publication_operation_is_new(
                        transaction,
                        &refresh.resource,
                        "refresh",
                        &refresh.id,
                        refresh,
                    )?;
                }
                for repair in &intent.replica_repairs {
                    operations_changed |= validate_replica_repair(transaction, repair)?;
                }
                if let Some(guard) = guard {
                    let at = current_index_tx(transaction).map_err(internal)?;
                    for desired in intent.subjects.values().filter(|desired| matches!(desired.kind.as_str(), "agent" | "stop")) {
                        let check = publication::check(transaction, desired, at, &self.origin).map_err(internal)?;
                        guard(desired, &check)?;
                    }
                }
                let changed = desired_changed || missions_changed || operations_changed || owned_plan.is_some();
                if !changed {
                    let store_index = current_index_tx(transaction).map_err(internal)?;
                    let mut subject_tokens = intent
                        .subjects
                        .keys()
                        .map(|subject| {
                            intent_leaves_tx(transaction, subject)
                                .map(|heads| (subject.clone(), heads))
                                .map_err(internal)
                        })
                        .collect::<Result<BTreeMap<_, _>, _>>()?;
                    for mission in intent.missions.values() {
                        subject_tokens.insert(
                            mission.subject.clone(),
                            mission_definition_token_tx(transaction, &mission.id).map_err(internal)?,
                        );
                    }
                    for declaration in intent.mission_runs.values() {
                        subject_tokens.insert(
                            declaration.subject.clone(),
                            latest_claim_id_tx(transaction, &declaration.subject)
                                .map_err(internal)?
                                .into_iter()
                                .collect(),
                        );
                    }
                    for declaration in intent.planning_sessions.values() {
                        subject_tokens.insert(
                            declaration.subject.clone(),
                            latest_claim_id_tx(transaction, &declaration.subject)
                                .map_err(internal)?
                                .into_iter()
                                .collect(),
                        );
                    }
                    for repair in &intent.replica_repairs {
                        let subject = format!(
                            "repair/{}",
                            repair
                                .record_ref
                                .strip_prefix("record/")
                                .unwrap_or(&repair.record_ref)
                        );
                        subject_tokens.insert(
                            subject.clone(),
                            claim_ids_at(transaction, &subject, None).map_err(internal)?,
                        );
                    }
                    let response = ApplyResponse {
                        changed: false,
                        store_index,
                        batch_id: None,
                        claim_ids: Vec::new(),
                        subject_tokens,
                        reconcile_subjects: Vec::new(),
                        resolved_kdl: String::new(),
                        operations: Vec::new(),
                    };
                    transaction
                        .execute(
                            "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                            params![
                                opaque_cache_key(idempotency_key),
                                serde_json::to_string(&response).map_err(internal)?
                            ],
                        )
                        .map_err(internal)?;
                    return cache_local_apply_receipt_tx(transaction,response,local_receipt,idempotency_key);
                }
                let now = write_time(transaction, &self.origin).map_err(internal)?;
                let sequence = next_replica_sequence(transaction, &self.origin).map_err(internal)?;
                let previous_hash = previous_batch_hash(transaction, &self.origin).map_err(internal)?;
                let batch_hash = batch_header_hash(&self.origin, sequence, previous_hash.as_deref(), now)
                    .map_err(internal)?;
                let batch_id = format!("batch/{}/{sequence}/{batch_hash}", self.origin);
                transaction
                    .execute(
                        "INSERT INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![batch_id, self.origin, sequence, previous_hash, batch_hash, now.to_string()],
                    )
                    .map_err(internal)?;

                let mut receipt_attached = false;
                let mut claim_ids = Vec::new();
                let mut tokens = BTreeMap::new();
                let mut reconcile_subjects = Vec::new();
                let mut operation_receipts = Vec::new();
                for (subject, desired) in &intent.subjects {
                    let revision = desired_revision(desired);
                    let current = current_desired_row_tx(transaction, subject).map_err(internal)?;
                    if current.as_ref().is_some_and(|row| row.revision == revision)
                        && !owned_plan.as_ref().is_some_and(|p| p.materialize.contains(subject)) {
                        tokens.insert(
                            subject.clone(),
                            intent_leaves_tx(transaction, subject).map_err(internal)?,
                        );
                        continue;
                    }
                    let predecessors = intent_leaves_tx(transaction, subject).map_err(internal)?;
                    let mut body = serde_json::to_value(desired).map_err(internal)?;
                    if let Some(plan) = &owned_plan { body["owned_set"] = json!(plan.preview.set); }
                    if let Some(set) = one_shot_sets.get(subject) { body["owned_set"] = json!(set); }
                    if !receipt_attached {
                        smallclaims::store::idempotency::attach(&mut body,idempotency_key).map_err(internal)?;
                        receipt_attached = true;
                    }
                    // The claim records its writer as its actor.
                    let claim_id = claim_hash(
                        &batch_id,
                        subject,
                        "intent.desired",
                        &self.origin,
                        actor,
                        &body,
                        &predecessors,
                    )
                    .map_err(internal)?;
                    let store_index = insert_claim(
                        transaction,
                        &claim_id,
                        &batch_id,
                        subject,
                        "intent.desired",
                        &self.origin,
                        actor,
                        &body,
                        &predecessors,
                        now,
                    )
                    .map_err(smallclaims::error::typed)?;
                    transaction
                        .execute(
                            "INSERT INTO desired(subject, kind, revision, claim_id, body, member, owner_run, owner_generation, owner_step) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                             ON CONFLICT(subject) DO UPDATE SET kind=excluded.kind, revision=excluded.revision, claim_id=excluded.claim_id, body=excluded.body, member=excluded.member, owner_run=excluded.owner_run, owner_generation=excluded.owner_generation, owner_step=excluded.owner_step",
                            params![
                                subject,
                                desired.kind,
                                revision,
                                claim_id,
                                canonical_json_text(&desired.desired).map_err(internal)?,
                                desired
                                    .member
                                    .as_ref()
                                    .map(canonical_serialized_json_text)
                                    .transpose()
                                    .map_err(internal)?,
                                desired.owner_run,
                                desired.owner_generation,
                                desired.owner_step,
                            ],
                        )
                        .map_err(internal)?;
                    insert_event(transaction, store_index, "intent.desired", subject, &body)
                        .map_err(internal)?;
                    claim_ids.push(claim_id.clone());
                    tokens.insert(subject.clone(), vec![claim_id]);
                    reconcile_subjects.push(subject.clone());
                }
                for mission in intent.missions.values() {
                    let current: Option<String> = transaction
                        .query_row(
                            "SELECT revision FROM mission_definitions WHERE mission_id=?1",
                            [&mission.id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(internal)?;
                    if current.as_deref() == Some(mission.revision.as_str()) {
                        tokens.insert(
                            mission.subject.clone(),
                            mission_definition_token_tx(transaction, &mission.id).map_err(internal)?,
                        );
                        continue;
                    }
                    let predecessors =
                        mission_definition_token_tx(transaction, &mission.id).map_err(internal)?;
                    let mut body = serde_json::to_value(mission).map_err(internal)?;
                    if let Some(plan) = &owned_plan { body["owned_set"] = json!(plan.preview.set); }
                    if let Some(provenance) = intent.mission_provenance.get(&mission.id)
                        && let Some(claim) = mission_provenance::record(transaction, &self.origin, mission, provenance, actor, &batch_id)? {
                        claim_ids.push(claim.id);
                    }
                    if !receipt_attached {
                        smallclaims::store::idempotency::attach(&mut body,idempotency_key).map_err(internal)?;
                        receipt_attached = true;
                    }
                    // The publication records its publisher, as a declaration records its writer.
                    let claim_id = claim_hash(
                        &batch_id,
                        &mission.subject,
                        "mission.published",
                        &self.origin,
                        actor,
                        &body,
                        &predecessors,
                    )
                    .map_err(internal)?;
                    let store_index = insert_claim(
                        transaction,
                        &claim_id,
                        &batch_id,
                        &mission.subject,
                        "mission.published",
                        &self.origin,
                        actor,
                        &body,
                        &predecessors,
                        now,
                    )
                    .map_err(smallclaims::error::typed)?;
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO mission_revisions(mission_id, revision, state, body, claim_id, created_index) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![mission.id, mission.revision, mission_state_name(&mission.state), serde_json::to_string(mission).map_err(internal)?, claim_id, store_index],
                        )
                        .map_err(internal)?;
                    transaction
                        .execute(
                            "INSERT INTO mission_definitions(mission_id, revision, state, claim_id) VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(mission_id) DO UPDATE SET revision=excluded.revision, state=excluded.state, claim_id=excluded.claim_id",
                            params![mission.id, mission.revision, mission_state_name(&mission.state), claim_id],
                        )
                        .map_err(internal)?;
                    insert_event(
                        transaction,
                        store_index,
                        "mission.published",
                        &mission.subject,
                        &body,
                    )
                    .map_err(internal)?;
                    claim_ids.push(claim_id.clone());
                    tokens.insert(mission.subject.clone(), vec![claim_id]);
                }
                for declaration in intent.mission_runs.values() {
                    let created =
                        create_declared_mission_run_tx(transaction, &self.origin, declaration, &batch_id)?;
                    if !created.is_empty() {
                        operation_receipts.push(PlannedAction {
                            subject: declaration.subject.clone(),
                            action: "start-mission-run".into(),
                            reason: "the named mission run was created".into(),
                        });
                    }
                    claim_ids.extend(created);
                    for revision in declaration.revisions.values() {
                        if !publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "revision",
                            &revision.id,
                            revision,
                        )? {
                            continue;
                        }
                        let (ids, proposed) = adopt_declared_mission_revision_tx(
                            transaction,
                            &self.origin,
                            declaration,
                            revision,
                            &batch_id,
                            actor,
                        )?;
                        claim_ids.extend(ids);
                        let receipt = append_publication_operation_tx(
                            transaction,
                            &self.origin,
                            &declaration.subject,
                            "revision",
                            &revision.id,
                            revision,
                            actor,
                            &batch_id,
                        )?;
                        claim_ids.push(receipt.id);
                        operation_receipts.push(PlannedAction {
                            subject: declaration.subject.clone(),
                            action: format!(
                                "{}:{}",
                                if proposed {
                                    "propose-revision"
                                } else {
                                    "revise"
                                },
                                revision.id
                            ),
                            reason: revision.reason.clone(),
                        });
                        if !proposed && let Some(cancellation) = &revision.cancellation {
                            let ids = cancel_mission_run_tx(
                                transaction,
                                &self.origin,
                                &declaration.subject,
                                &cancellation.reason,
                                Some(&batch_id),
                                now,
                            )
                            .map_err(internal)?;
                            claim_ids.extend(ids);
                        }
                    }
                    for reset in declaration.resets.values() {
                        if !publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "reset",
                            &reset.id,
                            reset,
                        )? {
                            continue;
                        }
                        let runtime = runtime_subject_for_run(
                            declaration
                                .subject
                                .strip_prefix("mission-run/")
                                .unwrap_or(&declaration.subject),
                            &reset.runtime,
                        );
                        let reset_claim = reset_declared_runtime_tx(
                            transaction,
                            &self.origin,
                            declaration,
                            reset,
                            &runtime,
                            &batch_id,
                        )?;
                        claim_ids.push(reset_claim.id);
                        let receipt = append_publication_operation_tx(
                            transaction,
                            &self.origin,
                            &declaration.subject,
                            "reset",
                            &reset.id,
                            reset,
                            actor,
                            &batch_id,
                        )?;
                        claim_ids.push(receipt.id);
                        reconcile_subjects.push(runtime.clone());
                        operation_receipts.push(PlannedAction {
                            subject: runtime,
                            action: format!("reset:{}", reset.id),
                            reason: reset.reason.clone(),
                        });
                    }
                    for cancellation in declaration.cancellations.values() {
                        if !publication_operation_is_new(
                            transaction,
                            &declaration.subject,
                            "cancellation",
                            &cancellation.id,
                            cancellation,
                        )? {
                            continue;
                        }
                        let ids = cancel_mission_run_tx(
                            transaction,
                            &self.origin,
                            &declaration.subject,
                            &cancellation.reason,
                            Some(&batch_id),
                            now,
                        )
                        .map_err(internal)?;
                        if !ids.is_empty() {
                            operation_receipts.push(PlannedAction {
                                subject: declaration.subject.clone(),
                                action: format!("cancel:{}", cancellation.id),
                                reason: cancellation.reason.clone(),
                            });
                        }
                        claim_ids.extend(ids);
                        let receipt = append_publication_operation_tx(
                            transaction,
                            &self.origin,
                            &declaration.subject,
                            "cancellation",
                            &cancellation.id,
                            cancellation,
                            actor,
                            &batch_id,
                        )?;
                        claim_ids.push(receipt.id);
                    }
                    tokens.insert(
                        declaration.subject.clone(),
                        latest_claim_id_tx(transaction, &declaration.subject)
                            .map_err(internal)?
                            .into_iter()
                            .collect(),
                    );
                    reconcile_subjects.push(declaration.subject.clone());
                }
                for declaration in intent.planning_sessions.values() {
                    let ids = apply_planning_session_declaration_tx(
                        transaction,
                        &self.origin,
                        declaration,
                        &batch_id,
                        actor,
                        &mut operation_receipts,
                    )?;
                    claim_ids.extend(ids);
                    tokens.insert(
                        declaration.subject.clone(),
                        latest_claim_id_tx(transaction, &declaration.subject)
                            .map_err(internal)?
                            .into_iter()
                            .collect(),
                    );
                    reconcile_subjects.push(declaration.subject.clone());
                }
                for refresh in &intent.resource_refreshes {
                    if !publication_operation_is_new(
                        transaction,
                        &refresh.resource,
                        "refresh",
                        &refresh.id,
                        refresh,
                    )? {
                        continue;
                    }
                    let ids = request_declared_resource_refresh_tx(
                        transaction,
                        &self.origin,
                        refresh,
                        &batch_id,
                    )?;
                    claim_ids.extend(ids);
                    let receipt = append_publication_operation_tx(
                        transaction,
                        &self.origin,
                        &refresh.resource,
                        "refresh",
                        &refresh.id,
                        refresh,
                        actor,
                        &batch_id,
                    )?;
                    claim_ids.push(receipt.id);
                    operation_receipts.push(PlannedAction {
                        subject: refresh.resource.clone(),
                        action: format!("refresh:{}", refresh.id),
                        reason: "the refresh request was accepted".into(),
                    });
                }
                for repair in &intent.replica_repairs {
                    if !validate_replica_repair(transaction, repair)? {
                        continue;
                    }
                    let record_id = repair
                        .record_ref
                        .strip_prefix("record/")
                        .unwrap_or(&repair.record_ref);
                    let subject = format!("repair/{record_id}");
                    let body = json!({
                        "fields": {
                            "record": repair.record_ref,
                            "replacement": repair.replacement_claim_id,
                            "reason": repair.reason,
                        }
                    });
                    let claim = append_claim_tx(
                        transaction,
                        &self.origin,
                        &subject,
                        "record.repaired",
                        actor,
                        &body,
                        &[],
                        Some(&batch_id),
                    )
                    .map_err(internal)?;
                    transaction
                        .execute(
                            "UPDATE replica_records SET state='repaired', replacement_claim_id=?2,
                                error_code=NULL, error_message=NULL, updated_at_unix_ms=?3
                             WHERE record_ref=?1 AND state IN ('invalid','unknown','repaired')",
                            params![
                                repair.record_ref,
                                repair.replacement_claim_id,
                                now_ms().to_string()
                            ],
                        )
                        .map_err(internal)?;
                    claim_ids.push(claim.id);
                    tokens.insert(
                        subject.clone(),
                        claim_ids_at(transaction, &subject, None).map_err(internal)?,
                    );
                    operation_receipts.push(PlannedAction {
                        subject,
                        action: "repair-record".into(),
                        reason: "the replacement claim resolved the invalid record".into(),
                    });
                }
                if let Some(plan) = &owned_plan {
                    let receipt = owned_sets::commit_tx(transaction, &self.origin, plan, actor, &batch_id)?;
                    tokens.insert(plan.preview.set.clone(), vec![receipt.id.clone()]);
                    claim_ids.push(receipt.id);
                }
                if receipt_attached { repair_operations_tx(transaction,&[opaque_cache_key(idempotency_key)]).map_err(internal)?; }
                owned_sets::project_tx(transaction)?;
                let store_index = current_index_tx(transaction).map_err(internal)?;
                let response = ApplyResponse {
                    changed: true,
                    store_index,
                    batch_id: Some(batch_id),
                    claim_ids,
                    subject_tokens: tokens,
                    reconcile_subjects,
                    resolved_kdl: String::new(),
                    operations: operation_receipts,
                };
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                        params![
                            opaque_cache_key(idempotency_key),
                            serde_json::to_string(&response).map_err(internal)?
                        ],
                    )
                    .map_err(internal)?;
                cache_local_apply_receipt_tx(transaction,response,local_receipt,idempotency_key)
            })
            .map_err(|error| St3Error::new("internal", error))?
    }

    pub fn append_claim(&self, input: &ClaimInput) -> Result<ClaimRecord, St3Error> {
        self.append_claim_outcome(input).map(|(claim, _)| claim)
    }

    /// Append a scheduler failure receipt only while its operation and desired revision remain current.
    pub fn append_observer_state(
        &self,
        input: &ClaimInput,
        desired_revision: &str,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<ClaimRecord, St3Error> {
        append_claim_with_subject_fences(
            &self.graph, input, None, None, None,
            Some((&input.subject, desired_revision, current)),
        ).map(|(claim, _)| claim)
    }

    pub fn append_claim_outcome(
        &self,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        self.graph.append_claim_outcome(input)
    }

    pub fn append_assistant_claim_outcome(
        &self,
        input: &ClaimInput,
        authorization: &assistant_write::AssistantWriteAuthorization,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        if input.kind != "message.sent" || authorization.scope != "write.assistant.messages"
            || authorization.recipient.as_deref() != input.fields.get("to").and_then(Value::as_str)
            || input.actor.as_deref() != Some("assistant/chatgpt")
            || input.fields.get("from").and_then(Value::as_str) != Some("assistant/chatgpt")
        { return Err(St3Error::new("forbidden", "assistant authorization does not bind this message")); }
        #[cfg(feature = "test-support")]
        let _gate = assistant_write_gate::pause(&authorization.grant_subject)?;
        append_claim_with_commit_context(&self.graph, input, None, None, None, None,
            ClaimCommitContext { assistant: Some(authorization), ..Default::default() })
    }

    pub fn append_claim_fenced_outcome(
        &self,
        input: &ClaimInput,
        fence: Option<&crate::mailbox::Fence>,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        append_claim_fenced_outcome(&self.graph, input, fence)
    }

    pub fn claims_page(
        &self,
        subject: Option<&str>,
        owner_run: Option<&str>,
        after_index: u64,
        before_index: Option<u64>,
        descending: bool,
        limit: usize,
    ) -> Result<ClaimsPage> {
        let connection = self.readers.get();
        let query = claims_page_query(subject.is_some(), descending);
        let mut statement = connection.prepare(&query)?;
        // The owner filter is evaluated per subject below. Other pages can stop
        // in SQLite at one row past the requested bound, before decoding bodies.
        let scan_limit = if owner_run.is_some() {
            i64::MAX
        } else {
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
        };
        let rows = statement.query_map(
            params![after_index, before_index, subject, scan_limit],
            claim_from_row,
        )?;
        let mut claims = Vec::new();
        for row in rows {
            let claim = row?;
            if owner_run.is_some_and(|run| !subject_owned_by(&connection, &claim.subject, run)) {
                continue;
            }
            claims.push(claim);
            if claims.len() > limit {
                break;
            }
        }
        let next_cursor = if claims.len() > limit {
            claims.pop();
            claims.last().map(|claim| claim.store_index)
        } else {
            None
        };
        Ok(ClaimsPage {
            claims,
            next_cursor,
        })
    }

    pub fn validate_claim_input(&self, input: &ClaimInput) -> Result<(), St3Error> {
        validate_claim_input(input)
    }

    pub fn append_client_claim(&self, input: &ClaimInput) -> Result<ClaimRecord, St3Error> {
        self.append_client_claim_outcome(input)
            .map(|(claim, _)| claim)
    }

    pub fn append_client_claim_outcome(
        &self,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        st3_schema::registry()
            .validate_public_claim(
                &input.subject,
                &input.kind,
                &input.fields,
                input.actor.as_deref(),
            )
            .map_err(|error| St3Error::new(error.code, error.message))?;
        self.append_claim_outcome(input)
    }

    /// Local observations written after `after`, oldest first. The records carry the
    /// `store_index` of the claim each one follows.
    pub fn local_observations_after(&self, after: u64, limit: usize) -> Result<Vec<ClaimRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE id > ?1 ORDER BY id LIMIT ?2"
        ))?;
        let rows = statement.query_map(
            params![after as i64, limit.min(i64::MAX as usize) as i64],
            |row| local_observation_from_row(&self.origin, row),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The last local observation the OpenTelemetry exporter delivered.
    pub fn otlp_export_cursor(&self) -> Result<u64> {
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT value FROM meta WHERE key='otlp_export_cursor'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    pub fn set_otlp_export_cursor(&self, cursor: u64) -> Result<()> {
        self.connection
            .batched(|transaction| {
                transaction.execute(
                    "INSERT INTO meta(key, value) VALUES ('otlp_export_cursor', ?1)
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [cursor.to_string()],
                )
            })
            .map_err(anyhow::Error::msg)??;
        Ok(())
    }

    /// The first local observation that follows the claim at `store_index`, if any. Event
    /// cursors that name only a claim resume from here.
    pub fn local_observation_floor_after_claim(&self, store_index: u64) -> Result<u64> {
        let connection = self.readers.get();
        connection
            .query_row(
                LOCAL_OBSERVATION_FLOOR_QUERY,
                [store_index],
                |row| row.get::<_, i64>(0),
            )
            .map(|id| (id.max(1) - 1) as u64)
            .map_err(Into::into)
    }

    /// The newest local observations, oldest first.
    pub fn local_observations_tail(&self, limit: usize) -> Result<Vec<ClaimRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&format!(
            "SELECT * FROM ({LOCAL_OBSERVATION_COLUMNS} ORDER BY id DESC LIMIT ?1) ORDER BY id"
        ))?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], |row| {
            local_observation_from_row(&self.origin, row)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete local observations older than `older_than_unix_ms`, and all but the newest
    /// `max_per_subject_kind` of each subject and kind. The newest observation of each
    /// subject and kind stays. Deletes run in short transactions of at most `chunk` rows, so
    /// a crash leaves a consistent table that the next pass finishes. Each chunk borrows the
    /// background writer so queued foreground requests can run between chunks.
    pub fn trim_local_observations(
        &self,
        older_than_unix_ms: u128,
        max_per_subject_kind: usize,
        chunk: usize,
    ) -> Result<usize> {
        let chunk = chunk.max(1).min(i64::MAX as usize) as i64;
        let max_per_subject_kind = max_per_subject_kind.max(1).min(i64::MAX as usize) as i64;
        let mut deleted = 0;
        // The newest observation of a subject and kind always stays. Rows past retention go
        // through the time index, and a row is "not the newest" when a later id exists for its
        // subject and kind, one seek in `local_observations_subject_kind_index`. This used to
        // rank every row of the table with a window function inside the writer's hold, every
        // chunk of every pass, even when nothing was due.
        loop {
            let connection = self.connection.write_background();
            let removed = connection.execute(
                "DELETE FROM local_observations WHERE id IN (
                    SELECT id FROM local_observations AS old
                    WHERE observed_at_unix_ms < ?1
                      AND NOT EXISTS (SELECT 1 FROM local_runtime_start_evidence WHERE observation_id=old.id)
                      AND EXISTS (SELECT 1 FROM local_observations AS newer
                                  WHERE newer.subject=old.subject AND newer.kind=old.kind
                                    AND newer.id>old.id)
                    ORDER BY observed_at_unix_ms
                    LIMIT ?2
                 )",
                params![
                    older_than_unix_ms.min(i64::MAX as u128) as i64,
                    chunk
                ],
            )?;
            drop(connection);
            deleted += removed;
            if (removed as i64) < chunk {
                break;
            }
        }
        // The cap: only a subject and kind with more rows than the cap has any. Find them on a
        // reader, then delete below the cap's id, a few rows at a time.
        let over_cap = {
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(
                "SELECT subject, kind FROM local_observations
                 GROUP BY subject, kind HAVING COUNT(*) > ?1",
            )?;
            statement
                .query_map([max_per_subject_kind], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (subject, kind) in over_cap {
            loop {
                let connection = self.connection.write_background();
                // The newest id past the cap: rank `cap + 1` from the newest.
                let boundary: Option<i64> = connection
                    .query_row(
                        "SELECT id FROM local_observations WHERE subject=?1 AND kind=?2
                         ORDER BY id DESC LIMIT 1 OFFSET ?3",
                        params![subject, kind, max_per_subject_kind],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(boundary) = boundary else {
                    break;
                };
                let removed = connection.execute(
                    "DELETE FROM local_observations WHERE id IN (
                        SELECT id FROM local_observations
                        WHERE subject=?1 AND kind=?2 AND id<=?3
                          AND NOT EXISTS (SELECT 1 FROM local_runtime_start_evidence WHERE observation_id=local_observations.id)
                        ORDER BY id LIMIT ?4
                     )",
                    params![subject, kind, boundary, chunk],
                )?;
                drop(connection);
                deleted += removed;
                if (removed as i64) < chunk {
                    break;
                }
            }
        }
        Ok(deleted)
    }

    pub fn idempotent_claim(&self, key: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT response FROM idempotency WHERE operation_id=?1",
                [opaque_cache_key(key)],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|response| serde_json::from_str(&response).map_err(anyhow::Error::from))
            .transpose()
    }

    pub fn operation_claim(&self, key: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        let operation_id = operation_id_for_key(key);
        connection
            .query_row(
                "SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind,
                        claims.origin, claims.actor, claims.body, claims.predecessors, claims.accepted_at_unix_ms
                 FROM operations JOIN claims ON claims.id=operations.canonical_claim_id
                 WHERE operations.id=?1 AND operations.state='active'",
                [operation_id],
                claim_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn apply_operational_repair(
        &self,
        token: &str,
    ) -> Result<OperationalRepairResult, St3Error> {
        if !token.starts_with("orpv0:") {
            return Err(St3Error::new(
                "invalid-repair-token",
                "an operational repair token must start with `orpv0:`",
            ));
        }
        let digest = token.trim_start_matches("orpv0:");
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(St3Error::new(
                "invalid-repair-token",
                "an operational repair token must contain one lowercase SHA-256 digest",
            ));
        }
        let repair_subject = format!("repair/{digest}");
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        if let Some(receipt) = transaction
            .query_row(
                &canonical_sql(
                    "SELECT id, body FROM claims
                 WHERE subject=?1 AND kind='repair.applied'
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                ),
                [&repair_subject],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(internal)?
        {
            let body = serde_json::from_str::<Value>(&receipt.1).map_err(internal)?;
            let fields = body.get("fields").unwrap_or(&body);
            let mut affected_subjects = fields
                .get("affected_subjects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            affected_subjects.sort();
            affected_subjects.dedup();
            transaction.commit().map_err(internal)?;
            return Ok(OperationalRepairResult {
                api_version: "st3.operational-repair.v0".into(),
                token: token.into(),
                applied: 0,
                already_applied: true,
                affected_subjects,
                claim_ids: Vec::new(),
                receipt_claim_id: Some(receipt.0),
            });
        }

        let plan = operational_repair_plan_tx(&transaction, now_ms()).map_err(internal)?;
        if plan.token != token {
            return Err(St3Error::new(
                "stale-repair-plan",
                format!(
                    "the operational repair plan changed; requested `{token}`, current `{}`",
                    plan.token
                ),
            )
            .with_detail("requested_token", token)
            .with_detail("current_token", plan.token));
        }
        if plan.items.is_empty() {
            transaction.commit().map_err(internal)?;
            return Ok(OperationalRepairResult {
                api_version: plan.api_version,
                token: token.into(),
                applied: 0,
                already_applied: false,
                affected_subjects: Vec::new(),
                claim_ids: Vec::new(),
                receipt_claim_id: None,
            });
        }

        let now = now_ms();
        let mut claim_ids = Vec::new();
        let mut affected_subjects = Vec::new();
        for item in &plan.items {
            affected_subjects.extend(item.affected_subjects.iter().cloned());
            match item.class.as_str() {
                "terminal-descendants" => {
                    let generation = item
                        .details
                        .get("generation")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            St3Error::new(
                                "invalid-repair-plan",
                                format!("repair item `{}` has no generation", item.id),
                            )
                        })?;
                    claim_ids.extend(cancel_descendant_mission_runs_tx(
                        &transaction,
                        &self.origin,
                        generation,
                        "daemon/runtime",
                        &item.reason,
                        now,
                    )?);
                }
                "orphaned-readiness" => {
                    if let Some(claim) = repair_step_state_tx(
                        &transaction,
                        &self.origin,
                        &item.subject,
                        "cancelled",
                        &item.reason,
                        now,
                    )? {
                        claim_ids.push(claim);
                    }
                }
                "expired-claim" => {
                    if let Some(claim) = repair_step_state_tx(
                        &transaction,
                        &self.origin,
                        &item.subject,
                        "ready",
                        &item.reason,
                        now,
                    )? {
                        claim_ids.push(claim);
                    }
                }
                "wake-contradiction" => {
                    let incarnation_id = item
                        .details
                        .get("incarnation_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let step_run = item
                        .details
                        .get("step_run")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let wake_attempts = item
                        .details
                        .get("wake_attempts")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    let claim = append_claim_tx(
                        &transaction,
                        &self.origin,
                        &item.subject,
                        "harness.diagnostic",
                        Some("daemon/runtime"),
                        &json!({"fields": {
                            "severity": "warning",
                            "status": "recovered",
                            "code": "work-wake-exhausted",
                            "reason": item.reason,
                            "incarnation_id": incarnation_id,
                            "step_run": step_run,
                            "wake_attempts": wake_attempts,
                        }}),
                        &[],
                        None,
                    )
                    .map_err(internal)?;
                    claim_ids.push(claim.id);
                }
                "impossible-state" => {
                    claim_ids.extend(repair_impossible_run_tx(
                        &transaction,
                        &self.origin,
                        &item.subject,
                        &item.reason,
                        now,
                    )?);
                }
                "cancelled-final-stall" => {
                    claim_ids.extend(repair_impossible_run_tx(
                        &transaction,
                        &self.origin,
                        &item.subject,
                        &item.reason,
                        now,
                    )?);
                }
                "mission-dispatch-stall" => {
                    let step_runs = item
                        .details
                        .get("step_runs")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            St3Error::new(
                                "invalid-repair-plan",
                                format!("repair item `{}` has no safe root steps", item.id),
                            )
                        })?;
                    for step_run in step_runs.iter().filter_map(Value::as_str) {
                        if let Some(claim) = repair_step_state_tx(
                            &transaction,
                            &self.origin,
                            step_run,
                            "ready",
                            &item.reason,
                            now,
                        )? {
                            claim_ids.push(claim);
                        }
                    }
                }
                class => {
                    return Err(St3Error::new(
                        "invalid-repair-plan",
                        format!("repair class `{class}` is not registered"),
                    ));
                }
            }
        }
        affected_subjects.sort();
        affected_subjects.dedup();
        let receipt = append_claim_tx(
            &transaction,
            &self.origin,
            &repair_subject,
            "repair.applied",
            Some("daemon/runtime"),
            &json!({"fields": {
                "token": token,
                "item_count": plan.items.len(),
                "affected_subjects": affected_subjects,
                "reason": "applied the exact graph-authorized operational repair plan",
            }}),
            &claim_ids,
            None,
        )
        .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(OperationalRepairResult {
            api_version: plan.api_version,
            token: token.into(),
            applied: plan.items.len(),
            already_applied: false,
            affected_subjects,
            claim_ids,
            receipt_claim_id: Some(receipt.id),
        })
    }

    pub fn status(&self, selected: Option<&str>) -> Result<StatusResponse> {
        self.status_at_view(selected, None, None, false)
    }

    pub fn status_at(
        &self,
        selected: Option<&str>,
        selected_owner_run: Option<&str>,
        at_index: Option<u64>,
    ) -> Result<StatusResponse> {
        self.status_at_view(selected, selected_owner_run, at_index, false)
    }

    pub fn status_history(
        &self,
        selected: Option<&str>,
        selected_owner_run: Option<&str>,
        at_index: Option<u64>,
    ) -> Result<StatusResponse> {
        self.status_at_view(selected, selected_owner_run, at_index, true)
    }

    /// Reduce only subjects whose IDs begin with `prefix`.
    ///
    /// Product projections use this instead of reducing every subject in the graph and filtering
    /// afterwards. The latter made small agent and host screens temporarily allocate the complete
    /// claim graph on large stores. `claims` picks what the answers carry: projections that never
    /// serialize `SubjectStatus::claims` pass [`ClaimsReduction::Summary`].
    pub fn status_for_subject_prefix_at(
        &self,
        prefix: &str,
        at_index: Option<u64>,
        include_history: bool,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        // Agent listings are expensive on large graphs. Hold this lock while building the
        // snapshot so concurrent callers share one reduction, then serve clones at the same
        // store index. A later index always rebuilds, preserving snapshot semantics.
        if prefix == "agent/" {
            // What this thread's reads can see, as every snapshot read checks: a read pinned to
            // a snapshot can see a commit a moment before the writer publishes its index.
            let current = current_index(&self.readers.get())?;
            let index = selected_index(current, at_index).map_err(anyhow::Error::new)?;
            let mut cache = self
                .smalltalk
                .agent_status_cache
                .lock()
                .expect("agent status cache poisoned");
            if let Some((_, _, _, _, status)) =
                cache.iter().find(|(cached_index, _, history, mode, _)| {
                    *cached_index == index && *history == include_history && *mode == claims
                })
            {
                let mut result = (**status).clone();
                result.store_index = index;
                return Ok(result);
            }
            let projection_index = self.agent_status_index(index)?;
            if let Some((cached_index, _, _, _, status)) = cache
                .iter_mut()
                .find(|(_, cached_projection, history, mode, _)| {
                    *cached_projection == projection_index
                        && *history == include_history
                        && *mode == claims
                })
            {
                *cached_index = index;
                let mut result = (**status).clone();
                result.store_index = index;
                return Ok(result);
            }
            let status = self.status_for_subject_prefix_uncached(
                prefix,
                Some(index),
                include_history,
                claims,
            )?;
            cache.push_back((
                index,
                projection_index,
                include_history,
                claims,
                Arc::new(status.clone()),
            ));
            if cache.len() > 8 {
                cache.pop_front();
            }
            return Ok(status);
        }
        self.status_for_subject_prefix_uncached(prefix, at_index, include_history, claims)
    }

    fn status_for_subject_prefix_uncached(
        &self,
        prefix: &str,
        at_index: Option<u64>,
        include_history: bool,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let current = current_index(&connection)?;
        let store_index = selected_index(current, at_index).map_err(anyhow::Error::new)?;
        let subjects = if prefix.starts_with("arrangement/") {
            connection.prepare_cached("SELECT subject FROM arrangements WHERE subject>=?1 AND subject<?2 ORDER BY subject")?
                .query_map(params![prefix, prefix_upper_bound(prefix).context("arrangement prefix bound")?], |row| row.get::<_,String>(0))?
                .collect::<Result<BTreeSet<_>,_>>()?
        } else { match prefix_upper_bound(prefix) {
            // One seek per subject instead of every claim of every subject in the range.
            Some(bound) => connection
                .prepare_cached(RANGE_SUBJECTS)?
                .query_map(params![store_index, prefix, bound], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<BTreeSet<_>, _>>()?,
            None => connection
                .prepare(
                    "SELECT DISTINCT subject FROM claims
                     WHERE store_index<=?1 AND subject GLOB ?2 ORDER BY subject",
                )?
                .query_map(params![store_index, format!("{prefix}*")], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<BTreeSet<_>, _>>()?,
        }};
        drop(connection);
        self.status_for_subject_names_at(subjects, store_index, include_history, claims)
    }

    /// Reduce only subjects that have emitted one claim kind at the selected snapshot.
    pub fn status_for_claim_kind_at(
        &self,
        kind: &str,
        at_index: Option<u64>,
        include_history: bool,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let current = current_index(&connection)?;
        let store_index = selected_index(current, at_index).map_err(anyhow::Error::new)?;
        let subjects = if kind == "arrangement.edited" {
            connection.prepare_cached("SELECT subject FROM arrangements ORDER BY subject")?
                .query_map([], |row| row.get::<_,String>(0))?.collect::<Result<BTreeSet<_>,_>>()?
        } else if kind == "runtime.observed" {
            // One seek per runtime instead of every runtime observation.
            connection
                .prepare_cached(RUNTIME_SUBJECTS)?
                .query_map([store_index], |row| row.get::<_, String>(0))?
                .collect::<Result<BTreeSet<_>, _>>()?
        } else {
            connection
                .prepare(
                    "SELECT DISTINCT subject FROM claims
                     WHERE kind=?1 AND store_index<=?2 ORDER BY subject",
                )?
                .query_map(params![kind, store_index], |row| row.get::<_, String>(0))?
                .collect::<Result<BTreeSet<_>, _>>()?
        };
        drop(connection);
        self.status_for_subject_names_at(subjects, store_index, include_history, claims)
    }

    /// Of `subjects`, those a current view can show at `store_index`. A runtime that nothing
    /// declares is current only while its folded status is live; the rest is history, and its
    /// reduction reads every claim it has. Each runtime's answer is kept until a claim about it
    /// arrives, so a view costs what it shows and what changed, not every runtime the store has
    /// ever held.
    /// Apply the claims that arrived through `store_index` to the subject cache, and say whether
    /// answers read at `store_index` may be kept: only those at the newest applied snapshot are
    /// known to hold afterwards.
    fn advance_subject_cache(&self, connection: &Connection, store_index: u64) -> Result<bool> {
        let mut cache = self
            .smalltalk
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if store_index > cache.through {
            let conflicts = connection
                .prepare_cached("SELECT COUNT(*) FROM operations WHERE state='conflict'")?
                .query_row([], |row| row.get::<_, u64>(0))?;
            if conflicts != cache.conflicts {
                cache.views.clear();
                cache.statuses.clear();
                cache.summary_statuses.clear();
                cache.runtime_statuses.clear();
                cache.card_statuses.clear();
                cache.conflicts = conflicts;
            } else if !cache.views.is_empty() || !cache.statuses.is_empty()
                || !cache.summary_statuses.is_empty() || !cache.runtime_statuses.is_empty()
                || !cache.card_statuses.is_empty() {
                // The subjects and actors of the claims that arrived.
                let mut statement = connection.prepare_cached(
                    "SELECT subject, actor FROM claims WHERE store_index>?1 AND store_index<=?2",
                )?;
                let mut changed = HashSet::new();
                for row in statement.query_map(params![cache.through, store_index], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })? {
                    let (subject, actor) = row?;
                    changed.insert(subject);
                    changed.extend(actor);
                }
                drop(statement);
                let stale = |subject: &String, owners: &[String]| {
                    changed.contains(subject) || owners.iter().any(|owner| changed.contains(owner))
                };
                cache
                    .views
                    .retain(|subject, entry| !stale(subject, &entry.owners));
                cache
                    .statuses
                    .retain(|subject, entry| !stale(subject, &entry.owners));
                cache.summary_statuses.retain(|subject, entry| !stale(subject, &entry.owners));
                cache.runtime_statuses.retain(|subject, entry| !stale(subject, &entry.owners));
                cache.card_statuses.retain(|subject, entry| !stale(subject, &entry.owners));
            }
            cache.through = store_index;
        }
        Ok(store_index == cache.through)
    }

    /// `subject`'s status at `store_index`, reduced again only when a claim it depends on
    /// arrived since it was last reduced. `newest` says whether to keep a new reduction.
    fn cached_subject_status(
        &self,
        connection: &Connection,
        subject: &str,
        store_index: u64,
        newest: bool,
        claims: ClaimsReduction,
    ) -> Result<(SubjectStatus, Option<PlannedAction>)> {
        if let Some(kept) = self.kept_subject_status(subject, store_index, claims) {
            return Ok(kept);
        }
        let (status, action) =
            subject_status_at(connection, subject, Some(store_index), None, claims)?
                .expect("a reduction without an owner filter always has a status");
        if newest {
            self.keep_subject_status(subject, store_index, claims, &status, &action);
        }
        Ok((status, action))
    }

    /// `subject`'s kept status reduction, if one holds at `store_index`.
    fn kept_subject_status(
        &self, subject: &str, store_index: u64, claims: ClaimsReduction,
    ) -> Option<(SubjectStatus, Option<PlannedAction>)> {
        let cache = self
            .smalltalk
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A summary and a full reduction of one subject are kept side by side; a public
        // response must never be served a summary.
        let entries = match claims {
            ClaimsReduction::Full => &cache.statuses,
            ClaimsReduction::Summary => &cache.summary_statuses,
            ClaimsReduction::Runtime => &cache.runtime_statuses,
            ClaimsReduction::AgentCard => &cache.card_statuses,
        };
        entries.get(subject)
            .filter(|entry| {
                entry.claims == claims
                    && entry.read_at <= store_index
                    && store_index <= cache.through
            })
            .map(|entry| (entry.status.clone(), entry.action.clone()))
    }

    /// Keep a reduction read at the newest applied snapshot, `store_index`.
    fn keep_subject_status(
        &self, subject: &str, store_index: u64, claims: ClaimsReduction,
        status: &SubjectStatus, action: &Option<PlannedAction>,
    ) {
        let mut cache = self
            .smalltalk
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Keep it only if no later snapshot was applied meanwhile.
        if cache.through != store_index {
            return;
        }
        let owners = status
            .owner_run
            .iter()
            .chain(status.projection.owner_generation.iter())
            .cloned()
            .collect();
        let entries = match claims {
            ClaimsReduction::Full => &mut cache.statuses,
            ClaimsReduction::Summary => &mut cache.summary_statuses,
            ClaimsReduction::Runtime => &mut cache.runtime_statuses,
            ClaimsReduction::AgentCard => &mut cache.card_statuses,
        };
        // At capacity, a cold summary remains correct and uncached. Invalidation
        // frees slots; no growing history map or full-status cache warming is needed.
        if matches!(claims, ClaimsReduction::AgentCard)
            && entries.len() >= AGENT_CARD_STATUS_LIMIT && !entries.contains_key(subject) {
            return;
        }
        entries.insert(
            subject.to_owned(),
            StatusEntry {
                read_at: store_index,
                owners,
                claims,
                status: status.clone(),
                action: action.clone(),
            },
        );
    }

    /// Of `subjects`, those a current view can show at `store_index`. A runtime that nothing
    /// declares is current only while its folded status is live, and one whose owning run or
    /// generation ended is history; the rest of history's reduction reads every claim it has.
    /// Each runtime's answer is kept until a claim it depends on arrives, so a view costs what
    /// it shows and what changed, not every runtime the store has ever held. `owners` says
    /// whether the reduction reads owners at the snapshot, as the answers here do.
    pub fn current_view_candidates(
        &self,
        connection: &Connection,
        subjects: BTreeSet<String>,
        store_index: u64,
        owners: bool,
    ) -> Result<BTreeSet<String>> {
        let mut reads = card_fold::CardReads::new(store_index);
        self.current_view_candidates_with(connection, subjects, store_index, owners, &mut reads)
    }

    /// [`Self::current_view_candidates`], reading the runtimes it has no answer for a chunk at
    /// a time into `reads`.
    fn current_view_candidates_with(
        &self,
        connection: &Connection,
        subjects: BTreeSet<String>,
        store_index: u64,
        owners: bool,
        reads: &mut card_fold::CardReads,
    ) -> Result<BTreeSet<String>> {
        let newest = self.advance_subject_cache(connection, store_index)?;
        let mut candidates = BTreeSet::new();
        let mut unknown = Vec::new();
        {
            let cache = self
                .smalltalk
                .subject_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for subject in subjects {
                if !runtime_subject(&subject) {
                    candidates.insert(subject);
                    continue;
                }
                // Without the owner checks, only an answer that used no declaration applies.
                let known = cache
                    .views
                    .get(&subject)
                    .filter(|entry| entry.head <= store_index && (owners || !entry.declared))
                    .map(|entry| entry.history);
                match known {
                    Some(false) => {
                        candidates.insert(subject);
                    }
                    Some(true) => {}
                    None => unknown.push(subject),
                }
            }
        }
        let mut entries =
            self.runtime_view_entries(connection, &unknown, store_index, owners, reads)?;
        for subject in unknown {
            let entry = entries
                .remove(&subject)
                .expect("every runtime read has a view entry");
            let history = entry.history;
            if newest && owners {
                let mut cache = self
                    .smalltalk
                    .subject_cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if cache.through == store_index {
                    cache.views.insert(subject.clone(), entry);
                }
            }
            if !history {
                candidates.insert(subject);
            }
        }
        Ok(candidates)
    }

    /// Forget every kept reduction, after claims were deleted, repaired or replayed into the
    /// graph without arriving anew.
    #[cfg(any(test, feature = "test-support"))]
    pub fn forget_current_views(&self) {
        self.smalltalk.forget_views();
    }

    /// Append a claim in a write transaction that then rolls back, as a failed write does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn roll_back_claim_for_test(&self, input: &ClaimInput) {
        let mut writer = self.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(&transaction, &self.origin, &input.subject, &input.kind,
            input.actor.as_deref(), &json!({"fields": input.fields}), &input.evidence, None)
            .unwrap();
        transaction.rollback().unwrap();
    }

    /// Drop the history an agreed checkpoint at `cut` would drop on this node.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trim_checkpoint_for_test(&self, cut: u128) -> usize {
        let plan = plan_drops(&self.checkpoint_sealed_set(cut).unwrap());
        let mut writer = self.connection.write();
        let transaction = writer.transaction().unwrap();
        use smallclaims::store::checkpoint::{checkpoint_name, delete_dropped_rows_tx, record_checkpoint_tombstones_tx};
        record_checkpoint_tombstones_tx(&transaction, &checkpoint_name(cut), &plan.envelopes, &plan.claims)
            .unwrap();
        delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims).unwrap();
        transaction.commit().unwrap();
        plan.claims.len()
    }

    pub fn status_for_subject_names_at(
        &self,
        subjects: BTreeSet<String>,
        store_index: u64,
        include_history: bool,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        self.readers.request_read(|| {
            let subjects = if include_history {
                subjects
            } else {
                self.current_view_candidates(&self.readers.get(), subjects, store_index, true)?
            };
            if subjects.len() <= 64 {
                return self.status_at_view_for_names(
                    None, None, Some(store_index), include_history, Some(subjects), claims,
                );
            }
            // The caller keeps its loan and reduces the LAST slice on it, so a full
            // four-way split uses three extra permits and no extra connection for the
            // parent. Other slices must never wait for admission: when a permit is
            // unavailable they also reduce inline on that same loan. Slices stay in
            // subject order regardless of which ones execute in parallel.
            enum Part<'scope> {
                Worker(std::thread::ScopedJoinHandle<'scope, Result<StatusResponse>>),
                Inline(Result<StatusResponse>),
            }
            let subjects = subjects.into_iter().collect::<Vec<_>>();
            let chunk_size = subjects.len().div_ceil(STATUS_WORKERS);
            let profile = crate::profile::current();
            let read_budget = smallclaims::read_budget::current();
            let parts = std::thread::scope(|scope| {
                subjects
                    .chunks(chunk_size)
                    .enumerate()
                    .map(|(position, chunk)| {
                        let names = chunk.iter().cloned().collect::<BTreeSet<_>>();
                        let read = move || self.status_at_view_for_names(
                            None, None, Some(store_index), include_history, Some(names), claims,
                        );
                        let last = position + 1 == subjects.len().div_ceil(chunk_size);
                        if !last && let Some(permit) = self.readers.try_admit_read() {
                            let profile = profile.clone();
                            let read_budget = read_budget.clone();
                            Part::Worker(scope.spawn(move || {
                                let _entered = crate::profile::enter(profile.as_ref());
                                smallclaims::read_budget::with(read_budget, || {
                                    self.readers.request_read_with_permit(permit, read)?
                                })
                            }))
                        } else {
                            Part::Inline(read())
                        }
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|part| match part {
                        Part::Worker(worker) => worker.join().expect("status projection worker panicked"),
                        Part::Inline(result) => result,
                    })
                    .collect::<Vec<_>>()
            });
            let mut merged = StatusResponse {
                store_index,
                subjects: Vec::new(),
                pending_actions: Vec::new(),
            };
            for part in parts {
                let part = part?;
                merged.subjects.extend(part.subjects);
                merged.pending_actions.extend(part.pending_actions);
            }
            Ok(merged)
        })?
    }

    fn status_at_view(
        &self,
        selected: Option<&str>,
        selected_owner_run: Option<&str>,
        at_index: Option<u64>,
        include_history: bool,
    ) -> Result<StatusResponse> {
        self.status_at_view_for_names(
            selected,
            selected_owner_run,
            at_index,
            include_history,
            None,
            // The response a public status endpoint serializes carries the full claims vector.
            ClaimsReduction::Full,
        )
    }

    fn status_at_view_for_names(
        &self,
        selected: Option<&str>,
        selected_owner_run: Option<&str>,
        at_index: Option<u64>,
        include_history: bool,
        selected_names: Option<BTreeSet<String>>,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        // A request reader loan alone can see a commit between statements. Keep the
        // selected declaration, runtime observation and conflict leaves in one snapshot.
        self.read_snapshot(|_| {
            self.status_at_view_for_names_in_snapshot(
                selected,
                selected_owner_run,
                at_index,
                include_history,
                selected_names,
                claims,
            )
        })
    }

    fn status_at_view_for_names_in_snapshot(
        &self,
        selected: Option<&str>,
        selected_owner_run: Option<&str>,
        at_index: Option<u64>,
        include_history: bool,
        selected_names: Option<BTreeSet<String>>,
        claims: ClaimsReduction,
    ) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let current = current_index(&connection)?;
        let store_index = selected_index(current, at_index).map_err(anyhow::Error::new)?;
        let bounded_names = selected_names.is_some();
        let mut subject_names = selected_names.unwrap_or_default();
        if let Some(selected) = selected {
            subject_names.insert(selected.to_owned());
        } else if !bounded_names {
            let mut statement = connection.prepare(
                "SELECT DISTINCT subject FROM claims WHERE store_index<=?1 ORDER BY subject",
            )?;
            let rows = statement.query_map([store_index], |row| row.get::<_, String>(0))?;
            subject_names.extend(rows.collect::<Result<Vec<_>, _>>()?);
            drop(statement);
            if !include_history {
                // Without a snapshot the reduction reads owners from the run tables, so only
                // what a runtime's own claims decide leaves it out.
                subject_names = self.current_view_candidates(
                    &connection,
                    subject_names,
                    store_index,
                    at_index.is_some(),
                )?;
            }
        }
        // A snapshot reduction is kept per subject until a claim it depends on arrives.
        let newest = match at_index {
            Some(_) if selected_owner_run.is_none() => {
                Some(self.advance_subject_cache(&connection, store_index)?)
            }
            _ => None,
        };
        let mut subjects = Vec::new();
        let mut pending_actions = Vec::new();
        for subject in subject_names {
            if subject.starts_with("glass/") || subject.starts_with("arrangement/") {
                continue;
            }
            let reduced = match newest {
                Some(newest) => Some(self.cached_subject_status(
                    &connection,
                    &subject,
                    store_index,
                    newest,
                    claims,
                )?),
                None => subject_status_at(
                    &connection,
                    &subject,
                    at_index,
                    selected_owner_run,
                    claims,
                )?,
            };
            let Some((status, action)) = reduced else {
                continue;
            };
            if selected.is_none() && !include_history && status.projection.layer != "current" {
                continue;
            }
            pending_actions.extend(action);
            subjects.push(status);
        }
        Ok(StatusResponse {
            store_index,
            subjects,
            pending_actions,
        })
    }

    /// One keyset page at the caller's pinned snapshot. Owner filtering never expands the
    /// indexed scan; the continuation records the last inspected event even for an empty answer.
    /// Both subject and owner filters run after the bounded global membership scan.
    pub fn events_page(
        &self,
        after: u64,
        subject: Option<&str>,
        owner_run: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<EventRecord>, Option<u64>, bool)> {
        let limit = limit.clamp(1, 200);
        let connection = self.readers.get();
        // Keep the global last-scanned-event contract without adding a subject index.
        let sql = "SELECT store_index,kind,subject,body FROM events
             WHERE store_index>?1 ORDER BY store_index LIMIT ?2";
        let raw = connection
            .prepare_cached(sql)?
            .query_map(params![after, limit.saturating_add(1)], |row| {
                let body = row.get::<_, String>(3)?;
                Ok(EventRecord {
                    store_index: row.get(0)?,
                    kind: row.get(1)?,
                    subject: row.get(2)?,
                    body: serde_json::from_str(&body).unwrap_or(Value::Null),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = raw.len() > limit;
        let next_after = raw
            .iter()
            .take(limit)
            .next_back()
            .map(|event| event.store_index);
        let items = raw
            .into_iter()
            .take(limit)
            .filter(|event| {
                subject.is_none_or(|subject| event.subject == subject)
                    && owner_run
                        .is_none_or(|run| subject_owned_by(&connection, &event.subject, run))
            })
            .collect();
        Ok((items, next_after, has_more))
    }

    pub fn event_payload_migration_pending(&self) -> Result<bool> {
        self.readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_event_payloads')",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn events_after(&self, after: u64, subject: Option<&str>) -> Result<Vec<EventRecord>> {
        self.events_after_filtered(after, subject, None)
    }

    pub fn events_after_bounded(&self, after: u64, limit: usize) -> Result<Vec<EventRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT store_index, kind, subject, body FROM events
             WHERE store_index > ?1 ORDER BY store_index LIMIT ?2",
        )?;
        let rows =
            statement.query_map(params![after, limit.min(i64::MAX as usize) as i64], |row| {
                let body = row.get::<_, String>(3)?;
                Ok(EventRecord {
                    store_index: row.get(0)?,
                    kind: row.get(1)?,
                    subject: row.get(2)?,
                    body: serde_json::from_str(&body).unwrap_or(Value::Null),
                })
            })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn events_tail_bounded(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let connection = self.readers.get();
        // Bound membership before fetching bodies. Ordering the joined view can make SQLite
        // visit every canonical claim, even when the response has a LIMIT.
        let sql = event_tail_sql(self.event_payload_migration_pending()?);
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], |row| {
            let body = row.get::<_, String>(3)?;
            Ok(EventRecord {
                store_index: row.get(0)?,
                kind: row.get(1)?,
                subject: row.get(2)?,
                body: serde_json::from_str(&body).unwrap_or(Value::Null),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn event_bounds(&self) -> Result<(u64, u64)> {
        smallclaims::store::events::bounds(&self.readers.get())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn prune_events_before(&self, retain_from: u64) -> Result<usize> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        smallclaims::store::events::raise_resume_floor_tx(
            &transaction,
            retain_from.saturating_sub(1),
        )?;
        let removed = transaction.execute(
            "DELETE FROM event_positions WHERE store_index < ?1",
            [retain_from],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Return the acceptance time that deterministically names a projection at `store_index`.
    /// Empty stores use Unix epoch; a projection never incorporates request wall-clock time.
    pub fn projection_time_at(&self, store_index: u64) -> Result<u128> {
        if store_index == 0 {
            return Ok(0);
        }
        let connection = self.readers.get();
        let value = connection
            .query_row(
                "SELECT accepted_at_unix_ms FROM claims WHERE store_index <= ?1 ORDER BY store_index DESC LIMIT 1",
                [store_index],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or_default();
        Ok(value)
    }

    /// `projection_time_at` for several cuts in one statement: each distinct cut is one indexed
    /// seek of `claims` by `store_index`, so the statement count does not grow with the cuts.
    pub fn projection_times_at(&self, store_indexes: &[u64]) -> Result<HashMap<u64, u128>> {
        let mut times = HashMap::with_capacity(store_indexes.len());
        let mut cuts = store_indexes
            .iter()
            .copied()
            .filter(|store_index| *store_index != 0)
            .collect::<Vec<_>>();
        cuts.sort_unstable();
        cuts.dedup();
        if let Some(zero) = store_indexes.iter().find(|store_index| **store_index == 0) {
            times.insert(*zero, 0);
        }
        if cuts.is_empty() {
            return Ok(times);
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT cut.value,
                    (SELECT accepted_at_unix_ms FROM claims
                     WHERE store_index <= cut.value ORDER BY store_index DESC LIMIT 1)
             FROM json_each(?1) AS cut",
        )?;
        let rows = statement.query_map([serde_json::to_string(&cuts)?], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (cut, value) = row?;
            times.insert(
                cut,
                value
                    .and_then(|value| value.parse::<u128>().ok())
                    .unwrap_or_default(),
            );
        }
        Ok(times)
    }

    pub fn events_after_filtered(
        &self,
        after: u64,
        subject: Option<&str>,
        owner_run: Option<&str>,
    ) -> Result<Vec<EventRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT store_index, kind, subject, body FROM events
             WHERE store_index > ?1 AND (?2 IS NULL OR subject=?2) ORDER BY store_index",
        )?;
        let rows = statement.query_map(params![after, subject], |row| {
            let body = row.get::<_, String>(3)?;
            Ok(EventRecord {
                store_index: row.get(0)?,
                kind: row.get(1)?,
                subject: row.get(2)?,
                body: serde_json::from_str(&body).unwrap_or(Value::Null),
            })
        })?;
        rows.filter_map(|row| match row {
            Ok(event)
                if owner_run
                    .is_some_and(|run| !subject_owned_by(&connection, &event.subject, run)) =>
            {
                None
            }
            row => Some(row),
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
    }

    pub fn desired_subjects(&self) -> Result<Vec<DesiredSubject>> {
        smallclaims::touched::note_read(|| "kind:intent.desired".to_owned());
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step FROM desired ORDER BY subject",
        )?;
        let rows = statement.query_map([], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Compare shared claims by canonical order, independent of replica arrival.
    pub fn claim_is_after(&self, left: &str, right: &str) -> Result<bool> {
        let connection = self.readers.get();
        Ok(canonical::claim_key(&connection, left)? > canonical::claim_key(&connection, right)?)
    }

    /// The subjects among `subjects` that a person or an agent stopped while `generation` of
    /// `run` declared them. Such a stop holds for the rest of that generation, across daemon
    /// restarts, until someone starts the seat again; the run does not declare it again.
    pub fn stopped_by_hand(
        &self,
        subjects: &[String],
        run: &str,
        generation: &str,
    ) -> Result<BTreeSet<String>> {
        let mut stopped = BTreeSet::new();
        for desired in self.desired_subjects_named(subjects)? {
            // A run's own stop and a generation's retirement record their owner; only a stop
            // published at the root, as `st agents stop` does, is someone's decision.
            if desired.kind != "stop"
                || desired.owner_run.is_some()
                || desired.owner_generation.is_some()
            {
                continue;
            }
            let Some(ended) = self.declaration_ended_by_stop(&desired.subject)? else {
                continue;
            };
            if ended.writer.is_some()
                && ended.declaration.owner_run.as_deref() == Some(run)
                && ended.declaration.owner_generation.as_deref() == Some(generation)
            {
                stopped.insert(desired.subject);
            }
        }
        Ok(stopped)
    }

    /// The current desired declaration of `subject` and the actor its claim records. A
    /// declaration from before claims recorded their writer, or the daemon's own, has none.
    pub fn desired_subject_with_writer(
        &self,
        subject: &str,
    ) -> Result<Option<(DesiredSubject, Option<String>)>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT desired.subject, desired.kind, desired.body, desired.member,
                        desired.owner_run, desired.owner_generation, desired.owner_step,
                        claims.actor
                 FROM desired LEFT JOIN claims ON claims.id = desired.claim_id
                 WHERE desired.subject = ?1",
                [subject],
                |row| Ok((desired_from_row(row)?, row.get::<_, Option<String>>(7)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Among `subjects`, each declaration whose member this build cannot read, with the reason.
    /// `desired_subjects` gives such a declaration no member at all.
    pub fn unreadable_members(&self, subjects: &[&str]) -> Result<Vec<(String, String)>> {
        if subjects.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT subject, member FROM desired
             WHERE member IS NOT NULL AND subject IN (SELECT value FROM json_each(?1))
             ORDER BY subject",
        )?;
        let rows = statement.query_map([serde_json::to_string(subjects)?], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut unreadable = Vec::new();
        for row in rows {
            let (subject, member) = row?;
            if let Err(error) = serde_json::from_str::<crate::model::MemberSpec>(&member) {
                unreadable.push((subject, error.to_string()));
            }
        }
        Ok(unreadable)
    }

    /// The desired subjects named in `subjects`, by their primary keys.
    pub fn desired_subjects_named(&self, subjects: &[String]) -> Result<Vec<DesiredSubject>> {
        for subject in subjects {
            smallclaims::touched::note_read(|| subject.clone());
        }
        if subjects.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE subject IN (SELECT value FROM json_each(?1)) ORDER BY subject",
        )?;
        let rows = statement.query_map([serde_json::to_string(subjects)?], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn unread_mail_count_before(&self, before_unix_ms: u128) -> Result<u64> {
        smallclaims::touched::note_read(|| "kind:message.sent".to_owned());
        if self.readers.has_pinned_reader() {
            return unread_mail::count_in_snapshot(&self.readers.get(), before_unix_ms);
        }
        let mut connection = self.connection.write();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count = unread_mail::count_before(&transaction, before_unix_ms)?;
        transaction.commit()?;
        Ok(count)
    }

    pub fn held_mail_count_before(&self, before: u128) -> Result<u64> {
        Ok(self.readers.get().query_row(
            "SELECT COUNT(*) FROM coordination_sends INDEXED BY coordination_sends_held WHERE held=1 AND sent_ms<?1",
            [i64::try_from(before).unwrap_or(i64::MAX)], |row| row.get(0))?)
    }

    /// Each seat's unread held mail sent before the cutoff: its count and oldest send time.
    /// A read: it never folds the unread queue, and reads the queued subjects as they stand.
    pub fn held_mail_before(&self, before_unix_ms: u128) -> Result<Vec<(String, u64, u128)>> {
        let mut seats = BTreeMap::<String, (u64, u128)>::new();
        for sent in unread_mail::sent_before(&self.readers.get(), before_unix_ms)? {
            if sent
                .tags
                .iter()
                .any(|tag| crate::silent::is_silent_tag(tag))
                && !crate::silent::always_wakes(&sent.from, &sent.tags)
            {
                let seat = seats.entry(sent.to).or_insert((0, sent.sent_unix_ms));
                seat.0 += 1;
                seat.1 = seat.1.min(sent.sent_unix_ms);
            }
        }
        Ok(seats
            .into_iter()
            .map(|(seat, (count, oldest))| (seat, count, oldest))
            .collect())
    }

    /// Latest member-pass fault, independently of runtime observations and unrelated diagnostics.
    pub fn member_reconcile_fault(
        &self,
        subject: &str,
        at_index: Option<u64>,
    ) -> Result<Option<String>> {
        let connection = self.readers.get();
        let body: Option<String> = connection
            .query_row(
                "SELECT body FROM claims WHERE subject=?1 AND kind='runtime.reconcile-decision'
             AND store_index<=?2 AND json_extract(body, '$.fields.key')='member-reconcile'
             ORDER BY store_index DESC LIMIT 1",
                params![subject, at_index.unwrap_or(i64::MAX as u64)],
                |row| row.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(None);
        };
        let body: Value = serde_json::from_str(&body)?;
        let fields = body.get("fields").unwrap_or(&body);
        Ok(
            (fields.get("decision").and_then(Value::as_str) == Some("member-fault")).then(|| {
                fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("member reconciliation failed")
                    .to_owned()
            }),
        )
    }

    /// The member fault of each of `subjects` that has one at `at_index`, as
    /// `member_reconcile_fault` reads it: one seek per subject rather than a scan of every
    /// reconcile decision the store holds.
    pub fn member_reconcile_faults_for(
        &self,
        subjects: &[String],
        at_index: u64,
    ) -> Result<BTreeMap<String, String>> {
        let connection = self.readers.get();
        // Each subject's newest member reconcile decision, for 256 subjects per statement.
        let mut statement = connection.prepare_cached(
            "SELECT subjects.value,
                    (SELECT body FROM claims WHERE subject=subjects.value
                       AND kind='runtime.reconcile-decision' AND store_index<=?2
                       AND json_extract(body, '$.fields.key')='member-reconcile'
                     ORDER BY store_index DESC LIMIT 1)
             FROM json_each(?1) subjects",
        )?;
        let mut decisions = Vec::new();
        for chunk in subjects.chunks(256) {
            for row in statement.query_map(params![serde_json::to_string(chunk)?, at_index], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })? {
                let (subject, body) = row?;
                decisions.extend(body.map(|body| (subject, body)));
            }
        }
        let mut faults = BTreeMap::new();
        for (subject, body) in decisions {
            let body: Value = serde_json::from_str(&body)?;
            let fields = body.get("fields").unwrap_or(&body);
            if fields.get("decision").and_then(Value::as_str) == Some("member-fault") {
                faults.insert(
                    subject,
                    fields
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("member reconciliation failed")
                        .to_owned(),
                );
            }
        }
        Ok(faults)
    }

    /// Every claim and local observation of `kind` for `subject`, in log order.
    pub fn observations_for(&self, subject: &str, kind: &str) -> Result<Vec<ClaimRecord>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let mut records = self.claims_for(subject, Some(kind))?;
        let connection = self.readers.get();
        let mut statement = connection.prepare(&format!(
            "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND kind=?2 ORDER BY id"
        ))?;
        let rows = statement.query_map(params![subject, kind], |row| {
            local_observation_from_row(&self.origin, row)
        })?;
        records.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        records.sort_by_key(claim_log_order);
        Ok(records)
    }

    /// The newest claim or local observation of `kind` for `subject`.
    pub fn latest_observation(&self, subject: &str, kind: &str) -> Result<Option<ClaimRecord>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let claim = self.latest_claim(subject, Some(kind))?;
        let local = {
            let connection = self.readers.get();
            connection
                .query_row(
                    &format!(
                        "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND kind=?2 ORDER BY id DESC LIMIT 1"
                    ),
                    params![subject, kind],
                    |row| local_observation_from_row(&self.origin, row),
                )
                .optional()?
        };
        Ok(claim.into_iter().chain(local).max_by_key(claim_log_order))
    }

    /// The current faults raised for `person` under the reviewer each one names, owners aside.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fault_items(&self, person: Option<&str>) -> Result<Vec<AttentionItemView>> {
        Ok(self
            .fault_snapshot(now_ms())?
            .into_iter()
            .map(|fault| fault.item)
            .filter(|item| person.is_none_or(|person| item.person == person))
            .collect())
    }

    /// Every current fault, each with the agent that owns it: the agent assigned to the failed
    /// step, else the run's requester when that is an agent, else the fleet's fault agent. No
    /// fault is a person's: the owning agent retries, revises or cancels, and asks a person
    /// only for what only a person can give.
    pub fn fault_snapshot(&self, as_of: u128) -> Result<Vec<FaultView>> {
        let mut items = self.subscription_fault_items(as_of)?;
        let (operational, failures) = self.operational_attention_items_with_failures(None, as_of)?;
        items.extend(operational);
        items.extend(
            self.broken_gate_items_from_failures(None, as_of, failures)?
                .into_iter()
                .filter(|item| !item.person.starts_with("person/")),
        );
        items.extend(self.checkpoint_attention_items(None, as_of)?);
        let fallback = self.fleet_fault_agent()?;
        self.current_attention(items, as_of)?
            .into_iter()
            .map(|item| {
                Ok(FaultView {
                    owner: self.fault_owner(&item, fallback.as_deref())?,
                    item,
                })
            })
            .collect()
    }

    pub fn selected_desired_token(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        current_desired_row(&connection, subject).map(|row| row.map(|row| row.claim_id))
    }

    /// The selected desired revision of `subject`, then each predecessor that differs from it
    /// only in presentation. Launch records, restart budgets and crash-loop parking follow
    /// labels; environment edits still get a fresh launch token and restart budget.
    pub fn launch_lineage(&self, subject: &str) -> Result<Vec<String>> {
        let connection = self.readers.get();
        launch_lineage_tx(&connection, subject)
    }

    /// Only seats whose durable resume requests name this host need transfer reconciliation.
    pub fn cross_host_resume_targets(&self, host: &str) -> Result<BTreeSet<String>> {
        smallclaims::touched::note_read(|| "kind:runtime.action.requested".to_owned());
        let connection = self.readers.get();
        let mut query = connection.prepare_cached(
            "SELECT DISTINCT subject FROM claims INDEXED BY claims_resume_host_index WHERE kind='runtime.action.requested'
             AND json_extract(body,'$.fields.action')='resume' AND json_extract(body,'$.fields.host')=?1")?;
        let rows = query.query_map([host], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<BTreeSet<_>>>()?)
    }

    /// `selected_desired_token` of each of `subjects` that has a declaration, in one statement.
    pub fn selected_desired_tokens(&self, subjects: &[&str]) -> Result<BTreeMap<String, String>> {
        let connection = self.readers.get();
        connection
            .prepare_cached(
                "SELECT subject, claim_id FROM desired
                 WHERE subject IN (SELECT value FROM json_each(?1))",
            )?
            .query_map([serde_json::to_string(subjects)?], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(Into::into)
    }

    pub fn selected_desired_revision(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        current_desired_row(&connection, subject).map(|row| row.map(|row| row.revision))
    }

    pub fn selected_desired_kind(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        current_desired_row(&connection, subject).map(|row| row.map(|row| row.kind))
    }

    pub fn selected_desired_origin(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT claims.origin FROM desired
                 JOIN claims ON claims.id=desired.claim_id
                 WHERE desired.subject=?1",
                [subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// The origin used to attribute a runtime to its owner. This deliberately follows
    /// `selected_actual_source_at`'s preference for a runtime observation over later
    /// non-runtime claims, without reducing the subject's entire history.
    pub fn selected_actual_origin(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        // Walk the accepted-time index newest first: sorting every runtime observation of the
        // subject cost each settled stop, checked on every pass, a tenth of a millisecond.
        let runtime_origin = connection
            .prepare_cached(&format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.origin", "runtime.observed")
            ))?
            .query_row(params![subject, i64::MAX as u64], |row| row.get(0))
            .optional()?;
        if runtime_origin.is_some() {
            return Ok(runtime_origin);
        }
        connection
            .query_row(
                &canonical_sql(&format!(
                    "SELECT origin FROM claims
                 WHERE subject=?1 AND kind!='intent.desired'
                   AND kind NOT LIKE 'harness.%'
                   AND kind!='runtime.readiness-deadline-reached'
                   AND kind!='reconcile.fault' AND {UNREPAIRED_CLAIM}
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                )),
                [subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Unstarted ref deliveries collapse to the latest observed head. Running missions
    /// retain their pinned discovery and exact CI gate, even when the ref moves again.
    pub fn stale_ref_request(&self, resource: &str, discovery: &str) -> Result<Option<String>> {
        stale_ref_request_tx(&self.readers.get(), resource, discovery)
    }

    // Expose the bare primary key for a seek, retaining the original equality as the
    // prefix/type/collation filter. Byte slicing preserves embedded NULs; the second
    // key also preserves SQLite blob-valued IDs without scanning other runs. The
    // prefix byte length follows the database encoding.
    const SCHEDULE_ACTIVE_STARTED_RUN_QUERY: &str = "SELECT EXISTS(
        SELECT 1 FROM claims AS started
        JOIN mission_runs AS run
          ON run.id IN (
               CAST(substr(CAST(json_extract(started.body, '$.fields.mission_run') AS BLOB), length(CAST('mission-run/' AS BLOB))+1) AS TEXT),
               substr(CAST(json_extract(started.body, '$.fields.mission_run') AS BLOB), length(CAST('mission-run/' AS BLOB))+1)
             )
         AND json_extract(started.body, '$.fields.mission_run')='mission-run/' || run.id
        WHERE started.subject=?1 AND started.kind='schedule.work-started'
          AND run.status NOT IN ('completed','cancelled','failed')
    )";

    /// Bounded claim history for one subject/kind projection, backed by the composite index.
    pub fn claims_for_subject_kind_at(
        &self,
        subject: &str,
        kind: &str,
        before_index: Option<u64>,
        descending: bool,
        limit: usize,
    ) -> Result<ClaimsPage> {
        let connection = self.readers.get();
        let order = if descending { "DESC" } else { "ASC" };
        let query = format!(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind=?2 AND (?3 IS NULL OR store_index<?3)
             ORDER BY store_index {order} LIMIT ?4"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map(
            params![subject, kind, before_index, limit.saturating_add(1) as u64],
            claim_from_row,
        )?;
        let mut claims = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if claims.len() > limit {
            claims.pop();
            claims.last().map(|claim| claim.store_index)
        } else {
            None
        };
        Ok(ClaimsPage {
            claims,
            next_cursor,
        })
    }

    /// Bounded timeline history for exactly one runtime incarnation. The
    /// expression index keeps old incarnations from consuming the page bound.
    /// The page merges replicated timeline claims, which older builds wrote, with
    /// this node's local timeline observations; a local observation sorts after the
    /// claim it follows.
    /// `next_cursor` means the query omitted existing rows, never physical retention.
    /// It is not a lossless local-observation cursor: several local IDs may follow
    /// the same store index.
    pub fn timeline_claims_for_incarnation_at(
        &self,
        subject: &str,
        incarnation: &str,
        before_index: Option<u64>,
        descending: bool,
        limit: usize,
    ) -> Result<ClaimsPage> {
        let claims = self.timeline_claim_rows_for_incarnation_at(
            subject,
            incarnation,
            before_index,
            descending,
            limit,
        )?;
        let local = {
            let connection = self.readers.get();
            let order = if descending { "DESC" } else { "ASC" };
            let mut statement = connection.prepare(&format!(
                "{LOCAL_OBSERVATION_COLUMNS}
                 WHERE subject=?1 AND kind='harness.timeline'
                   AND json_extract(body, '$.fields.incarnation_id')=?2
                   AND (?3 IS NULL OR after_store_index<?3)
                 ORDER BY id {order} LIMIT ?4"
            ))?;
            let rows = statement.query_map(
                params![
                    subject,
                    incarnation,
                    before_index,
                    limit.saturating_add(1) as u64
                ],
                |row| local_observation_from_row(&self.origin, row),
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut merged = claims.claims;
        merged.extend(local);
        merged.sort_by_key(claim_log_order);
        if descending {
            merged.reverse();
        }
        let more = claims.next_cursor.is_some() || merged.len() > limit;
        merged.truncate(limit);
        let next_cursor = more
            .then(|| merged.last().map(|claim| claim.store_index))
            .flatten();
        Ok(ClaimsPage {
            claims: merged,
            next_cursor,
        })
    }
    /// Resolve retained appends in one incarnation scan, without loading their bodies.
    /// Full store/local positions distinguish query omission from a missing append.
    pub fn timeline_entry_append_positions_at(
        &self,
        subject: &str,
        incarnation: &str,
        entries: &BTreeSet<String>,
        before_index: Option<u64>,
    ) -> Result<BTreeMap<String, (u64, u64)>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "WITH operations AS (
                SELECT json_extract(body, '$.fields.entry_id') AS entry_id,
                    store_index, 0 AS local_id
                FROM claims
                WHERE subject=?1 AND kind='harness.timeline'
                    AND json_extract(body, '$.fields.incarnation_id')=?2
                    AND json_extract(body, '$.fields.operation')='append'
                    AND json_extract(body, '$.fields.entry_id') IN (SELECT value FROM json_each(?3))
                    AND (?4 IS NULL OR store_index<?4)
                UNION ALL
                SELECT json_extract(body, '$.fields.entry_id') AS entry_id,
                    after_store_index AS store_index, id AS local_id
                FROM local_observations
                WHERE subject=?1 AND kind='harness.timeline'
                    AND json_extract(body, '$.fields.incarnation_id')=?2
                    AND json_extract(body, '$.fields.operation')='append'
                    AND json_extract(body, '$.fields.entry_id') IN (SELECT value FROM json_each(?3))
                    AND (?4 IS NULL OR after_store_index<?4)
            )
            SELECT entry_id, store_index, local_id FROM (
                SELECT *, ROW_NUMBER() OVER (
                    PARTITION BY entry_id ORDER BY store_index, local_id
                ) AS position FROM operations
            ) WHERE position=1",
        )?;
        let rows = statement.query_map(
            params![subject, incarnation, serde_json::to_string(entries)?, before_index],
            |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))),
        )?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Resolve physical prefix coverage independently of the bounded timeline window.
    /// Read only typed truncation bounds, never materialize the omitted transcript bodies.
    pub fn timeline_retention_is_explicit_at(
        &self,
        subject: &str,
        incarnation: &str,
        before_index: Option<u64>,
        required_through: u64,
    ) -> Result<bool> {
        if required_through == 0 {
            return Ok(true);
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT body -> '$.fields.body.omitted_from_sequence',
                    body -> '$.fields.body.omitted_to_sequence'
             FROM claims
             WHERE subject=?1 AND kind='harness.timeline'
                 AND json_extract(body, '$.fields.incarnation_id')=?2
                 AND json_extract(body, '$.fields.operation')='append'
                 AND json_extract(body, '$.fields.entry_type')='truncation'
                 AND (?3 IS NULL OR store_index<?3)
             UNION ALL
             SELECT body -> '$.fields.body.omitted_from_sequence',
                    body -> '$.fields.body.omitted_to_sequence'
             FROM local_observations
             WHERE subject=?1 AND kind='harness.timeline'
                 AND json_extract(body, '$.fields.incarnation_id')=?2
                 AND json_extract(body, '$.fields.operation')='append'
                 AND json_extract(body, '$.fields.entry_type')='truncation'
                 AND (?3 IS NULL OR after_store_index<?3)",
        )?;
        let rows = statement.query_map(params![subject, incarnation, before_index], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut intervals = Vec::new();
        for row in rows {
            let (from, to) = row?;
            // Raw JSON scalars preserve unsigned bounds and reject strings, floats and booleans.
            if let Some((from, to)) = from.and_then(|value| value.parse::<u64>().ok())
                .zip(to.and_then(|value| value.parse::<u64>().ok()))
                .filter(|(from, to)| from <= to)
            {
                intervals.push((from, to));
            }
        }
        intervals.sort_unstable();
        let mut covered_through = 0_u64;
        for (from, to) in intervals {
            if from > covered_through.saturating_add(1) {
                break;
            }
            covered_through = covered_through.max(to);
            if covered_through >= required_through {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The newest local timeline observation for one incarnation at or before a snapshot.
    pub fn latest_local_timeline_at(
        &self,
        subject: &str,
        incarnation: &str,
        snapshot_index: u64,
    ) -> Result<Option<u128>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT observed_at_unix_ms FROM local_observations
                 WHERE subject=?1 AND kind='harness.timeline'
                   AND json_extract(body, '$.fields.incarnation_id')=?2
                   AND after_store_index<=?3
                 ORDER BY id DESC LIMIT 1",
                params![subject, incarnation, snapshot_index],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.map(|value| value.max(0) as u128))
            .map_err(Into::into)
    }

    fn timeline_claim_rows_for_incarnation_at(
        &self,
        subject: &str,
        incarnation: &str,
        before_index: Option<u64>,
        descending: bool,
        limit: usize,
    ) -> Result<ClaimsPage> {
        let connection = self.readers.get();
        let order = if descending { "DESC" } else { "ASC" };
        let query = format!(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims
             WHERE subject=?1 AND kind='harness.timeline'
               AND json_extract(body, '$.fields.incarnation_id')=?2
               AND (?3 IS NULL OR store_index<?3)
             ORDER BY store_index {order} LIMIT ?4"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map(
            params![
                subject,
                incarnation,
                before_index,
                limit.saturating_add(1) as u64
            ],
            claim_from_row,
        )?;
        let mut claims = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if claims.len() > limit {
            claims.pop();
            claims.last().map(|claim| claim.store_index)
        } else {
            None
        };
        Ok(ClaimsPage {
            claims,
            next_cursor,
        })
    }

    /// Bounded claim history for one kind, backed by `claims_kind_index`.
    pub fn claims_for_kind_at(
        &self,
        kind: &str,
        before_index: Option<u64>,
        descending: bool,
        limit: usize,
    ) -> Result<ClaimsPage> {
        let connection = self.readers.get();
        let order = if descending { "DESC" } else { "ASC" };
        let query = format!(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims WHERE kind=?1 AND (?2 IS NULL OR store_index<?2)
             ORDER BY store_index {order} LIMIT ?3"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map(
            params![kind, before_index, limit.saturating_add(1) as u64],
            claim_from_row,
        )?;
        let mut claims = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if claims.len() > limit {
            claims.pop();
            claims.last().map(|claim| claim.store_index)
        } else {
            None
        };
        Ok(ClaimsPage {
            claims,
            next_cursor,
        })
    }

    /// The store index of the subject's newest claim of `kind`, or 0 when it has none. Claims are
    /// append-only, so an unchanged index means the subject's claims of that kind are unchanged.
    pub fn newest_claim_index(&self, subject: &str, kind: &str) -> Result<u64> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(
                "SELECT COALESCE(MAX(store_index), 0) FROM claims WHERE subject=?1 AND kind=?2",
            )?
            .query_row(params![subject, kind], |row| row.get(0))?)
    }

    pub fn latest_actual_value(&self, subject: &str) -> Result<Option<Value>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        // The actual state folds only this subject's append-only claims, so the subject's newest
        // claim identifies it. A write elsewhere in the graph must not make every reconcile pass
        // re-read and re-parse the history of every stopped runtime.
        let newest: u64 = connection
            .prepare_cached("SELECT COALESCE(MAX(store_index), 0) FROM claims WHERE subject=?1")?
            .query_row([subject], |row| row.get(0))?;
        if let Some((_, value)) = self
            .smalltalk
            .actual_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(subject)
            .filter(|(index, _)| *index == newest)
        {
            return Ok(value.clone());
        }
        // A claim committed after `newest` can only make this value newer than its key, and the
        // next read then misses and folds again.
        let value = latest_actual(&connection, subject)?;
        let mut cache = self
            .smalltalk
            .actual_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= ACTUAL_CACHE_LIMIT && !cache.contains_key(subject) {
            cache.clear();
        }
        cache.insert(subject.to_owned(), (newest, value.clone()));
        Ok(value)
    }

    /// Positive attachment proof under the indexed current-incarnation diagnostic fence.
    pub fn claude_channel_attached(&self, subject: &str, incarnation: &str) -> Result<bool> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        let body: Option<String> = connection
            .prepare_cached(&claude_attachment_query())?
            .query_row(params![subject, i64::MAX, incarnation], |row| row.get(1))
            .optional()?;
        Ok(body
            .map(|body| serde_json::from_str::<Value>(&body))
            .transpose()?
            .is_some_and(|body| body["fields"]["code"] == "claude-channel-attached"))
    }

    pub fn issue_capability(
        &self,
        kind: &str,
        subject: &str,
        incarnation_id: Option<&str>,
        lifetime_ms: u64,
    ) -> Result<(String, u128)> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let secret = hex::encode(bytes);
        let secret_hash = hex::encode(Sha256::digest(secret.as_bytes()));
        let expires = now_ms().saturating_add(lifetime_ms as u128);
        let connection = self.connection.write();
        connection.execute(
            "INSERT INTO capabilities(secret_hash, kind, subject, incarnation_id, expires_at_unix_ms, used)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![secret_hash, kind, subject, incarnation_id, expires.to_string()],
        )?;
        Ok((secret, expires))
    }

    pub fn capability(&self, secret: &str, expected_kind: &str) -> Result<Capability, St3Error> {
        let hash = hex::encode(Sha256::digest(secret.as_bytes()));
        let connection = self.readers.get();
        let capability = connection
            .query_row(
                "SELECT kind, subject, incarnation_id, expires_at_unix_ms, used FROM capabilities WHERE secret_hash=?1",
                [&hash],
                |row| {
                    let expires = row.get::<_, String>(3)?;
                    Ok(Capability {
                        kind: row.get(0)?,
                        subject: row.get(1)?,
                        incarnation_id: row.get(2)?,
                        expires_at_unix_ms: expires.parse().unwrap_or_default(),
                        used: row.get::<_, i64>(4)? != 0,
                    })
                },
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("invalid-capability", "the operation capability is not valid"))?;
        if capability.kind != expected_kind {
            return Err(St3Error::new(
                "invalid-capability",
                "the operation capability has the wrong kind",
            ));
        }
        if capability.expires_at_unix_ms < now_ms() {
            return Err(St3Error::new(
                "expired-capability",
                "the operation capability expired",
            ));
        }
        Ok(capability)
    }

    pub fn consume_capability(
        &self,
        secret: &str,
        expected_kind: &str,
    ) -> Result<Capability, St3Error> {
        let capability = self.capability(secret, expected_kind)?;
        if capability.used {
            return Ok(capability);
        }
        let hash = hex::encode(Sha256::digest(secret.as_bytes()));
        let connection = self.connection.write();
        let changed = connection
            .execute(
                "UPDATE capabilities SET used=1 WHERE secret_hash=?1 AND used=0",
                [&hash],
            )
            .map_err(internal)?;
        if changed != 1 {
            return Err(St3Error::new(
                "used-capability",
                "the operation capability was already consumed",
            ));
        }
        Ok(Capability {
            used: false,
            ..capability
        })
    }

    /// Event-only envelopes do not change the structural graph and can advance from a healthy
    /// frontier. The incremental projector also handles selected structural and run claims from
    /// a healthy frontier. Keep the full replay for stale projections and ambiguous operation or
    /// renewal ordering.
    ///
    /// A kind belongs here only when a full replay does nothing with its claims but record their
    /// event and operation, and no projection reads them while it builds the graph: then the
    /// order they arrive in cannot change the graph. Lanes, runtime actions and subscription
    /// intake write such claims every few minutes, and each one used to replay the whole graph
    /// while holding the store's only writer.
    fn simple_replication_kind(kind: &str) -> bool {
        matches!(
            kind,
            "lane.approved"
                | "arrangement.edited"
                | "lane.joined"
                | "lane.left"
                | "lane.marked"
                | "lane.moved"
                | "runtime.action.deadline-reached"
                | "runtime.action.failed"
                | "runtime.action.requested"
                | "runtime.action.succeeded"
                | "subscription.mission-deferred"
                | "subscription.mission-failed"
                | "subscription.mission-request-cancelled"
                | "subscription.mission-request-released"
                | "subscription.mission-requested"
                | "subscription.mission-started"
                | "observer.refresh-requested"
                | "publication.operation"
                | "attention.requested"
                | "gate.requested"
                | "gate.result"
                | "loop.state"
                | "observer.observed"
                | "observer.state"
                | "resource.observed"
                | "subscription.state"
                | "message.closed"
                | "message.delivered"
                | "message.read"
                | "message.sent"
                | "message.staged"
                | "harness.diagnostic"
                | "harness.observed"
                | "harness.session-file"
                | "harness.timeline"
                | "harness.usage"
                | "runtime.observed"
                | "runtime.readiness-deadline-reached"
                | "runtime.reconcile-decision"
                | "runtime.restart-window-reset"
                | "daemon.diagnostic"
                | "daemon.started"
                | "transport.observed"
                | "render.applied"
                | "workspace.observed"
                | "work.claimed"
                | "work.renewed"
                | "work.progress"
                | "work.nudged"
                | "work.submitted"
                | "work.failed"
                | "work.released"
                | "work.extended"
                | "subagent.appeared"
                | "subagent.renewed"
                | "subagent.ended"
                | "sekret.called"
                | "sekret.exited"
                | "sekret.refused"
                | "sekret.changed"
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_desired_member_for_test(&self, subject: &str, member: &str) {
        self.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE desired SET member=?2 WHERE subject=?1",
                params![subject, member],
            )
            .unwrap();
    }
}

fn runtime_subject_for_run(run_id: &str, runtime: &str) -> String {
    let Some((kind, local)) = runtime.split_once('/') else {
        return runtime.to_owned();
    };
    if !matches!(kind, "agent" | "exec" | "pty") || local.starts_with(&format!("{run_id}/")) {
        runtime.to_owned()
    } else {
        format!("{kind}/{run_id}/{local}")
    }
}

fn create_declared_mission_run_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    declaration: &MissionRunDeclaration,
    batch_id: &str,
) -> Result<Vec<String>, St3Error> {
    let Some(creation) = &declaration.creation else {
        return Ok(Vec::new());
    };
    let run_id = declaration
        .subject
        .strip_prefix("mission-run/")
        .unwrap_or(&declaration.subject);
    let exists = transaction
        .query_row("SELECT 1 FROM mission_runs WHERE id=?1", [run_id], |_| {
            Ok(())
        })
        .optional()
        .map_err(internal)?
        .is_some();
    if exists {
        return Ok(Vec::new());
    }
    owned_sets::guard_mission_start(transaction, &creation.mission)?;
    let mission: Option<MissionSpec> = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![creation.mission, creation.revision],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
        .map(|body| serde_json::from_str(&body))
        .transpose()
        .map_err(internal)?;
    let Some(mission) = mission else {
        return Err(St3Error::new(
            "missing-mission",
            missing_mission_revision(transaction, &creation.mission, &creation.revision)?,
        ));
    };
    if mission.state != MissionState::Ready {
        return Err(St3Error::new(
            "mission-not-ready",
            format!("mission `mission/{}` is not ready", creation.mission),
        ));
    }
    validate_mission_run_timeout(&mission, &creation.mode)?;
    let inputs = resolve_mission_run_inputs(transaction, &mission, &creation.inputs)?;
    enforce_mission_run_capacity(transaction, &mission)?;
    if let Some(after) = &creation.after {
        ensure_awaited_mission_run_tx(transaction, &declaration.subject, after)?;
    }
    let mission = crate::mission::run_mission(mission, creation.after.as_deref())?;
    let generation_id = Uuid::now_v7().simple().to_string();
    let generation_subject = format!("run-generation/{generation_id}");
    let root_mission_run = declaration.subject.clone();
    let mut variables = BTreeMap::from([
        ("ST_MISSION".into(), mission.id.clone()),
        ("ST_MISSION_REVISION".into(), mission.revision.clone()),
        ("ST_MISSION_RUN".into(), run_id.to_owned()),
        ("ST_RUN_GENERATION".into(), generation_id.clone()),
        ("ST_REQUESTER".into(), creation.requester.clone()),
        ("ST_ROOT_MISSION_RUN".into(), root_mission_run.clone()),
        (
            "ST_ROOT_MISSION_RUN_ID".into(),
            root_mission_run
                .strip_prefix("mission-run/")
                .unwrap_or(&root_mission_run)
                .into(),
        ),
        ("ST_WORKSPACE".into(), creation.workspace.clone()),
        ("ST_PARENT_STEP_RUN".into(), String::new()),
    ]);
    for (name, input) in &inputs {
        variables.insert(format!("input.{name}"), input.value.clone());
    }
    extend_loop_variables(&mut variables, &inputs);
    let now = now_ms();
    transaction
        .execute(
            "INSERT INTO mission_runs(id, mission_id, initial_revision, current_generation_id, root_revision, root_run_id, parent_step_run, workspace, requester, inputs, mode, status, phase, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?3, ?1, NULL, ?5, ?6, ?7, ?8, 'running', 'normal', ?9, ?9)",
            params![
                run_id,
                mission.id,
                mission.revision,
                generation_id,
                creation.workspace,
                creation.requester,
                serde_json::to_string(&inputs).map_err(internal)?,
                creation.mode,
                now.to_string(),
            ],
        )
        .map_err(internal)?;
    let deadline_at_unix_ms =
        insert_mission_deadline_tx(
            transaction,
            run_id,
            mission.timeout_ms,
            creation.after.is_none().then_some(now),
        )?;
    if let Some(after) = &creation.after {
        transaction
            .execute(
                "INSERT INTO mission_run_after(run_id, after_run) VALUES (?1, ?2)",
                params![run_id, after],
            )
            .map_err(internal)?;
    }
    transaction
        .execute(
            "INSERT INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, NULL, 'running', ?4, 'initial mission run', ?5, ?5)",
            params![
                generation_id,
                run_id,
                mission.revision,
                creation.requester,
                now.to_string()
            ],
        )
        .map_err(internal)?;
    let mut flat = Vec::new();
    flatten_steps(&mission, None, &[], &mut flat);
    for (step, selector, constraints) in flat {
        let (assignee, available_to, agentless) = interpolate_selector(&selector, &variables)?;
        let step_subject = format!("step-run/{generation_id}/{}", step.path);
        let mut step_variables = variables.clone();
        step_variables.insert("ST_STEP".into(), step.path.clone());
        step_variables.insert("ST_STEP_RUN".into(), step_subject.clone());
        step_variables.insert("ST_ATTEMPT".into(), "1".into());
        step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
        step_variables.insert(
            "ST_PARENT_STEP_RUN".into(),
            crate::mission::parent_step_path(&mission, &step.path)
                .map(|path| format!("step-run/{generation_id}/{path}"))
                .unwrap_or_default(),
        );
        let title = step
            .title
            .as_deref()
            .map(|value| crate::mission::interpolate(value, &step_variables))
            .transpose()?;
        let goals = interpolate_goals(&step.goals, &step_variables)?;
        let constraints = interpolate_goals(&constraints, &step_variables)?;
        transaction
            .execute(
                "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, created_at_unix_ms, updated_at_unix_ms, constraints)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 1, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
                params![
                    step_subject,
                    run_id,
                    generation_id,
                    step.path,
                    step.definition_hash,
                    assignee,
                    serde_json::to_string(&available_to).map_err(internal)?,
                    agentless,
                    title,
                    goals,
                    now.to_string(),
                    constraints
                ],
            )
            .map_err(internal)?;
    }
    let mut body = json!({
        "fields": {
            "status": "running",
            "mission": mission.subject,
            "revision": mission.revision,
            "initial_revision": mission.revision,
            "current_generation": generation_subject,
            "root_revision": mission.revision,
            "root_mission_run": root_mission_run,
            "parent_step_run": Value::Null,
            "default_selector": Value::Null,
            "workspace": creation.workspace,
            "requester": creation.requester,
            "inputs": inputs,
            "mode": creation.mode,
            "timeout_ms": mission.timeout_ms,
            "deadline_at_unix_ms": deadline_at_unix_ms,
        }
    });
    if let Some(after) = &creation.after {
        body["fields"]["after"] = json!(after);
    }
    add_run_report_fields(
        &mut body,
        &mission,
        creation.report_to.as_deref(),
        creation.report_completed,
    );
    let run_claim = append_claim_tx(
        transaction,
        origin,
        &declaration.subject,
        "mission-run.created",
        Some(&creation.requester),
        &body,
        &[],
        Some(batch_id),
    )
    .map_err(internal)?;
    let generation_claim = append_claim_tx(
        transaction,
        origin,
        &generation_subject,
        "run-generation.created",
        Some(&creation.requester),
        &json!({"fields": {
            "run": declaration.subject,
            "revision": mission.revision,
            "status": "running",
            "reason": "initial mission run"
        }}),
        &[],
        Some(batch_id),
    )
    .map_err(internal)?;
    Ok(vec![run_claim.id, generation_claim.id])
}

/// A run can wait only for another run that this store already knows.
fn ensure_awaited_mission_run_tx(
    connection: &Connection,
    run: &str,
    after: &str,
) -> Result<(), St3Error> {
    let after_id = after.strip_prefix("mission-run/").unwrap_or(after);
    let exists = connection
        .query_row("SELECT 1 FROM mission_runs WHERE id=?1", [after_id], |_| {
            Ok(())
        })
        .optional()
        .map_err(internal)?
        .is_some();
    if !exists {
        return Err(St3Error::new(
            "unknown-mission-run",
            format!("mission run `{run}` waits for `{after}`, which does not exist"),
        ));
    }
    Ok(())
}

/// The claim that changes who a run reports to after it started. Its latest one replaces what
/// the creation claim recorded; one with no `report_to` means the run reports to nobody.
pub const RUN_REPORT_KIND: &str = "mission-run.report-to";

/// The agent, stall limit and completion report that a report claim or a creation claim
/// records, or `None` when it names no agent.
pub fn run_report_fields(body: &Value) -> Option<(String, u64, bool, bool)> {
    let fields = body.get("fields").unwrap_or(body);
    Some((
        fields.get("report_to")?.as_str()?.to_owned(),
        fields
            .get("stalled_after_ms")
            .and_then(Value::as_u64)
            .unwrap_or(crate::mission::DEFAULT_STALLED_AFTER_MS),
        fields
            .get("report_completed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fields
            .get("report_stalls")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    ))
}

fn adopt_declared_mission_revision_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    declaration: &MissionRunDeclaration,
    operation: &MissionRevisionOperation,
    batch_id: &str,
    actor: Option<&str>,
) -> Result<(Vec<String>, bool), St3Error> {
    let actor = actor.ok_or_else(|| {
        St3Error::new(
            "missing-publication-actor",
            "a mission-run revision needs an explicit actor",
        )
    })?;
    let actor = normalize_actor_for_publication(actor);
    let run_id = declaration
        .subject
        .strip_prefix("mission-run/")
        .unwrap_or(&declaration.subject);
    let current = mission_run_view_tx(transaction, run_id).map_err(internal)?;
    if generation_id_from_subject(&current.generation)
        != generation_id_from_subject(&operation.from_generation)
    {
        return Err(St3Error::new(
            "stale-run-generation",
            format!("revision `{}` names a stale run generation", operation.id),
        ));
    }
    if !matches!(current.status.as_str(), "running" | "standing" | "blocked")
        || current.phase != "normal"
    {
        return Err(St3Error::new(
            "mission-run-not-revisable",
            format!(
                "mission run `{}` is {} in its {} phase",
                declaration.subject, current.status, current.phase
            ),
        ));
    }
    let current_mission_id = current
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&current.mission);
    if operation.mission != current_mission_id {
        return Err(St3Error::new(
            "wrong-mission-revision",
            format!(
                "revision `{}` targets mission `{}` instead of `{current_mission_id}`",
                operation.id, operation.mission
            ),
        ));
    }
    let old: MissionSpec = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![current_mission_id, current.revision],
            |row| row.get::<_, String>(0),
        )
        .map_err(internal)
        .and_then(|body| serde_json::from_str(&body).map_err(internal))?;
    let Some(next) = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![operation.mission, operation.revision],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
    else {
        return Err(St3Error::new(
            "missing-mission-revision",
            format!(
                "revision `{}` names a mission that is not stored: {}",
                operation.id,
                missing_mission_revision(transaction, &operation.mission, &operation.revision)?
            ),
        ));
    };
    let next: MissionSpec = serde_json::from_str(&next).map_err(internal)?;
    if next.state != MissionState::Ready {
        return Err(St3Error::new(
            "mission-revision-not-ready",
            "a running mission can adopt only a ready revision",
        ));
    }
    if old.inputs != next.inputs {
        return Err(St3Error::new(
            "run-input-mutation",
            "a run revision cannot change its input declarations",
        ));
    }
    let old = crate::mission::run_mission(old, current.after.as_deref())?;
    let next = crate::mission::run_mission(next, current.after.as_deref())?;
    let variables = mission_run_variables(&current, &next.revision);
    let (compatible, reviewers) = analyze_mission_revision(&old, &next, &current.requester)?;
    let compatible = carried_revision_step_paths(&old, &next, &current.steps, compatible);
    if !reviewers.is_empty() || matches!(old.revision_cutover, RevisionCutover::WhenIdle) {
        if operation.cancellation.is_some() {
            return Err(St3Error::new(
                "revision-cancellation-needs-immediate-cutover",
                "a revision with a deferred or reviewed cutover cannot contain cancellation",
            ));
        }
        if transaction
            .query_row(
                "SELECT 1 FROM revision_proposals WHERE run_id=?1 AND status IN ('pending-approval','draining')",
                [run_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some()
        {
            return Err(St3Error::new(
                "revision-proposal-already-pending",
                "the mission run already has one pending revision proposal",
            ));
        }
        let proposal_id = hex::encode(Sha256::digest(format!(
            "st3.declared-revision-proposal.v1\0{}\0{}",
            declaration.subject, operation.id
        )))[..32]
            .to_owned();
        let cutover = old.revision_cutover.clone();
        let status = if reviewers.is_empty() {
            "draining"
        } else {
            "pending-approval"
        };
        let preview_hash = hex::encode(Sha256::digest(
            serde_json::to_vec(&json!({
                "source_generation": current.generation,
                "candidate_revision": next.revision,
                "compatible_steps": compatible,
                "reviewers": reviewers,
                "cutover": cutover,
            }))
            .map_err(internal)?,
        ));
        let now = now_ms();
        transaction
            .execute(
                "INSERT INTO revision_proposals(id, run_id, source_generation_id, candidate_revision, actor, reason, status, cutover, compatible_steps, reviewers, approvals, preview_hash, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '[]', ?11, ?12, ?12)",
                params![
                    proposal_id,
                    run_id,
                    generation_id_from_subject(&current.generation),
                    next.revision,
                    actor,
                    operation.reason,
                    status,
                    revision_cutover_name(&cutover),
                    serde_json::to_string(&compatible).map_err(internal)?,
                    serde_json::to_string(&reviewers).map_err(internal)?,
                    preview_hash,
                    now.to_string(),
                ],
            )
            .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET phase='revision-draining', updated_at_unix_ms=?2 WHERE id=?1",
                    params![run_id, now.to_string()],
                )
                .map_err(internal)?;
        }
        let subject = format!("revision-proposal/{proposal_id}");
        let claim = append_claim_tx(
            transaction,
            origin,
            &subject,
            "revision-proposal.created",
            Some(&actor),
            &json!({"fields": {
                "run": current.subject,
                "source_generation": current.generation,
                "candidate_revision": next.revision,
                "reason": operation.reason,
                "status": status,
                "cutover": revision_cutover_name(&cutover),
                "compatible_steps": compatible,
                "reviewers": reviewers,
                "preview_hash": preview_hash,
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
                    params![run_id, claim.accepted_at_unix_ms.to_string()],
                )
                .map_err(internal)?;
        }
        return Ok((vec![claim.id], true));
    }

    let predecessor_id = generation_id_from_subject(&current.generation).to_owned();
    let predecessor_subject = current.generation.clone();
    let generation_id = Uuid::now_v7().simple().to_string();
    let generation_subject = format!("run-generation/{generation_id}");
    let now = now_ms();
    transaction
        .execute(
            "INSERT INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?7)",
            params![generation_id, run_id, next.revision, predecessor_id, actor, operation.reason, now.to_string()],
        )
        .map_err(internal)?;
    let mut variables = variables;
    variables.insert("ST_RUN_GENERATION".into(), generation_id.clone());
    let mut flat = Vec::new();
    flatten_steps(&next, None, &[], &mut flat);
    let mut claim_ids = Vec::new();
    for (step, selector, constraints) in flat {
        let (assignee, available_to, agentless) = interpolate_selector(&selector, &variables)?;
        let subject = format!("step-run/{generation_id}/{}", step.path);
        let carried = compatible
            .contains(&step.path)
            .then(|| current.steps.iter().find(|old| old.step == step.path))
            .flatten();
        let attempt = carried.map(|old| old.attempt).unwrap_or(1);
        let mut step_variables = variables.clone();
        step_variables.insert("ST_STEP".into(), step.path.clone());
        step_variables.insert("ST_STEP_RUN".into(), subject.clone());
        step_variables.insert("ST_ATTEMPT".into(), attempt.to_string());
        step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
        step_variables.insert(
            "ST_PARENT_STEP_RUN".into(),
            crate::mission::parent_step_path(&next, &step.path)
                .map(|path| format!("step-run/{generation_id}/{path}"))
                .or_else(|| current.parent_step_run.clone())
                .unwrap_or_default(),
        );
        let title = step
            .title
            .as_deref()
            .map(|value| crate::mission::interpolate(value, &step_variables))
            .transpose()?;
        let goals = interpolate_goals(&step.goals, &step_variables)?;
        let constraints = interpolate_goals(&constraints, &step_variables)?;
        let (status, worker_reported) = carried
            .map(carried_step_projection)
            .unwrap_or(("pending", false));
        let (lease_owner, lease_incarnation, lease_expires) = carried_step_lease(carried, status);
        let blocked_reason = carried.and_then(|old| old.blocked_reason.as_deref());
        let not_before = carried
            .and_then(|old| old.not_before_unix_ms)
            .map(|value| value.to_string());
        transaction
            .execute(
                "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported, blocked_reason, not_before_unix_ms, readiness_epoch, created_at_unix_ms, updated_at_unix_ms, constraints, lease_owner, lease_incarnation, lease_expires_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17, ?18, ?19, ?20, ?21)",
                params![subject, run_id, generation_id, step.path, step.definition_hash, status, attempt, assignee, serde_json::to_string(&available_to).map_err(internal)?, agentless, title, goals, worker_reported, blocked_reason, not_before, carried.map(|old| old.readiness_epoch).unwrap_or(0), now.to_string(), constraints, lease_owner, lease_incarnation, lease_expires.map(|expiry| expiry.to_string())],
            )
            .map_err(internal)?;
        if let Some(old) = carried {
            let mut fields = json!({
                "source_step_run": old.subject,
                "source_generation": predecessor_subject,
                "status": status,
                "attempt": attempt,
                "worker_reported": worker_reported,
                "claimant": lease_owner,
                "claim_incarnation": lease_incarnation,
                "claim_expires_at_unix_ms": lease_expires
            });
            if let Some(claimant) = dropped_claimant(old, status) {
                fields["claimant"] = Value::String(claimant.into());
            }
            let claim = append_claim_tx(
                transaction,
                origin,
                &subject,
                "step-run.carried",
                Some(&actor),
                &json!({"fields": fields}),
                &[],
                Some(batch_id),
            )
            .map_err(internal)?;
            claim_ids.push(claim.id);
        }
    }
    claim_ids.extend(revision_seats::carry_completed_agents_tx(
        transaction,
        origin,
        &current,
        &next,
        &compatible,
        &generation_subject,
        Some(batch_id),
    )?);
    cancel_descendant_mission_runs_tx(
        transaction,
        origin,
        &predecessor_id,
        &actor,
        "the parent run generation was superseded",
        now,
    )?;
    transaction
        .execute(
            "UPDATE run_generations SET status='superseded', updated_at_unix_ms=?2 WHERE id=?1",
            params![predecessor_id, now.to_string()],
        )
        .map_err(internal)?;
    transaction
        .execute(
            "UPDATE mission_runs SET current_generation_id=?2, status='running', phase='normal', updated_at_unix_ms=?3 WHERE id=?1",
            params![run_id, generation_id, now.to_string()],
        )
        .map_err(internal)?;
    let superseded = append_claim_tx(
        transaction,
        origin,
        &predecessor_subject,
        "run-generation.superseded",
        Some(&actor),
        &json!({"fields": {
            "status": "superseded",
            "successor": generation_subject,
            "reason": operation.reason,
        }}),
        &[],
        Some(batch_id),
    )
    .map_err(internal)?;
    claim_ids.push(superseded.id);
    claim_ids.extend(terminalize_generation_steps_tx(
        transaction,
        origin,
        &predecessor_id,
        "the owning run generation was superseded",
        Some(&actor),
        Some(batch_id),
        now,
    )?);
    let created = append_claim_tx(
        transaction,
        origin,
        &generation_subject,
        "run-generation.created",
        Some(&actor),
        &json!({"fields": {
            "run": current.subject,
            "revision": next.revision,
            "predecessor": predecessor_subject,
            "status": "running",
            "reason": operation.reason,
            "compatible_steps": compatible,
        }}),
        &[],
        Some(batch_id),
    )
    .map_err(internal)?;
    claim_ids.push(created.id);
    Ok((claim_ids, false))
}

fn reset_declared_runtime_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    declaration: &MissionRunDeclaration,
    operation: &RuntimeResetOperation,
    runtime: &str,
    batch_id: &str,
) -> Result<ClaimRecord, St3Error> {
    let current_generation = transaction
        .query_row(
            "SELECT current_generation_id FROM mission_runs WHERE id=?1",
            [declaration
                .subject
                .strip_prefix("mission-run/")
                .unwrap_or(&declaration.subject)],
            |row| row.get::<_, String>(0),
        )
        .map_err(internal)?;
    if generation_id_from_subject(&operation.from_generation) != current_generation {
        return Err(St3Error::new(
            "stale-run-generation",
            format!("reset `{}` names a stale run generation", operation.id),
        ));
    }
    let desired = current_desired_row_tx(transaction, runtime)
        .map_err(internal)?
        .ok_or_else(|| {
            St3Error::new(
                "runtime-not-desired",
                format!("runtime `{runtime}` has no selected desired state"),
            )
        })?;
    if !matches!(desired.kind.as_str(), "agent" | "exec" | "pty") {
        return Err(St3Error::new(
            "not-a-runtime",
            format!("subject `{runtime}` is not an agent, exec, or PTY runtime"),
        ));
    }
    let actual = latest_actual(transaction, runtime)
        .map_err(internal)?
        .ok_or_else(|| {
            St3Error::new(
                "runtime-not-incarnated",
                format!("runtime `{runtime}` has no current incarnation"),
            )
        })?;
    let incarnation = actual
        .get("fields")
        .unwrap_or(&actual)
        .get("incarnation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "runtime-not-incarnated",
                format!("runtime `{runtime}` has no current incarnation"),
            )
        })?;
    append_claim_tx(
        transaction,
        origin,
        runtime,
        "runtime.restart-window-reset",
        None,
        &json!({"fields": {
            "desired_token": launch_lineage_tx(transaction, runtime)
                .map_err(internal)?
                .pop()
                .unwrap_or(desired.claim_id),
            "incarnation_id": incarnation,
            "reason": operation.reason,
        }}),
        &[],
        Some(batch_id),
    )
    .map_err(internal)
}

fn request_declared_resource_refresh_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    operation: &ResourceRefreshOperation,
    batch_id: &str,
) -> Result<Vec<String>, St3Error> {
    let desired = {
        let mut statement = transaction
            .prepare("SELECT subject, body, revision FROM desired WHERE kind='observer'")
            .map_err(internal)?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?
    };
    let observers = desired
        .into_iter()
        .filter_map(|(subject, body, revision)| {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|desired| crate::graph::observer_spec(&desired))
                .filter(|spec| !spec.stopped && spec.resource == operation.resource)
                .map(|_| (subject, revision))
        })
        .collect::<Vec<_>>();
    if observers.is_empty() {
        return Err(St3Error::new(
            "resource-not-observed",
            format!("resource `{}` has no active observer", operation.resource),
        ));
    }
    let mut claims = Vec::new();
    for (observer, revision) in observers {
        let attempt = hex::encode(Sha256::digest(format!(
            "st3.resource-refresh.v1\0{}\0{}\0{}",
            operation.resource, operation.id, observer
        )));
        let claim = append_claim_tx(
            transaction,
            origin,
            &observer,
            "observer.refresh-requested",
            None,
            &json!({"fields": {
                "revision": revision,
                "attempt": attempt,
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claims.push(claim.id);
    }
    Ok(claims)
}

fn apply_planning_session_declaration_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    declaration: &PlanningSessionDeclaration,
    batch_id: &str,
    actor: Option<&str>,
    receipts: &mut Vec<PlannedAction>,
) -> Result<Vec<String>, St3Error> {
    let id = declaration
        .subject
        .strip_prefix("planning-session/")
        .unwrap_or(&declaration.subject);
    let mut claim_ids = Vec::new();
    let mut exists = transaction
        .query_row(
            "SELECT requester, status FROM planning_sessions WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(internal)?;
    if exists.is_none()
        && let Some(creation) = &declaration.creation
    {
        let planner = crate::graph::planning_planner_subject(&declaration.subject);
        transaction
            .execute(
                "INSERT INTO planning_sessions(id, mission_id, request_ref, workspace, requester, planner, planner_spec_json, status, target_run_id, source_generation_id, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planning', ?8, ?9, ?10, ?10)",
                params![
                    id,
                    creation.mission,
                    creation.request,
                    creation.workspace,
                    creation.requester,
                    planner,
                    serde_json::to_string(&creation.planner).map_err(internal)?,
                    creation.target_run.as_deref().map(|value| value.strip_prefix("mission-run/").unwrap_or(value)),
                    creation.target_generation.as_deref().map(|value| value.strip_prefix("run-generation/").unwrap_or(value)),
                    now_ms().to_string(),
                ],
            )
            .map_err(internal)?;
        let mut fields = serde_json::Map::from_iter([
            (
                "mission".into(),
                Value::String(format!("mission/{}", creation.mission)),
            ),
            ("request".into(), Value::String(creation.request.clone())),
            ("planner".into(), Value::String(planner.clone())),
            (
                "planner_config".into(),
                serde_json::to_value(&creation.planner).map_err(internal)?,
            ),
            (
                "workspace".into(),
                Value::String(creation.workspace.clone()),
            ),
            (
                "requester".into(),
                Value::String(creation.requester.clone()),
            ),
        ]);
        if let Some(run) = &creation.target_run {
            fields.insert("target_run".into(), Value::String(run.clone()));
        }
        if let Some(generation) = &creation.target_generation {
            fields.insert(
                "target_generation".into(),
                Value::String(generation.clone()),
            );
        }
        let claim = append_claim_tx(
            transaction,
            origin,
            &declaration.subject,
            "planning-session.started",
            Some(&creation.requester),
            &json!({"fields": fields}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
        // The planner seat starts idle, so its instructions arrive as the request message.
        let request = append_claim_tx(
            transaction,
            origin,
            &format!(
                "message/{}",
                &hex::encode(Sha256::digest(format!("planning-request:{id}").as_bytes()))[..16]
            ),
            "message.sent",
            Some(&creation.requester),
            &json!({"fields": {
                "from": creation.requester,
                "to": planner,
                "content": crate::graph::planning_planner_request(id, creation),
                "status": "sent",
                "title": "Launch request",
                "in_reply_to": null,
                "tags": ["launch"],
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claim_ids.push(request.id);
        receipts.push(PlannedAction {
            subject: declaration.subject.clone(),
            action: "start-launch".into(),
            reason: "the named launch was created".into(),
        });
        exists = Some((creation.requester.clone(), "planning".into()));
    }
    let Some((requester, mut status)) = exists else {
        return Err(St3Error::new(
            "missing-launch",
            format!("launch `{}` does not exist", declaration.subject),
        ));
    };
    let actor = actor.map(normalize_actor_for_publication);
    if (!declaration.feedback.is_empty() || !declaration.cancellations.is_empty())
        && actor.as_deref() != Some(requester.as_str())
    {
        return Err(St3Error::new(
            "launch-review-not-authorized",
            format!(
                "the publication actor cannot change launch `{}`",
                declaration.subject
            ),
        ));
    }
    for feedback in declaration.feedback.values() {
        if !publication_operation_is_new(
            transaction,
            &declaration.subject,
            "feedback",
            &feedback.id,
            feedback,
        )? {
            continue;
        }
        if !matches!(status.as_str(), "review" | "revision-requested") {
            return Err(St3Error::new(
                "invalid-launch-transition",
                format!(
                    "launch `{}` cannot accept feedback while {status}",
                    declaration.subject
                ),
            ));
        }
        transaction
            .execute(
                "UPDATE planning_sessions SET status='revision-requested', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, now_ms().to_string()],
            )
            .map_err(internal)?;
        transaction
            .execute("DELETE FROM planning_previews WHERE session_id=?1", [id])
            .map_err(internal)?;
        let claim = append_claim_tx(
            transaction,
            origin,
            &declaration.subject,
            "planning-session.revision-requested",
            actor.as_deref(),
            &json!({"fields": {
                "feedback": feedback.document,
                "variant": feedback.variant,
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
        let message_subject = format!(
            "message/{}",
            &hex::encode(Sha256::digest(
                format!("{}:feedback:{}", declaration.subject, feedback.id).as_bytes()
            ))[..16]
        );
        let planner = crate::graph::planning_planner_subject(&declaration.subject);
        let message = append_claim_tx(
            transaction,
            origin,
            &message_subject,
            "message.sent",
            actor.as_deref(),
            &json!({"fields": {
                "from": actor,
                "to": planner,
                "content": feedback.document,
                "status": "sent",
                "title": "Planning feedback",
                "in_reply_to": Value::Null,
                "tags": ["launch"],
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claim_ids.push(message.id);
        receipts.push(PlannedAction {
            subject: declaration.subject.clone(),
            action: format!("feedback:{}", feedback.id),
            reason: "the launch feedback was published".into(),
        });
        let receipt = append_publication_operation_tx(
            transaction,
            origin,
            &declaration.subject,
            "feedback",
            &feedback.id,
            feedback,
            actor.as_deref(),
            batch_id,
        )?;
        claim_ids.push(receipt.id);
        status = "revision-requested".into();
    }
    for cancellation in declaration.cancellations.values() {
        if !publication_operation_is_new(
            transaction,
            &declaration.subject,
            "cancellation",
            &cancellation.id,
            cancellation,
        )? {
            continue;
        }
        if status == "cancelled" {
            let receipt = append_publication_operation_tx(
                transaction,
                origin,
                &declaration.subject,
                "cancellation",
                &cancellation.id,
                cancellation,
                actor.as_deref(),
                batch_id,
            )?;
            claim_ids.push(receipt.id);
            receipts.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: format!("cancel:{}", cancellation.id),
                reason: "the launch was already cancelled".into(),
            });
            continue;
        }
        transaction
            .execute(
                "UPDATE planning_sessions SET status='cancelled', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, now_ms().to_string()],
            )
            .map_err(internal)?;
        let claim = append_claim_tx(
            transaction,
            origin,
            &declaration.subject,
            "planning-session.cancelled",
            actor.as_deref(),
            &json!({"fields": {
                "reason": cancellation.reason,
                "requester": requester,
            }}),
            &[],
            Some(batch_id),
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
        receipts.push(PlannedAction {
            subject: declaration.subject.clone(),
            action: format!("cancel:{}", cancellation.id),
            reason: cancellation.reason.clone(),
        });
        let receipt = append_publication_operation_tx(
            transaction,
            origin,
            &declaration.subject,
            "cancellation",
            &cancellation.id,
            cancellation,
            actor.as_deref(),
            batch_id,
        )?;
        claim_ids.push(receipt.id);
        status = "cancelled".into();
    }
    Ok(claim_ids)
}

fn normalize_actor_for_publication(actor: &str) -> String {
    if actor.contains('/') {
        actor.to_owned()
    } else {
        format!("person/{actor}")
    }
}

#[derive(Clone)]
pub struct DesiredRow {
    kind: String,
    revision: String,
    claim_id: String,
    body: String,
    member: Option<String>,
    owner_run: Option<String>,
    owner_generation: Option<String>,
}

fn resolve_mission_run_inputs(
    transaction: &Connection,
    mission: &MissionSpec,
    supplied: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, MissionRunInput>, St3Error> {
    let declared = mission.inputs.keys().cloned().collect::<BTreeSet<_>>();
    let provided = supplied.keys().cloned().collect::<BTreeSet<_>>();
    if declared != provided {
        let missing = declared.difference(&provided).cloned().collect::<Vec<_>>();
        let extra = provided.difference(&declared).cloned().collect::<Vec<_>>();
        return Err(St3Error::new(
            "invalid-mission-inputs",
            format!(
                "the mission inputs do not match; missing [{}]; extra [{}]",
                missing.join(", "),
                extra.join(", ")
            ),
        ));
    }
    let mut resolved = BTreeMap::new();
    for (name, declaration) in &mission.inputs {
        let value = supplied.get(name).expect("the exact input set was checked");
        let input = match declaration.kind {
            MissionInputKind::Text => MissionRunInput {
                kind: MissionInputKind::Text,
                value: value.clone(),
                subject: None,
                claim_id: None,
            },
            MissionInputKind::Resource => {
                let (subject, requested_claim) = value
                    .rsplit_once('@')
                    .map_or((value.as_str(), None), |(subject, claim)| {
                        (subject, Some(claim))
                    });
                if !subject.starts_with("resource/") {
                    return Err(St3Error::new(
                        "invalid-resource-input",
                        format!("mission input `{name}` needs a resource subject"),
                    ));
                }
                let claim_id = if let Some(claim_id) = requested_claim {
                    let exists = transaction
                        .query_row(
                            "SELECT 1 FROM claims WHERE id=?1 AND subject=?2 AND kind='resource.observed'",
                            params![claim_id, subject],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(internal)?
                        .is_some();
                    if !exists {
                        return Err(St3Error::new(
                            "missing-resource-input-version",
                            format!("mission input `{name}` references an unavailable claim"),
                        ));
                    }
                    claim_id.to_owned()
                } else {
                    transaction
                        .query_row(
                            &canonical_sql("SELECT id FROM claims WHERE subject=?1 AND kind='resource.observed' ORDER BY COALESCE(json_extract(body, '$.fields.attribution_only'), 0), CANONICAL_DESC(claims) LIMIT 1"),
                            [subject],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(internal)?
                        .ok_or_else(|| {
                            St3Error::new(
                                "missing-resource-input",
                                format!(
                                    "mission input `{name}` references a resource without an observation"
                                ),
                            )
                        })?
                };
                MissionRunInput {
                    kind: MissionInputKind::Resource,
                    value: format!("{subject}@{claim_id}"),
                    subject: Some(subject.to_owned()),
                    claim_id: Some(claim_id),
                }
            }
        };
        resolved.insert(name.clone(), input);
    }
    Ok(resolved)
}

fn enforce_mission_run_capacity(
    transaction: &Connection,
    requested: &MissionSpec,
) -> Result<(), St3Error> {
    let mut statement = transaction
        .prepare(
            "SELECT mission_runs.id, run_generations.revision
             FROM mission_runs JOIN run_generations
               ON run_generations.id=mission_runs.current_generation_id
             WHERE mission_runs.mission_id=?1
               AND mission_runs.status IN ('running','standing','blocked')
               AND mission_runs.phase NOT IN ('final-cancelled','cleanup-cancelled')
             ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
        )
        .map_err(internal)?;
    let active = statement
        .query_map([&requested.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let mut limit = requested.max_active_runs;
    for (_, revision) in &active {
        let body = transaction
            .query_row(
                "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                params![requested.id, revision],
                |row| row.get::<_, String>(0),
            )
            .map_err(internal)?;
        let revision = serde_json::from_str::<MissionSpec>(&body).map_err(internal)?;
        limit = match (limit, revision.max_active_runs) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
    }
    if limit.is_some_and(|limit| active.len() >= limit as usize) {
        return Err(St3Error::new(
            "mission-run-capacity",
            format!(
                "mission `{}` reached its active run limit; active runs: {}",
                requested.id,
                active
                    .iter()
                    .map(|(id, _)| format!("mission-run/{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(())
}

fn current_desired_row(connection: &Connection, subject: &str) -> Result<Option<DesiredRow>> {
    connection
        .prepare_cached(
            "SELECT kind, revision, claim_id, body, member, owner_run, owner_generation
             FROM desired WHERE subject=?1",
        )?
        .query_row([subject], |row| {
            Ok(DesiredRow {
                kind: row.get(0)?,
                revision: row.get(1)?,
                claim_id: row.get(2)?,
                body: row.get(3)?,
                member: row.get(4)?,
                owner_run: row.get(5)?,
                owner_generation: row.get(6)?,
            })
        })
        .optional()
        .map_err(Into::into)
}

fn normalize_message_party(value: &str) -> String {
    if value.is_empty() || value == "requester" || value.contains('/') {
        value.into()
    } else {
        format!("agent/{value}")
    }
}

fn subject_owned_by(connection: &Connection, subject: &str, owner_run: &str) -> bool {
    current_desired_row(connection, subject)
        .ok()
        .flatten()
        .and_then(|row| row.owner_run)
        .is_some_and(|owner| owner == owner_run)
}

fn desired_row_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Option<DesiredRow>> {
    let Some(at_index) = at_index else {
        return current_desired_row(connection, subject);
    };
    if owned_sets::owner(connection, subject, Some(at_index))
        .map_err(anyhow::Error::new)?
        .is_some()
    {
        return retirements::overlay_row(
            connection,
            subject,
            Some(at_index),
            owned_sets::desired_at(connection, subject, at_index)?,
        );
    }
    let mut statement = connection.prepare_cached(&canonical_sql(
        "SELECT id, body, predecessors FROM claims
         WHERE subject=?1 AND kind='intent.desired' AND store_index<=?2
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let rows = statement
        .query_map(params![subject, at_index], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    retirements::overlay_row(
        connection,
        subject,
        Some(at_index),
        select_desired_row(rows)?,
    )
}

/// The selected declaration of a subject's `(id, body, predecessors)` declaration claims in
/// canonical order: the highest revision among the leaves no later declaration replaced.
fn select_desired_row(rows: Vec<(String, String, String)>) -> Result<Option<DesiredRow>> {
    let referenced = rows
        .iter()
        .flat_map(|(_, _, predecessors)| {
            serde_json::from_str::<Vec<String>>(predecessors).unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    let mut selected = None::<(String, String, DesiredSubject)>;
    for (id, body, _) in rows
        .into_iter()
        .filter(|(id, _, _)| !referenced.contains(id))
    {
        let desired: DesiredSubject = serde_json::from_str(&body)?;
        let revision = desired_revision(&desired);
        if selected
            .as_ref()
            .is_none_or(|(current_revision, current_id, _)| {
                (revision.as_str(), id.as_str()) > (current_revision.as_str(), current_id.as_str())
            })
        {
            selected = Some((revision, id, desired));
        }
    }
    selected
        .map(|(revision, claim_id, desired)| {
            Ok(DesiredRow {
                kind: desired.kind,
                revision,
                claim_id,
                body: canonical_json_text(&desired.desired)?,
                member: desired
                    .member
                    .map(|member| canonical_serialized_json_text(&member))
                    .transpose()?,
                owner_run: desired.owner_run,
                owner_generation: desired.owner_generation,
            })
        })
        .transpose()
}

fn current_desired_row_tx(
    transaction: &Connection,
    subject: &str,
) -> Result<Option<DesiredRow>> {
    transaction
        .query_row(
            "SELECT kind, revision, claim_id, body, member, owner_run, owner_generation
             FROM desired WHERE subject=?1",
            [subject],
            |row| {
                Ok(DesiredRow {
                    kind: row.get(0)?,
                    revision: row.get(1)?,
                    claim_id: row.get(2)?,
                    body: row.get(3)?,
                    member: row.get(4)?,
                    owner_run: row.get(5)?,
                    owner_generation: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn desired_revision(desired: &DesiredSubject) -> String {
    let mut desired = desired.clone();
    desired.desired = canonical_json_value(&desired.desired);
    canonical_hash(&desired).expect("desired subject serializes")
}

/// Every reference in the ready missions, current declarations and active runs of the graph
/// that no longer resolves.
fn unresolved_graph_references(
    connection: &Connection,
    origin: &str,
) -> Result<Vec<String>, St3Error> {
    let missions = {
        let mut statement = connection
            .prepare(
                "SELECT r.body FROM mission_definitions d
                     JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision
                     WHERE d.state='ready'
                     ORDER BY d.mission_id",
            )
            .map_err(internal)?;
        let bodies = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        // A definition this build cannot read is not a reference problem.
        bodies
            .iter()
            .filter_map(|body| serde_json::from_str::<MissionSpec>(body).ok())
            .collect::<Vec<_>>()
    };
    let desired = {
        let mut statement = connection
            .prepare(
                "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
                     FROM desired WHERE kind IN ('agent', 'subscription', 'schedule')
                     ORDER BY subject",
            )
            .map_err(internal)?;
        statement
            .query_map([], desired_from_row)
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?
    };
    let mut unresolved =
        crate::references::graph(&missions, &desired, &ReferenceGraph { connection }, origin)?;
    let mut statement = connection
        .prepare(
            "SELECT r.id, r.mission_id, g.revision FROM mission_runs r
                 LEFT JOIN run_generations g ON g.id=r.current_generation_id
                 WHERE r.phase!='terminal'
                   AND NOT EXISTS (
                     SELECT 1 FROM mission_revisions m
                     WHERE m.mission_id=r.mission_id AND m.revision=g.revision
                   )
                 ORDER BY r.id",
        )
        .map_err(internal)?;
    let runs = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    for (run, mission, revision) in runs {
        unresolved.push(match revision {
            Some(revision) => format!(
                "mission run `mission-run/{run}` runs a mission that is not stored: {}",
                missing_mission_revision(connection, &mission, &revision)?
            ),
            None => format!("mission run `mission-run/{run}` has no stored current generation"),
        });
    }
    Ok(unresolved)
}

/// The store as the reference checks read it.
struct ReferenceGraph<'a> {
    connection: &'a Connection,
}

impl crate::references::Graph for ReferenceGraph<'_> {
    fn declared(&self, subject: &str) -> Result<bool, St3Error> {
        self.connection
            .query_row("SELECT 1 FROM desired WHERE subject=?1", [subject], |_| {
                Ok(())
            })
            .optional()
            .map(|row| row.is_some())
            .map_err(internal)
    }

    fn mission_published(&self, mission: &str) -> Result<bool, St3Error> {
        self.connection
            .query_row(
                "SELECT 1 FROM mission_revisions WHERE mission_id=?1 LIMIT 1",
                [mission],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(internal)
    }

    fn mission_revision(
        &self,
        mission: &str,
        revision: &str,
    ) -> Result<Option<MissionSpec>, St3Error> {
        self.connection
            .query_row(
                "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                params![mission, revision],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?
            .map(|body| serde_json::from_str(&body).map_err(internal))
            .transpose()
    }

    fn missing_revision(&self, mission: &str, revision: &str) -> Result<String, St3Error> {
        missing_mission_revision(self.connection, mission, revision)
    }
}

/// Why `mission/MISSION@REVISION` is not stored here. A claim ID has the same shape as a mission
/// revision, so when the pin is the ID of the claim that published a revision, this names the
/// revision to pin instead.
fn missing_mission_revision(
    connection: &Connection,
    mission: &str,
    revision: &str,
) -> Result<String, St3Error> {
    let pin = format!("mission/{mission}@{revision}");
    let published = connection
        .query_row(
            "SELECT mission_id, revision FROM mission_revisions WHERE claim_id=?1",
            [revision],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(internal)?;
    if let Some((published_mission, published_revision)) = published {
        return Ok(format!(
            "`{pin}` pins the ID of the claim that published `mission/{published_mission}@{published_revision}`, not a mission revision; pin `mission/{published_mission}@{published_revision}`"
        ));
    }
    let claim = connection
        .query_row("SELECT kind FROM claims WHERE id=?1", [revision], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(internal)?;
    Ok(match claim {
        Some(kind) => {
            format!("`{pin}` pins the ID of a `{kind}` claim, not a mission revision")
        }
        None => format!("mission `{pin}` is not stored on this host"),
    })
}

fn validate_documents(
    transaction: &Transaction<'_>,
    references: &BTreeSet<String>,
) -> Result<(), St3Error> {
    for reference in references {
        let (name, hash) = split_document_ref(reference)?;
        let bytes: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT b.bytes FROM documents d JOIN blobs b ON b.hash=d.hash WHERE d.name=?1 AND d.hash=?2",
                params![name, hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        let Some(bytes) = bytes else {
            return Err(St3Error::new(
                "missing-document",
                format!("document `{reference}` is not stored"),
            ));
        };
        if hex::encode(Sha256::digest(&bytes)) != hash {
            return Err(St3Error::new(
                "document-hash-mismatch",
                format!("stored bytes for `{reference}` do not match the reference"),
            ));
        }
    }
    Ok(())
}

fn validate_claim_kind(kind: &str) -> Result<(), St3Error> {
    if st3_schema::is_known_claim(kind) {
        Ok(())
    } else {
        Err(St3Error::new(
            "unknown-claim-kind",
            format!("claim kind `{kind}` is not registered in st3.v1"),
        ))
    }
}

fn validate_claim_subject(subject: &str) -> Result<(), St3Error> {
    st3_schema::registry()
        .validate_subject(subject)
        .map(|_| ())
        .map_err(|error| St3Error::new(error.code, error.message))
}

fn validate_actor(actor: &str) -> Result<(), St3Error> {
    if actor == "requester" {
        return Ok(());
    }
    validate_claim_subject(actor).map_err(|_| {
        St3Error::new(
            "invalid-claim-actor",
            "a claim actor must be `requester` or a full subject",
        )
    })
}

fn validate_claim_fields(input: &ClaimInput) -> Result<&'static st3_schema::ClaimSpec, St3Error> {
    st3_schema::registry()
        .validate_claim(&input.subject, &input.kind, &input.fields)
        .map_err(|error| St3Error::new(error.code, error.message))
}

fn validate_claim_cardinality(
    transaction: &Transaction<'_>,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: &BTreeMap<String, Value>,
    cardinality: &st3_schema::Cardinality,
) -> Result<(), St3Error> {
    let duplicate = match cardinality {
        st3_schema::Cardinality::Append | st3_schema::Cardinality::StateTransition => false,
        st3_schema::Cardinality::Once => transaction
            .query_row(
                "SELECT 1 FROM claims WHERE subject=?1 AND kind=?2 LIMIT 1",
                params![subject, kind],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some(),
        st3_schema::Cardinality::OncePerActor => transaction
            .query_row(
                "SELECT 1 FROM claims WHERE subject=?1 AND kind=?2 AND actor IS ?3 LIMIT 1",
                params![subject, kind, actor],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some(),
        st3_schema::Cardinality::OncePerAttempt => {
            let attempt = fields.get("attempt").and_then(Value::as_u64).unwrap_or(1);
            transaction
                .query_row(
                    "SELECT 1 FROM claims
                     WHERE subject=?1 AND kind=?2
                       AND CAST(COALESCE(json_extract(body, '$.fields.attempt'), 1) AS INTEGER)=?3
                     LIMIT 1",
                    params![subject, kind, attempt],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some()
        }
    };
    if duplicate {
        return Err(St3Error::new(
            "claim-cardinality",
            format!(
                "claim kind `{}` already exists at its allowed cardinality for `{}`",
                kind, subject
            ),
        ));
    }
    Ok(())
}

fn normalize_resource_observation(
    transaction: &Connection,
    input: &ClaimInput,
) -> Result<Option<BTreeMap<String, Value>>, St3Error> {
    if input.kind != "resource.observed" {
        return Ok(None);
    }
    let prior_kind = latest_resource_kind(transaction, &input.subject)?;
    let desired_kind = current_desired_row_tx(transaction, &input.subject)
        .map_err(internal)?
        .filter(|desired| desired.kind == "resource")
        .and_then(|desired| serde_json::from_str::<Value>(&desired.body).ok())
        .and_then(|desired| canonical_child_string(&desired, "kind"));
    let requested_kind = input.fields.get("kind").and_then(Value::as_str);
    let kind = requested_kind
        .map(str::to_owned)
        .or_else(|| prior_kind.clone())
        .or(desired_kind)
        .ok_or_else(|| {
            St3Error::new(
                "missing-resource-kind",
                "the first resource observation needs a registered resource kind",
            )
        })?;
    if prior_kind.as_deref().is_some_and(|prior| prior != kind) {
        return Err(St3Error::new(
            "immutable-resource-kind",
            format!(
                "resource `{}` already has kind `{}`",
                input.subject,
                prior_kind.unwrap()
            ),
        ));
    }
    let mut facts = resource_facts(&input.fields)?;
    let spec = st3_schema::registry()
        .validate_resource_facts(&kind, &facts)
        .map_err(|error| St3Error::new(error.code, error.message))?;
    if let Some(previous) = latest_actual(transaction, &input.subject).map_err(internal)? {
        let previous = previous
            .get("facts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| previous.as_object().cloned().unwrap_or_default());
        // Issue and pull request attribution belongs to its publisher, not to the latest
        // observer or fixer. Keep each named field, including a mission-run-only opener, across
        // observations, so a partial snapshot such as a merge never erases it.
        if carries_opener(&kind) && input.fields.get("attribution_only") != Some(&Value::Bool(true)) {
            for name in ["opened_by", "opened_by_run"] {
                if let Some(value) = previous.get(name) {
                    facts.insert(name.into(), value.clone());
                }
            }
        }
        for (name, field) in &spec.fields {
            if field.immutable
                && let Some(value) = facts.get(name)
                && previous.get(name).is_some_and(|prior| prior != value)
            {
                return Err(St3Error::new(
                    "immutable-resource-field",
                    format!(
                        "resource field `{name}` cannot change for `{}`",
                        input.subject
                    ),
                ));
            }
        }
    }
    let mut fields = input.fields.clone();
    if carries_opener(&kind) {
        if fields.contains_key("facts") {
            fields.insert("facts".into(), serde_json::to_value(facts).map_err(internal)?);
        } else {
            fields.extend(facts);
        }
    }
    fields.insert("kind".into(), Value::String(kind));
    Ok(Some(fields))
}

/// The resource kinds whose `opened_by` and `opened_by_run` outlast every later observation
/// (Nathan, 2026-10-03, #778 rule 5).
fn carries_opener(kind: &str) -> bool {
    matches!(kind, "vcs.issue" | "vcs.pull-request")
}

fn resource_facts(fields: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>, St3Error> {
    if let Some(value) = fields.get("facts") {
        return value
            .as_object()
            .map(|facts| {
                facts
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .ok_or_else(|| {
                St3Error::new(
                    "invalid-resource-observation",
                    "resource observation facts must be an object",
                )
            });
    }
    const ENVELOPE: &[&str] = &[
        "kind",
        "observed_at",
        "observer",
        "baseline",
        "changed_fields",
        "attribution_only",
    ];
    Ok(fields
        .iter()
        .filter(|(name, _)| !ENVELOPE.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

fn latest_resource_kind(
    transaction: &Connection,
    subject: &str,
) -> Result<Option<String>, St3Error> {
    let bodies = {
        let mut statement = transaction
            .prepare(
                &canonical_sql("SELECT body FROM claims WHERE subject=?1 AND kind='resource.observed' ORDER BY CANONICAL_DESC(claims)"),
            )
            .map_err(internal)?;
        statement
            .query_map([subject], |row| row.get::<_, String>(0))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?
    };
    Ok(bodies.into_iter().find_map(|body| {
        serde_json::from_str::<Value>(&body)
            .ok()?
            .pointer("/fields/kind")?
            .as_str()
            .map(str::to_owned)
    }))
}

fn registered_client_claim_kind(kind: &str) -> bool {
    st3_schema::is_known_claim(kind)
}

const LOCAL_OBSERVATION_COLUMNS: &str = "SELECT id, after_store_index, subject, kind, actor, body, observed_at_unix_ms FROM local_observations";

// MIN(id) makes SQLite walk rowids from the beginning until it finds a match, even with the
// frontier index present. Unary + disables that shortcut so this reads only the covering
// after_store_index range; id is the integer primary key carried by each index entry.
const LOCAL_OBSERVATION_FLOOR_QUERY: &str =
    "SELECT COALESCE(MIN(+id), (SELECT COALESCE(MAX(id), 0) + 1 FROM local_observations))
     FROM local_observations WHERE after_store_index >= ?1";

use st3_kernel::model::LOCAL_OBSERVATION_ID_PREFIX;
pub use st3_kernel::model::local_observation_position;

fn local_retention(kind: &str) -> bool {
    st3_schema::registry()
        .claim(kind)
        .is_some_and(|spec| spec.retention == st3_schema::Retention::Local)
}

fn system_local_retention(kind: &str) -> bool {
    st3_schema::registry()
        .claim(kind)
        .is_some_and(|spec| spec.retention == st3_schema::Retention::SystemLocal)
}

fn latest_retention(kind: &str) -> bool {
    st3_schema::registry()
        .claim(kind)
        .is_some_and(|spec| spec.retention == st3_schema::Retention::Latest)
}

/// How long st remembers that it counted a response. A harness keeps a bounded record of recent
/// responses that st replays after a restart; a response older than this is never counted.
pub const USAGE_RESPONSE_HORIZON_MS: u128 = 30 * 24 * 60 * 60 * 1000;

/// The responses st remembered counting before it recorded when each was made. They become
/// dated now, so they are forgotten one horizon after this upgrade.
pub fn migrate_local_usage_seen(connection: &Connection) -> Result<()> {
    let legacy: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_usage_seen')",
        [],
        |row| row.get(0),
    )?;
    if legacy {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             INSERT OR IGNORE INTO local_usage_responses(subject, source_id, observed_at_unix_ms)
             SELECT subject, source_id, {} FROM local_usage_seen;
             DROP TABLE local_usage_seen;
             COMMIT;",
            now_ms()
        ))?;
    }
    Ok(())
}

/// While a harness works, its usage replicates at most once in this window. Once it stops
/// working, its newest usage replicates at once.
const USAGE_PUBLISH_INTERVAL_MS: u128 = 5 * 60 * 1000;

fn validate_local_observation(input: &ClaimInput) -> Result<(), St3Error> {
    if input.expected_subject.is_some() {
        return Err(St3Error::new(
            "local-observation-head",
            "a local observation does not take part in the subject's claim head",
        ));
    }
    st3_schema::registry()
        .validate_claim(&input.subject, &input.kind, &input.fields)
        .map_err(|error| St3Error::new(error.code, error.message))?;
    // A timeline entry keeps the sequence its driver gave it. A replicated claim could
    // fall back to its unique store index, but local observations share the index of the
    // claim they follow, so they could not order themselves.
    if input.kind == "harness.timeline" && !input.fields.contains_key("sequence") {
        return Err(St3Error::new(
            "missing-claim-field",
            "a local timeline observation needs the sequence its driver assigned",
        ));
    }
    Ok(())
}

/// Insert one local observation. A repeated idempotency key returns the first observation
/// and `false`.
fn insert_local_observation_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    input: &ClaimInput,
    observed_at: u128,
) -> Result<(ClaimRecord, bool), St3Error> {
    let mut body = json!({
        "fields": &input.fields,
        "evidence": input.evidence,
    });
    let dedupe_key = input
        .idempotency_key
        .as_deref()
        .map(|key| local_observation_dedupe_key(&input.kind, key));
    let request_digest = claim_operation(input)?.map(|(_, digest)| digest);
    if let Some(key) = &dedupe_key
        && let Some((existing, stored_digest)) = transaction
            .query_row(
                "SELECT id, after_store_index, subject, kind, actor, body, observed_at_unix_ms, request_digest
                 FROM local_observations WHERE dedupe_key=?1",
                [key],
                |row| {
                    Ok((
                        local_observation_from_row(origin, row)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?
    {
        // A retry must repeat the original request exactly, as for a claim.
        if stored_digest != request_digest {
            return Err(St3Error::new(
                "idempotency-mismatch",
                "the idempotency key already identifies a different request",
            )
            .with_detail("stored_digest", json!(stored_digest))
            .with_detail("request_digest", json!(request_digest)));
        }
        return Ok((existing, false));
    }
    // A local observation may cite a claim or another local observation of this node.
    for evidence in &input.evidence {
        let exists = match evidence
            .strip_prefix(LOCAL_OBSERVATION_ID_PREFIX)
            .and_then(|rest| rest.rsplit_once('/'))
        {
            Some((evidence_origin, position)) => {
                evidence_origin == origin
                    && transaction
                        .query_row(
                            "SELECT 1 FROM local_observations WHERE id=?1",
                            [position],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(internal)?
                        .is_some()
            }
            None => {
                checkpoint::claim_or_tombstone_exists(transaction, evidence).map_err(internal)?
            }
        };
        if !exists {
            return Err(St3Error::new(
                "missing-evidence",
                format!("evidence claim `{evidence}` is not stored"),
            ));
        }
    }
    let after_store_index = current_index(transaction).map_err(internal)?;
    if input.kind == "harness.timeline"
        && input.fields.get("entry_type").and_then(Value::as_str) == Some("usage")
        && input.fields.get("operation").and_then(Value::as_str) == Some("append")
        && input
            .fields
            .get("body")
            .and_then(|body| body.get("semantics"))
            .and_then(Value::as_str)
            == Some("response")
    {
        let desired: Option<(Option<String>, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT owner_run, owner_generation, owner_step FROM desired WHERE subject=?1",
                [&input.subject],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        let incarnation = input
            .fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Bind the response to the step its agent held when the harness recorded it, not when
        // st ingested it: a replayed backlog must not charge older responses to a step that
        // became active later.
        let responded_at = input
            .fields
            .get("observed_at_unix_ms")
            .and_then(Value::as_u64)
            .map_or(observed_at, |at| (at as u128).min(observed_at));
        let active_step: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT subject, 'mission-run/' || run_id, 'run-generation/' || generation_id
                 FROM step_runs WHERE lease_owner=?1 AND lease_incarnation=?2
                   AND CAST(lease_expires_at_unix_ms AS INTEGER)>?3
                   AND COALESCE(CAST(activated_at_unix_ms AS INTEGER), 0)<=?3
                   AND status IN ('claimed', 'working', 'submitted')
                 ORDER BY updated_at_unix_ms DESC LIMIT 1",
                params![input.subject, incarnation, responded_at as i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        let owner_run = active_step
            .as_ref()
            .map(|item| item.1.as_str())
            .or_else(|| desired.as_ref().and_then(|item| item.0.as_deref()))
            .unwrap_or("");
        let owner_step = active_step
            .as_ref()
            .map(|item| item.0.as_str())
            .or_else(|| desired.as_ref().and_then(|item| item.2.as_deref()))
            .unwrap_or("");
        let owner_generation = active_step
            .as_ref()
            .map(|item| item.2.as_str())
            .or_else(|| desired.as_ref().and_then(|item| item.1.as_deref()));
        let usage = &input.fields["body"];
        let model = usage
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let account = usage.get("account").and_then(Value::as_str).unwrap_or("");
        let bucket = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let input_tokens = bucket("input_tokens");
        let output_tokens = bucket("output_tokens");
        let cache_write_tokens = bucket("cache_write_tokens");
        let cache_write_1h_tokens = bucket("cache_write_1h_tokens").min(cache_write_tokens);
        let cached_tokens = bucket("cached_tokens");
        let total_tokens = bucket("total_tokens");
        // A price the harness reported for this response wins over st's table. A response
        // neither can price stays unpriced, never free.
        let reported_cost = usage
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(|cost| (cost * 1_000_000.0).round() as u64);
        let estimated_cost = crate::pricing::cost_microusd(
            model,
            crate::pricing::Tokens {
                input: input_tokens,
                output: output_tokens,
                cache_read: cached_tokens,
                cache_write: cache_write_tokens,
                cache_write_1h: cache_write_1h_tokens,
            },
        );
        let cost_microusd = reported_cost.or(estimated_cost).unwrap_or(0);
        let unpriced_tokens = if reported_cost.or(estimated_cost).is_some() {
            0
        } else {
            total_tokens
        };
        body["fields"]["attribution"] = json!({
            "agent_id": input.subject,
            "mission_run_id": if owner_run.is_empty() { None } else { Some(owner_run) },
            "generation_id": owner_generation,
            "step_id": if owner_step.is_empty() { None } else { Some(owner_step) },
        });
        body["fields"]["host"] = Value::String(origin.to_owned());
        // What st charged this response, so the OpenTelemetry history carries the same cost
        // the graph's totals do.
        body["fields"]["spend"] = json!({
            "cost_microusd": cost_microusd,
            "basis": if reported_cost.is_some() {
                "reported"
            } else if estimated_cost.is_some() {
                "estimated"
            } else {
                "unpriced"
            },
            "pricing": crate::pricing::PRICING_REVISION,
        });
        let source_id = input
            .fields
            .get("source_id")
            .and_then(Value::as_str)
            .or_else(|| input.fields.get("entry_id").and_then(Value::as_str))
            .unwrap_or("");
        let new_response = responded_at
            >= observed_at.saturating_sub(USAGE_RESPONSE_HORIZON_MS)
            && transaction
                .execute(
                    "INSERT OR IGNORE INTO local_usage_responses(subject, source_id, observed_at_unix_ms)
                     VALUES (?1, ?2, ?3)",
                    params![input.subject, source_id, responded_at as i64],
                )
                .map_err(internal)?
                != 0;
        if new_response {
            transaction.execute(
                "INSERT INTO local_usage_spend(subject, incarnation_id, model, account, owner_run, owner_step, host,
                    input_tokens, output_tokens, cache_write_tokens, cache_write_1h_tokens, cached_tokens,
                    total_tokens, cost_microusd, reported_cost_microusd, unpriced_tokens, observed_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(subject, incarnation_id, model, account, owner_run, owner_step, host) DO UPDATE SET
                    input_tokens=input_tokens+excluded.input_tokens,
                    output_tokens=output_tokens+excluded.output_tokens,
                    cache_write_tokens=cache_write_tokens+excluded.cache_write_tokens,
                    cache_write_1h_tokens=cache_write_1h_tokens+excluded.cache_write_1h_tokens,
                    cached_tokens=cached_tokens+excluded.cached_tokens,
                    total_tokens=total_tokens+excluded.total_tokens,
                    cost_microusd=cost_microusd+excluded.cost_microusd,
                    reported_cost_microusd=reported_cost_microusd+excluded.reported_cost_microusd,
                    unpriced_tokens=unpriced_tokens+excluded.unpriced_tokens,
                    observed_at_unix_ms=excluded.observed_at_unix_ms",
                params![input.subject, incarnation, model, account, owner_run, owner_step, origin,
                    input_tokens, output_tokens, cache_write_tokens, cache_write_1h_tokens, cached_tokens,
                    total_tokens, cost_microusd, reported_cost.unwrap_or(0), unpriced_tokens,
                    observed_at as i64],
            ).map_err(internal)?;
            // Keep each distinct price/source/effective-rate combination cumulative. A later
            // table revision or long-context response must not relabel earlier spend.
            let mut key_fields = BTreeMap::from([
                ("semantics".into(), json!("response_rollup")),
                ("incarnation_id".into(), json!(incarnation)),
                ("model".into(), json!(model)),
                ("owner_run".into(), json!(owner_run)),
                ("owner_step".into(), json!(owner_step)),
                ("host".into(), json!(origin)),
            ]);
            if !account.is_empty() {
                key_fields.insert("account".into(), json!(account));
            }
            let slot = usage_slot(&key_fields);
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT pricing FROM local_usage_provenance WHERE subject=?1 AND slot=?2",
                    params![input.subject, slot],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            let mut provenance: Vec<Value> = previous
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(internal)?
                .unwrap_or_default();
            let tokens = crate::pricing::Tokens {
                input: input_tokens,
                output: output_tokens,
                cache_read: cached_tokens,
                cache_write: cache_write_tokens,
                cache_write_1h: cache_write_1h_tokens,
            };
            let mut entry = json!({
                "cost_source": if reported_cost.is_some() { "provider_reported" }
                    else if estimated_cost.is_some() { "computed" } else { "unpriced" },
            });
            // A provider figure does not reveal the provider's own price table or rates.
            if reported_cost.is_none() {
                entry["price_table_id"] = json!(crate::pricing::PRICE_TABLE_ID);
                entry["price_table_version"] = json!(crate::pricing::price_table_version());
                if let Some(rates) = crate::pricing::applied_rates(model, tokens) {
                    let mut rates = serde_json::to_value(rates).map_err(internal)?;
                    rates.as_object_mut().unwrap().remove("long_context");
                    entry["rates_usd_per_million_tokens"] = rates;
                }
            }
            let index = provenance
                .iter()
                .position(|previous| pricing_identity(previous) == pricing_identity(&entry));
            if let Some(index) = index {
                entry = provenance.remove(index);
            }
            for (name, value) in USAGE_BUCKETS.into_iter().zip([
                total_tokens,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_write_1h_tokens,
                cached_tokens,
                cost_microusd,
                reported_cost.unwrap_or(0),
                unpriced_tokens,
            ]) {
                entry[name] = json!(entry[name].as_u64().unwrap_or(0).saturating_add(value));
            }
            provenance.push(entry);
            provenance.sort_by_key(pricing_identity);
            transaction
                .execute(
                    "INSERT INTO local_usage_provenance(subject, slot, pricing) VALUES (?1, ?2, ?3)
                 ON CONFLICT(subject, slot) DO UPDATE SET pricing=excluded.pricing",
                    params![
                        input.subject,
                        slot,
                        serde_json::to_string(&provenance).map_err(internal)?
                    ],
                )
                .map_err(internal)?;
        }
    }
    smallclaims::touched::note_wrote(|| format!("{} {}", input.kind, input.subject));
    transaction
        .execute(
            "INSERT INTO local_observations(
                after_store_index, subject, kind, actor, body, dedupe_key, request_digest,
                observed_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                after_store_index,
                input.subject,
                input.kind,
                input.actor,
                canonical_json_text(&body).map_err(internal)?,
                dedupe_key,
                request_digest,
                observed_at.min(i64::MAX as u128) as i64,
            ],
        )
        .map_err(internal)?;
    let id = transaction.last_insert_rowid();
    Ok((
        ClaimRecord {
            id: local_observation_id(origin, id),
            store_index: after_store_index,
            batch_id: String::new(),
            subject: input.subject.clone(),
            kind: input.kind.clone(),
            origin: origin.to_owned(),
            actor: input.actor.clone(),
            operation_id: None,
            request_digest: None,
            body,
            predecessors: Vec::new(),
            accepted_at_unix_ms: observed_at,
        },
        true,
    ))
}

fn latest_claim_of_kind_tx(
    transaction: &Transaction<'_>,
    subject: &str,
    kind: &str,
) -> Result<Option<ClaimRecord>, St3Error> {
    transaction
        .query_row(
            // The same statement `Store::latest_claim` runs. Without the index hint SQLite picks
            // `claims_subject_kind_index` and sorts every claim of the seat and kind.
            &latest_claim_of_kind_query(),
            params![subject, kind],
            claim_from_row,
        )
        .optional()
        .map_err(internal)
}

/// Replicate an observation of the same kind and subject as an ordinary claim.
fn publish_latest_claim_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: &Value,
) -> Result<ClaimRecord, St3Error> {
    let predecessors = latest_claim_id_tx(transaction, subject)
        .map_err(internal)?
        .into_iter()
        .collect::<Vec<_>>();
    append_claim_tx(
        transaction,
        origin,
        subject,
        kind,
        actor,
        &json!({ "fields": fields, "evidence": [] }),
        &predecessors,
        None,
    )
    .map_err(claim_append_error)
}

fn latest_harness_of_incarnation_tx(
    connection: &Transaction<'_>,
    subject: &str,
    incarnation: Option<&str>,
) -> Result<Option<ClaimRecord>, St3Error> {
    let latest = latest_claim_of_kind_tx(connection, subject, "harness.observed")?;
    if latest.as_ref().is_none_or(|claim| {
        claim.body.pointer("/fields/incarnation_id").and_then(Value::as_str) == incarnation
    }) {
        return Ok(latest);
    }
    let comparison = if incarnation.is_some() { "=?2" } else { "IS ?2" };
    let indexed = if incarnation.is_some() {
        " INDEXED BY claims_incarnation_accepted_index"
    } else {
        ""
    };
    connection
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims{indexed}
             JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND {INCARNATION_OF_CLAIM}{comparison} AND claims.kind='harness.observed'
             ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
        ))
        .map_err(internal)?
        .query_row(params![subject, incarnation], claim_from_row)
        .optional()
        .map_err(internal)
}

/// Publish a harness observation when any field other than its observation time differs
/// from the subject's latest replicated observation. When the harness stops working, its
/// usage that is still only local replicates too.
fn publish_changed_harness_state_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    input: &ClaimInput,
    now: u128,
) -> Result<Option<ClaimRecord>, St3Error> {
    let latest = latest_harness_of_incarnation_tx(
        transaction,
        &input.subject,
        input.fields.get("incarnation_id").and_then(Value::as_str),
    )?;
    let mut fields = input.fields.clone();
    if let Some(previous) = latest.as_ref().and_then(|claim| claim.body.get("fields")) {
        for name in ["blocked_on", "ask"] {
            if !fields.contains_key(name)
                && let Some(value) = previous.get(name)
            {
                fields.insert(name.into(), value.clone());
            }
        }
        if fields.get("provider_auth").is_none_or(Value::is_null)
            && let Some(value) = previous.get("provider_auth").filter(|value| value.is_boolean())
        {
            fields.insert("provider_auth".into(), value.clone());
        }
    }
    if matches!(fields.get("state").and_then(Value::as_str), Some("ended" | "indeterminate")) {
        fields.insert("blocked_on".into(), Value::Null);
        fields.insert("ask".into(), Value::Null);
    }
    let unchanged = latest.as_ref().is_some_and(|claim| {
        claim
            .body
            .get("fields")
            .and_then(Value::as_object)
            .is_some_and(|previous| {
                fields.iter().all(|(name, value)| {
                    matches!(name.as_str(), "observed_at_ms" | "observed_since_ms" | "status_transition")
                        || previous.get(name) == Some(value)
                })
            })
    });
    // Refresh remote freshness at most once a minute, without manufacturing transitions.
    if unchanged && latest.as_ref().is_some_and(|claim| now.saturating_sub(claim.accepted_at_unix_ms) < 60_000) {
        return Ok(None);
    }
    let observed_at = fields.get("observed_at_ms").and_then(Value::as_u64)
        .map_or(now, u128::from).min(now);
    let same_state = latest.as_ref().is_some_and(|claim| {
        let previous = &claim.body["fields"];
        let carried = |name: &str| fields.get(name).or_else(|| previous.get(name));
        claim.body["fields"]["state"] == fields["state"]
            && (seat_status::permission_blocked(
                previous["state"].as_str(),
                previous["blocked_on"].as_str(),
                previous["ask"].as_str(),
            ) == seat_status::permission_blocked(
                fields["state"].as_str(),
                carried("blocked_on").and_then(Value::as_str),
                carried("ask").and_then(Value::as_str),
            ))
            && previous.get("provider_auth").and_then(Value::as_bool)
                == carried("provider_auth").and_then(Value::as_bool)
            && claim.body["fields"].get("incarnation_id") == fields.get("incarnation_id")
    });
    let since = if same_state {
        let previous = latest.as_ref().unwrap();
        match previous.body["fields"]["observed_since_ms"].as_u64() {
            Some(since) => u128::from(since),
            None => seat_status::state_run_since(transaction, &input.subject,
                fields.get("incarnation_id").and_then(Value::as_str).unwrap_or_default(),
                fields.get("state").and_then(Value::as_str).unwrap_or_default(), i64::MAX as u64).map_err(internal)?
                .unwrap_or_else(|| seat_status::observation_time(previous)),
        }
    } else {
        observed_at
    };
    fields.insert("observed_since_ms".into(), json!(since as u64));
    fields.insert("status_transition".into(), json!(!same_state));
    let claim = publish_latest_claim_tx(
        transaction,
        origin,
        &input.subject,
        &input.kind,
        input.actor.as_deref(),
        &json!(fields),
    )?;
    if input.fields.get("state").and_then(Value::as_str) != Some("working") {
        publish_pending_usage_tx(
            transaction, origin, &input.subject,
            fields.get("incarnation_id").and_then(Value::as_str).unwrap_or(""), now,
        )?;
    }
    Ok(Some(claim))
}

fn usage_native_session(
    connection: &Connection,
    subject: &str,
    fields: &BTreeMap<String, Value>,
) -> Result<Option<String>> {
    if fields.get("semantics").and_then(Value::as_str) != Some("response_rollup") {
        return Ok(None);
    }
    let text = |key| fields.get(key).and_then(Value::as_str).unwrap_or("");
    let binding: Option<String> = connection
        .query_row(
            &canonical_sql(
                "SELECT json_extract(body, '$.fields.session_id') FROM claims
         WHERE subject=?1 AND kind='harness.session-file'
           AND json_extract(body, '$.fields.incarnation_id')=?2
           AND json_extract(body, '$.fields.harness')=?3
         ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ),
            params![subject, text("incarnation_id"), text("driver")],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(binding) = binding.filter(|id| !id.trim().is_empty()) {
        return Ok(Some(binding));
    }
    // The native claim may have been trimmed after this slot first published. Its own
    // published snapshot is still an exact-incarnation carrier, never a current-seat guess.
    let previous: Option<Option<String>> = connection
        .query_row(
            "SELECT json_extract(published_fields, '$.native_session_id') FROM local_latest_slots
         WHERE subject=?1 AND kind='harness.usage' AND slot=?2",
            params![subject, usage_slot(fields)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(previous.flatten().filter(|id| !id.trim().is_empty()))
}

const USAGE_BUCKETS: [&str; 9] = [
    "total_tokens",
    "input_tokens",
    "output_tokens",
    "cache_write_tokens",
    "cache_write_1h_tokens",
    "cached_tokens",
    "cost_microusd",
    "reported_cost_microusd",
    "unpriced_tokens",
];

fn pricing_identity(entry: &Value) -> String {
    json!([
        entry.get("price_table_id"),
        entry.get("price_table_version"),
        entry.get("cost_source"),
        entry.get("rates_usd_per_million_tokens")
    ])
    .to_string()
}

fn usage_slot(fields: &BTreeMap<String, Value>) -> String {
    if fields.get("semantics").and_then(Value::as_str) == Some("response_rollup") {
        // Each rollup key is its own cumulative series. Sharing one slot per incarnation would
        // let a pending rollup of the next step or account replace the previous one's final
        // totals before they replicated.
        return json!([
            fields.get("incarnation_id"),
            fields.get("semantics"),
            fields.get("model"),
            fields.get("account"),
            fields.get("owner_run"),
            fields.get("owner_step"),
            fields.get("host"),
        ])
        .to_string();
    }
    json!([fields.get("incarnation_id"), fields.get("semantics")]).to_string()
}

/// Publish a usage observation when its slot (incarnation and semantics) has no claim yet,
/// its compactions or model changed, this node last replicated the slot at least
/// `USAGE_PUBLISH_INTERVAL_MS` ago, or the harness is not working. Otherwise it stays local
/// and pending until one of those holds or the harness stops working.
fn publish_due_usage_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    input: &ClaimInput,
    local: &ClaimRecord,
    now: u128,
) -> Result<Option<ClaimRecord>, St3Error> {
    let slot = usage_slot(&input.fields);
    let last = transaction
        .query_row(
            "SELECT published_at_unix_ms, published_fields FROM local_latest_slots
             WHERE subject=?1 AND kind='harness.usage' AND slot=?2",
            params![input.subject, slot],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let working = latest_claim_of_kind_tx(transaction, &input.subject, "harness.observed")?
        .is_some_and(|claim| {
            claim.body.pointer("/fields/state").and_then(Value::as_str) == Some("working")
        });
    let due = match &last {
        None => true,
        Some((published_at, published_fields)) => {
            let published: BTreeMap<String, Value> =
                serde_json::from_str(published_fields).unwrap_or_default();
            !working
                || now.saturating_sub((*published_at).max(0) as u128) >= USAGE_PUBLISH_INTERVAL_MS
                || ["compactions", "model"]
                    .iter()
                    .any(|name| published.get(*name) != input.fields.get(*name))
        }
    };
    if !due {
        let local_id = local_observation_position(local)
            .ok_or_else(|| St3Error::new("internal", "a local observation has no position"))?;
        transaction
            .execute(
                "UPDATE local_latest_slots SET pending_local_id=?3
                 WHERE subject=?1 AND kind='harness.usage' AND slot=?2",
                params![input.subject, slot, local_id as i64],
            )
            .map_err(internal)?;
        return Ok(None);
    }
    publish_usage_slot_tx(
        transaction,
        origin,
        &input.subject,
        input.actor.as_deref(),
        &input.fields,
        now,
    )
    .map(Some)
}

fn publish_usage_slot_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    actor: Option<&str>,
    fields: &BTreeMap<String, Value>,
    now: u128,
) -> Result<ClaimRecord, St3Error> {
    let mut fields = fields.clone();
    // Native binding can arrive after the response was captured. Enrich at the scheduled
    // publication (including stop), without adding a publication or changing the slot.
    if !fields.contains_key("native_session_id")
        && let Some(binding) =
            usage_native_session(transaction, subject, &fields).map_err(internal)?
    {
        fields.insert("native_session_id".into(), json!(binding));
    }
    let fields_text = canonical_json_text(&json!(fields)).map_err(internal)?;
    transaction
        .execute(
            "INSERT INTO local_latest_slots(
                subject, kind, slot, published_at_unix_ms, published_fields, pending_local_id
             ) VALUES (?1, 'harness.usage', ?2, ?3, ?4, NULL)
             ON CONFLICT(subject, kind, slot) DO UPDATE SET
                published_at_unix_ms=excluded.published_at_unix_ms,
                published_fields=excluded.published_fields,
                pending_local_id=NULL",
            params![
                subject,
                usage_slot(&fields),
                now.min(i64::MAX as u128) as i64,
                fields_text
            ],
        )
        .map_err(internal)?;
    publish_latest_claim_tx(
        transaction,
        origin,
        subject,
        "harness.usage",
        actor,
        &json!(fields),
    )
}

/// Replicate the newest local usage of each pending slot of `subject`.
fn publish_pending_usage_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    incarnation: &str,
    now: u128,
) -> Result<Vec<ClaimRecord>, St3Error> {
    let pending = {
        let mut statement = transaction
            .prepare(&format!(
                "{LOCAL_OBSERVATION_COLUMNS} WHERE id IN (
                    SELECT pending_local_id FROM local_latest_slots
                    WHERE subject=?1 AND kind='harness.usage' AND pending_local_id IS NOT NULL
                 ) ORDER BY id"
            ))
            .map_err(internal)?;
        let rows = statement
            .query_map([subject], |row| local_observation_from_row(origin, row))
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?
    };
    transaction
        .execute(
            "UPDATE local_latest_slots SET pending_local_id=NULL
             WHERE subject=?1 AND kind='harness.usage'",
            [subject],
        )
        .map_err(internal)?;
    let mut published = pending
        .into_iter()
        .map(|observation| {
            let fields =
                schema_fields_for_body("harness.usage", &observation.body).map_err(internal)?;
            publish_usage_slot_tx(
                transaction,
                origin,
                subject,
                observation.actor.as_deref(),
                &fields,
                now,
            )
        })
        .collect::<Result<Vec<_>, St3Error>>()?;
    // Even a single response can have published before its native binding arrived, leaving
    // no pending token change. Capture that binding in the normal stop flush as well.
    let unbound = {
        let mut statement = transaction
            .prepare(
                "SELECT published_fields FROM local_latest_slots
             WHERE subject=?1 AND kind='harness.usage'
               AND json_extract(published_fields, '$.semantics')='response_rollup'
               AND json_extract(published_fields, '$.incarnation_id')=?2
               AND json_extract(published_fields, '$.native_session_id') IS NULL",
            )
            .map_err(internal)?;
        let rows = statement
            .query_map(params![subject, incarnation], |row| row.get::<_, String>(0))
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?
    };
    for fields in unbound {
        let mut fields: BTreeMap<String, Value> =
            serde_json::from_str(&fields).map_err(internal)?;
        if let Some(binding) =
            usage_native_session(transaction, subject, &fields).map_err(internal)?
        {
            fields.insert("native_session_id".into(), json!(binding));
            published.push(publish_usage_slot_tx(
                transaction,
                origin,
                subject,
                Some(subject),
                &fields,
                now,
            )?);
        }
    }
    Ok(published)
}

fn local_observation_id(origin: &str, id: i64) -> String {
    format!("{LOCAL_OBSERVATION_ID_PREFIX}{origin}/{id}")
}

/// Claims in store order, each local observation after the claim it follows.
pub fn claim_log_order(record: &ClaimRecord) -> (u64, u64) {
    (
        record.store_index,
        local_observation_position(record).unwrap_or(0),
    )
}

fn local_observation_dedupe_key(kind: &str, key: &str) -> String {
    format!(
        "local/{}",
        hex::encode(Sha256::digest(
            [
                b"st3.local-observation.v1\0".as_slice(),
                kind.as_bytes(),
                b"\0",
                key.as_bytes()
            ]
            .concat()
        ))
    )
}

fn local_observation_from_row(
    origin: &str,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ClaimRecord> {
    let id: i64 = row.get(0)?;
    let body: String = row.get(5)?;
    Ok(ClaimRecord {
        id: local_observation_id(origin, id),
        store_index: row.get(1)?,
        batch_id: String::new(),
        subject: row.get(2)?,
        kind: row.get(3)?,
        origin: origin.to_owned(),
        actor: row.get(4)?,
        operation_id: None,
        request_digest: None,
        body: serde_json::from_str(&body).unwrap_or(Value::Null),
        predecessors: Vec::new(),
        accepted_at_unix_ms: row.get::<_, i64>(6)?.max(0) as u128,
    })
}

fn publication_operation_key(subject: &str, action: &str, id: &str) -> String {
    operation_id_for_key(&format!("st3.publish.v1\0{subject}\0{action}\0{id}"))
}

fn publication_operation_digest<T: Serialize>(
    subject: &str,
    action: &str,
    value: &T,
) -> Result<String, St3Error> {
    canonical_hash(&("st3.publish-operation.v1", subject, action, value)).map_err(internal)
}

fn publication_operation_is_new<T: Serialize>(
    connection: &Connection,
    subject: &str,
    action: &str,
    id: &str,
    value: &T,
) -> Result<bool, St3Error> {
    let operation_id = publication_operation_key(subject, action, id);
    let digest = publication_operation_digest(subject, action, value)?;
    match operation_tx(connection, &operation_id).map_err(internal)? {
        None => Ok(true),
        Some((stored, _, state)) if state == "active" && stored == digest => Ok(false),
        Some((_, _, state)) if state == "conflict" => Err(St3Error::new(
            "idempotency-conflict",
            format!("operation `{id}` has conflicting replicated claims"),
        )),
        Some(_) => Err(St3Error::new(
            "immutable-operation-id",
            format!("operation `{id}` repeats with different content"),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_publication_operation_tx<T: Serialize>(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    action: &str,
    id: &str,
    value: &T,
    actor: Option<&str>,
    batch_id: &str,
) -> Result<ClaimRecord, St3Error> {
    let operation_id = publication_operation_key(subject, action, id);
    let request_digest = publication_operation_digest(subject, action, value)?;
    let body = json!({
        "fields": {
            "operation": id,
            "action": action,
            "status": "accepted",
        },
        "_operation": {
            "id": operation_id,
            "request_digest": request_digest,
        },
    });
    let claim = append_claim_tx(
        transaction,
        origin,
        subject,
        "publication.operation",
        actor,
        &body,
        &[],
        Some(batch_id),
    )
    .map_err(internal)?;
    register_operation_tx(transaction, &claim).map_err(internal)?;
    Ok(claim)
}

fn claim_by_id_tx(connection: &Connection, id: &str) -> Result<Option<ClaimRecord>> {
    connection
        .prepare_cached("SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms FROM claims WHERE id=?1")?
        .query_row(
            [id],
            claim_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn launch_lineage_tx(connection: &Connection, subject: &str) -> Result<Vec<String>> {
    let Some(row) = current_desired_row(connection, subject)? else {
        return Ok(Vec::new());
    };
    launch_lineage_from_tx(connection, row.claim_id)
}

fn launch_lineage_from_tx(connection: &Connection, current: String) -> Result<Vec<String>> {
    desired_lineage_from_tx(connection, current, presentation_only_change)
}

fn desired_lineage_from_tx(
    connection: &Connection,
    mut current: String,
    equivalent: fn(&Value, &Value) -> bool,
) -> Result<Vec<String>> {
    let mut lineage = vec![current.clone()];
    while let Some(claim) = claim_by_id_tx(connection, &current)? {
        // A claim that merges concurrent revisions has one predecessor per fork; follow the
        // first one that is still the same launch.
        let mut next = None;
        for predecessor in &claim.predecessors {
            if lineage.contains(predecessor) {
                continue;
            }
            if let Some(previous) = claim_by_id_tx(connection, predecessor)?
                && previous.kind == "intent.desired"
                && previous.subject == claim.subject
                && equivalent(&previous.body, &claim.body)
            {
                next = Some(previous.id);
                break;
            }
        }
        let Some(next) = next else {
            break;
        };
        current.clone_from(&next);
        lineage.push(next);
    }
    Ok(lineage)
}

/// Preserve the existing launch-token boundary for restart budgets and crash-loop recovery.
fn presentation_only_change(previous: &Value, next: &Value) -> bool {
    let parse = |body: &Value| {
        let mut desired = serde_json::from_value::<DesiredSubject>(body.clone()).ok()?;
        (desired.kind == "agent").then_some(())?;
        desired.set_display_name(None).ok()?;
        Some(desired)
    };
    matches!((parse(previous), parse(next)), (Some(previous), Some(next)) if previous == next)
}

/// Keep suspension under exactly the launch comparator used by preview and reconcile.
/// Desired kind and subject are fences, even when their member launch happens to match.
fn same_declared_launch(previous: &Value, next: &Value) -> bool {
    let parse = |body: &Value| {
        let desired = serde_json::from_value::<DesiredSubject>(body.clone()).ok()?;
        (desired.kind == "agent").then_some(())?;
        Some((desired.subject, desired.member?))
    };
    matches!((parse(previous), parse(next)), (Some((previous_subject, previous)), Some((next_subject, next)))
        if previous_subject == next_subject && next.launch_changes(&previous).is_empty())
}

/// The version of the rules that derive `operations` from claims. Every write and projection
/// keeps it current, so a start rebuilds it only when this changes, or for a store no build with
/// this rule has opened. Rebuilding it read every claim and held a start for seconds before the
/// API could answer. The planning tables are small and rebuilt on every start, since a planning
/// claim written through the generic claim path is projected only by a rebuild.
const DERIVED_TABLES_VERSION: &str = "2";

fn rebuild_derived_tables_once_tx(transaction: &Transaction<'_>) -> Result<()> {
    rebuild_planning_tx(transaction)?;
    let built: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key='derived_tables_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if built.as_deref() == Some(DERIVED_TABLES_VERSION) {
        return Ok(());
    }
    rebuild_operations_tx(transaction)?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('derived_tables_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [DERIVED_TABLES_VERSION],
    )?;
    Ok(())
}

fn mark_work_extensions_projected_tx(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('work_extended_projection_rules','1')",
        [],
    )?;
    Ok(())
}

/// Upgrade only run trees with competing creation claims; ordinary runs already fold the same.
fn migrate_occurrence_creation_projections_tx(transaction: &Transaction<'_>) -> Result<()> {
    if transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='occurrence_creation_projection_rules' AND value='1')", [], |row| row.get::<_, bool>(0))? {
        return Ok(());
    }
    let subjects = transaction.prepare("SELECT subject FROM claims WHERE kind='mission-run.created' GROUP BY subject HAVING COUNT(*)>1")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut roots = BTreeSet::new();
    for subject in subjects {
        if let Some(root) = run_tree_of_tx(transaction, &subject).map_err(anyhow::Error::new)? {
            roots.insert(root);
        }
    }
    for root in roots {
        rebuild_run_tree_tx(transaction, &root).map_err(anyhow::Error::new)?;
    }
    transaction.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('occurrence_creation_projection_rules','1')",
        [],
    )?;
    Ok(())
}

fn work_extension_roots_tx(transaction: &Transaction<'_>) -> Result<(BTreeSet<String>, bool)> {
    let subjects = transaction
        .prepare("SELECT DISTINCT subject FROM claims WHERE kind='work.extended'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut roots = BTreeSet::new();
    let mut unresolved = false;
    for subject in subjects {
        if let Some(root) = run_tree_of_tx(transaction, &subject).map_err(anyhow::Error::new)? {
            roots.insert(root);
            // A generation claim can name the owning run before its creation reaches us.
            // The reference alone does not establish that the extension was projected.
            unresolved |= !transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM step_runs WHERE subject=?1)",
                [&subject],
                |row| row.get::<_, bool>(0),
            )?;
        } else {
            unresolved = true;
        }
    }
    Ok((roots, unresolved))
}

/// Older full replays skipped extensions while local writes and run-tree rebuilds applied them.
/// Repair only their owning trees once, preserving local lease renewals and historical checkpoint
/// authority. This does not rebuild unrelated trees or operations.
fn migrate_work_extension_projections_tx(transaction: &Transaction<'_>) -> Result<usize> {
    let current: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='work_extended_projection_rules' AND value='1')",
        [],
        |row| row.get(0),
    )?;
    if current {
        return Ok(0);
    }
    let (roots, _) = work_extension_roots_tx(transaction)?;
    let rebuilt = roots.len();
    for root in roots {
        rebuild_run_tree_tx(transaction, &root).map_err(anyhow::Error::new)?;
    }
    if !work_extension_roots_tx(transaction)?.1 {
        mark_work_extensions_projected_tx(transaction)?;
    }
    Ok(rebuilt)
}

fn rebuild_planning_tx(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute("DELETE FROM planning_previews", [])?;
    transaction.execute("DELETE FROM planning_candidates", [])?;
    transaction.execute("DELETE FROM planning_sessions", [])?;
    let mut statement = transaction.prepare(
        &canonical_sql("SELECT id
         FROM claims WHERE kind >= 'planning-session.' AND kind < 'planning-session/'
         ORDER BY CANONICAL_ASC(claims)"),
    )?;
    let claim_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    clear_quarantined_claims_tx(transaction, "projection:planning")?;
    for id in claim_ids {
        let claim = claim_by_id_tx(transaction, &id)?
            .context("a planning claim disappeared during replay")?;
        // A claim admission accepted but this projection cannot read, such as one from a faulty
        // or older producer, is quarantined alone instead of failing the whole graph.
        project_claim_isolated_tx(transaction, "projection:planning", &claim, || {
            project_planning_claim_tx(transaction, &claim).map_err(|error| {
                St3Error::new("unprojectable-planning-claim", format!("{error:#}"))
            })
        })?;
    }
    Ok(())
}

fn project_planning_claim_tx(transaction: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
    let id = claim
        .subject
        .strip_prefix("planning-session/")
        .context("a planning claim has an invalid subject")?;
    let fields = claim
        .body
        .get("fields")
        .and_then(Value::as_object)
        .context("a planning claim has no fields")?;
    let text = |name: &str| fields.get(name).and_then(Value::as_str);
    let accepted = claim.accepted_at_unix_ms.to_string();
    match claim.kind.as_str() {
        "planning-session.started" => {
            let mission = text("mission")
                .context("planning-session.started has no mission")?
                .strip_prefix("mission/")
                .unwrap_or_else(|| text("mission").expect("the mission was checked"));
            let target_run =
                text("target_run").map(|value| value.strip_prefix("mission-run/").unwrap_or(value));
            let target_generation = text("target_generation")
                .map(|value| value.strip_prefix("run-generation/").unwrap_or(value));
            transaction.execute(
                "INSERT INTO planning_sessions(id, mission_id, request_ref, workspace, requester, planner, planner_spec_json, status, target_run_id, source_generation_id, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planning', ?8, ?9, ?10, ?10)",
                params![
                    id,
                    mission,
                    text("request").context("planning-session.started has no request")?,
                    text("workspace").context("planning-session.started has no workspace")?,
                    text("requester").or(claim.actor.as_deref()).context("planning-session.started has no requester")?,
                    text("planner").context("planning-session.started has no planner")?,
                    fields.get("planner_config").map(serde_json::to_string).transpose()?.unwrap_or_else(|| "{\"provider\":\"codex\"}".into()),
                    target_run,
                    target_generation,
                    accepted,
                ],
            )?;
        }
        "planning-session.candidate-submitted" => {
            let variant = text("variant").unwrap_or("default");
            let revision = fields
                .get("candidate_revision")
                .or_else(|| fields.get("revision"))
                .and_then(Value::as_u64)
                .context("a planning candidate has no revision")?;
            transaction.execute(
                "INSERT INTO planning_candidates(session_id, variant, revision, markdown_ref, kdl_ref, mission_revision, submitted_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    variant,
                    revision,
                    text("markdown").context("a planning candidate has no Markdown")?,
                    text("kdl").context("a planning candidate has no KDL")?,
                    text("mission_revision").context("a planning candidate has no mission revision")?,
                    accepted,
                ],
            )?;
            transaction.execute(
                "DELETE FROM planning_previews WHERE session_id=?1 AND variant=?2",
                params![id, variant],
            )?;
            transaction.execute(
                "UPDATE planning_sessions SET status='review', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, accepted],
            )?;
        }
        "planning-session.previewed" => {
            let variant = text("variant").unwrap_or("default");
            let revision = fields
                .get("candidate_revision")
                .and_then(Value::as_u64)
                .context("a launch preview has no candidate revision")?;
            let mission = fields
                .get("mission")
                .context("a launch preview has no mission response")?;
            transaction.execute(
                "INSERT INTO planning_previews(session_id, variant, candidate_revision, hash, store_index, graph, diff, mission_response, created_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id, variant) DO UPDATE SET candidate_revision=excluded.candidate_revision, hash=excluded.hash,
                   store_index=excluded.store_index, graph=excluded.graph, diff=excluded.diff,
                   mission_response=excluded.mission_response, created_at_unix_ms=excluded.created_at_unix_ms",
                params![
                    id,
                    variant,
                    revision,
                    text("preview_hash").context("a launch preview has no hash")?,
                    fields.get("store_index").and_then(Value::as_u64).context("a launch preview has no store index")?,
                    text("graph").context("a launch preview has no graph")?,
                    text("diff").context("a launch preview has no diff")?,
                    serde_json::to_string(mission)?,
                    accepted,
                ],
            )?;
        }
        "planning-session.revision-requested" => {
            transaction.execute(
                "UPDATE planning_sessions SET status='revision-requested', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, accepted],
            )?;
            transaction.execute("DELETE FROM planning_previews WHERE session_id=?1", [id])?;
        }
        "planning-session.approved" => {
            transaction.execute(
                "UPDATE planning_sessions SET status='approved', published_revision=?2, updated_at_unix_ms=?3 WHERE id=?1",
                params![id, text("mission_revision"), accepted],
            )?;
        }
        "planning-session.cancelled" => {
            transaction.execute(
                "UPDATE planning_sessions SET status='cancelled', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, accepted],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn known_replicated_claim_kind(kind: &str) -> bool {
    matches!(
        kind,
        "intent.desired" | "doc.bound" | "mission.published" | "mission.produced"
    ) || registered_client_claim_kind(kind)
}

/// Claim families that decide where a subject runs and whether it can be reached: its
/// declaration (`intent.*`), its runtime observations and actions (`runtime.*`), and a host's
/// transport (`transport.*`). A replicated claim of a kind this build does not know, in one of
/// these families, could change the answer.
const RUNTIME_AUTHORITY_FAMILIES: &[&str] = &["intent.", "runtime.", "transport."];

/// Whether a replica record this build cannot project could bear on a subject's runtime
/// authority. An invalid record, or an unknown one without a kind, always could. An unknown kind
/// outside the authority families (a newer member's `harness.limits`, say) is kept and waits for
/// an upgrade, but it never makes the subject's reachability indeterminate.
fn replica_record_bears_authority(state: &str, kind: Option<&str>) -> bool {
    state != "unknown"
        || kind.is_none_or(|kind| {
            RUNTIME_AUTHORITY_FAMILIES
                .iter()
                .any(|family| kind.starts_with(family))
        })
}

// Start with the sparse conflict set, then seek each operation's claims. Unary `+` removes
// the TEXT column affinity so SQLite can use the JSON-expression index; operation IDs are
// strings (`smallclaims::store::operation_parts`), so their comparison is unchanged.
const CONFLICT_CLAIM_QUERY: &str =
    "SELECT operations.id FROM operations CROSS JOIN claims INDEXED BY claims_operation_index
     -- Remove TEXT affinity to allow a seek of the JSON-expression index.
     ON +operations.id=json_extract(claims.body, '$._operation.id')
     WHERE operations.state='conflict'
       AND json_extract(claims.body, '$._operation.id') IS NOT NULL
       AND claims.subject=?1 AND claims.store_index<=?2
     ORDER BY operations.id LIMIT 1";

// Seek once per distinct kind; the cut check seeks on the third index term.
const SUBJECT_KINDS_AT_QUERY: &str = "WITH RECURSIVE kinds(kind) AS (
    SELECT (SELECT kind FROM claims INDEXED BY claims_subject_kind_index
            WHERE subject=?1 ORDER BY kind LIMIT 1)
    UNION ALL
    SELECT (SELECT kind FROM claims INDEXED BY claims_subject_kind_index
            WHERE subject=?1 AND kind>kinds.kind ORDER BY kind LIMIT 1)
    FROM kinds WHERE kind IS NOT NULL
)
SELECT kind FROM kinds WHERE kind IS NOT NULL AND EXISTS(
    SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
    WHERE subject=?1 AND kind=kinds.kind AND store_index<=?2
) ORDER BY kind";

fn has_unknown_claim_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Option<String>> {
    if at_index.is_none() {
        let mut statement = connection.prepare_cached(
            "SELECT state, kind_hint FROM replica_records
             WHERE subject_hint=?1 AND state IN ('unknown','invalid')
             ORDER BY record_ref",
        )?;
        let mut rows = statement.query([subject])?;
        while let Some(row) = rows.next()? {
            let (state, kind) = (row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?);
            if replica_record_bears_authority(&state, kind.as_deref()) {
                return Ok(Some(format!(
                    "replica-{state}:{}",
                    kind.unwrap_or_else(|| "unknown-kind".into())
                )));
            }
        }
    }
    let through = at_index.unwrap_or(i64::MAX as u64);
    let conflict = connection
        .prepare_cached(CONFLICT_CLAIM_QUERY)?
        .query_row(params![subject, through], |row| row.get::<_, String>(0))
        .optional()?;
    if let Some(operation) = conflict {
        return Ok(Some(format!("idempotency-conflict:{operation}")));
    }
    // Seek once per distinct kind, then test the cut with the index's third term.
    // DISTINCT would still visit every retained diagnostic of the subject.
    let mut statement = connection.prepare_cached(SUBJECT_KINDS_AT_QUERY)?;
    let kinds = statement
        .query_map(params![subject, through], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(first_unknown_kind(kinds))
}

fn first_unknown_kind(kinds: impl IntoIterator<Item = String>) -> Option<String> {
    kinds.into_iter().find(|kind| !known_replicated_claim_kind(kind))
}

fn validate_message_transition(
    transaction: &Transaction<'_>,
    input: &ClaimInput,
) -> Result<bool, St3Error> {
    let requested = match input.kind.as_str() {
        "message.sent" => "sent",
        "message.staged" => "staged",
        "message.delivered" => "delivered",
        "message.read" => "read",
        "message.closed" => "closed",
        _ => return Ok(false),
    };
    if !input.subject.starts_with("message/") {
        return Err(St3Error::new(
            "invalid-message-subject",
            "a message lifecycle claim needs a `message/` subject",
        ));
    }
    if input.fields.get("status").and_then(Value::as_str) != Some(requested) {
        return Err(St3Error::new(
            "invalid-message-status",
            format!("`{}` needs status `{requested}`", input.kind),
        ));
    }
    if matches!(requested, "read" | "closed") && input.actor.is_none() {
        return Err(St3Error::new(
            "missing-message-actor",
            format!("`{}` needs an actor", input.kind),
        ));
    }

    let current: Option<String> = transaction
        .query_row(
            &canonical_sql("SELECT kind FROM claims WHERE subject=?1 AND kind IN ('message.sent','message.staged','message.delivered','message.read','message.closed') ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            [&input.subject],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    let current = current.as_deref().map(|kind| match kind {
        "message.sent" => "sent",
        "message.staged" => "staged",
        "message.delivered" => "delivered",
        "message.read" => "read",
        "message.closed" => "closed",
        _ => unreachable!("the SQL query selects message lifecycle kinds"),
    });
    let current = if current.is_some() {
        current
    } else {
        let declared: Option<String> = transaction
            .query_row(
                "SELECT kind FROM desired WHERE subject=?1",
                [&input.subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        declared
            .as_deref()
            .filter(|kind| *kind == "message")
            .map(|_| "sent")
    };
    if requested != "sent" && current == Some(requested) {
        if input.fields.contains_key("delegation") {
            return Err(St3Error::new(
                "delegation-refused",
                "this message is already closed; only an identical operation may replay",
            ));
        }
        return Ok(true);
    }
    if matches!(
        (current, requested),
        (Some("delivered" | "read" | "closed"), "staged")
            | (Some("read" | "closed"), "delivered")
            | (Some("closed"), "read")
    ) && input.actor.is_some()
    {
        let index: Option<u64> = transaction
            .query_row("SELECT created_index FROM message_index WHERE subject=?1", [&input.subject], |row| row.get(0))
            .optional().map_err(internal)?;
        if let Some(index) = index {
            let message = message_view_tx(transaction, &input.subject, index).map_err(internal)?;
            if !message.to.is_empty() && input.actor.as_deref() == Some(message.to.as_str()) {
                // Legacy channels also replay receipts after a lost response/reconnect.
                // Settle to existing evidence without admitting a backward lifecycle claim.
                return Ok(true);
            }
        }
    }
    let valid = matches!(
        (current, requested),
        (None, "sent")
            | (Some("sent"), "staged")
            | (Some("staged"), "delivered")
            // Older and non-native transports do not expose a durable staging boundary.
            | (Some("sent"), "delivered")
            | (Some("delivered"), "read")
            | (Some("read"), "closed")
    );
    // A person's displayed message has no native harness handoff. Its explicit read receipt
    // is also delivery evidence; only the actual person recipient may skip that boundary.
    let person_read = if requested == "read"
        && matches!(current, Some("sent" | "staged"))
        && input
            .actor
            .as_deref()
            .is_some_and(|actor| actor.starts_with("person/"))
    {
        let index: Option<u64> = transaction
            .query_row(
                "SELECT created_index FROM message_index WHERE subject=?1",
                [&input.subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        match index {
            Some(index) => {
                message_view_tx(transaction, &input.subject, index)
                    .map_err(internal)?
                    .to
                    .as_str()
                    == input.actor.as_deref().unwrap()
            }
            None => false,
        }
    } else {
        false
    };
    let daemon_withdrawal = matches!(current, Some("sent" | "staged"))
        && requested == "closed"
        && input.actor.as_deref() == Some("daemon/runtime");
    let delegated_close = requested == "closed" && input.fields.contains_key("delegation");
    if !valid && !person_read && !daemon_withdrawal && !delegated_close {
        return Err(St3Error::new(
            "invalid-message-transition",
            format!(
                "message `{}` cannot move from `{}` to `{requested}`",
                input.subject,
                current.unwrap_or("absent")
            ),
        ));
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
pub fn append_claim_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced_batch: Option<&str>,
) -> Result<ClaimRecord> {
    let fields = body.get("fields").unwrap_or(body);
    assistant_write::refuse_unverified(kind, actor, fields.get("from").and_then(Value::as_str),
        ["sender_class", "assistant_grant_id", "assistant_owner", "assistant_session_actor"].iter().any(|key| fields.get(*key).is_some()))
        .map_err(anyhow::Error::new)?;
    // Daemon notifications already inside a larger transaction use the same admission seam
    // as API sends. Replication inserts authenticated history through its separate import path.
    if kind == retirements::KIND {
        return Err(anyhow::Error::new(retirements::writer_required()));
    }
    let admission = |connection: &Connection| {
        retirements::admit_recipient(
            connection,
            body.pointer("/fields/to").and_then(Value::as_str),
        )
    };
    let context = ClaimCommitContext {
        admission: (kind == "message.sent").then_some(&admission as &ClaimAdmission<'_>),
        signature: None,
        ..Default::default()
    };
    if let Some(admission) = context.admission {
        admission(transaction).map_err(anyhow::Error::new)?;
    }
    append_admitted_claim_tx(
        transaction,
        origin,
        subject,
        kind,
        actor,
        body,
        predecessors,
        forced_batch,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_admitted_claim_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced_batch: Option<&str>,
) -> Result<ClaimRecord> {
    if matches!(kind, "intent.desired" | "mission.published") {
        owned_sets::refuse_unmanaged(transaction, subject).map_err(anyhow::Error::new)?;
    }
    crate::terminal_binding::validate_claim(kind, body, actor).map_err(anyhow::Error::new)?;
    st3_schema::accountable::validate_claim(subject, kind, body, actor).map_err(anyhow::Error::new)?;
    st3_schema::owned_terminals::validate_declaration_owner(subject, kind, actor)
        .map_err(anyhow::Error::new)?;
    st3_schema::glasses::validate_owner(subject, actor).map_err(anyhow::Error::new)?;
    st3_schema::arrangements::validate_actor(subject, actor).map_err(anyhow::Error::new)?;
    st3_schema::directive_notes::validate_actor(subject, kind, actor).map_err(anyhow::Error::new)?;
    if kind == "owned-set.revised" {
        owned_sets::validate_receipt(subject, body).map_err(anyhow::Error::new)?;
    }
    let fields = schema_fields_for_body(kind, body)?;
    if kind == "person.delegation-set"
        || fields.contains_key("delegation")
        || fields.contains_key("acted_for")
        || (kind == "work.person-done" && actor.is_some_and(|actor| actor.starts_with("agent/")))
    {
        let evidence = serde_json::from_value::<Vec<String>>(
            body.get("evidence").cloned().unwrap_or_else(|| json!([])),
        )?;
        delegation::validate_write(transaction, subject, kind, actor, &fields, &evidence)
            .map_err(anyhow::Error::new)?;
    }
    let claim_spec = st3_schema::registry()
        .validate_claim(subject, kind, &fields)
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    if kind == st3_schema::directive_notes::KIND {
        directive_notes::validate_publication(transaction, origin).map_err(anyhow::Error::new)?;
    }
    validate_claim_cardinality(
        transaction,
        subject,
        kind,
        actor,
        &fields,
        &claim_spec.cardinality,
    )
    .map_err(anyhow::Error::new)?;
    let record = append_claim_record_tx(
        transaction,
        origin,
        subject,
        kind,
        actor,
        body,
        predecessors,
        forced_batch,
    )?;
    insert_event(transaction, record.store_index, kind, subject, body)?;
    projectors::project_claim(projectors::APPEND, transaction, &record)?;
    normalize_local_projection_timestamps_tx(
        transaction,
        subject,
        kind,
        body,
        record.accepted_at_unix_ms,
    )?;
    Ok(record)
}

#[allow(clippy::too_many_arguments)]
fn append_receipt_claim_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced_batch: Option<&str>,
    key: &str,
) -> Result<ClaimRecord> {
    let mut body = body.clone();
    smallclaims::store::idempotency::attach(&mut body, key)?;
    let record = append_claim_tx(
        transaction,
        origin,
        subject,
        kind,
        actor,
        &body,
        predecessors,
        forced_batch,
    )?;
    register_operation_tx(transaction, &record)?;
    Ok(record)
}

fn mission_run_creation_time(fields: &Value, accepted_at: u128) -> u128 {
    fields
        .get("deadline_at_unix_ms")
        .and_then(Value::as_u64)
        .zip(fields.get("timeout_ms").and_then(Value::as_u64))
        .and_then(|(deadline, timeout)| deadline.checked_sub(timeout))
        .map(u128::from)
        .unwrap_or(accepted_at)
}

/// Local writers often prepare rows before appending their claim. Use the resulting claim's
/// immutable acceptance timestamp, exactly as replay does, for shared creation/change dates.
/// Local lease renewal dates remain separate overlays.
fn normalize_local_projection_timestamps_tx(
    transaction: &Transaction<'_>,
    subject: &str,
    kind: &str,
    body: &Value,
    at: u128,
) -> Result<()> {
    let fields = body.get("fields").unwrap_or(body);
    let at = at.to_string();
    match kind {
        "mission-run.created" => {
            let at = mission_run_creation_time(fields, at.parse()?).to_string();
            transaction.execute(
                "UPDATE mission_runs SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE id=?1",
                params![subject.strip_prefix("mission-run/").unwrap_or(subject), at],
            )?;
            if let Some(generation) = fields.get("current_generation").and_then(Value::as_str) {
                let generation = generation_id_from_subject(generation);
                transaction.execute("UPDATE run_generations SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE id=?1",params![generation,at])?;
                transaction.execute("UPDATE step_runs SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE generation_id=?1",params![generation,at])?;
            }
        }
        "mission-run.state" => {
            transaction.execute(
                "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
                params![subject.strip_prefix("mission-run/").unwrap_or(subject), at],
            )?;
        }
        "run-generation.created" if fields.get("predecessor").and_then(Value::as_str).is_some() => {
            let generation = generation_id_from_subject(subject);
            transaction.execute("UPDATE run_generations SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE id=?1",params![generation,at])?;
            transaction.execute("UPDATE step_runs SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE generation_id=?1",params![generation,at])?;
            transaction.execute(
                "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE current_generation_id=?1",
                params![generation, at],
            )?;
        }
        "run-generation.state" | "run-generation.superseded" => {
            transaction.execute(
                "UPDATE run_generations SET updated_at_unix_ms=?2 WHERE id=?1",
                params![generation_id_from_subject(subject), at],
            )?;
        }
        "revision-proposal.created" => {
            transaction.execute("UPDATE revision_proposals SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE id=?1",
                params![subject.strip_prefix("revision-proposal/").unwrap_or(subject),at])?;
        }
        "revision-proposal.approved"
        | "revision-proposal.cancelled"
        | "revision-proposal.applied" => {
            transaction.execute(
                "UPDATE revision_proposals SET updated_at_unix_ms=?2 WHERE id=?1",
                params![
                    subject
                        .strip_prefix("revision-proposal/")
                        .unwrap_or(subject),
                    at
                ],
            )?;
        }
        "step-run.state" if fields.get("status").and_then(Value::as_str) == Some("ready") => {
            transaction.execute(
                "UPDATE step_runs SET activated_at_unix_ms=?2 WHERE subject=?1",
                params![subject, at],
            )?;
        }
        "planning-session.started" => {
            transaction.execute("UPDATE planning_sessions SET created_at_unix_ms=?2,updated_at_unix_ms=?2 WHERE id=?1",
                params![subject.trim_start_matches("planning-session/"),at])?;
        }
        "planning-session.candidate" => {
            let id = subject.trim_start_matches("planning-session/");
            transaction.execute(
                "UPDATE planning_sessions SET updated_at_unix_ms=?2 WHERE id=?1",
                params![id, at],
            )?;
            transaction.execute("UPDATE planning_candidates SET submitted_at_unix_ms=?4 WHERE session_id=?1 AND variant=?2 AND revision=?3",
                params![id,fields.get("variant").and_then(Value::as_str).unwrap_or("default"),fields.get("candidate_revision").or_else(||fields.get("revision")).and_then(Value::as_u64),at])?;
        }
        "planning-session.previewed" => {
            transaction.execute("UPDATE planning_previews SET created_at_unix_ms=?3 WHERE session_id=?1 AND variant=?2",
                params![subject.trim_start_matches("planning-session/"),fields.get("variant").and_then(Value::as_str).unwrap_or("default"),at])?;
        }
        "planning-session.approved"
        | "planning-session.cancelled"
        | "planning-session.revision-requested" => {
            transaction.execute(
                "UPDATE planning_sessions SET updated_at_unix_ms=?2 WHERE id=?1",
                params![subject.trim_start_matches("planning-session/"), at],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn schema_fields_for_body(kind: &str, body: &Value) -> Result<BTreeMap<String, Value>> {
    if let Some(fields) = body.get("fields").and_then(Value::as_object) {
        return Ok(fields
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect());
    }
    if kind == "intent.desired" {
        let desired: DesiredSubject = serde_json::from_value(body.clone())?;
        let mut fields = BTreeMap::from([
            ("kind".into(), Value::String(desired.kind.clone())),
            ("revision".into(), Value::String(desired_revision(&desired))),
            ("desired".into(), desired.desired),
        ]);
        // Preserve explicit fields when classifying a raw declaration body.
        if let Some(accountable) = body.get("accountable") {
            fields.insert("accountable".into(), accountable.clone());
        }
        return Ok(fields);
    }
    if kind == "mission.published" {
        let mission: MissionSpec = serde_json::from_value(body.clone())?;
        return Ok(BTreeMap::from([
            ("revision".into(), Value::String(mission.revision.clone())),
            ("state".into(), serde_json::to_value(&mission.state)?),
            ("body".into(), body.clone()),
        ]));
    }
    Ok(body
        .as_object()
        .context("a claim body must be an object")?
        .iter()
        .filter(|(name, _)| name.as_str() != "_operation")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

fn cache_local_apply_receipt_tx(
    transaction: &Transaction<'_>,
    response: ApplyResponse,
    local_receipt: Option<(&str, &Value)>,
    primary_key: &str,
) -> Result<ApplyResponse, St3Error> {
    if let Some((key, template)) = local_receipt {
        let mut receipt = template.clone();
        receipt["event_cursor"] = json!(response.store_index);
        transaction
            .execute(
                "INSERT OR IGNORE INTO idempotency(operation_id,response) VALUES(?1,?2)",
                params![
                    opaque_cache_key(key),
                    serde_json::to_string(&receipt).map_err(internal)?
                ],
            )
            .map_err(internal)?;
    }
    mark_replay_safe_receipt_tx(transaction, primary_key).map_err(internal)?;
    mark_replay_safe_receipt_tx(transaction, &format!("owned-set-request:{primary_key}"))
        .map_err(internal)?;
    if let Some((key, _)) = local_receipt {
        mark_replay_safe_receipt_tx(transaction, key).map_err(internal)?;
    }
    Ok(response)
}

/// The lease expiry of a step's newest replicated `work.claimed`, `work.renewed` or
/// `work.progress` claim. Each kind's newest accepted time is one seek of the subject-kind
/// index; only the claims accepted in those milliseconds are put in canonical order, so the read
/// stays the same size however many reports and renewals the step has.
fn last_replicated_lease_query() -> String {
    let newest = |kind: &str| {
        format!(
            "SELECT '{kind}', (SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_kind_accepted_index
                 WHERE subject=?1 AND kind='{kind}'
                 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1)"
        )
    };
    canonical_sql(&format!(
        "WITH newest(kind, accepted) AS ({} UNION ALL {} UNION ALL {})
         SELECT json_extract(claims.body, '$.fields.claim_expires_at_unix_ms')
         FROM newest CROSS JOIN claims INDEXED BY claims_subject_kind_accepted_index
           ON claims.subject=?1 AND claims.kind=newest.kind
          AND length(claims.accepted_at_unix_ms)=length(newest.accepted)
          AND claims.accepted_at_unix_ms=newest.accepted
         ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        newest("work.claimed"),
        newest("work.renewed"),
        newest("work.progress"),
    ))
}

fn mark_replay_safe_receipt_tx(transaction: &Transaction<'_>, key: &str) -> Result<()> {
    transaction.execute(
        "UPDATE idempotency SET replay_safe=1 WHERE operation_id=?1",
        [opaque_cache_key(key)],
    )?;
    Ok(())
}

fn event_tail_sql(migrating: bool) -> &'static str {
    if migrating {
        "SELECT p.store_index,c.kind,p.subject,c.body FROM (
                 SELECT store_index,subject FROM (
                     SELECT store_index,subject FROM event_positions ORDER BY store_index DESC LIMIT ?1
                 ) UNION ALL SELECT store_index,subject FROM (
                     SELECT store_index,subject FROM local_event_payloads
                     ORDER BY store_index DESC LIMIT ?1
                 ) legacy WHERE NOT EXISTS(SELECT 1 FROM event_positions n WHERE n.store_index=legacy.store_index)
                 ORDER BY store_index DESC LIMIT ?1
             ) p CROSS JOIN claims c ON c.store_index=p.store_index ORDER BY p.store_index"
    } else {
        "SELECT p.store_index,c.kind,p.subject,c.body FROM (
                 SELECT store_index,subject FROM event_positions ORDER BY store_index DESC LIMIT ?1
             ) p CROSS JOIN claims c ON c.store_index=p.store_index ORDER BY p.store_index"
    }
}

fn insert_event(
    transaction: &Transaction<'_>,
    store_index: u64,
    _kind: &str,
    subject: &str,
    _body: &Value,
) -> Result<()> {
    if subject.starts_with("glass/") {
        return Ok(());
    }
    transaction.execute(
        "INSERT OR IGNORE INTO event_positions(store_index, subject) VALUES (?1, ?2)",
        params![store_index, subject],
    )?;
    Ok(())
}

fn message_view_tx(
    connection: &Connection,
    subject: &str,
    created_index: u64,
) -> Result<MessageView> {
    let actual = latest_actual(connection, subject)?.unwrap_or(Value::Null);
    let desired = current_desired_row(connection, subject)?
        .and_then(|row| serde_json::from_str::<Value>(&row.body).ok());
    let field = |name: &str| actual.get(name).and_then(Value::as_str).map(str::to_owned);
    let child = |name: &str| {
        desired
            .as_ref()
            .and_then(|value| canonical_child_string(value, name))
    };
    let from = normalize_message_party(
        &field("from")
            .or_else(|| child("from"))
            .unwrap_or_else(|| "requester".into()),
    );
    let to = normalize_message_party(&field("to").or_else(|| child("to")).unwrap_or_default());
    // A replicated claim is another member's word: keep only references a file name can be
    // built from, so a hostile hash never reaches a path or a URL.
    let (attachments, body) = crate::message_body::split_attachments(actual.get("attachments"));
    Ok(MessageView {
        subject: subject.into(),
        from,
        to,
        sender_class: field("sender_class").filter(|class| class == "external-assistant"),
        assistant_grant_id: if actual.get("sender_class").and_then(Value::as_str) == Some("external-assistant") {
            field("assistant_grant_id")
        } else { None },
        content: field("content")
            .or_else(|| child("content"))
            .unwrap_or_default(),
        status: field("status").unwrap_or_else(|| "sent".into()),
        title: field("title").or_else(|| child("title")),
        in_reply_to: field("in_reply_to").or_else(|| child("in-reply-to")),
        tags: actual
            .get("tags")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|tag| !crate::silent::is_remaining_tag(tag))
                    .map(crate::silent::view_tag)
                    .collect()
            })
            .unwrap_or_else(|| {
                desired
                    .as_ref()
                    .map(|value| {
                        canonical_child_strings(value, "tag")
                            .into_iter()
                            .filter(|tag| !crate::silent::is_remaining_tag(tag))
                            .map(|tag| crate::silent::view_tag(&tag))
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        attachments,
        body_ref: body.as_ref().map(|body| crate::message_body::reference(&body.sha256)),
        body_bytes: body.as_ref().map(|body| body.size),
        body_origin: body.map(|body| body.origin),
        created_index,
    })
}

fn latest_actual(connection: &Connection, subject: &str) -> Result<Option<Value>> {
    latest_actual_at(connection, subject, None)
}

fn selected_actual_source_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    desired_host: Option<&str>,
) -> Result<(Option<String>, Option<String>, bool)> {
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    if subject.starts_with("arrangement/") {
        return Ok((arrangements::arrangement_at(connection, subject, at_index)?.and_then(|v| v["revision"].as_str().map(str::to_owned)), None, false));
    }
    // Canonical order, not arrival order, so every node holding these claims selects the same
    // source. Only claims about the subject's actual state can be selected or conflict, so the
    // read leaves out its harness reports.
    let mut statement = connection.prepare_cached(&format!(
        "SELECT claims.id, claims.kind, claims.origin,
                CASE WHEN claims.kind='runtime.observed' THEN claims.body END
         FROM claims INDEXED BY claims_subject_kind_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND {ACTUAL_STATE_CLAIM} AND claims.store_index<=?2
           AND {UNREPAIRED_CLAIM}
         ORDER BY {CANONICAL_ORDER}"
    ))?;
    let rows = statement
        .query_map(params![subject, at_index], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?
                    .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                    .unwrap_or(Value::Null),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let rows = rows
        .iter()
        .map(|(id, kind, origin, body)| (id.as_str(), kind.as_str(), origin.as_str(), body))
        .collect::<Vec<_>>();
    let Some((selected_id, selected_origin, rivals)) = select_actual_source(&rows, desired_host)
    else {
        return Ok((None, None, false));
    };
    // Another host's observation conflicts unless the selected claim descends from it. Only then
    // does the walk need the subject's whole causal history, harness reports included.
    let rivals = rivals.iter().map(String::as_str).collect::<Vec<_>>();
    let runtime_conflict = !rivals.is_empty()
        && !subject_descends_from_all(connection, subject, at_index, &selected_id, &rivals)?;
    Ok((Some(selected_id), Some(selected_origin), runtime_conflict))
}

/// The selected actual-state claim of `(id, kind, origin, runtime body)` rows in canonical
/// order, its origin, and the rival observations it must descend from to hold alone.
fn select_actual_source(
    rows: &[(&str, &str, &str, &Value)],
    desired_host: Option<&str>,
) -> Option<(String, String, Vec<String>)> {
    let selected = rows
        .iter()
        .rev()
        .find(|(_, kind, _, _)| *kind == "runtime.observed")
        .or_else(|| rows.last());
    let &(selected_id, _, selected_origin, selected_body) = selected?;
    // An origin's newer runtime observation supersedes its older observations. In particular,
    // its stop must retire its earlier running claim even when the new owner's intent follows
    // the desired-state branch rather than descending from that runtime branch.
    let mut observed_origins = BTreeSet::new();
    let rivals = rows
        .iter()
        .rev()
        .filter(|(_, kind, origin, _)| {
            *kind == "runtime.observed" && observed_origins.insert(*origin)
        })
        .filter(|(id, _, origin, body)| {
            *id != selected_id
                && *origin != selected_origin
                && !nonowner_terminal_observation(
                    desired_host,
                    selected_origin,
                    selected_body,
                    origin,
                    body,
                )
        })
        .map(|(id, _, _, _)| (*id).to_owned())
        .collect::<Vec<_>>();
    Some((selected_id.to_owned(), selected_origin.to_owned(), rivals))
}

/// Whether claim `descendant` of `subject` descends from every claim of `ancestors`, walking the
/// predecessors of the subject's claims at or before store index `at_index`.
fn subject_descends_from_all(
    connection: &Connection,
    subject: &str,
    at_index: u64,
    descendant: &str,
    ancestors: &[&str],
) -> Result<bool> {
    // A checkpoint may have dropped claims on the path from the selected claim to an older
    // observation. Their tombstones keep the links, so the walk passes through them.
    let mut predecessors = connection
        .prepare_cached("SELECT id, predecessors FROM checkpoint_claims WHERE subject=?1")?
        .query_map([subject], |row| {
            Ok((
                row.get::<_, String>(0)?,
                serde_json::from_str::<Vec<String>>(&row.get::<_, String>(1)?).unwrap_or_default(),
            ))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    predecessors.extend(
        connection
            .prepare_cached(
                "SELECT id, predecessors FROM claims WHERE subject=?1 AND store_index<=?2",
            )?
            .query_map(params![subject, at_index], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    serde_json::from_str::<Vec<String>>(&row.get::<_, String>(1)?)
                        .unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?,
    );
    let descends_from = |ancestor: &str| {
        let mut pending = vec![descendant];
        let mut visited = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if current == ancestor {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(parents) = predecessors.get(current) {
                pending.extend(parents.iter().map(String::as_str));
            }
        }
        false
    };
    Ok(ancestors.iter().all(|ancestor| descends_from(ancestor)))
}

fn nonowner_terminal_observation(
    desired_host: Option<&str>,
    selected_origin: &str,
    selected_body: &Value,
    other_origin: &str,
    other_body: &Value,
) -> bool {
    let selected_fields = selected_body.get("fields").unwrap_or(selected_body);
    let owner = desired_host.or_else(|| {
        if selected_fields.get("status").and_then(Value::as_str) == Some("running") {
            selected_fields.get("host").and_then(Value::as_str)
        } else {
            None
        }
    });
    let other_fields = other_body.get("fields").unwrap_or(other_body);
    owner == Some(selected_origin)
        && other_origin != selected_origin
        && matches!(
            other_fields.get("status").and_then(Value::as_str),
            Some("stopped" | "absent" | "exited" | "vanished")
        )
}

fn latest_actual_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Option<Value>> {
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    if subject.starts_with("arrangement/") {
        return arrangements::arrangement_at(connection, subject, at_index);
    }
    // Folded in canonical order, so two nodes holding the same claims agree however they
    // received them. `NOT LIKE` also leaves out harness kinds in any letter case, as it always
    // has; the ranges let SQLite seek past the subject's harness reports.
    // Only the integer identity and canonical keys enter SQLite's sorter. Keep its cursor
    // open while fetching each body on this same connection: even an unpinned caller then
    // reads both statements from one SQLite read cut, without retaining history bodies.
    let mut statement = connection.prepare_cached(&format!(
        "SELECT claims.store_index
         FROM claims INDEXED BY claims_subject_kind_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND {ACTUAL_STATE_CLAIM}
           AND claims.kind NOT LIKE 'harness.%'
           AND claims.store_index<=?2 AND {UNREPAIRED_CLAIM}
         ORDER BY {CANONICAL_ORDER}"
    ))?;
    let mut rows = statement.query(params![subject, at_index])?;
    let mut bodies = connection
        .prepare_cached("SELECT kind, body FROM claims WHERE store_index=?1")?;
    fold_latest_parsed_values(std::iter::from_fn(|| {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(error) => return Some(Err(error.into())),
        };
        Some((|| {
            let index: u64 = row.get(0)?;
            let (kind, body): (String, String) =
                bodies.query_row([index], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok((kind, serde_json::from_str::<Value>(&body)?))
        })())
    }))
}

/// A subject's actual state from already parsed claims in canonical order. It takes
/// the bodies, so each field moves into the folded state instead of being copied.
fn fold_latest_values(rows: impl IntoIterator<Item = (String, Value)>) -> Result<Option<Value>> {
    fold_latest_parsed_values(rows.into_iter().map(Ok))
}

/// Fold one body at a time, propagating SQL/JSON errors without caching a partial answer.
fn fold_latest_parsed_values(
    rows: impl IntoIterator<Item = Result<(String, Value)>>,
) -> Result<Option<Value>> {
    let mut merged = serde_json::Map::new();
    let registry = st3_schema::registry();
    let mut any = false;
    for row in rows {
        let (kind, value) = row?;
        any = true;
        let source = match value {
            Value::Object(mut body) => match body.remove("fields") {
                Some(fields) => fields,
                None => Value::Object(body),
            },
            other => other,
        };
        if let Value::Object(fields) = source {
            if kind == "resource.observed" {
                resources::merge_observation(&mut merged, &fields);
                continue;
            }
            if registry
                .claim(&kind)
                .is_some_and(|spec| spec.cardinality == st3_schema::Cardinality::StateTransition)
            {
                for field in registry
                    .claim(&kind)
                    .into_iter()
                    .flat_map(|spec| spec.fields.keys())
                {
                    merged.remove(field);
                }
            }
            for (key, value) in fields {
                merged.insert(key, value);
            }
        }
    }
    Ok(any.then_some(Value::Object(merged)))
}

/// `columns` of subject `?1`'s claims of one `kind` at or before store index `?2`, newest first
/// in canonical order. SQLite walks the accepted-time index newest first and sorts only claims
/// accepted in the same millisecond, so a fold that stops early reads only what it uses. Left to
/// itself it reads the kind by store index and sorts every claim of the kind the subject ever
/// had, every observation a seat ever made, before it returns the first.
/// Subject `?1`'s `harness.observed` claims that name incarnation `?3`, at or before store index
/// `?2`, newest first in canonical order.
fn harness_observations_of_incarnation_query() -> String {
    format!(
        "SELECT claims.id, claims.body, claims.accepted_at_unix_ms
         FROM claims INDEXED BY claims_harness_incarnation_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND {INCARNATION_OF_CLAIM}=?3
           AND claims.kind='harness.observed' AND +claims.store_index<=?2
         ORDER BY {CANONICAL_ORDER_DESC}"
    )
}

/// Subject `?1`'s `harness.observed` claims that name no incarnation as text, at or before store
/// index `?2`, accepted no earlier than `?3` (the runtime observation's accepted time).
fn harness_observations_without_incarnation_query() -> String {
    format!(
        "SELECT claims.id, claims.body, claims.accepted_at_unix_ms
         FROM claims INDEXED BY claims_harness_unnamed_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.observed' AND +claims.store_index<=?2
           AND (length(claims.accepted_at_unix_ms)>length(?3)
                OR (length(claims.accepted_at_unix_ms)=length(?3)
                    AND claims.accepted_at_unix_ms>=?3))
           AND ({INCARNATION_OF_CLAIM} IS NULL OR typeof({INCARNATION_OF_CLAIM})!='text')
         ORDER BY {CANONICAL_ORDER_DESC}"
    )
}

fn newest_claims_of_kind_query(columns: &str, kind: &str) -> String {
    // Runtime observations supply live authority and its epoch. Harness observations keep
    // their existing canonical-history contract, including retained repaired originals.
    let membership = if kind == "runtime.observed" {
        UNREPAIRED_CLAIM
    } else {
        "1"
    };
    format!(
        "SELECT {columns}
         FROM claims INDEXED BY claims_subject_kind_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='{kind}' AND +claims.store_index<=?2
           AND {membership}
         ORDER BY {CANONICAL_ORDER_DESC}"
    )
}

fn check_mailbox_incarnation(
    connection: &Connection,
    fence: &crate::mailbox::Fence,
) -> Result<(), St3Error> {
    let runtime: Option<(String, String)> = connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query("claims.id, claims.body", "runtime.observed")
        ))
        .map_err(internal)?
        .query_row(params![fence.subject, i64::MAX], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()
        .map_err(internal)?;
    let (runtime_claim, runtime_body) = runtime.unwrap_or_default();
    let runtime: Value = serde_json::from_str(&runtime_body).unwrap_or(Value::Null);
    let fields = runtime.get("fields").unwrap_or(&runtime);
    let live = fields.get("status").and_then(Value::as_str) == Some("running")
        && fields.get("incarnation_id").and_then(Value::as_str) == Some(&fence.incarnation);
    // A runtime observation that does not name this incarnation describes something older: a
    // predecessor, or the daemon's own "exited" for a seat that died while no daemon watched,
    // which names no incarnation at all. Only an observation of this very incarnation is final.
    let describes_another = fields.get("incarnation_id").and_then(Value::as_str)
        != Some(fence.incarnation.as_str());
    if !live
        && fence.epoch == 0
        && (describes_another
            || matches!(
                fields.get("status").and_then(Value::as_str),
                None | Some("starting")
            ))
    {
        let harness: Option<String> = connection
            .prepare_cached(&format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.body", "harness.observed")
            ))
            .map_err(internal)?
            .query_row(params![fence.subject, i64::MAX], |row| row.get(0))
            .optional()
            .map_err(internal)?;
        if let Some(harness) = harness {
            let harness: Value = serde_json::from_str(&harness).map_err(internal)?;
            let fields = harness.get("fields").unwrap_or(&harness);
            if matches!(
                fields.get("state").and_then(Value::as_str),
                Some("starting" | "ready" | "idle" | "working" | "blocked")
            )
                && fields.get("incarnation_id").and_then(Value::as_str) == Some(&fence.incarnation)
            {
                // The new driver can start while the graph still describes its predecessor,
                // and its hooks can advance before reconciliation publishes runtime.running.
                // Retry without allocating ownership or authorizing any mailbox reads/receipts.
                // Ended and indeterminate observations remain terminal.
                return Err(St3Error::new(
                    "mailbox-session-starting",
                    "waiting for the seat's running incarnation",
                ));
            }
        }
    }
    if !live
        || mailbox_harness_ended(connection, &fence.subject, &fence.incarnation, &runtime_claim)
            .map_err(internal)?
    {
        return Err(St3Error::new(
            "stale-mailbox-session",
            "this is not the seat's live incarnation",
        ));
    }

    Ok(())
}

/// State-only counterpart of the current harness display fold. Keep canonical order, legacy
/// epoch bounds, prompt diagnostics and newer worker activity, without folding display fields.
fn newest_mailbox_harness_state_query() -> String {
    format!(
        "SELECT claims.id, json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.state' ELSE '$.fields.state' END)
         FROM claims INDEXED BY claims_harness_state_incarnation_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE subject=?1 AND kind='harness.observed'
           AND json_type(body, CASE WHEN json_type(body, '$.fields') IS NULL
               THEN '$.state' ELSE '$.fields.state' END)='text'
           AND CASE WHEN json_type(body, CASE WHEN json_type(body, '$.fields') IS NULL
               THEN '$.incarnation_id' ELSE '$.fields.incarnation_id' END)='text'
               THEN {INCARNATION_OF_CLAIM} END IS ?2
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    )
}

fn mailbox_harness_ended(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    runtime_claim: &str,
) -> Result<bool> {
    let runtime_key = canonical::claim_key(connection, runtime_claim)?;
    let mut latest = None;
    for observed_incarnation in [Some(incarnation), None] {
        let observation: Option<(String, String)> = connection
            .prepare_cached(&newest_mailbox_harness_state_query())?
            .query_row(params![subject, observed_incarnation], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some((claim, state)) = observation {
            let key = canonical::claim_key(connection, &claim)?;
            if (observed_incarnation.is_some() || key > runtime_key)
                && latest.as_ref().is_none_or(|(_, current)| key > *current)
            {
                latest = Some((state, key));
            }
        }
    }
    let Some((state, harness_key)) = latest else {
        return Ok(false);
    };
    if state != "ended" {
        return Ok(false);
    }

    if update_prompt_fence(connection, subject, incarnation, i64::MAX as u64)?.is_some() {
        return Ok(false);
    }

    // These positive prompt fences precede the harness state in the display fold. A restored
    // login removes the override. Read them only when they can change an ended decision.
    let diagnostic: Option<String> = connection
        .prepare_cached(&format!(
            "SELECT json_extract(claims.body, '$.fields.code')
         FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.diagnostic'
           AND json_extract(claims.body, '$.fields.incarnation_id')=?2
           AND json_extract(claims.body, '$.fields.code')
               IN ('provider-auth-expired','provider-auth-restored','provider-trust-prompt')
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
        ))?
        .query_row(params![subject, incarnation], |row| row.get(0))
        .optional()?;
    if diagnostic.is_some_and(|code| code != "provider-auth-restored") {
        return Ok(false);
    }

    let work: Option<String> = connection
        .prepare_cached(&canonical_sql(
            "SELECT id FROM claims WHERE actor=?1
           AND kind IN ('work.claimed','work.progress')
           AND json_extract(body, '$.fields.claim_incarnation')=?2
         ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        ))?
        .query_row(params![subject, incarnation], |row| row.get(0))
        .optional()?;
    if let Some(claim) = work {
        let key = canonical::claim_key(connection, &claim)?;
        if key > runtime_key && key > harness_key {
            return Ok(false);
        }
    }
    Ok(true)
}

pub use crate::incremental::{Change, ChangeFeed};

impl crate::otlp::ObservationSource for Store {
    fn export_cursor(&self) -> Result<u64> {
        self.otlp_export_cursor()
    }

    fn observations_after(&self, cursor: u64, limit: usize) -> Result<Vec<ClaimRecord>> {
        self.local_observations_after(cursor, limit)
    }

    fn save_export_cursor(
        self: Arc<Self>,
        cursor: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let store = self.clone();
            self.connection
                .write_workers()
                .spawn_blocking(move || {
                    smallclaims::task_cpu::blocking("observations.export", || {
                        store.set_otlp_export_cursor(cursor)
                    })
                })
                .await??;
            Ok(())
        })
    }
}

impl crate::incremental::ChangeSource for Store {
    fn changes_since(&self, index: u64, local: i64) -> Result<ChangeFeed> {
        Store::changes_since(self, index, local)
    }
}

fn change_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    Ok(Change {
        subject: row.get(0)?,
        kind: row.get(1)?,
        actor: row.get(2)?,
        body: row.get(3)?,
    })
}

/// See [`Store::mailbox_watermark`].
#[derive(Clone, Debug, PartialEq)]
pub struct MailboxWatermark {
    index: u64,
    local: i64,
    owner: Option<(String, i64, bool)>,
    desired: Option<String>,
}

fn check_mailbox_fence(
    connection: &Connection,
    fence: &crate::mailbox::Fence,
    host: &str,
) -> Result<(), St3Error> {
    check_mailbox_incarnation(connection, fence)?;
    mailbox_lease::check_lease_fence(connection, fence, host)?;
    let owner: Option<(String, u64)> = connection
        .prepare_cached(
            "SELECT owner.incarnation, owner.epoch FROM local_mailbox_owners owner
             JOIN local_mailbox_bindings binding ON binding.token=?3
               AND binding.subject=owner.subject AND binding.component=owner.component
               AND binding.incarnation=owner.incarnation AND binding.epoch=owner.epoch
             WHERE owner.subject=?1 AND owner.component=?2",
        )
        .map_err(internal)?
        .query_row(
            params![fence.subject, fence.component, fence.token],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    if owner != Some((fence.incarnation.clone(), fence.epoch)) {
        return Err(St3Error::new(
            "stale-mailbox-session",
            "a newer channel owns this seat",
        ));
    }
    Ok(())
}

fn update_prompt_fence(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    at_index: u64,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let claim = connection.prepare_cached(&format!(
        "SELECT claims.id, claims.body, claims.accepted_at_unix_ms
         FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.diagnostic' AND claims.store_index<=?2
           AND json_extract(claims.body, '$.fields.incarnation_id')=?3
           AND json_extract(claims.body, '$.fields.code') IN ('provider-update-prompt','provider-update-restored')
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    ))?.query_row(params![subject, at_index, incarnation], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    }).optional()?;
    let Some((claim, body, at)) = claim else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body)?;
    let fields = &body["fields"];
    if fields["code"] != "provider-update-prompt" {
        return Ok(None);
    }
    let text = |name: &str| fields[name].as_str().map(str::to_owned);
    Ok(Some(crate::model::CurrentHarnessView {
        state: "blocked".into(),
        driver: text("driver"),
        incarnation_id: incarnation.into(),
        transport: Some("native".into()),
        reason: text("reason"),
        blocked_on: Some("human".into()),
        ask: None,
        input_buffer: None,
        exit: None,
        claim,
        since_unix_ms: at.parse()?,
        observed_at_unix_ms: at.parse()?,
    }))
}

fn current_harness_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let mut view = current_harness_fold_at(connection, subject, at_index, true, false)?;
    if let Some(harness) = view.as_mut() {
        seat_status::enrich_harness(connection, subject, at_index, harness)?;
    }
    Ok(view)
}

fn claude_attachment_query() -> String {
    format!(
        "SELECT claims.id, claims.body, claims.accepted_at_unix_ms
         FROM claims INDEXED BY claims_claude_attachment_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.diagnostic' AND claims.store_index<=?2
           AND {INCARNATION_OF_CLAIM}=?3
           AND json_extract(claims.body, '$.fields.code') IN ('claude-channel-unattached','claude-channel-attached')
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    )
}

/// Channel readiness is independent of hook activity. A current driver owns this fence;
/// MCP initialization under its delivery binding, or runtime replacement, clears it.
fn claude_attachment_fence(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    at_index: u64,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let claim = connection.prepare_cached(&claude_attachment_query())?.query_row(params![subject, at_index, incarnation], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    }).optional()?;
    let Some((claim, body, at)) = claim else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body)?;
    if body["fields"]["code"] == "claude-channel-attached" {
        return Ok(None);
    }
    let starting = body["fields"]["status"] == "starting";
    Ok(Some(crate::model::CurrentHarnessView {
        state: if starting { "starting" } else { "blocked" }.into(),
        driver: Some("claude".into()),
        incarnation_id: incarnation.into(),
        transport: Some("claude-channel".into()),
        reason: Some("claude-channel-unattached".into()),
        blocked_on: Some("channel".into()),
        ask: None,
        input_buffer: None,
        exit: None,
        claim,
        since_unix_ms: at.parse()?,
        observed_at_unix_ms: at.parse()?,
    }))
}

/// Whether `subject` has any claim of login evidence in this runtime epoch: a claim in
/// `claims_harness_login_candidate_index` (a `provider_auth` of false, a reason of `providerAuth`,
/// a state of `needs-login`, or a `provider-auth-expired` diagnostic) that names `incarnation_id`,
/// or that names no incarnation as text and was accepted no earlier than the runtime observation
/// (`runtime_accepted_at`), the same two sets the fold reads. Every way the fold can end in
/// `needs-login` rests on such a claim: a credential report of false or an expired-login
/// diagnostic for this incarnation, or an observation of the epoch whose reason is `providerAuth`
/// or whose own state is `needs-login`. Today's claim validation refuses the state `needs-login`
/// in an observation, so only claims written before that rule can carry it; the probe still asks,
/// so a store holding one keeps raising its login item. It reads only that small partial index,
/// not every observation the seat ever made.
fn login_evidence_in_epoch(
    connection: &Connection,
    subject: &str,
    at_index: u64,
    incarnation_id: &str,
    runtime_accepted_at: &str,
) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_harness_login_candidate_index
           WHERE claims.subject=?1 AND (
             (kind='harness.observed' AND (
               json_type(body, '$.fields.provider_auth')='false'
               OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                   THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
               OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                   THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
             OR (kind='harness.diagnostic'
               AND json_extract(body, '$.fields.code')='provider-auth-expired'))
           AND store_index<=?2
           AND ({INCARNATION_OF_CLAIM}=?3
                OR (({INCARNATION_OF_CLAIM} IS NULL OR typeof({INCARNATION_OF_CLAIM})!='text')
                    AND (length(claims.accepted_at_unix_ms)>length(?4)
                         OR (length(claims.accepted_at_unix_ms)=length(?4)
                             AND claims.accepted_at_unix_ms>=?4)))))"
    );
    Ok(connection
        .prepare_cached(&sql)?
        .query_row(params![subject, at_index, incarnation_id, runtime_accepted_at], |row| {
            row.get(0)
        })?)
}

/// `login_only` answers only whether the seat needs a login: it returns at once, before any
/// fence, when the seat has no claim of login evidence in this runtime epoch
/// ([`login_evidence_in_epoch`]). Every view it returns otherwise is the one the full fold would
/// return, and the view it skips is never `needs-login`.
fn current_harness_fold_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    include_work_activity: bool,
    login_only: bool,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    let runtime = connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query("claims.id, claims.body", "runtime.observed")
        ))?
        .query_row(params![subject, at_index], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    let Some((runtime_claim, runtime_body)) = runtime else {
        return Ok(None);
    };
    let runtime_key = canonical::claim_key(connection, &runtime_claim)?;
    let runtime_body: Value = serde_json::from_str(&runtime_body)?;
    let runtime_fields = runtime_body.get("fields").unwrap_or(&runtime_body);
    let Some(incarnation_id) = runtime_fields.get("incarnation_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if login_only
        && !login_evidence_in_epoch(connection, subject, at_index, incarnation_id, &runtime_key.0.to_string())?
    {
        return Ok(None);
    }

    // An admission refusal remains visible even after omp exits, and OpenCode's ready
    // observations cannot claim healthy delivery while its exact build failed the probe.
    let admission = connection
        .prepare_cached(&canonical_sql(
            "SELECT id, accepted_at_unix_ms, json_extract(body, '$.fields.reason') FROM claims
         WHERE subject=?1 AND kind='harness.diagnostic' AND store_index<=?2
           AND json_extract(body, '$.fields.code')='harness-admission-failed'
           AND json_extract(body, '$.fields.incarnation_id')=?3
         ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        ))?
        .query_row(params![subject, at_index, incarnation_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?;
    if let Some((claim, observed_at, reason)) = admission {
        return Ok(Some(crate::model::CurrentHarnessView {
            state: "indeterminate".into(),
            driver: None,
            incarnation_id: incarnation_id.into(),
            transport: None,
            reason: Some(reason),
            blocked_on: None,
            ask: None,
            input_buffer: None,
            exit: None,
            claim,
            since_unix_ms: observed_at.parse()?,
            observed_at_unix_ms: observed_at.parse()?,
        }));
    }
    if runtime_fields.get("status").and_then(Value::as_str) != Some("running") {
        return Ok(None);
    }

    // A terminal modal holds even if a parallel native channel reports idle or work progress.
    // Only a successful subsequent screen observation or a new runtime lifts this fence.
    if let Some(harness) = update_prompt_fence(connection, subject, incarnation_id, at_index)? {
        return Ok(Some(harness));
    }

    // A native credential refusal is independent of activity, and work claims cannot erase it.
    let auth = connection.prepare_cached(&canonical_sql(
        "SELECT id, accepted_at_unix_ms, body FROM claims INDEXED BY claims_harness_auth_incarnation_index WHERE subject=?1 AND kind='harness.observed'
         AND +store_index<=?2 AND json_extract(body, '$.fields.incarnation_id')=?3
         AND json_type(body, '$.fields.provider_auth') IN ('true','false')
         ORDER BY CANONICAL_DESC(claims) LIMIT 1"))?.query_row(params![subject, at_index, incarnation_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
        .optional()?;
    let mut auth_restored = false;
    if let Some((claim, time, body)) = auth {
        let body: Value = serde_json::from_str(&body)?;
        let fields = &body["fields"];
        auth_restored = fields["provider_auth"] == true;
        if fields["provider_auth"] == false {
            return Ok(Some(crate::model::CurrentHarnessView {
                state: "needs-login".into(),
                driver: fields["driver"].as_str().map(str::to_owned),
                incarnation_id: incarnation_id.into(),
                transport: fields["transport"].as_str().map(str::to_owned),
                reason: Some("providerAuth".into()),
                blocked_on: Some("human".into()),
                ask: None,
                input_buffer: None,
                exit: None,
                claim,
                since_unix_ms: time.parse()?,
                observed_at_unix_ms: time.parse()?,
            }));
        }
    }

    // A login prompt or a workspace trust prompt is positive evidence that the current Claude
    // incarnation cannot accept work. Neither has a hook edge, and channel initialization or work
    // activity can otherwise overwrite a one-off blocked observation. Fence the entire incarnation
    // instead.
    let prompt_rejection = connection
        .prepare_cached(&format!(
            "SELECT claims.id, claims.accepted_at_unix_ms, json_extract(claims.body, '$.fields.code'), claims.body
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='harness.diagnostic'
               AND claims.store_index<=?2
               AND json_extract(claims.body, '$.fields.code')
                   IN ('provider-auth-expired', 'provider-auth-restored', 'provider-trust-prompt')
               AND json_extract(claims.body, '$.fields.incarnation_id')=?3
             ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
        ))?
        .query_row(
            params![subject, at_index, incarnation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    // A lifted login fence no longer holds the incarnation.
    if let Some((claim, observed_at_unix_ms, code, body)) = prompt_rejection
        && code != "provider-auth-restored"
    {
        let (state, reason) = if code == "provider-trust-prompt" {
            ("blocked", "providerTrustPrompt")
        } else {
            ("needs-login", "providerAuth")
        };
        let body: Value = serde_json::from_str(&body)?;
        let fields = &body["fields"];
        let driver = fields["driver"].as_str().unwrap_or("claude");
        return Ok(Some(crate::model::CurrentHarnessView {
            state: state.into(),
            driver: Some(driver.into()),
            incarnation_id: incarnation_id.to_owned(),
            transport: Some(if driver == "claude" { "claude-channel" } else { "native" }.into()),
            reason: Some(reason.into()),
            blocked_on: Some("human".into()),
            ask: None,
            input_buffer: None,
            exit: None,
            claim,
            since_unix_ms: observed_at_unix_ms.parse::<u128>()?,
            observed_at_unix_ms: observed_at_unix_ms.parse::<u128>()?,
        }));
    }

    if let Some(harness) = claude_attachment_fence(connection, subject, incarnation_id, at_index)? {
        return Ok(Some(harness));
    }

    // The observations of this runtime epoch, newest first in canonical order, so every node that
    // holds the same claims shows the same harness state: those that name this incarnation, and
    // those that name none and follow the runtime observation. Only those are read. Reading every
    // observation the seat ever made, with a canonical key lookup for each, cost a seat whose
    // current incarnation had reported nothing yet thousands of reads on every status and pass.
    let row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(String, String, String)> {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    };
    let mut unnamed = connection
        .prepare_cached(&harness_observations_without_incarnation_query())?
        .query_map(params![subject, at_index, runtime_key.0.to_string()], row)?
        .map(|row| {
            smallclaims::read_budget::check()?;
            let (claim, body, observed_at_unix_ms) = row?;
            let key = canonical::claim_key(connection, &claim)?;
            Ok::<_, anyhow::Error>((key, claim, body, observed_at_unix_ms))
        })
        .filter(|row| row.as_ref().map_or(true, |(key, ..)| *key > runtime_key))
        .collect::<Result<Vec<_>>>()?;
    unnamed.sort_by(|a, b| b.0.cmp(&a.0));
    let mut unnamed = unnamed.into_iter().peekable();
    let mut statement = connection.prepare_cached(&harness_observations_of_incarnation_query())?;
    let mut named = statement.query_map(params![subject, at_index, incarnation_id], row)?;
    let mut next_named = None;
    let mut current = None;
    let mut optional = BTreeMap::<&'static str, Option<String>>::new();
    loop {
        smallclaims::read_budget::check()?;
        if next_named.is_none() {
            next_named = match named.next() {
                Some(row) => {
                    let (claim, body, observed_at_unix_ms) = row?;
                    Some((
                        None,
                        claim,
                        body,
                        observed_at_unix_ms,
                    ))
                }
                None => None,
            };
        }
        // SQL already streams named observations in canonical order. Looking their keys up
        // again adds one query per sparse heartbeat; only the merge with an unnamed row
        // needs a key here. A work overlay resolves the selected key below if necessary.
        let take_unnamed = match (next_named.as_mut(), unnamed.peek()) {
            (Some((named_key, claim, ..)), Some((unnamed_key, ..))) => {
                let key = match named_key {
                    Some(key) => key,
                    None => named_key.insert(canonical::claim_key(connection, claim)?),
                };
                unnamed_key > key
            }
            (None, Some(_)) => true,
            (_, None) => false,
        };
        let next = if take_unnamed {
            unnamed.next().map(|(key, claim, body, at)| (Some(key), claim, body, at))
        } else {
            next_named.take()
        };
        let Some((key, claim, body, observed_at_unix_ms)) = next else {
            break;
        };
        let observed_at_unix_ms = observed_at_unix_ms.parse::<u128>()?;
        let body: Value = serde_json::from_str(&body)?;
        let fields = body.get("fields").unwrap_or(&body);
        if current.is_none()
            && let Some(state) = fields.get("state").and_then(Value::as_str)
        {
            current = Some((state.to_owned(), claim, observed_at_unix_ms, key));
        }
        if matches!(fields.get("state").and_then(Value::as_str), Some("ended" | "indeterminate")) {
            optional.entry("blocked_on").or_insert(None);
            optional.entry("ask").or_insert(None);
        }
        for name in [
            "driver",
            "transport",
            "reason",
            "blocked_on",
            "ask",
            "input_buffer",
            "exit",
        ] {
            if !optional.contains_key(name)
                && let Some(value) = fields.get(name)
            {
                optional.insert(name, value.as_str().map(str::to_owned));
            }
        }
        // Newer observations carry a complete snapshot, including explicit nulls.
        // Once every field and the latest state are known, older observations cannot
        // affect this view. Sparse legacy observations still fall through to history.
        if current.is_some() && optional.len() == 7 {
            break;
        }
    }
    let work_activity = if include_work_activity { connection
        .query_row(
            &canonical_sql(
                "SELECT id, store_index, accepted_at_unix_ms FROM claims
             WHERE actor=?1 AND store_index<=?2
               AND kind IN ('work.claimed','work.progress')
               AND json_extract(body, '$.fields.claim_incarnation')=?3
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ),
            params![subject, at_index, incarnation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(claim, store_index, observed_at_unix_ms)| {
            let key = canonical::claim_key(connection, &claim)?;
            Ok::<_, anyhow::Error>((claim, store_index, observed_at_unix_ms, key))
        })
        .transpose()?
    } else { None };
    let harness_key = match (&work_activity, current.as_ref()) {
        (Some(_), Some((_, claim, _, key))) => Some(match key {
            Some(key) => key.clone(),
            None => canonical::claim_key(connection, claim)?,
        }),
        _ => None,
    };
    if let Some((claim, _store_index, observed_at_unix_ms, key)) = work_activity
        && include_work_activity
        && key > runtime_key
        && !seat_status::permission_blocked(
            current.as_ref().map(|(state, _, _, _)| state.as_str()),
            optional.get("blocked_on").and_then(|v| v.as_deref()),
            optional.get("ask").and_then(|v| v.as_deref()),
        )
        && harness_key.as_ref().is_none_or(|harness_key| key > *harness_key)
    {
        return Ok(Some(crate::model::CurrentHarnessView {
            state: "working".into(),
            driver: optional.remove("driver").flatten(),
            incarnation_id: incarnation_id.to_owned(),
            transport: optional.remove("transport").flatten(),
            reason: None,
            blocked_on: None,
            ask: None,
            input_buffer: None,
            exit: None,
            claim,
            since_unix_ms: observed_at_unix_ms.parse::<u128>()?,
            observed_at_unix_ms: observed_at_unix_ms.parse::<u128>()?,
        }));
    }
    let Some((mut state, claim, observed_at_unix_ms, _)) = current else {
        return Ok(None);
    };
    if seat_status::permission_blocked(
        Some(state.as_str()),
        optional.get("blocked_on").and_then(|v| v.as_deref()),
        optional.get("ask").and_then(|v| v.as_deref()),
    ) {
        state = "blocked".into();
    } else if matches!(state.as_str(), "ended" | "indeterminate") {
        optional.insert("blocked_on", None);
        optional.insert("ask", None);
    }
    if optional.get("reason").and_then(|r| r.as_deref()) == Some("providerAuth") {
        if auth_restored {
            // Sparse successful reports must not inherit an older credential-refusal reason.
            optional.insert("reason", None);
        } else {
            state = "needs-login".into();
        }
    }
    Ok(Some(crate::model::CurrentHarnessView {
        state,
        driver: optional.remove("driver").flatten(),
        incarnation_id: incarnation_id.to_owned(),
        transport: optional.remove("transport").flatten(),
        reason: optional.remove("reason").flatten(),
        blocked_on: optional.remove("blocked_on").flatten(),
        ask: optional.remove("ask").flatten(),
        input_buffer: optional.remove("input_buffer").flatten(),
        exit: optional.remove("exit").flatten(),
        claim,
        since_unix_ms: observed_at_unix_ms,
        observed_at_unix_ms,
    }))
}

fn working_episode_query() -> String {
    format!(
        "SELECT claims.id, json_extract(claims.body, '$.fields.state'), claims.accepted_at_unix_ms
         FROM claims INDEXED BY claims_incarnation_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.observed'
           AND {INCARNATION_OF_CLAIM}=?2
           AND json_extract(claims.body, '$.fields.incarnation_id')=?2
           AND +claims.store_index<=?3
         ORDER BY length(claims.accepted_at_unix_ms) DESC, claims.accepted_at_unix_ms DESC"
    )
}

fn agent_working_since_at(
    connection: &Connection,
    agent: &str,
    incarnation: &str,
    snapshot_index: u64,
) -> Result<Option<u128>> {
    // Stream accepted-time groups backwards. Resolve complete canonical keys only for
    // ties, before applying the state fold. Non-working ends the episode; null does not.
    // Tie lookups must share the stream's cut, including for standalone callers.
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let since = (|| -> Result<Option<String>> {
        let mut statement = connection.prepare_cached(&working_episode_query())?;
        let mut rows = statement.query_map(params![agent, incarnation, snapshot_index], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut group = Vec::<(String, Option<String>, String)>::new();
        let mut since = None;
        let fold_group = |group: &mut Vec<(String, Option<String>, String)>,
                          since: &mut Option<String>|
         -> Result<bool> {
            if group.len() > 1 {
                let mut keyed = group
                    .drain(..)
                    .map(|row| Ok((canonical::claim_key(connection, &row.0)?, row)))
                    .collect::<Result<Vec<_>>>()?;
                keyed.sort_by(|a, b| b.0.cmp(&a.0));
                group.extend(keyed.into_iter().map(|(_, row)| row));
            }
            for (_, state, time) in group.drain(..) {
                match state.as_deref() {
                    Some("working") => *since = Some(time),
                    Some(_) => return Ok(true),
                    None => {}
                }
            }
            Ok(false)
        };
        for row in &mut rows {
            smallclaims::read_budget::check()?;
            let row = row?;
            if group.last().is_some_and(|previous| previous.2 != row.2)
                && fold_group(&mut group, &mut since)?
            {
                return Ok(since);
            }
            group.push(row);
        }
        fold_group(&mut group, &mut since)?;
        Ok(since)
    })()?;
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(since.and_then(|time| time.parse().ok()))
}

fn claim_ids_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Vec<String>> {
    claim_ids_at_after_first_row(connection, subject, at_index, || {})
}

fn claim_ids_at_after_first_row(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    after_first_row: impl FnOnce(),
) -> Result<Vec<String>> {
    // The old single statement held one cut. Tie lookups must retain that cut too,
    // while writer and pinned-read callers keep ownership of their transaction.
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    let mut ids = Vec::new();
    {
        let mut statement = connection.prepare_cached(
            "SELECT id, accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index
             WHERE subject=?1 AND store_index<=?2
             ORDER BY length(accepted_at_unix_ms), accepted_at_unix_ms",
        )?;
        let mut rows = statement.query_map(params![subject, at_index], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let first = rows.next().transpose()?;
        if first.is_some() {
            after_first_row();
        }
        let mut ties = Vec::<(String, String)>::new();
        let mut flush = |ties: &mut Vec<(String, String)>| -> Result<()> {
            if ties.len() > 1 {
                let mut keyed = ties
                    .drain(..)
                    .map(|row| Ok((canonical::claim_key(connection, &row.0)?, row)))
                    .collect::<Result<Vec<_>>>()?;
                keyed.sort_by(|a, b| a.0.cmp(&b.0));
                ties.extend(keyed.into_iter().map(|(_, row)| row));
            }
            ids.extend(ties.drain(..).map(|(id, _)| id));
            Ok(())
        };
        for row in first.into_iter().map(Ok).chain(rows) {
            let row = row?;
            if ties.last().is_some_and(|previous| previous.1 != row.1) {
                flush(&mut ties)?;
            }
            ties.push(row);
        }
        flush(&mut ties)?;
    }
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(ids)
}

/// The ids sharing one accepted time at the cut, in canonical order. Canonical order inside a
/// single accepted time is decided by the whole claim key, so exactly this group — never the
/// subject's entire history — needs key-by-key resolution.
fn canonical_tie_group_at(
    connection: &Connection,
    subject: &str,
    at_index: u64,
    accepted_at_unix_ms: &str,
) -> Result<Vec<String>> {
    let mut statement = connection.prepare_cached(
        "SELECT id FROM claims INDEXED BY claims_subject_accepted_index
         WHERE subject=?1 AND store_index<=?2 AND accepted_at_unix_ms=?3",
    )?;
    let ids = statement
        .query_map(params![subject, at_index, accepted_at_unix_ms], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut keyed = ids
        .into_iter()
        .map(|id| Ok((canonical::claim_key(connection, &id)?, id)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(keyed.into_iter().map(|(_, id)| id).collect())
}

/// The `window`-th oldest canonical claim id, or `None` while the subject has `window` claims
/// or fewer at the cut. Streams the accepted-time index up to the window and resolves
/// canonical keys only for the accepted-time group the window boundary falls inside: every
/// earlier group canonically precedes that group whole, so only the offset within it matters.
fn window_boundary_at(
    connection: &Connection,
    subject: &str,
    at_index: u64,
    window: usize,
) -> Result<Option<String>> {
    let mut statement = connection.prepare_cached(
        "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index
         WHERE subject=?1 AND store_index<=?2
         ORDER BY length(accepted_at_unix_ms), accepted_at_unix_ms LIMIT ?3",
    )?;
    let times = statement
        .query_map(params![subject, at_index, window], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if times.len() < window {
        return Ok(None);
    }
    let boundary_time = times.last().expect("a full window has a last time");
    let within_window = times
        .iter()
        .filter(|time| time.as_str() == boundary_time)
        .count();
    let group = canonical_tie_group_at(connection, subject, at_index, boundary_time)?;
    Ok(group.get(within_window - 1).cloned())
}

/// The bounded claims summary [`ClaimsReduction::Summary`] computes instead of the full
/// canonical id vector: the canonically-latest id, the claim count at the cut, and the
/// session-dating window's last canonical id once the count passes it. Each answer reads the
/// subject's accepted-time index once, and canonical tie keys are resolved only for the top
/// tie group and the group the window boundary falls inside, so the cost no longer follows
/// the subject's history beyond the count and window index scans. The statements share one
/// read cut, exactly like `claim_ids_at`.
fn claims_summary_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<ClaimsSummary> {
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    let count = usize::try_from(
        connection
            .prepare_cached(
                "SELECT COUNT(*) FROM claims INDEXED BY claims_subject_accepted_index
                 WHERE subject=?1 AND store_index<=?2",
            )?
            .query_row(params![subject, at_index], |row| row.get::<_, i64>(0))?,
    )?;
    // The canonically-first claim: the session list's fallback date for a subject without any
    // other evidence of when it started.
    let first = if count == 0 {
        None
    } else {
        let oldest = connection
            .prepare_cached(
                "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index
                 WHERE subject=?1 AND store_index<=?2
                 ORDER BY length(accepted_at_unix_ms), accepted_at_unix_ms LIMIT 1",
            )?
            .query_row(params![subject, at_index], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        oldest
            .map(|time| {
                canonical_tie_group_at(connection, subject, at_index, &time)
                    .map(|group| group.into_iter().next())
            })
            .transpose()?
            .flatten()
    };
    let latest = if count == 0 {
        None
    } else {
        let top = connection
            .prepare_cached(
                "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_subject_accepted_index
                 WHERE subject=?1 AND store_index<=?2
                 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1",
            )?
            .query_row(params![subject, at_index], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        top.map(|time| {
            canonical_tie_group_at(connection, subject, at_index, &time)
                .map(|mut group| group.pop())
        })
        .transpose()?
        .flatten()
    };
    let session_boundary = if count > SESSION_CLAIMS {
        window_boundary_at(connection, subject, at_index, SESSION_CLAIMS)?
    } else {
        None
    };
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(ClaimsSummary {
        first,
        latest,
        count,
        session_boundary,
    })
}

fn desired_conflicts_at(
    connection: &Connection,
    subject: &str,
    winner: Option<&str>,
    at_index: Option<u64>,
) -> Result<Vec<String>> {
    if let Some(conflicts) = owned_sets::conflicts_at(connection, subject, at_index)? {
        return Ok(conflicts);
    }
    let at_index = at_index.unwrap_or(i64::MAX as u64);
    let mut statement = connection.prepare_cached(&canonical_sql(
        "SELECT id, predecessors FROM claims
         WHERE subject=?1 AND kind='intent.desired' AND store_index<=?2
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let rows = statement
        .query_map(params![subject, at_index], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let referenced = rows
        .iter()
        .flat_map(|(_, predecessors)| {
            serde_json::from_str::<Vec<String>>(predecessors).unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    Ok(rows
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !referenced.contains(id) && Some(id.as_str()) != winner)
        .collect())
}

fn intent_leaves_tx(transaction: &Transaction<'_>, subject: &str) -> Result<Vec<String>> {
    let mut statement = transaction.prepare(
        "SELECT id, predecessors FROM claims
         WHERE subject=?1 AND kind='intent.desired'
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY id",
    )?;
    let rows = statement
        .query_map([subject], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let referenced = rows
        .iter()
        .flat_map(|(_, predecessors)| {
            serde_json::from_str::<Vec<String>>(predecessors).unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    Ok(rows
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !referenced.contains(id))
        .collect())
}

fn mission_definition_token_tx(
    transaction: &Transaction<'_>,
    mission_id: &str,
) -> Result<Vec<String>> {
    Ok(transaction
        .query_row(
            "SELECT claim_id FROM mission_definitions WHERE mission_id=?1",
            [mission_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .into_iter()
        .collect())
}

fn desired_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DesiredSubject> {
    let desired = row.get::<_, String>(2)?;
    let desired = serde_json::from_str(&desired).unwrap_or(Value::Null);
    let accountable = st3_schema::accountable::from_desired(&desired)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
            2, rusqlite::types::Type::Text, Box::new(error)))?
        .map(str::to_owned);
    let member = row.get::<_, Option<String>>(3)?;
    Ok(DesiredSubject {
        accountable,
        subject: row.get(0)?,
        kind: row.get(1)?,
        desired,
        member: member.and_then(|value| serde_json::from_str(&value).ok()),
        owner_run: row.get(4)?,
        owner_generation: row.get(5)?,
        owner_step: row.get(6)?,
    })
}

fn current_human_review(
    connection: &Connection,
    request: ClaimRecord,
) -> Result<Option<HumanReviewView>> {
    Ok(human_review_currency(connection, request)?.ok())
}

/// The step that runs a loop, from the subject a loop's gates are asked of:
/// `loop-run/GENERATION/PATH`, or a subject under it such as `.../round/N/candidate/M` for a
/// person's choice between candidates. The longest step run that prefixes it is the loop's.
fn loop_step_tx(connection: &Connection, owner: &str) -> Result<Option<StepRunView>> {
    let Some(path) = owner.strip_prefix("loop-run/") else {
        return Ok(None);
    };
    let mut prefix = path;
    // GENERATION/PATH: a step run has at least two segments.
    while prefix.contains('/') {
        if let Some(step) = step_run_row_tx(connection, &format!("step-run/{prefix}"))? {
            return Ok(Some(step));
        }
        prefix = prefix.rsplit_once('/').map_or("", |(head, _)| head);
    }
    Ok(None)
}

/// Item `item` of the for-each loop `loop_run` (a loop from before such loops were removed):
/// the round it runs in, its place in the loop's item snapshot, and whether the loop has
/// recorded that round's result.
fn loop_item_round_tx(
    connection: &Connection,
    loop_run: &str,
    item: &str,
) -> Result<Option<(u32, bool)>> {
    let items: Option<String> = connection
        .query_row(
            &canonical_sql(
                "SELECT json_extract(claims.body, '$.fields.items') FROM claims
                 WHERE claims.subject=?1 AND claims.kind='loop.state'
                   AND json_extract(claims.body, '$.fields.items') IS NOT NULL
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1",
            ),
            [loop_run],
            |row| row.get(0),
        )
        .optional()?;
    let Some(items) = items else {
        return Ok(None);
    };
    let Some(index) = serde_json::from_str::<Vec<Value>>(&items)?
        .iter()
        .position(|candidate| candidate.get("id").and_then(Value::as_str) == Some(item))
    else {
        return Ok(None);
    };
    let round = u32::try_from(index + 1)?;
    let recorded = connection
        .query_row(
            "SELECT 1 FROM claims
             WHERE subject=?1 AND kind='loop.round-result'
               AND json_extract(body, '$.fields.round')=?2
               AND json_extract(body, '$.fields.candidate') IS NULL",
            params![loop_run, round],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(Some((round, recorded)))
}

/// A step run's own row, without its queue and wake enrichment.
fn step_run_row_tx(connection: &Connection, subject: &str) -> Result<Option<StepRunView>> {
    connection
        .query_row(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee,
                    available_to, agentless, title, goals, worker_reported, lease_owner,
                    lease_incarnation, lease_expires_at_unix_ms, blocked_reason,
                    not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms,
                    readiness_epoch, constraints
             FROM step_runs WHERE subject=?1",
            [subject],
            step_run_from_row,
        )
        .optional()
        .map_err(Into::into)
}

/// The review a human gate request asks, while its gate still waits on it, or why it no longer
/// does. A request is current while its owner (a step, mission or loop run) is open on the
/// generation, revision, definition and attempt it was asked for. An answer does not make a
/// request stale: the reviewers' list leaves out answered requests separately.
fn human_review_currency(
    connection: &Connection,
    request: ClaimRecord,
) -> Result<std::result::Result<HumanReviewView, String>> {
    let fields = request.body.get("fields").unwrap_or(&request.body);
    let text = |name: &str| fields.get(name).and_then(Value::as_str);
    let (Some(owner), Some(reviewer), Some(mission_revision), Some(step_definition)) = (
        text("owner"),
        text("reviewer"),
        text("mission_revision"),
        text("step_definition"),
    ) else {
        return Ok(Err(
            "its request does not name the exact owner, reviewer and revision it asks about".into(),
        ));
    };
    let Some(attempt) = fields
        .get("attempt")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return Ok(Err("its request does not name an attempt".into()));
    };
    let short = |revision: &str| revision.chars().take(12).collect::<String>();
    // Only the run's header decides whether a review is current: its steps' queue and wake
    // enrichment is the costliest read in st and would run for every open review.
    let (run, step, title) = if owner.starts_with("step-run/") || owner.starts_with("loop-run/") {
        // A loop's gates are asked of its loop subject; the step running the loop decides
        // whether one is current.
        let step = if owner.starts_with("loop-run/") {
            loop_step_tx(connection, owner)?
        } else {
            step_run_row_tx(connection, owner)?
        };
        let Some(step) = step else {
            return Ok(Err(format!("`{owner}` names no step run")));
        };
        let run = mission_run_header_tx(
            connection,
            step.run.strip_prefix("mission-run/").unwrap_or(&step.run),
        )
        .optional()?;
        let Some(run) = run else {
            return Ok(Err(format!("its mission run `{}` is gone", step.run)));
        };
        if is_terminal_run_state(&run.status) {
            return Ok(Err(format!("its mission run is {}", run.status)));
        }
        if is_terminal_run_state(&step.status) {
            return Ok(Err(format!("the step is {}", step.status)));
        }
        if step.generation != run.generation {
            return Ok(Err(format!(
                "its mission run moved on to {}, which asks for its own review",
                run.generation
            )));
        }
        if mission_revision != run.revision {
            return Ok(Err(format!(
                "it was asked for mission revision {}, and the run is on {}",
                short(mission_revision),
                short(&run.revision)
            )));
        }
        if step_definition != step.definition_hash {
            return Ok(Err(
                "the step's definition changed since it was asked".into()
            ));
        }
        // A for-each loop runs each item as its own round and asks the item's gates with
        // that round as their attempt, whatever attempt the step is on.
        let loop_run = format!(
            "loop-run/{}",
            step.subject
                .strip_prefix("step-run/")
                .unwrap_or(&step.subject)
        );
        if let Some(item) = owner.strip_prefix(&format!("{loop_run}/item/")) {
            match loop_item_round_tx(connection, &loop_run, item)? {
                None => return Ok(Err(format!("its loop has no item `{item}`"))),
                Some((round, _)) if round != attempt => {
                    return Ok(Err(format!(
                        "it was asked for round {attempt}, and item `{item}` runs in round {round}"
                    )));
                }
                Some((round, true)) => {
                    return Ok(Err(format!(
                        "the loop already recorded item `{item}` (round {round})"
                    )));
                }
                Some((_, false)) => {}
            }
        } else if attempt != step.attempt {
            return Ok(Err(format!(
                "it was asked for attempt {attempt}, and the step is on attempt {} ({})",
                step.attempt, step.status
            )));
        }
        let title = step.title.clone();
        (run, Some(step.step), title)
    } else if owner.starts_with("mission-run/") {
        let run = mission_run_header_tx(
            connection,
            owner.strip_prefix("mission-run/").unwrap_or(owner),
        )
        .optional()?;
        let Some(run) = run else {
            return Ok(Err(format!("`{owner}` names no mission run")));
        };
        if is_terminal_run_state(&run.status) {
            return Ok(Err(format!("the mission run is {}", run.status)));
        }
        if mission_revision != run.revision || step_definition != run.revision {
            return Ok(Err(format!(
                "it was asked for mission revision {}, and the run is on {}",
                short(mission_revision),
                short(&run.revision)
            )));
        }
        if attempt != 1 {
            return Ok(Err(format!(
                "it was asked for attempt {attempt}, and a mission gate has one"
            )));
        }
        (run, None, None)
    } else {
        return Ok(Err(format!("`{owner}` cannot own a human gate")));
    };
    // The person's attention shows a review only while its run still matters, so the review is
    // offered only then: a review with no card would wait on an answer nobody is asked for.
    if let Err(why) = person_work::run_liveness(connection, &run.subject, None, false)? {
        return Ok(Err(why));
    }
    let strings = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Ok(HumanReviewView {
        operation: fields
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or(&request.subject)
            .to_owned(),
        request: request.id,
        owner: owner.to_owned(),
        mission: run.mission,
        mission_run: run.subject,
        generation: run.generation,
        step,
        title,
        reviewer: reviewer.to_owned(),
        mode: fields
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("approve")
            .to_owned(),
        question: fields
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("Approve this work?")
            .to_owned(),
        review_targets: strings("review_targets"),
        decisions: strings("decisions"),
        attempt,
        requested_at_unix_ms: request.accepted_at_unix_ms,
    }))
}

/// Whether claim `result` answers the human gate request `request`: a `gate.result` on the
/// request's operation, by its reviewer, bound to it, with a verdict. The reviewers' list and
/// the gate both read answers by this rule, so a review the list no longer offers is always one
/// whose answer the gate acts on.
fn reviewer_answer_sql(result: &str) -> String {
    format!(
        "{result}.subject=request.subject
         AND {result}.kind='gate.result'
         AND json_extract({result}.body, '$.fields.request')=request.id
         AND ({result}.actor=json_extract(request.body, '$.fields.reviewer')
              OR ({result}.actor LIKE 'agent/%'
                  AND json_extract({result}.body, '$.fields.acted_for')=json_extract(request.body, '$.fields.reviewer')
                  AND json_extract({result}.body, '$.fields.delegation.episode')=request.id))
         AND json_extract({result}.body, '$.fields.verdict') IN ('pass','fail','feedback')"
    )
}

/// The reviewer's answer to the human gate request `request`, the first one by
/// `reviewer_answer_sql` when a race left two.
fn human_review_answer_tx(connection: &Connection, request: &str) -> Result<Option<ClaimRecord>> {
    connection
        .query_row(
            &canonical_sql(&format!(
                "SELECT claims.id, claims.store_index, claims.batch_id, claims.subject,
                        claims.kind, claims.origin, claims.actor, claims.body,
                        claims.predecessors, claims.accepted_at_unix_ms
                 FROM claims request JOIN claims ON {}
                 WHERE request.id=?1 AND request.kind='gate.requested'
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1",
                reviewer_answer_sql("claims")
            )),
            [request],
            claim_from_row,
        )
        .optional()
        .map_err(Into::into)
}

const MESSAGE_CANONICAL_KEY_BATCH_SIZE: usize = 128;

fn sort_messages_canonically(connection: &Connection, messages: &mut [MessageView]) -> Result<()> {
    let mut keys = BTreeMap::<String, canonical::ClaimKey>::new();
    for chunk in messages.chunks(MESSAGE_CANONICAL_KEY_BATCH_SIZE) {
        let subjects = serde_json::to_string(
            &chunk.iter().map(|message| message.subject.as_str()).collect::<Vec<_>>(),
        )?;
        let mut statement = connection.prepare_cached(message_canonical_keys_query())?;
        let mut rows = statement.query([subjects])?;
        for (ordinal, message) in chunk.iter().enumerate() {
            let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            // An absent earliest claim or its batch leaves a gap. A SQL step error
            // can precede this check; with multiple faults, error precedence is
            // unspecified. Every error still leaves the input vector unchanged.
            if row.get::<_, usize>(6)? != ordinal {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            let (time, writer, sequence, batch, position, id) = message_canonical_key_row(row)?;
            keys.insert(
                message.subject.clone(),
                (time.parse()?, writer, sequence, batch, position, id),
            );
        }
        anyhow::ensure!(rows.next()?.is_none(), "duplicate canonical message key row");
    }
    // Gather every key successfully before changing the caller's message vector.
    messages.sort_by(|left, right| keys[&left.subject].cmp(&keys[&right.subject]));
    Ok(())
}

fn message_canonical_keys_query() -> &'static str {
    // Drive only the supplied subjects. The same required kind index bounds each
    // earliest-ID lookup, independently of unrelated claims on those subjects.
    // Join the selected ID afterwards: a missing batch cannot select a younger ID.
    // One stable statement/parameter for every group size; CROSS JOIN keeps the
    // supplied JSON array as the outer loop, never a scan of the claims table.
    static QUERY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| canonical_sql(
        &format!(
            "SELECT chosen.accepted_at_unix_ms, batches.origin, batches.replica_sequence,
                    chosen.batch_id, {}, chosen.id, requested.key
             FROM json_each(?1) AS requested CROSS JOIN claims AS chosen CROSS JOIN batches
             WHERE chosen.id=(
                 SELECT id FROM claims INDEXED BY claims_subject_kind_index
                 WHERE subject=requested.value AND kind IN ('message.sent','intent.desired')
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1
             ) AND batches.id=chosen.batch_id
             ORDER BY requested.key",
            canonical::position_sql("chosen"),
        )
    ));
    &QUERY
}

fn message_canonical_key_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, u64, String, u64, String)> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, u64>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, u64>(4)?,
        row.get::<_, String>(5)?,
    ))
}

#[cfg(any(test, feature = "test-support"))]
pub fn message_canonical_key_query() -> &'static str {
    // Requires the named index: create_schema installs it before reader-pool creation.
    // A dropped or renamed index makes preparation fail, rather than using a slower fallback.
    static QUERY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| canonical_sql(
        &format!(
            "SELECT chosen.accepted_at_unix_ms, batches.origin, batches.replica_sequence,
                    chosen.batch_id, {}, chosen.id
             FROM claims AS chosen JOIN batches ON batches.id=chosen.batch_id
             WHERE chosen.id=(
                 SELECT id FROM claims INDEXED BY claims_subject_kind_index WHERE subject=?1
                   AND kind IN ('message.sent','intent.desired')
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1
             )",
            canonical::position_sql("chosen"),
        )
    ));
    &QUERY
}

#[cfg(any(test, feature = "test-support"))]
pub fn message_canonical_key_tx(connection: &Connection, subject: &str) -> Result<canonical::ClaimKey> {
    // Select the earliest ID without a batch join. An absent batch for that ID is an error,
    // not permission to select a younger claim. Decode exactly the shared key's columns.
    let (time, writer, sequence, batch, position, id) = connection
        .prepare_cached(message_canonical_key_query())?
        .query_row([subject], message_canonical_key_row)?;
    Ok((time.parse()?, writer, sequence, batch, position, id))
}

#[cfg(test)]
#[path = "store/message_canonical_sort_tests.rs"]
pub mod message_canonical_sort_tests;

fn message_bare_recipient(value: &str) -> &str {
    value
        .strip_prefix("agent/")
        .filter(|suffix| !suffix.contains('/'))
        .unwrap_or(value)
}

fn message_reminder_rank(tags: &[String]) -> Option<(&str, u64)> {
    let reminder = tags.iter().find_map(|tag| tag.strip_prefix("reminder:"))?;
    let version = tags
        .iter()
        .find_map(|tag| tag.strip_prefix("version:"))
        .and_then(|version| version.parse::<u64>().ok())
        .unwrap_or_default();
    Some((reminder, version))
}

fn selected_actionable_messages(
    connection: &Connection,
    messages: Vec<MessageView>,
) -> Result<Vec<MessageView>> {
    let mut selected_reminders = BTreeMap::<String, (u64, MessageView)>::new();
    let mut selected = Vec::new();
    for message in messages {
        let Some((reminder, version)) = message_reminder_rank(&message.tags) else {
            selected.push(message);
            continue;
        };
        let replace = selected_reminders
            .get(reminder)
            .is_none_or(|(current, selected)| {
                version > *current || (version == *current && message.subject > selected.subject)
            });
        if replace {
            selected_reminders.insert(reminder.to_owned(), (version, message));
        }
    }
    selected.extend(selected_reminders.into_values().map(|(_, message)| message));
    sort_messages_canonically(connection, &mut selected)?;
    Ok(selected)
}

/// Why the step whose end closes an attention request has ended since `attempt`, or `None`
/// while that attempt still runs. A step st no longer knows has not ended.
/// The same canonical total-order key used by shared SQL folds, including equal-time episodes.
type ClaimMoment = canonical::ClaimKey;

fn claim_moment(connection: &Connection, row: &rusqlite::Row<'_>) -> rusqlite::Result<ClaimMoment> {
    canonical::claim_key(connection, &row.get::<_, String>(1)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, error.into())
    })
}

/// When the current declaration of `subject` became a `stop`, or `None` while it still runs.
fn declaration_stopped_tx(connection: &Connection, subject: &str) -> Result<Option<ClaimMoment>> {
    let Some(row) = current_desired_row(connection, subject)? else {
        return Ok(None);
    };
    let body = serde_json::from_str::<Value>(&row.body).unwrap_or(Value::Null);
    let stopped = row.kind == "stop"
        || body
            .get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| {
                children.len() == 1
                    && children[0].get("name").and_then(Value::as_str) == Some("stop")
            });
    if !stopped {
        return Ok(None);
    }
    Ok(connection
        .query_row(
            "SELECT accepted_at_unix_ms, id FROM claims WHERE id=?1",
            [&row.claim_id],
            |row| claim_moment(connection, row),
        )
        .optional()?)
}

fn attention_action(label: &str, argv: &[&str]) -> AttentionActionView {
    AttentionActionView {
        label: label.into(),
        argv: argv.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn attention_item_from_failure(request: AttentionRequestView) -> AttentionItemView {
    AttentionItemView {
        episode: request.request.clone(),
        priority: if request.severity == "warning" {
            "normal"
        } else {
            "high"
        }
        .into(),
        kind: "fault".into(),
        review_mode: None,
        subject: request.subject.clone(),
        person: request.reviewer,
        requester_id: Some(request.actor),
        launch_id: None,
        variant_id: None,
        message_id: None, prompt_call: None, answers: Vec::new(), conversation: None,
        title: request.title,
        detail: request.reason,
        mission: None,
        mission_run: None,
        step: None,
        targets: request.targets,
        requested_at_unix_ms: request.requested_at_unix_ms,
        actions: vec![attention_action(
            "inspect source",
            &["st", "subject", &request.subject],
        )],
        request: None,
    }
}

fn is_terminal_run_state(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn is_terminal_generation_state(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "superseded")
}

fn operational_repair_plan_tx(
    connection: &Connection,
    snapshot_unix_ms: u128,
) -> Result<OperationalRepairPlan> {
    let snapshot_index = current_index(connection)?;
    let mut items = Vec::new();
    let mut covered_descendant_steps = BTreeSet::new();

    let mut terminal_roots = connection.prepare(
        "SELECT id, current_generation_id FROM mission_runs
         WHERE status IN ('completed','failed','cancelled') OR phase='terminal'
         ORDER BY id",
    )?;
    let terminal_roots = terminal_roots
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (root_id, generation) in terminal_roots {
        let descendants = descendant_mission_run_ids_tx(connection, &generation)?;
        let mut affected = Vec::new();
        let mut descendant_runs = Vec::new();
        for descendant in descendants {
            let (status, phase, child_generation) = connection.query_row(
                "SELECT status, phase, current_generation_id FROM mission_runs WHERE id=?1",
                [&descendant],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?;
            let mut statement = connection.prepare(
                "SELECT subject FROM step_runs
                 WHERE run_id=?1 AND status NOT IN ('completed','failed','cancelled')
                 ORDER BY subject",
            )?;
            let steps = statement
                .query_map([&descendant], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let active_run = matches!(status.as_str(), "running" | "standing" | "blocked")
                || phase != "terminal";
            if active_run || !steps.is_empty() {
                descendant_runs.push(format!("mission-run/{descendant}"));
                if active_run {
                    affected.push(format!("mission-run/{descendant}"));
                    affected.push(format!("run-generation/{child_generation}"));
                }
                for step in steps {
                    covered_descendant_steps.insert(step.clone());
                    affected.push(step);
                }
            }
        }
        if !affected.is_empty() {
            affected.sort();
            affected.dedup();
            descendant_runs.sort();
            push_operational_repair_item(
                &mut items,
                "terminal-descendants",
                &format!("mission-run/{root_id}"),
                affected,
                "a terminal root still owns active descendant runs or work",
                BTreeMap::from([
                    ("generation".into(), Value::String(generation)),
                    (
                        "descendant_runs".into(),
                        Value::Array(descendant_runs.into_iter().map(Value::String).collect()),
                    ),
                ]),
            )?;
        }
    }

    let mut orphaned = connection.prepare(
        "SELECT step_runs.subject
         FROM step_runs
         JOIN mission_runs ON mission_runs.id=step_runs.run_id
         JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
         JOIN run_generations ON run_generations.id=step_runs.generation_id
         WHERE step_runs.status NOT IN ('completed','failed','cancelled')
           AND (
             mission_runs.status IN ('completed','failed','cancelled')
             OR mission_runs.phase='terminal'
             OR step_runs.generation_id<>mission_runs.current_generation_id
             OR run_generations.status IN ('completed','failed','cancelled','superseded')
             OR root_runs.status IN ('completed','failed','cancelled')
             OR root_runs.phase='terminal'
           )
         ORDER BY step_runs.subject",
    )?;
    let orphaned = orphaned
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in orphaned {
        if covered_descendant_steps.contains(&subject) {
            continue;
        }
        push_operational_repair_item(
            &mut items,
            "orphaned-readiness",
            &subject,
            vec![subject.clone()],
            "non-terminal work has a terminal, superseded, or non-current owner",
            BTreeMap::new(),
        )?;
    }

    let mut expired = connection.prepare(
        "SELECT step_runs.subject, step_runs.lease_expires_at_unix_ms
         FROM step_runs
         JOIN mission_runs ON mission_runs.id=step_runs.run_id
         JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
         JOIN run_generations ON run_generations.id=step_runs.generation_id
         WHERE step_runs.status IN ('claimed','working','verifying','blocked')
           AND step_runs.lease_expires_at_unix_ms IS NOT NULL
           AND step_runs.generation_id=mission_runs.current_generation_id
           AND mission_runs.status NOT IN ('completed','failed','cancelled')
           AND mission_runs.phase<>'terminal'
           AND run_generations.status NOT IN ('completed','failed','cancelled','superseded')
           AND root_runs.status NOT IN ('completed','failed','cancelled')
           AND root_runs.phase<>'terminal'
         ORDER BY step_runs.subject",
    )?;
    let expired = expired
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (subject, expiry) in expired {
        let expiry = expiry.parse::<u128>().unwrap_or(u128::MAX);
        if expiry > snapshot_unix_ms {
            continue;
        }
        push_operational_repair_item(
            &mut items,
            "expired-claim",
            &subject,
            vec![subject.clone()],
            "the worker lease expired and its durable claim must be released",
            BTreeMap::from([(
                "lease_expires_at_unix_ms".into(),
                Value::String(expiry.to_string()),
            )]),
        )?;
    }

    let mut latest_wake_diagnostics =
        BTreeMap::<(String, String, String), (String, Value, u128)>::new();
    let mut wake_statement = connection.prepare(&canonical_sql(
        "SELECT id, subject, body, accepted_at_unix_ms FROM claims
         WHERE kind='harness.diagnostic'
           AND json_extract(body, '$.fields.code')='work-wake-exhausted'
           AND accepted_at_unix_ms<=?1
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let wake_rows = wake_statement.query_map([snapshot_unix_ms.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in wake_rows {
        let (claim_id, agent, body, accepted) = row?;
        let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        let fields = body.get("fields").unwrap_or(&body);
        let step = fields
            .get("step_run")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let incarnation = fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        latest_wake_diagnostics.insert(
            (agent, step.into(), incarnation.into()),
            (claim_id, body, accepted.parse::<u128>().unwrap_or_default()),
        );
    }
    for ((agent, step, incarnation), (diagnostic, body, accepted)) in latest_wake_diagnostics {
        let fields = body.get("fields").unwrap_or(&body);
        if fields.get("status").and_then(Value::as_str) != Some("failed") {
            continue;
        }
        let step_state = connection
            .query_row(
                "SELECT status, assignee FROM step_runs WHERE subject=?1",
                [&step],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let harness = current_harness_at(connection, &agent, None)?;
        let contradicted = step_state.as_ref().is_none_or(|(status, assignee)| {
            status != "ready" || assignee.as_deref() != Some(agent.as_str())
        }) || harness.as_ref().is_some_and(|harness| {
            harness.incarnation_id != incarnation
                || (harness.state == "working" && harness.observed_at_unix_ms >= accepted)
        });
        if !contradicted {
            continue;
        }
        let mut affected = vec![agent.clone()];
        if !step.is_empty() {
            affected.push(step.clone());
        }
        push_operational_repair_item(
            &mut items,
            "wake-contradiction",
            &agent,
            affected,
            "a wake-exhaustion diagnostic is contradicted by current work or harness state",
            BTreeMap::from([
                ("diagnostic_claim".into(), Value::String(diagnostic)),
                ("step_run".into(), Value::String(step)),
                ("incarnation_id".into(), Value::String(incarnation)),
                (
                    "wake_attempts".into(),
                    fields
                        .get("wake_attempts")
                        .cloned()
                        .unwrap_or_else(|| Value::from(0)),
                ),
            ]),
        )?;
    }

    let mut dispatch_stalls = connection.prepare(
        "SELECT mission_runs.id, mission_runs.current_generation_id,
                mission_runs.updated_at_unix_ms, mission_revisions.body
         FROM mission_runs
         JOIN run_generations
           ON run_generations.id=mission_runs.current_generation_id
         JOIN mission_revisions
           ON mission_revisions.mission_id=mission_runs.mission_id
          AND mission_revisions.revision=run_generations.revision
         WHERE mission_runs.status='running' AND mission_runs.phase='normal'
         ORDER BY mission_runs.id",
    )?;
    let dispatch_stalls = dispatch_stalls
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (run, generation, updated, mission_body) in dispatch_stalls {
        let updated = updated.parse::<u128>().unwrap_or(snapshot_unix_ms);
        if snapshot_unix_ms.saturating_sub(updated) < 60_000 {
            continue;
        }
        let mission = match serde_json::from_str::<MissionSpec>(&mission_body) {
            Ok(mission) => mission,
            Err(_) => continue,
        };
        if !mission.baselines.is_empty() {
            continue;
        }
        let mut statement = connection.prepare(
            "SELECT subject, step_path, status, agentless, assignee, available_to,
                    not_before_unix_ms
             FROM step_runs WHERE generation_id=?1 ORDER BY subject",
        )?;
        let steps = statement
            .query_map([&generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let normal = steps
            .iter()
            .filter(|(_, path, _, _, _, _, _)| {
                crate::mission::find_step(&mission, path).is_some_and(|step| !step.finally)
            })
            .collect::<Vec<_>>();
        if normal.is_empty()
            || normal.iter().any(|(_, _, status, _, _, _, _)| {
                matches!(
                    status.as_str(),
                    "ready" | "claimed" | "working" | "verifying" | "blocked"
                )
            })
        {
            continue;
        }
        let desired_agent = |agent: &str| -> rusqlite::Result<bool> {
            connection
                .query_row(
                    "SELECT 1 FROM desired WHERE subject=?1 AND kind='agent'",
                    [agent],
                    |_| Ok(()),
                )
                .optional()
                .map(|row| row.is_some())
        };
        let mut safe_step_runs = Vec::new();
        for (subject, path, status, agentless, assignee, available_to, not_before) in normal {
            if status != "pending"
                || not_before
                    .as_deref()
                    .and_then(|value| value.parse::<u128>().ok())
                    .is_some_and(|deadline| deadline > snapshot_unix_ms)
            {
                continue;
            }
            let Some(step) = crate::mission::find_step(&mission, path) else {
                continue;
            };
            if !step.dependencies.is_empty() || !step.baselines.is_empty() {
                continue;
            }
            let available = if *agentless {
                true
            } else {
                let mut candidates = assignee.iter().cloned().collect::<Vec<_>>();
                candidates
                    .extend(serde_json::from_str::<Vec<String>>(available_to).unwrap_or_default());
                candidates
                    .into_iter()
                    .any(|candidate| desired_agent(&candidate).unwrap_or(false))
            };
            if available {
                safe_step_runs.push(subject.clone());
            }
        }
        if safe_step_runs.is_empty() {
            continue;
        }
        let subject = format!("mission-run/{run}");
        let mut affected = vec![subject.clone()];
        affected.extend(safe_step_runs.iter().cloned());
        push_operational_repair_item(
            &mut items,
            "mission-dispatch-stall",
            &subject,
            affected,
            "a running mission has safely admissible root work that remained pending for more than one minute",
            BTreeMap::from([
                (
                    "generation".into(),
                    Value::String(format!("run-generation/{generation}")),
                ),
                (
                    "step_runs".into(),
                    Value::Array(safe_step_runs.into_iter().map(Value::String).collect()),
                ),
                (
                    "updated_at_unix_ms".into(),
                    Value::String(updated.to_string()),
                ),
            ]),
        )?;
    }

    let mut cancelled_final = connection.prepare(
        "SELECT id, current_generation_id, updated_at_unix_ms
         FROM mission_runs
         WHERE status='running' AND phase='final-cancelled'
         ORDER BY id",
    )?;
    let cancelled_final = cancelled_final
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (run, generation, updated) in cancelled_final {
        let updated = updated.parse::<u128>().unwrap_or(snapshot_unix_ms);
        let age_ms = snapshot_unix_ms.saturating_sub(updated);
        if age_ms < 60_000 {
            continue;
        }
        let mut statement = connection.prepare(
            "SELECT subject, status, not_before_unix_ms
             FROM step_runs
             WHERE generation_id=?1 AND status NOT IN ('completed','failed','cancelled')
             ORDER BY subject",
        )?;
        let steps = statement
            .query_map([&generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if steps.is_empty()
            || steps.iter().any(|(_, status, not_before)| {
                status != "pending"
                    || not_before
                        .as_deref()
                        .and_then(|value| value.parse::<u128>().ok())
                        .is_some_and(|deadline| deadline > snapshot_unix_ms)
            })
        {
            continue;
        }
        let subject = format!("mission-run/{run}");
        let mut affected = vec![subject.clone()];
        affected.extend(steps.iter().map(|(subject, _, _)| subject.clone()));
        push_operational_repair_item(
            &mut items,
            "cancelled-final-stall",
            &subject,
            affected,
            "a cancelled mission has had only unscheduled pending final work for more than one minute",
            BTreeMap::from([
                (
                    "generation".into(),
                    Value::String(format!("run-generation/{generation}")),
                ),
                (
                    "updated_at_unix_ms".into(),
                    Value::String(updated.to_string()),
                ),
            ]),
        )?;
    }

    let mut impossible = connection.prepare(
        "SELECT mission_runs.id, mission_runs.status, mission_runs.phase,
                run_generations.status
         FROM mission_runs JOIN run_generations
           ON run_generations.id=mission_runs.current_generation_id
         WHERE (mission_runs.status IN ('completed','failed','cancelled') AND mission_runs.phase<>'terminal')
            OR (mission_runs.status IN ('running','standing','blocked') AND mission_runs.phase='terminal')
            OR (mission_runs.status IN ('running','standing','blocked')
                AND run_generations.status IN ('completed','failed','cancelled','superseded'))
         ORDER BY mission_runs.id",
    )?;
    let impossible = impossible
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (run, status, phase, generation_status) in impossible {
        let subject = format!("mission-run/{run}");
        push_operational_repair_item(
            &mut items,
            "impossible-state",
            &subject,
            vec![subject.clone()],
            "the mission run status, phase, and selected generation disagree",
            BTreeMap::from([
                ("status".into(), Value::String(status)),
                ("phase".into(), Value::String(phase)),
                ("generation_status".into(), Value::String(generation_status)),
            ]),
        )?;
    }

    items.sort_by(|left, right| {
        (&left.class, &left.subject, &left.id).cmp(&(&right.class, &right.subject, &right.id))
    });
    let digest = canonical_hash(&("st3.operational-repair.v0", &items))?;
    Ok(OperationalRepairPlan {
        api_version: "st3.operational-repair.v0".into(),
        token: format!("orpv0:{digest}"),
        snapshot_index,
        status: if items.is_empty() {
            "clean".into()
        } else {
            "changes".into()
        },
        items,
    })
}

fn push_operational_repair_item(
    items: &mut Vec<OperationalRepairItem>,
    class: &str,
    subject: &str,
    mut affected_subjects: Vec<String>,
    reason: &str,
    details: BTreeMap<String, Value>,
) -> Result<()> {
    affected_subjects.sort();
    affected_subjects.dedup();
    let digest = canonical_hash(&(
        "st3.operational-repair-item.v0",
        class,
        subject,
        &affected_subjects,
        reason,
        &details,
    ))?;
    items.push(OperationalRepairItem {
        id: format!("repair-item/{digest}"),
        class: class.into(),
        subject: subject.into(),
        affected_subjects,
        reason: reason.into(),
        details,
    });
    Ok(())
}

fn repair_step_state_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    status: &str,
    reason: &str,
    now: u128,
) -> Result<Option<String>, St3Error> {
    let current = transaction
        .query_row(
            "SELECT status, readiness_epoch FROM step_runs WHERE subject=?1",
            [subject],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((current, current_epoch)) = current else {
        return Ok(None);
    };
    if current == status {
        return Ok(None);
    }
    let readiness_epoch = current_epoch.saturating_add(u32::from(status == "ready"));
    transaction
        .execute(
            "UPDATE step_runs SET status=?2, blocked_reason=?3, lease_owner=NULL,
                    lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                    not_before_unix_ms=CASE WHEN ?2='ready' THEN NULL ELSE not_before_unix_ms END,
                    activated_at_unix_ms=CASE WHEN ?2='ready' THEN ?4 ELSE activated_at_unix_ms END,
                    readiness_epoch=?5, updated_at_unix_ms=?4 WHERE subject=?1",
            params![subject, status, reason, now.to_string(), readiness_epoch],
        )
        .map_err(internal)?;
    let claim = append_claim_tx(
        transaction,
        origin,
        subject,
        "step-run.state",
        Some("daemon/runtime"),
        &json!({"fields": {
            "status": status,
            "reason": reason,
            "readiness_epoch": readiness_epoch,
        }}),
        &[],
        None,
    )
    .map_err(internal)?;
    Ok(Some(claim.id))
}

fn repair_impossible_run_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    reason: &str,
    now: u128,
) -> Result<Vec<String>, St3Error> {
    let run_id = subject.strip_prefix("mission-run/").unwrap_or(subject);
    let current = transaction
        .query_row(
            "SELECT status, phase, current_generation_id FROM mission_runs WHERE id=?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    let Some((status, phase, generation)) = current else {
        return Ok(Vec::new());
    };
    let status = if is_terminal_run_state(&status) {
        status
    } else {
        "cancelled".into()
    };
    if phase == "terminal"
        && transaction
            .query_row(
                "SELECT status FROM mission_runs WHERE id=?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(internal)?
            == status
    {
        return Ok(Vec::new());
    }
    transaction
        .execute(
            "UPDATE mission_runs SET status=?2, phase='terminal', updated_at_unix_ms=?3 WHERE id=?1",
            params![run_id, status, now.to_string()],
        )
        .map_err(internal)?;
    transaction
        .execute(
            "UPDATE run_generations SET status=?2, updated_at_unix_ms=?3 WHERE id=?1",
            params![generation, status, now.to_string()],
        )
        .map_err(internal)?;
    let body = json!({"fields": {"status": status, "phase": "terminal", "reason": reason}});
    let mut claim_ids = Vec::new();
    for (claim_subject, kind) in [
        (format!("mission-run/{run_id}"), "mission-run.state"),
        (
            format!("run-generation/{generation}"),
            "run-generation.state",
        ),
    ] {
        claim_ids.push(
            append_claim_tx(
                transaction,
                origin,
                &claim_subject,
                kind,
                Some("daemon/runtime"),
                &body,
                &[],
                None,
            )
            .map_err(internal)?
            .id,
        );
    }
    claim_ids.extend(cancel_descendant_mission_runs_tx(
        transaction,
        origin,
        &generation,
        "daemon/runtime",
        reason,
        now,
    )?);
    claim_ids.extend(terminalize_run_steps_tx(
        transaction,
        origin,
        run_id,
        reason,
        Some("daemon/runtime"),
        None,
        now,
    )?);
    Ok(claim_ids)
}

/// A subject's operational annotation. At a snapshot, its owning run's and generation's states
/// are read through `owner_actual`.
#[allow(clippy::too_many_arguments)]
fn operational_annotation(
    connection: &Connection,
    subject: &str,
    desired_kind: Option<&str>,
    has_desired: bool,
    owner_run: Option<&str>,
    owner_generation: Option<&str>,
    actual: Option<&Value>,
    at_index: Option<u64>,
    owner_actual: &mut dyn FnMut(&str) -> Result<Option<Value>>,
) -> Result<OperationalAnnotation> {
    let fields = actual.map(|value| value.get("fields").unwrap_or(value));
    let status = fields
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str);
    let incarnation = fields
        .and_then(|value| value.get("incarnation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let runtime_subject = runtime_subject(subject);
    let stopped = matches!(status, Some("stopped" | "absent" | "exited"));
    let mut historical = Vec::new();
    if runtime_subject && stopped && (!has_desired || desired_kind == Some("stop")) {
        historical.push("stopped".to_owned());
    }
    // Nothing declares this runtime and no runtime status was ever observed for it, such as an
    // agent known only from its own harness observations. It is a leftover, not a live runtime.
    if runtime_subject && !has_desired && status.is_none() {
        historical.push("undeclared".to_owned());
    }

    if let Some(owner_run) = owner_run {
        let (run_status, current_generation, mode) = if at_index.is_some() {
            let owner = owner_actual(owner_run)?;
            let fields = owner
                .as_ref()
                .map(|value| value.get("fields").unwrap_or(value));
            (
                fields
                    .and_then(|fields| fields.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                None,
                fields
                    .and_then(|fields| fields.get("mode"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        } else {
            let run_id = owner_run.strip_prefix("mission-run/").unwrap_or(owner_run);
            connection
                .query_row(
                    "SELECT status, current_generation_id, mode FROM mission_runs WHERE id=?1",
                    [run_id],
                    |row| {
                        Ok((
                            Some(row.get::<_, String>(0)?),
                            Some(format!("run-generation/{}", row.get::<_, String>(1)?)),
                            Some(row.get::<_, String>(2)?),
                        ))
                    },
                )
                .optional()?
                .unwrap_or((None, None, None))
        };
        if run_status.as_deref().is_some_and(is_terminal_run_state) {
            historical.push("terminal-owner".to_owned());
            if mode.as_deref() == Some("eval") {
                historical.push("eval".to_owned());
            }
        }
        if let Some(owner_generation) = owner_generation {
            let generation_status = if at_index.is_some() {
                let generation = owner_actual(owner_generation)?;
                generation
                    .as_ref()
                    .map(|value| value.get("fields").unwrap_or(value))
                    .and_then(|fields| fields.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                let generation_id = generation_id_from_subject(owner_generation);
                connection
                    .query_row(
                        "SELECT status FROM run_generations WHERE id=?1",
                        [generation_id],
                        |row| row.get(0),
                    )
                    .optional()?
            };
            if current_generation
                .as_deref()
                .is_some_and(|current| current != owner_generation)
                || generation_status.as_deref() == Some("superseded")
            {
                historical.push("superseded".to_owned());
            } else if generation_status
                .as_deref()
                .is_some_and(is_terminal_generation_state)
            {
                historical.push("terminal-generation".to_owned());
            }
        }
    }
    historical.sort();
    historical.dedup();
    let layer = if historical.is_empty() {
        "current"
    } else {
        "history"
    };
    let healthy_runtime =
        !runtime_subject || matches!(status, Some("running" | "ready" | "working" | "idle"));
    let mut reasons = historical;
    if layer == "current" && runtime_subject && has_desired && !healthy_runtime {
        reasons.push("unhealthy".into());
    }
    Ok(OperationalAnnotation {
        layer: layer.into(),
        actionable: layer == "current" && healthy_runtime,
        reasons,
        owner_generation: owner_generation.map(str::to_owned),
        runtime_incarnation: incarnation,
    })
}

/// How long a mission whose run failed or was cancelled stays in the current missions view.
pub const RECENTLY_ENDED_MS: u128 = 24 * 60 * 60 * 1000;

/// The condition of a broken exec gate's operational failure. Its item goes to the publisher of
/// the run's mission revision: a person reads it in their attention, an agent as a fault.
pub const GATE_BROKEN_CONDITION: &str = "gate-broken";

/// The earliest end that still counts as recent.
pub fn recently_ended_since() -> u128 {
    now_ms().saturating_sub(RECENTLY_ENDED_MS)
}

/// Who a run reports to and whether it also reports completion: the agent whoever started the
/// run named, else the mission's own `report-to`, and completion if either asked and there is
/// someone to tell. `None` when nobody is named.
fn resolved_run_report(
    mission: &MissionSpec,
    report_to: Option<&str>,
    report_completed: bool,
) -> Option<(String, bool, bool)> {
    let report_to = report_to.or(mission.report_to.as_deref())?;
    Some((
        report_to.to_owned(),
        report_completed || mission.report_completed,
        mission.report_stalls,
    ))
}

/// Record on a run's creation claim who hears of its failure, cancellation or stall. A run with
/// nobody to tell carries no report fields at all, so nothing about it changes.
fn add_run_report_fields(
    body: &mut Value,
    mission: &MissionSpec,
    report_to: Option<&str>,
    report_completed: bool,
) {
    let Some((report_to, completed, stalls)) =
        resolved_run_report(mission, report_to, report_completed)
    else {
        return;
    };
    let fields = &mut body["fields"];
    fields["report_to"] = json!(report_to);
    fields["stalled_after_ms"] = json!(
        mission
            .stalled_after_ms
            .unwrap_or(crate::mission::DEFAULT_STALLED_AFTER_MS)
    );
    if !stalls {
        fields["report_stalls"] = json!(false);
    }
    if completed {
        fields["report_completed"] = json!(true);
    }
}

fn insert_mission_deadline_tx(
    transaction: &Transaction<'_>,
    run_id: &str,
    timeout_ms: Option<u64>,
    started_at_unix_ms: Option<u128>,
) -> Result<Option<u128>, St3Error> {
    let Some(timeout_ms) = timeout_ms else {
        return Ok(None);
    };
    let deadline_at_unix_ms =
        started_at_unix_ms.map(|started| started.saturating_add(u128::from(timeout_ms)));
    transaction
        .execute(
            "INSERT INTO mission_run_deadlines(run_id, timeout_ms, deadline_at_unix_ms)
             VALUES (?1, ?2, ?3)",
            params![
                run_id,
                timeout_ms,
                deadline_at_unix_ms.map(|deadline| deadline.to_string())
            ],
        )
        .map_err(internal)?;
    Ok(deadline_at_unix_ms)
}

/// Admission is the first completion of the synthetic after-run step. The retained budget
/// becomes one deadline in the same transaction, and later retries cannot restart its clock.
fn arm_waiting_mission_deadline_tx(
    transaction: &Transaction<'_>,
    step: &str,
    admitted_at_unix_ms: u128,
    previously_completed: bool,
) -> Result<(), St3Error> {
    if step.rsplit('/').next() != Some(crate::mission::AFTER_RUN_STEP) || previously_completed {
        return Ok(());
    }
    // The batched writer has its own clock. Keep this admission row on the accepted
    // event timestamp too, so local activity and replay describe the same transition.
    transaction
        .execute(
            "UPDATE step_runs SET updated_at_unix_ms=?3
         WHERE subject=?1 AND step_path=?2
           AND EXISTS(SELECT 1 FROM mission_run_after WHERE run_id=step_runs.run_id)",
            params![
                step,
                crate::mission::AFTER_RUN_STEP,
                admitted_at_unix_ms.to_string()
            ],
        )
        .map_err(internal)?;
    let waiting: Option<(String, u64)> = transaction
        .query_row(
            "SELECT deadlines.run_id, deadlines.timeout_ms
             FROM step_runs
             JOIN mission_run_after ON mission_run_after.run_id=step_runs.run_id
             JOIN mission_run_deadlines AS deadlines ON deadlines.run_id=step_runs.run_id
             WHERE step_runs.subject=?1 AND step_runs.step_path=?2
               AND deadlines.deadline_at_unix_ms IS NULL
               AND NOT EXISTS(SELECT 1 FROM step_runs AS earlier INDEXED BY step_runs_run_index
                 WHERE earlier.run_id=step_runs.run_id AND earlier.step_path=?2
                   AND earlier.status='completed' AND earlier.subject<>step_runs.subject)",
            params![step, crate::mission::AFTER_RUN_STEP],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    if let Some((run, timeout_ms)) = waiting {
        let deadline = admitted_at_unix_ms.saturating_add(u128::from(timeout_ms));
        transaction
            .execute(
                "UPDATE mission_run_deadlines SET deadline_at_unix_ms=?2
                 WHERE run_id=?1 AND deadline_at_unix_ms IS NULL",
                params![run, deadline.to_string()],
            )
            .map_err(internal)?;
    }
    Ok(())
}

fn validate_mission_run_timeout(mission: &MissionSpec, mode: &str) -> Result<(), St3Error> {
    if mode != "eval" {
        return Ok(());
    }
    let timeout_ms = mission.timeout_ms.ok_or_else(|| {
        St3Error::new(
            "missing-eval-timeout",
            format!("eval entry mission `{}` needs a timeout", mission.id),
        )
    })?;
    if timeout_ms > MAX_EVAL_TIMEOUT_MS {
        return Err(St3Error::new(
            "eval-timeout-too-large",
            format!(
                "eval entry mission `{}` timeout exceeds the 20 minute limit",
                mission.id
            ),
        ));
    }
    Ok(())
}

fn claim_append_error(error: anyhow::Error) -> St3Error {
    match error.downcast::<St3Error>() {
        Ok(error) => error,
        Err(error) => internal(error),
    }
}

fn validate_replica_repair(
    connection: &Connection,
    repair: &ReplicaRepairDeclaration,
) -> Result<bool, St3Error> {
    let state = connection
        .query_row(
            "SELECT state FROM replica_records WHERE record_ref=?1",
            [&repair.record_ref],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| {
            St3Error::new(
                "unknown-replica-record",
                format!("replica record `{}` does not exist", repair.record_ref),
            )
        })?;
    if !matches!(state.as_str(), "invalid" | "unknown" | "repaired") {
        return Err(St3Error::new(
            "record-does-not-need-repair",
            format!("replica record `{}` is {state}", repair.record_ref),
        ));
    }
    let replacement_exists = connection
        .query_row(
            "SELECT 1 FROM claims WHERE id=?1",
            [&repair.replacement_claim_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(internal)?
        .is_some();
    if !replacement_exists {
        return Err(St3Error::new(
            "unknown-replacement-claim",
            format!(
                "replacement claim `{}` does not exist or is not valid",
                repair.replacement_claim_id
            ),
        ));
    }
    let subject = format!(
        "repair/{}",
        repair
            .record_ref
            .strip_prefix("record/")
            .unwrap_or(&repair.record_ref)
    );
    let mut statement = connection
        .prepare("SELECT body FROM claims WHERE subject=?1 AND kind='record.repaired' ORDER BY id")
        .map_err(internal)?;
    let bodies = statement
        .query_map([subject], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    if let Some(body) = bodies.into_iter().next() {
        let body: Value = serde_json::from_str(&body).map_err(internal)?;
        let fields = body.get("fields").unwrap_or(&body);
        let same = fields.get("record").and_then(Value::as_str) == Some(repair.record_ref.as_str())
            && fields.get("replacement").and_then(Value::as_str)
                == Some(repair.replacement_claim_id.as_str())
            && fields.get("reason").and_then(Value::as_str) == Some(repair.reason.as_str());
        if same {
            return Ok(false);
        }
        return Err(St3Error::new(
            "conflicting-repair",
            format!(
                "replica record `{}` already has another repair",
                repair.record_ref
            ),
        ));
    }
    Ok(true)
}

#[cfg(test)]
pub mod fleet_admission_tests;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static SUBJECT_REDUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static STATUS_AFTER_DESIRED_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// Steps whose queue, timing and wake a read enriched, so a test can see a read's work.
    pub static STEPS_ENRICHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Pinned mission definitions a read parsed, so a test can see one parse serve a whole run.
    pub static DEFINITIONS_PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The SQLite work each work action did inside its writer job, by idempotency key, so a test
/// can see what one action holds the single writer for.
#[cfg(any(test, feature = "test-support"))]
pub static WORK_ACTION_WRITER_WORK: Mutex<
    BTreeMap<String, smallclaims::sqlite::work::SqliteWork>,
> = Mutex::new(BTreeMap::new());

#[cfg(any(test, feature = "test-support"))]
pub struct WorkActionWriterProbe {
    key: String,
    scope: Option<smallclaims::sqlite::work::SqliteWorkScope>,
}

#[cfg(any(test, feature = "test-support"))]
impl WorkActionWriterProbe {
    fn start(key: &str) -> Self {
        Self {
            key: key.to_owned(),
            scope: Some(smallclaims::sqlite::work::SqliteWorkScope::start()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for WorkActionWriterProbe {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.take() {
            WORK_ACTION_WRITER_WORK
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(std::mem::take(&mut self.key), scope.finish());
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn streamed_digest_matches_materialized_rows() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE digest_rows (value TEXT); INSERT INTO digest_rows VALUES ('one'), ('é'), ('');")
        .unwrap();
    let queries = &[("sample", "SELECT value FROM digest_rows ORDER BY rowid")];
    let streamed = digest_queries(&connection, queries).unwrap();

    let mut digest = Sha256::new();
    digest.update(b"st3-logical-digest-v1\0");
    for (name, query) in queries {
        digest.update(name.as_bytes());
        digest.update([0]);
        let rows = connection
            .prepare(query)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for row in rows {
            digest.update((row.len() as u64).to_be_bytes());
            digest.update(row.as_bytes());
        }
    }
    assert_eq!(streamed, hex::encode(digest.finalize()));
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_receipt_measures_how_far_behind_this_node_is() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    for index in 0..1_200 {
        source
            .append_client_claim(&ClaimInput {
                subject: format!("resource/sync-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let peers = ["source".to_owned()];
    assert!(target.replication_peer_sync(&peers).is_empty());

    let total = source.replication_inventory().unwrap().envelopes.len() as u64;
    let local = target.replication_inventory().unwrap().envelopes.len() as u64;
    let pull = || {
        let summary = target.export_replication_summary(FLEET).unwrap();
        // Classic 512-envelope pages, so the backlog takes more than one exchange.
        let inventory = ReplicationInventory {
            accepts: None,
            ..summary.inventory
        };
        let response = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange("source", FLEET, &response)
            .unwrap();
        (
            response.envelopes.len() as u64,
            target.replication_peer_sync(&peers)["source"].clone(),
        )
    };
    let (mut received, sync) = pull();
    assert_eq!(sync.peer_only_envelopes, total - received);
    assert_eq!(sync.local_only_envelopes, local);
    assert!(sync.catching_up, "more than one exchange remains");
    let status = target
        .replication_status(true, Some(FLEET), &peers)
        .unwrap();
    assert_eq!(status.peers[0].sync.as_ref(), Some(&sync));

    let (more, sync) = pull();
    received += more;
    assert_eq!(sync.peer_only_envelopes, total - received);
    assert!(!sync.catching_up, "the rest fits in one exchange");

    // An unconfigured relay's measurement is not reported.
    assert!(
        target
            .replication_peer_sync(&["elsewhere".to_owned()])
            .is_empty()
    );
}

#[cfg(any(test, feature = "test-support"))]
impl Store {
    /// The store index the replicated projection last reached.
    fn projected_through(&self) -> u64 {
        self.readers
            .get()
            .query_row(
                "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_peer_that_says_how_many_envelopes_it_takes_gets_pages_that_size() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    for index in 0..5_000 {
        source
            .append_client_claim(&ClaimInput {
                subject: format!("resource/page-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();

    // This build says what it takes in every inventory it sends.
    let summary = target.export_replication_summary(FLEET).unwrap();
    assert_eq!(summary.inventory.accepts, Some(REPLICATION_PAGE_LIMIT));
    let page = source
        .export_replication_exchange(FLEET, &summary.inventory)
        .unwrap();
    assert_eq!(page.envelopes.len(), REPLICATION_PAGE_LIMIT as usize);
    assert_eq!(page.inventory.accepts, Some(REPLICATION_PAGE_LIMIT));

    // An older peer's inventory has no such field and gets the classic page.
    let older = serde_json::to_value(&summary.inventory).unwrap();
    let mut older = older.as_object().unwrap().clone();
    older.remove("accepts");
    let older: ReplicationInventory = serde_json::from_value(Value::Object(older)).unwrap();
    assert_eq!(older.accepts, None);
    let page = source.export_replication_exchange(FLEET, &older).unwrap();
    assert_eq!(page.envelopes.len(), REPLICATION_EXCHANGE_ENVELOPE_LIMIT);

    // A peer that asks for more than this build sends gets this build's page.
    let greedy = ReplicationInventory {
        accepts: Some(u32::MAX),
        ..summary.inventory.clone()
    };
    let page = source.export_replication_exchange(FLEET, &greedy).unwrap();
    assert_eq!(page.envelopes.len(), REPLICATION_PAGE_LIMIT as usize);
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_catching_up_node_projects_once_per_interval_and_again_when_caught_up() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    for index in 0..1_800 {
        source
            .append_client_claim(&ClaimInput {
                subject: format!("resource/sync-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let pull = || {
        let summary = target.export_replication_summary(FLEET).unwrap();
        // Classic 512-envelope pages, so the backlog takes more than one exchange.
        let inventory = ReplicationInventory {
            accepts: None,
            ..summary.inventory
        };
        let response = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange("source", FLEET, &response)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target
            .project_replication_backlog_unless_catching_up()
            .unwrap()
    };

    // The first exchange projects, so the node shows something at once.
    assert_eq!(pull(), Some(true));
    assert!(target.replication_catching_up());
    let first = target.projected_through();
    assert_eq!(first, target.index().unwrap());

    // While more than one exchange remains, a node that just projected defers.
    assert_eq!(pull(), None);
    assert!(target.replication_catching_up());
    assert!(target.replication_projection_deferred());
    assert_eq!(target.projected_through(), first);

    // Once the interval has passed, it projects again even while catching up.
    target
        .last_replication_projection_unix_ms
        .store(0, Ordering::Release);
    assert_eq!(
        target
            .project_replication_backlog_unless_catching_up()
            .unwrap(),
        Some(true)
    );
    assert!(!target.replication_projection_deferred());
    assert_eq!(target.projected_through(), target.index().unwrap());

    // Once no more than one exchange remains, every exchange projects at once.
    assert_eq!(pull(), Some(true));
    assert!(!target.replication_catching_up());
    assert_eq!(target.projected_through(), target.index().unwrap());
    assert!(!target.replication_projection_deferred());
}

/// A member catching up receives thousands of envelopes in one exchange (#898). Admitting them
/// in one transaction held the only writer for seconds, so every lease renewal and message
/// behind it waited; admission now commits in chunks, and the writer serves others between them.
#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_large_page_is_admitted_in_chunks_that_release_the_writer() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let envelopes = 562;
    for index in 0..envelopes {
        source
            .append_claim(&ClaimInput {
                subject: format!("custom/catch-up/{index}"),
                kind: "custom.catch-up.note".into(),
                actor: Some("person/tester".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let mut inventory = target.replication_inventory().unwrap();
    inventory.accepts = Some(REPLICATION_PAGE_LIMIT);
    let response = source
        .export_replication_exchange(FLEET, &inventory)
        .unwrap();
    assert!(response.envelopes.len() >= envelopes);
    target
        .receive_replication_exchange("source", FLEET, &response)
        .unwrap();
    smallclaims::store::ADMISSION_TRANSACTIONS.with(|count| count.set(0));
    let admission = target.validate_replication_backlog().unwrap();
    assert!(
        smallclaims::store::ADMISSION_TRANSACTIONS.with(std::cell::Cell::get)
            >= envelopes.div_ceil(smallclaims::store::ADMISSION_CHUNK_ENVELOPES)
    );
    assert!(admission.changed);
    target.project_replication_backlog().unwrap();
    assert!(
        target
            .claims_for(&format!("custom/catch-up/{}", envelopes - 1), None)
            .unwrap()
            .len()
            == 1
    );
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn catch_up_projection_cannot_clear_an_admission_deferred_after_its_index_read() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    for index in 0..1_800 {
        source
            .put_document(
                &format!("doc/late-admission/{index}"),
                b"An invented late admission.",
                &None,
                &format!("late-admission-{index}"),
            )
            .unwrap();
    }
    assert!(
        target
            .project_replication_backlog_before_clear(|| {
                // Pass A has already observed no pending claims. Admit a new page and defer
                // pass B before A tries to clear that observation, deterministically.
                let mut inventory = target.replication_inventory().unwrap();
                inventory.accepts = None;
                let exchange = source
                    .export_replication_exchange(FLEET, &inventory)
                    .unwrap();
                target
                    .receive_replication_exchange("source", FLEET, &exchange)
                    .unwrap();
                assert!(target.validate_replication_backlog().unwrap().changed);
                assert!(target.replication_catching_up());
                // Pass A stamped its start before this closure. On a slow host the receive
                // above can outlast the deferral window, and pass B would then project, and
                // wait on A's own projection lock. Stamp now so the window is still open.
                target
                    .last_replication_projection_unix_ms
                    .store(now_ms() as u64, Ordering::Release);
                assert_eq!(
                    target
                        .project_replication_backlog_unless_catching_up()
                        .unwrap(),
                    None
                );
                assert!(target.replication_projection_deferred());
            })
            .unwrap()
    );
    assert!(target.projected_through() < target.index().unwrap());
    assert!(
        target.replication_projection_deferred(),
        "an older projection cleared the newer admission's deferral"
    );
    target
        .last_replication_projection_unix_ms
        .store(0, Ordering::Release);
    assert_eq!(
        target
            .project_replication_backlog_unless_catching_up()
            .unwrap(),
        Some(true)
    );
    assert!(!target.replication_projection_deferred());
    assert_eq!(target.projected_through(), target.index().unwrap());
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn catch_up_projection_commits_frontiers_and_serves_writes_between_them() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    target.project_replication_backlog().unwrap();
    for index in 0..306 {
        source
            .put_document(
                &format!("doc/catch-up/{index}"),
                b"An invented catch-up document.",
                &None,
                &format!("catch-up-doc-{index}"),
            )
            .unwrap();
    }
    let mut inventory = target.replication_inventory().unwrap();
    inventory.accepts = Some(REPLICATION_PAGE_LIMIT);
    let exchange = source
        .export_replication_exchange(FLEET, &inventory)
        .unwrap();
    target
        .receive_replication_exchange("source", FLEET, &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    let admitted = target.index().unwrap();
    let mut frontiers = Vec::new();
    FULL_REPLAYS.with(|count| count.set(0));
    assert!(
        target
            .project_replication_backlog_with_yield(|| {
                // A reader sees a committed prefix while sync comparisons remain deferred. A normal
                // document write can take the writer before the remaining catch-up claims project.
                let frontier = target.projected_through();
                assert!(frontier < admitted);
                assert!(target.replication_projection_deferred());
                assert!(frontiers.last().is_none_or(|last| frontier > *last));
                frontiers.push(frontier);
                target
                    .put_document(
                        &format!("doc/during-catch-up/{}", frontiers.len()),
                        b"A local write while catch-up runs.",
                        &None,
                        &format!("during-catch-up-{}", frontiers.len()),
                    )
                    .unwrap();
            })
            .unwrap()
    );
    assert!(
        frontiers.len() >= 2,
        "catch-up kept the writer for its entire backlog"
    );
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    assert_eq!(target.projected_through(), admitted);
    assert!(
        target.replication_projection_deferred(),
        "later claims still need a pass"
    );
    assert!(target.project_replication_backlog().unwrap());
    assert!(!target.replication_projection_deferred());
    assert_eq!(target.projected_through(), target.index().unwrap());
    let digest = || {
        target
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .graph_digest
    };
    let incremental = digest();
    target.replay_replication_graph().unwrap();
    assert_eq!(
        digest(),
        incremental,
        "interleaved writes must agree with canonical replay"
    );
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn batches_accepted_in_one_millisecond_extend_the_projection_without_a_replay() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let sync = || {
        let response = source
            .export_replication_exchange(FLEET, &target.replication_inventory().unwrap())
            .unwrap();
        assert!(!response.envelopes.is_empty());
        target
            .receive_replication_exchange("source", FLEET, &response)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        assert!(target.project_replication_backlog().unwrap());
    };
    let put = |index: usize| {
        source
            .put_document(
                &format!("doc/same-millisecond-{index}"),
                b"An invented document.",
                &None,
                &format!("same-millisecond-{index}"),
            )
            .unwrap();
    };
    // The first projection of an empty node is a full replay.
    put(0);
    sync();

    // Pin two different batches to the same millisecond: relying on the wall clock
    // can never force a collision on a slower machine.
    let accepted_at = source
        .claims_page(None, None, 0, None, false, 10)
        .unwrap()
        .claims
        .last()
        .unwrap()
        .accepted_at_unix_ms
        + 1;
    source.set_write_clock_at(accepted_at).unwrap();
    put(1);
    put(2);
    let written = source
        .claims_page(None, None, 0, None, false, 10)
        .unwrap()
        .claims
        .into_iter()
        .filter(|claim| claim.kind == "doc.bound")
        .collect::<Vec<_>>();
    let [.., previous, last] = written.as_slice() else {
        panic!("the two document bindings are missing");
    };
    assert_eq!(previous.accepted_at_unix_ms, accepted_at);
    assert_eq!(last.accepted_at_unix_ms, accepted_at);
    assert_ne!(previous.batch_id, last.batch_id);
    FULL_REPLAYS.with(|replays| replays.set(0));
    sync();
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    assert_eq!(
        target
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .graph_digest,
        source
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .graph_digest
    );
}

/// A document republished all day, such as a probe report, arrives from its writer after this
/// node's own newer claims (#898). Each binding keeps the earliest claim of its version whatever
/// order bindings arrive in, so it extends the projection without rebuilding every version, and
/// the result is the one a replay from nothing gives.
#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_document_binding_that_arrives_out_of_order_does_not_rebuild_every_version() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let sync = || {
        let response = source
            .export_replication_exchange(FLEET, &target.replication_inventory().unwrap())
            .unwrap();
        target
            .receive_replication_exchange("source", FLEET, &response)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
    };
    let publish = |version: usize| {
        source
            .put_document(
                "doc/probe-report",
                format!("An invented probe report, version {version}.").as_bytes(),
                &source.latest_document_token("doc/probe-report").unwrap(),
                &format!("probe-report-{version}"),
            )
            .unwrap();
    };
    for version in 0..40 {
        publish(version);
    }
    sync();

    // The target writes a claim of its own that sorts after the next binding.
    let accepted_at = source
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT MAX(CAST(accepted_at_unix_ms AS INTEGER)) FROM claims",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as u128
        + 10;
    target.set_write_clock_at(accepted_at + 10).unwrap();
    target
        .append_claim(&ClaimInput {
            subject: "custom/target/newer".into(),
            kind: "custom.target.note".into(),
            actor: Some("person/tester".into()),
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    target.project_replication_backlog().unwrap();
    source.set_write_clock_at(accepted_at).unwrap();
    publish(40);
    FULL_REPLAYS.with(|replays| replays.set(0));
    BASE_REBUILDS.with(|rebuilds| rebuilds.set(0));
    sync();
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    assert_eq!(BASE_REBUILDS.with(std::cell::Cell::get), 0);

    let documents = |store: &Store| {
        store
            .connection
            .lock()
            .unwrap()
            .prepare(
                "SELECT name, hash, created_index, binding_claim_id, binding_key FROM documents
                 ORDER BY name, hash",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let incremental = documents(&target);
    assert_eq!(incremental.len(), 41);
    target.replay_replication_graph().unwrap();
    assert_eq!(documents(&target), incremental);
}

pub use crate::model::canonical_child_string;

fn canonical_child_strings(value: &Value, name: &str) -> Vec<String> {
    value
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some(name))
        .filter_map(|child| {
            child
                .get("arguments")
                .and_then(Value::as_array)
                .and_then(|arguments| arguments.first())
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
pub fn validate_replicated_claim(
    transaction: &Connection,
    batch: &ReplicaBatch,
    claim: &ClaimRecord,
) -> Result<ReplicatedClaimAdmission, St3Error> {
    smallclaims::store::validate_replicated_claim(
        transaction,
        batch,
        claim,
        &SmalltalkRuntime::default(),
    )
}

/// Whether this build knows a replicated claim's kind and fields, once the graph has verified
/// its batch and hash. The graph then checks that a valid claim's blobs are stored.
fn classify_replicated_claim_with_registry(
    claim: &ClaimRecord,
    registry: &st3_schema::Registry,
) -> Result<ReplicatedClaimAdmission, St3Error> {
    if !registry.claims.contains_key(&claim.kind) && !st3_schema::is_custom_claim_kind(&claim.kind)
    {
        return Ok(ReplicatedClaimAdmission::UnknownKind);
    }
    let fields = schema_fields_for_body(&claim.kind, &claim.body).map_err(|error| {
        St3Error::new(
            "invalid-replicated-claim",
            format!(
                "replicated claim `{}` has an invalid body: {error}",
                claim.id
            ),
        )
    })?;
    if let Err(error) = registry.validate_replicated_claim(&claim.subject, &claim.kind, &fields) {
        // A peer may already publish a field introduced by a newer schema. Keep
        // that authenticated record retryable so an upgrade can admit the
        // original claim without replacing or rewriting it.
        if error.code == "unknown-claim-field" {
            return Ok(ReplicatedClaimAdmission::UnknownField);
        }
        return Err(St3Error::new(
            "invalid-replicated-claim",
            format!(
                "replicated claim `{}` violates {}: {}",
                claim.id, error.code, error.message
            ),
        ));
    }
    if claim.kind == retirements::KIND
        && (fields.get("owner_host").and_then(Value::as_str) != Some(claim.origin.as_str())
            || fields.get("retired_by").and_then(Value::as_str) != claim.actor.as_deref())
    {
        return Err(St3Error::new(
            "invalid-replicated-claim",
            "terminal retirement must name its writing owner and actor",
        ));
    }
    assistant_write::validate_replicated(&claim.kind, claim.actor.as_deref(), claim.body.get("fields").unwrap_or(&claim.body))?;
    crate::terminal_binding::validate_claim(&claim.kind, &claim.body, claim.actor.as_deref())?;
    st3_schema::accountable::validate_claim(&claim.subject, &claim.kind, &claim.body, claim.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    if claim.kind == "owned-set.revised" { owned_sets::validate_receipt(&claim.subject, &claim.body)?; }
    st3_schema::owned_terminals::validate_declaration_owner(
        &claim.subject,
        &claim.kind,
        claim.actor.as_deref(),
    )
    .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::glasses::validate_owner(&claim.subject, claim.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::arrangements::validate_actor(&claim.subject, claim.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::directive_notes::validate_actor(&claim.subject, &claim.kind, claim.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    if claim.subject.starts_with("glass/")
        && (!fields.contains_key("base_revision") || !fields.contains_key("replaced_revision"))
    {
        return Err(St3Error::new(
            "invalid-replicated-claim",
            "glass claims must record base_revision and replaced_revision",
        ));
    }
    Ok(ReplicatedClaimAdmission::Valid)
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn terminal_binding_replica_admission_rejects_a_foreign_actor() {
    let store = Store::open_memory("orchid").unwrap();
    for (key, source) in [
        (
            "terminal",
            "version 2\nterminal \"person/avery/019a0000-0000-7000-8000-000000000001\" { command \"shell\"; restart \"never\"; }",
        ),
        (
            "binding",
            "version 2\nagent \"example/bound\" { harness \"claude\" {}; bind-terminal \"pty/person/avery/019a0000-0000-7000-8000-000000000001\" incarnation=\"shell:created\" id=\"019a0000-0000-7000-8000-000000000002\"; }",
        ),
    ] {
        let intent = crate::parse_intent(source, "orchid").unwrap();
        let expected = intent
            .subjects
            .keys()
            .map(|subject| (subject.clone(), Vec::new()))
            .collect();
        store
            .apply_as(&intent, &expected, key, Some("person/avery"))
            .unwrap();
    }
    let mut claim = store
        .latest_claim("agent/example/bound", Some("intent.desired"))
        .unwrap()
        .unwrap();
    assert!(matches!(
        classify_replicated_claim_with_registry(&claim, st3_schema::registry()),
        Ok(ReplicatedClaimAdmission::Valid)
    ));
    claim.actor = Some("person/intruder".into());
    assert_eq!(
        classify_replicated_claim_with_registry(&claim, st3_schema::registry())
            .err()
            .unwrap()
            .code,
        "terminal-owner-forbidden"
    );
}

/// Reapply this host's lease renewals that have not replicated yet after a projection replay,
/// and forget each one whose lease has ended.
fn reapply_local_work_lease_renewals_tx(transaction: &Transaction<'_>) -> Result<(), St3Error> {
    let mut statement = transaction
        .prepare(
            "SELECT subject, attempt, lease_owner, lease_incarnation,
                    lease_expires_at_unix_ms, updated_at_unix_ms
             FROM local_work_lease_renewals",
        )
        .map_err(internal)?;
    let renewals = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    drop(statement);
    for (subject, attempt, owner, incarnation, expires, updated) in renewals {
        transaction
            .execute(
                "UPDATE step_runs SET lease_expires_at_unix_ms=?5, updated_at_unix_ms=?6
                 WHERE subject=?1 AND attempt=?2 AND lease_owner=?3 AND lease_incarnation=?4
                   AND status NOT IN ('completed','failed','cancelled')
                   AND COALESCE(CAST(lease_expires_at_unix_ms AS INTEGER), 0) < CAST(?5 AS INTEGER)",
                params![subject, attempt, owner, incarnation, expires, updated],
            )
            .map_err(internal)?;
    }
    transaction
        .execute(
            "DELETE FROM local_work_lease_renewals WHERE NOT EXISTS (
               SELECT 1 FROM step_runs WHERE step_runs.subject=local_work_lease_renewals.subject
                 AND step_runs.attempt=local_work_lease_renewals.attempt
                 AND step_runs.lease_owner=local_work_lease_renewals.lease_owner
                 AND step_runs.lease_incarnation=local_work_lease_renewals.lease_incarnation
                 AND step_runs.status NOT IN ('completed','failed','cancelled')
             )",
            [],
        )
        .map_err(internal)?;
    Ok(())
}

/// Advance from a healthy frontier when the new claims have unambiguous operation IDs and
/// structural claims sort after everything projected, in the order the full replay uses.
/// Whether a full replay projects claims of `kind` into the graph tables the graph digest
/// commits, or into planning. A replay records every other kind only as an event and an
/// operation, which the incremental projection does in any order.
fn replay_projects_kind(kind: &str) -> bool {
    matches!(
        kind,
        "intent.desired"
            | "agent.retired"
            | "owned-set.revised"
            | "doc.bound"
            | "mission.published"
            | "mission-run.created"
            | "mission-run.state"
            | "run-generation.created"
            | "run-generation.state"
            | "run-generation.superseded"
            | "revision-proposal.created"
            | "revision-proposal.approved"
            | "revision-proposal.cancelled"
            | "revision-proposal.applied"
            | "step-run.carried"
            | "step-run.state"
            | "step-run.retried"
    ) || kind.starts_with("work.")
        || kind.starts_with("planning-session.")
}

/// A part of the graph that the incremental projection rebuilds on its own when a claim reaches
/// it out of the replay's order: a mission run tree, named by its root run's id, one part of a
/// tree whose root never ended (see `run_part_of_tx`), or the desired subject, document or
/// mission one claim is about. Rebuilding one costs what its own history costs, not what the
/// store's does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Aggregate {
    RunTree(String),
    RunPart(String),
    Desired(String),
    Document(String),
    Mission(String),
}

/// Mark an aggregate to be rebuilt, and say why in the profile: which trigger rebuilds run trees
/// on a busy node decides which of them is worth fixing. The reason is one of a fixed list of
/// strings, so the profile gains at most that many counters; nothing is read to decide it.
fn mark_dirty(dirty: &mut BTreeSet<Aggregate>, aggregate: Aggregate, reason: &str) {
    if matches!(aggregate, Aggregate::RunTree(_) | Aggregate::RunPart(_)) && crate::profile::enabled() {
        crate::profile::note(&format!("projection: run tree dirty: {reason}"));
    }
    dirty.insert(aggregate);
}

/// Whether claims of `kind` belong to a mission run tree.
fn run_tree_kind(kind: &str) -> bool {
    replay_projects_kind(kind)
        && !matches!(kind, "intent.desired" | "agent.retired"
                | "owned-set.revised" | "doc.bound" | "mission.published")
        && !kind.starts_with("planning-session.")
}

/// The aggregate a structural claim belongs to, or `None` when no projected aggregate holds it.
fn aggregate_of_tx(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<Option<Aggregate>, St3Error> {
    Ok(match claim.kind.as_str() {
        "intent.desired" | "agent.retired" => Some(Aggregate::Desired(claim.subject.clone())),
        "doc.bound" => Some(Aggregate::Document(claim.subject.clone())),
        "mission.published" => Some(Aggregate::Mission(claim.subject.clone())),
        _ => run_part_of_tx(transaction, &claim.subject)?,
    })
}

/// The run a generation belongs to: from the graph, or from the claims that create it when the
/// generation is not projected yet.
fn generation_run_tx(
    transaction: &Transaction<'_>,
    generation: &str,
) -> Result<Option<String>, St3Error> {
    if let Some(run) = transaction
        .query_row(
            "SELECT run_id FROM run_generations WHERE id=?1",
            [generation],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
    {
        return Ok(Some(run));
    }
    let subject = format!("run-generation/{generation}");
    let created = transaction
        .query_row(
            &canonical_sql("SELECT json_extract(body, '$.fields.run') FROM claims
             WHERE subject=?1 AND kind='run-generation.created' ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
            [&subject],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(internal)?
        .flatten();
    if let Some(run) = created {
        return Ok(Some(run.trim_start_matches("mission-run/").to_owned()));
    }
    let asked = transaction
        .query_row(
            &canonical_sql("SELECT json_extract(body, '$.fields.run') FROM claims
             WHERE kind='work.person-asked' AND json_extract(body, '$.fields.generation')=?1
             ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
            [&subject],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(internal)?
        .flatten();
    if let Some(run) = asked {
        return Ok(Some(run.trim_start_matches("mission-run/").to_owned()));
    }
    // A run's first generation is named by the claim that creates the run.
    Ok(transaction
        .query_row(
            &canonical_sql(
                "SELECT subject FROM claims WHERE kind='mission-run.created'
               AND json_extract(body, '$.fields.current_generation')=?1
             ORDER BY CANONICAL_ASC(claims) LIMIT 1",
            ),
            [&subject],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
        .map(|run| run.trim_start_matches("mission-run/").to_owned()))
}

/// The root run of the mission run tree `subject` belongs to: a run, a generation, a step run
/// or a revision proposal.
fn run_tree_of_tx(
    transaction: &Transaction<'_>,
    subject: &str,
) -> Result<Option<String>, St3Error> {
    match run_of_subject_tx(transaction, subject)? {
        Some(run) => run_root_tx(transaction, &run).map(Some),
        None => Ok(None),
    }
}

/// The run a run-tree subject belongs to: the run itself, a generation's or step's run, or a
/// proposal's run.
fn run_of_subject_tx(
    transaction: &Transaction<'_>,
    subject: &str,
) -> Result<Option<String>, St3Error> {
    Ok(if let Some(run) = subject.strip_prefix("mission-run/") {
        Some(run.to_owned())
    } else if let Some(generation) = subject.strip_prefix("run-generation/") {
        generation_run_tx(transaction, generation)?
    } else if let Some(step) = subject.strip_prefix("step-run/") {
        generation_run_tx(transaction, step.split('/').next().unwrap_or(step))?
    } else if let Some(proposal) = subject.strip_prefix("revision-proposal/") {
        let projected = transaction
            .query_row(
                "SELECT run_id FROM revision_proposals WHERE id=?1",
                [proposal],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?;
        match projected {
            Some(run) => Some(run),
            None => transaction
                .query_row(
                    &canonical_sql(
                        "SELECT json_extract(body, '$.fields.run') FROM claims
                     WHERE subject=?1 AND kind='revision-proposal.created'
                     ORDER BY CANONICAL_ASC(claims) LIMIT 1",
                    ),
                    [subject],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(internal)?
                .flatten()
                .map(|run| run.trim_start_matches("mission-run/").to_owned()),
        }
    } else {
        None
    })
}

/// The root a run names, or the owner of an ask run, or the run itself.
fn run_root_tx(transaction: &Transaction<'_>, run: &str) -> Result<String, St3Error> {
    if let Some(root) = transaction
        .query_row(
            "SELECT root_run_id FROM mission_runs WHERE id=?1",
            [run],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
    {
        return Ok(root);
    }
    let root = transaction
        .query_row(
            &canonical_sql("SELECT json_extract(body, '$.fields.root_mission_run') FROM claims
             WHERE subject=?1 AND kind='mission-run.created' ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
            [format!("mission-run/{run}")],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(internal)?
        .flatten();
    let root = match root {
        Some(root) => Some(root),
        None => transaction
            .query_row(
                &canonical_sql("SELECT json_extract(body, '$.fields.owner_run') FROM claims
                 WHERE kind='work.person-asked' AND json_extract(body, '$.fields.run')=?1
                 ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
                [format!("mission-run/{run}")],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(internal)?
            .flatten(),
    };
    Ok(root
        .map(|root| root.trim_start_matches("mission-run/").to_owned())
        .unwrap_or_else(|| run.to_owned()))
}

/// Every value of the first column `sql` returns for `value`, without nulls.
fn string_column_tx(
    transaction: &Transaction<'_>,
    sql: &str,
    value: &str,
) -> Result<Vec<String>, St3Error> {
    let mut statement = transaction.prepare_cached(sql).map_err(internal)?;
    let values = statement
        .query_map([value], |row| row.get::<_, Option<String>>(0))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(values.into_iter().flatten().collect())
}

/// The step a run was started from, from the graph or from the claim that creates it. A
/// schedule or subscription that delivers a run is named the same way, as `step-run/` and its
/// subject.
fn parent_step_of_run_tx(
    transaction: &Transaction<'_>,
    run: &str,
) -> Result<Option<String>, St3Error> {
    if let Some(parent) = transaction
        .query_row(
            "SELECT parent_step_run FROM mission_runs WHERE id=?1",
            [run],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(internal)?
    {
        return Ok(parent);
    }
    Ok(transaction
        .query_row(
            &canonical_sql("SELECT json_extract(body, '$.fields.parent_step_run') FROM claims
             WHERE subject=?1 AND kind='mission-run.created' ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
            [format!("mission-run/{run}")],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(internal)?
        .flatten())
}

/// The generation a step run belongs to, or `None` when `parent` names a delivering schedule or
/// subscription rather than a step.
fn step_generation(parent: &str) -> Option<&str> {
    let generation = parent.strip_prefix("step-run/")?.split('/').next()?;
    (generation.len() == 32 && generation.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(generation)
}

/// Whether a root run has ended, or any claim ends it: projected so, or claimed so in any order.
fn run_ever_ended_tx(transaction: &Transaction<'_>, run: &str) -> Result<bool, St3Error> {
    transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM mission_runs WHERE id=?1
                 AND (status IN ('completed','failed','cancelled') OR phase='terminal'))
             OR EXISTS(SELECT 1 FROM claims WHERE subject=?2 AND kind='mission-run.state'
                 AND (COALESCE(json_extract(body, '$.fields.status'), json_extract(body, '$.status'))
                        IN ('completed','failed','cancelled')
                      OR COALESCE(json_extract(body, '$.fields.phase'), json_extract(body, '$.phase'))
                        ='terminal'))",
            params![run, format!("mission-run/{run}")],
            |row| row.get(0),
        )
        .map_err(internal)
}

/// The part of a mission run tree that a claim about `subject` rebuilds with when it arrives out
/// of the replay's order.
///
/// Runs in one tree touch each other only from the root down: a run that ends cancels the runs
/// started from its steps, and every step claim reads whether the root has ended. No run reads
/// or writes a sibling, and no child changes its parent. So while the root has never ended, the
/// tree splits into parts that each rebuild alone to what a replay of the whole tree gives: the
/// root with the person asks it owns, and each run started from the root (by one of its steps,
/// a schedule or a subscription) with every run below it and the asks those runs own. The intake
/// and CI standing roots hold thousands of runs, and one late claim about one of them rebuilt
/// them all in one writer hold of up to 10 s.
///
/// A tree whose root has ended, or whose links have not all arrived, rebuilds whole.
fn run_part_of_tx(
    transaction: &Transaction<'_>,
    subject: &str,
) -> Result<Option<Aggregate>, St3Error> {
    let Some(tree) = run_tree_of_tx(transaction, subject)? else {
        return Ok(None);
    };
    let whole = Ok(Some(Aggregate::RunTree(tree.clone())));
    let Some(mut run) = run_of_subject_tx(transaction, subject)? else {
        return whole;
    };
    // Walk up to the root, remembering the run just below it. An ask run names its owner as its
    // root, and the owner may itself be a child.
    let mut below_root: Option<String> = None;
    for _ in 0..64 {
        let parent = match parent_step_of_run_tx(transaction, &run)? {
            Some(step) => match step_generation(&step) {
                Some(generation) => match generation_run_tx(transaction, generation)? {
                    Some(parent) => parent,
                    None => return whole,
                },
                // A schedule or subscription delivers runs straight under the root.
                None => run_root_tx(transaction, &run)?,
            },
            None => {
                let owner = run_root_tx(transaction, &run)?;
                if owner == run {
                    let root = run;
                    if run_ever_ended_tx(transaction, &root)? {
                        return whole;
                    }
                    return Ok(Some(Aggregate::RunPart(match below_root {
                        // An ask the root owns rebuilds with the root.
                        Some(top) if parent_step_of_run_tx(transaction, &top)?.is_some() => top,
                        _ => root,
                    })));
                }
                owner
            }
        };
        if parent == run {
            return whole;
        }
        below_root = Some(run);
        run = parent;
    }
    whole
}

/// Rebuild one part of a run tree whose root never ended (see `run_part_of_tx`): the root with
/// the asks it owns, or a run started from the root, each with every run started from their steps
/// and every ask they own, all the way down.
fn rebuild_run_part_tx(transaction: &Transaction<'_>, top: &str) -> Result<(), St3Error> {
    #[cfg(any(test, feature = "test-support"))]
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(rebuilds.get() + 1));
    let _span = crate::profile::span("projection/run-tree");
    crate::profile::note("projection: run tree part rebuilt");
    // The tree's root: up through parent steps, deliveries and ask owners.
    let mut root = top.to_owned();
    for _ in 0..64 {
        let parent = match parent_step_of_run_tx(transaction, &root)? {
            Some(step) => match step_generation(&step) {
                Some(generation) => generation_run_tx(transaction, generation)?,
                None => Some(run_root_tx(transaction, &root)?),
            },
            None => Some(run_root_tx(transaction, &root)?),
        };
        match parent {
            Some(parent) if parent != root => root = parent,
            _ => break,
        }
    }
    // Runs not projected yet are found by the claims that create them, which name the root.
    let mut created = transaction
        .prepare_cached(
            "SELECT subject, json_extract(body, '$.fields.parent_step_run') FROM claims
             WHERE kind='mission-run.created'
               AND json_extract(body, '$.fields.root_mission_run')=?1",
        )
        .map_err(internal)?
        .query_map([format!("mission-run/{root}")], |row| {
            Ok((
                row.get::<_, String>(0)?
                    .trim_start_matches("mission-run/")
                    .to_owned(),
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    created.sort();
    let asks_of = |run: &str| -> Result<Vec<String>, St3Error> {
        let mut asks = string_column_tx(
            transaction,
            "SELECT id FROM mission_runs WHERE root_run_id=?1 AND id<>?1 AND parent_step_run IS NULL",
            run,
        )?;
        asks.extend(
            string_column_tx(
                transaction,
                "SELECT json_extract(body, '$.fields.run') FROM claims
                 WHERE kind='work.person-asked' AND json_extract(body, '$.fields.owner_run')=?1",
                &format!("mission-run/{run}"),
            )?
            .into_iter()
            .map(|run| run.trim_start_matches("mission-run/").to_owned()),
        );
        if run == root {
            asks.extend(
                created
                    .iter()
                    .filter(|(child, parent)| parent.is_none() && *child != root)
                    .map(|(child, _)| child.clone()),
            );
        }
        Ok(asks)
    };
    let mut runs = BTreeSet::from([top.to_owned()]);
    // The root's part holds the root and its asks; the runs started from the root's steps, by a
    // schedule or by a subscription are parts of their own.
    let mut pending = if top == root {
        let asks = asks_of(top)?;
        runs.extend(asks.iter().cloned());
        asks
    } else {
        vec![top.to_owned()]
    };
    while let Some(run) = pending.pop() {
        let mut found = asks_of(&run)?;
        if run != root {
            // An ask run names its owner as its root, so the runs that name this run are its asks.
            found.extend(string_column_tx(
                transaction,
                "SELECT id FROM mission_runs WHERE root_run_id=?1 AND id<>?1",
                &run,
            )?);
        }
        for generation in run_generation_ids_tx(transaction, &run)? {
            let start = format!("step-run/{generation}/");
            let end = format!("step-run/{generation}0");
            let mut statement = transaction
                .prepare_cached(
                    "SELECT id FROM mission_runs WHERE parent_step_run>=?1 AND parent_step_run<?2",
                )
                .map_err(internal)?;
            found.extend(
                statement
                    .query_map(params![start, end], |row| row.get::<_, String>(0))
                    .map_err(internal)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(internal)?,
            );
            found.extend(
                created
                    .iter()
                    .filter(|(_, parent)| {
                        parent
                            .as_deref()
                            .is_some_and(|parent| parent.starts_with(&start))
                    })
                    .map(|(child, _)| child.clone()),
            );
        }
        for run in found {
            if runs.insert(run.clone()) {
                pending.push(run);
            }
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    RUN_PART_RUNS.with(|count| count.set(count.get() + runs.len()));
    rebuild_runs_tx(transaction, &runs)
}

/// The generations of one run: projected, created by claims, or named by its asks.
fn run_generation_ids_tx(
    transaction: &Transaction<'_>,
    run: &str,
) -> Result<BTreeSet<String>, St3Error> {
    let subject = format!("mission-run/{run}");
    let mut generations = string_column_tx(
        transaction,
        "SELECT id FROM run_generations WHERE run_id=?1",
        run,
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();
    for generation in string_column_tx(
        transaction,
        "SELECT subject FROM claims WHERE kind='run-generation.created'
           AND json_extract(body, '$.fields.run')=?1",
        &subject,
    )?
    .into_iter()
    .chain(string_column_tx(
        transaction,
        "SELECT json_extract(body, '$.fields.current_generation') FROM claims
         WHERE subject=?1 AND kind='mission-run.created'",
        &subject,
    )?)
    .chain(string_column_tx(
        transaction,
        "SELECT json_extract(body, '$.fields.generation') FROM claims
         WHERE kind='work.person-asked' AND json_extract(body, '$.fields.run')=?1",
        &subject,
    )?) {
        generations.insert(generation.trim_start_matches("run-generation/").to_owned());
    }
    Ok(generations)
}

/// The claims of `subjects`, and of every step run under `generations`, in the replay's order,
/// without repaired records.
fn claims_in_replay_order_tx(
    transaction: &Transaction<'_>,
    subjects: &BTreeSet<String>,
    generations: &BTreeSet<String>,
    kinds: Option<&[&str]>,
) -> Result<Vec<ClaimRecord>, St3Error> {
    let kind_filter = kinds
        .map(|kinds| {
            format!(
                " AND claims.kind IN ({})",
                kinds
                    .iter()
                    .map(|kind| format!("'{kind}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .unwrap_or_default();
    let query = |filter: &str| {
        format!(
            "SELECT {CLAIM_COLUMNS}, batches.origin, batches.replica_sequence,
                    {}
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE {filter}{kind_filter}
               AND NOT EXISTS (SELECT 1 FROM replica_records
                               WHERE replica_records.claim_id=claims.id
                                 AND replica_records.state='repaired')",
            canonical::position_sql("claims")
        )
    };
    let row = |row: &rusqlite::Row<'_>| {
        let claim = claim_from_row(row)?;
        let key = canonical::key_from_record(&claim, row.get(10)?, row.get(11)?, row.get(12)?);
        Ok((claim, key))
    };
    let mut found = Vec::new();
    let mut by_subject = transaction
        .prepare_cached(&query("claims.subject=?1"))
        .map_err(internal)?;
    for subject in subjects {
        for claim in by_subject.query_map([subject], row).map_err(internal)? {
            found.push(claim.map_err(internal)?);
        }
    }
    let mut by_range = transaction
        .prepare_cached(&query("claims.subject>=?1 AND claims.subject<?2"))
        .map_err(internal)?;
    for generation in generations {
        // Every `step-run/GENERATION/...` subject, found by a range on the subject index.
        let start = format!("step-run/{generation}/");
        let end = format!("step-run/{generation}0");
        for claim in by_range
            .query_map(params![start, end], row)
            .map_err(internal)?
        {
            found.push(claim.map_err(internal)?);
        }
    }
    found.sort_by(|left, right| left.1.cmp(&right.1));
    found.dedup_by(|left, right| left.0.id == right.0.id);
    Ok(found.into_iter().map(|(claim, ..)| claim).collect())
}

/// Rebuild one mission run tree from nothing, as a full replay would, but from the tree's own
/// claims: forget its runs, generations, steps and proposals, then project its claims again in
/// the replay's order and passes.
fn rebuild_run_tree_tx(transaction: &Transaction<'_>, root: &str) -> Result<(), St3Error> {
    #[cfg(any(test, feature = "test-support"))]
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(rebuilds.get() + 1));
    let _span = crate::profile::span("projection/run-tree");
    crate::profile::note("projection: run tree rebuilt");
    let strings = |sql: &str, value: &str| string_column_tx(transaction, sql, value);
    let mut runs = BTreeSet::from([root.to_owned()]);
    runs.extend(strings(
        "SELECT id FROM mission_runs WHERE root_run_id=?1",
        root,
    )?);
    runs.extend(
        strings(
            "SELECT subject FROM claims WHERE kind='mission-run.created'
               AND json_extract(body, '$.fields.root_mission_run')=?1",
            &format!("mission-run/{root}"),
        )?
        .into_iter()
        .map(|run| run.trim_start_matches("mission-run/").to_owned()),
    );
    runs.extend(
        strings(
            "SELECT json_extract(body, '$.fields.run') FROM claims
             WHERE kind='work.person-asked' AND json_extract(body, '$.fields.owner_run')=?1",
            &format!("mission-run/{root}"),
        )?
        .into_iter()
        .map(|run| run.trim_start_matches("mission-run/").to_owned()),
    );
    rebuild_runs_tx(transaction, &runs)
}

/// Forget `runs` and their generations, steps and proposals, then project their claims again in
/// the replay's order and passes.
fn rebuild_runs_tx(transaction: &Transaction<'_>, runs: &BTreeSet<String>) -> Result<(), St3Error> {
    let strings = |sql: &str, value: &str| string_column_tx(transaction, sql, value);
    let mut generations = BTreeSet::new();
    let mut proposals = BTreeSet::new();
    for run in runs {
        let subject = format!("mission-run/{run}");
        generations.extend(run_generation_ids_tx(transaction, run)?);
        proposals.extend(strings(
            "SELECT id FROM revision_proposals WHERE run_id=?1",
            run,
        )?);
        proposals.extend(
            strings(
                "SELECT subject FROM claims WHERE kind='revision-proposal.created'
                   AND json_extract(body, '$.fields.run')=?1",
                &subject,
            )?
            .into_iter()
            .map(|proposal| proposal.trim_start_matches("revision-proposal/").to_owned()),
        );
    }
    let subjects = runs
        .iter()
        .map(|run| format!("mission-run/{run}"))
        .chain(
            generations
                .iter()
                .map(|generation| format!("run-generation/{generation}")),
        )
        .chain(
            proposals
                .iter()
                .map(|proposal| format!("revision-proposal/{proposal}")),
        )
        .collect::<BTreeSet<_>>();
    let claims = claims_in_replay_order_tx(transaction, &subjects, &generations, None)?;
    for run in runs {
        for table in [
            "step_runs",
            "revision_proposals",
            "mission_run_deadlines",
            "mission_run_after",
            "run_generations",
        ] {
            transaction
                .execute(&format!("DELETE FROM {table} WHERE run_id=?1"), [run])
                .map_err(internal)?;
        }
        transaction
            .execute("DELETE FROM mission_runs WHERE id=?1", [run])
            .map_err(internal)?;
    }
    for claim in &claims {
        transaction
            .execute(
                "DELETE FROM projection_health WHERE aggregate=?1",
                [format!("projection:runs:{}", claim.id)],
            )
            .map_err(internal)?;
    }
    for pass in 0..3 {
        for claim in &claims {
            let included = match pass {
                0 => claim.kind == "mission-run.created",
                1 => {
                    claim.kind.starts_with("work.")
                        || matches!(
                            claim.kind.as_str(),
                            "mission-run.state"
                                | "run-generation.created"
                                | "run-generation.state"
                                | "run-generation.superseded"
                                | "revision-proposal.created"
                                | "revision-proposal.approved"
                                | "revision-proposal.cancelled"
                                | "revision-proposal.applied"
                                | "step-run.carried"
                                | "step-run.state"
                                | "step-run.retried"
                        )
                }
                _ => claim.kind == "step-run.carried",
            };
            if !included {
                continue;
            }
            project_claim_isolated_tx(transaction, "projection:runs", claim, || match pass {
                0 => project_mission_run_created(transaction, claim),
                1 => project_mission_run_update(transaction, claim),
                _ => reconcile_carried_step_tx(transaction, claim),
            })?;
        }
    }
    reapply_local_work_lease_renewals_tx(transaction)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Run trees this thread rebuilt from their own claims.
    static RUN_TREE_REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Runs this thread rebuilt as parts of run trees whose root never ended.
    static RUN_PART_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Desired subjects, documents and missions this thread rebuilt from their own claims.
    static BASE_REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Rebuild one desired subject, document or mission from its own claims in the replay's order.
fn rebuild_base_aggregate_tx(
    transaction: &Transaction<'_>,
    aggregate: &Aggregate,
) -> Result<(), St3Error> {
    let (subject, kind) = match aggregate {
        Aggregate::Desired(subject) => (subject, "intent.desired"),
        Aggregate::Document(subject) => (subject, "doc.bound"),
        Aggregate::Mission(subject) => (subject, "mission.published"),
        Aggregate::RunTree(_) | Aggregate::RunPart(_) => return Ok(()),
    };
    crate::profile::note(&format!("projection: {kind} subject rebuilt"));
    #[cfg(any(test, feature = "test-support"))]
    BASE_REBUILDS.with(|rebuilds| rebuilds.set(rebuilds.get() + 1));
    match aggregate {
        Aggregate::Desired(subject) => {
            transaction
                .execute("DELETE FROM desired WHERE subject=?1", [subject])
                .map_err(internal)?;
        }
        Aggregate::Mission(subject) => {
            transaction
                .execute(
                    "DELETE FROM mission_definitions WHERE mission_id=?1",
                    [subject.trim_start_matches("mission/")],
                )
                .map_err(internal)?;
        }
        _ => {}
    }
    let claims = claims_in_replay_order_tx(
        transaction,
        &BTreeSet::from([subject.clone()]),
        &BTreeSet::new(),
        Some(&[kind]),
    )?;
    for claim in &claims {
        project_claim_isolated_tx(transaction, "projection:base", claim, || {
            match claim.kind.as_str() {
                "intent.desired" => {
                    let desired = serde_json::from_value::<DesiredSubject>(claim.body.clone())
                        .map_err(internal)?;
                    select_replicated_desired(transaction, claim, &desired)
                }
                "doc.bound" => select_replicated_document(transaction, claim, claim.store_index),
                _ => select_replicated_mission(transaction, claim, claim.store_index),
            }
        })?;
    }
    if let Aggregate::Desired(subject) = aggregate {
        retirements::project_subject(transaction, subject)?;
    }
    Ok(())
}

/// Identify the claim being processed, without copying its fields, payload or authority data.
fn incremental_claim_error(error: St3Error, claim: &ClaimRecord, stage: &'static str) -> St3Error {
    let operation = operation_parts(&claim.body).map(|(operation, _)| operation);
    let truncated = [&claim.id, &claim.subject, &claim.kind]
        .into_iter()
        .any(|value| value.chars().take(257).count() > 256)
        || operation.is_some_and(|value| value.chars().take(257).count() > 256);
    let bounded = |value: &str| value.chars().take(256).collect::<String>();
    let error = error
        .with_detail("projection_stage", stage)
        .with_detail("projection_claim_id", bounded(&claim.id))
        .with_detail("projection_subject", bounded(&claim.subject))
        .with_detail("projection_claim_kind", bounded(&claim.kind))
        .with_detail("projection_context_truncated", truncated);
    match operation {
        Some(operation) => error.with_detail("projection_operation_id", bounded(operation)),
        None => error,
    }
}

/// Retain the guard reason for the always-on log at the actual fallback boundary.
fn replay_needed(reason: &'static str) -> IncrementalProjection {
    crate::profile::note(&format!("replay: {reason}"));
    IncrementalProjection::Replay(reason)
}

#[cfg(any(test, feature = "test-support"))]
pub fn try_project_all_simple_replication_tx(
    transaction: &Transaction<'_>,
    origin: &str,
) -> Result<bool, St3Error> {
    try_project_simple_replication_tx(
        transaction,
        origin,
        current_index_tx(transaction).map_err(internal)?,
    )
    .map(|decision| decision == IncrementalProjection::Projected)
}

fn try_project_simple_replication_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    through: u64,
) -> Result<IncrementalProjection, St3Error> {
    let health: Option<(String, u64)> = transaction
        .query_row(
            "SELECT status, last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((status, frontier)) = health else {
        return Ok(replay_needed("missing-health"));
    };
    if status != "healthy" {
        return Ok(replay_needed("unhealthy-projection"));
    }
    if frontier > current_index_tx(transaction).map_err(internal)? {
        return Ok(replay_needed("frontier-ahead"));
    }
    let mut statement = transaction
        .prepare(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                    predecessors, accepted_at_unix_ms
             FROM claims WHERE store_index > ?1 AND store_index <= ?2 ORDER BY store_index",
        )
        .map_err(internal)?;
    let claims = statement
        .query_map([frontier, through], claim_from_row)
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    drop(statement);

    // This node applied its own claims to the graph when it wrote them, and a quiet lease renewal
    // can move a step on since without writing one, so projecting one of them again could undo
    // what followed it. A run tree that holds one of them is rebuilt from its own claims in the
    // replay's order instead; another writer's claims about any other tree project as they come.
    let mut work_claims = claims
        .iter()
        .filter(|claim| claim.kind.starts_with("work.") && claim.origin != origin)
        .collect::<Vec<_>>();
    let order_keys = claims
        .iter()
        .filter(|claim| run_tree_kind(&claim.kind))
        .map(|claim| {
            Ok((
                claim.id.clone(),
                canonical::claim_key(transaction, &claim.id).map_err(|error| {
                    incremental_claim_error(internal(error), claim, "canonical-key")
                })?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, St3Error>>()?;
    work_claims.sort_by_key(|claim| &order_keys[&claim.id]);
    // The full replay orders claims by accepted time, then by batch writer and sequence. A
    // structural claim extends the projection only when it sorts after everything projected
    // so far; the claims of one batch share that key and keep their order within the batch.
    let mut last_key = if claims
        .iter()
        .any(|claim| !Store::simple_replication_kind(&claim.kind))
    {
        transaction.query_row(&canonical_sql(
            "SELECT id FROM claims INDEXED BY claims_accepted_order_index WHERE store_index<=?1 ORDER BY CANONICAL_DESC(claims) LIMIT 1"
        ), [frontier], |row| row.get::<_, String>(0)).optional().map_err(internal)?
            .map(|id| canonical::claim_key(transaction, &id).map_err(internal)).transpose()?
    } else {
        None
    };
    // A claim that reaches a part of the graph out of the replay's order marks that part to be
    // rebuilt from its own claims; everything else extends the graph as it arrives.
    let mut dirty = BTreeSet::<Aggregate>::new();
    let mut rebuild_planning = false;
    // A run tree needs rebuilding for this node's own claims only when another writer's claims
    // about the same tree arrived in the same range, since only then can the two be out of the
    // replay's order. A tree that holds this node's claims alone already has them: they were
    // applied when written, and projecting them again, or rebuilding the tree to do so, changes
    // nothing. Rebuilding every such tree on every pass cost 15 s of writer time, 130,000
    // statements, for one large tree on a busy node.
    // A tree whose root never ended splits into parts that rebuild alone (`run_part_of_tx`), so
    // a local claim is rebuilt only when another writer's claims reached its own part, or reached
    // its tree where that tree cannot split.
    let mut foreign_parts = BTreeSet::new();
    let mut foreign_whole = BTreeSet::new();
    for claim in claims
        .iter()
        .filter(|claim| claim.origin != origin && run_tree_kind(&claim.kind))
    {
        let part = run_part_of_tx(transaction, &claim.subject)
            .map_err(|error| incremental_claim_error(error, claim, "run-tree-owner"))?;
        if let Some(root) = run_tree_of_tx(transaction, &claim.subject)
            .map_err(|error| incremental_claim_error(error, claim, "run-tree-owner"))?
            && matches!(part, Some(Aggregate::RunTree(_)))
        {
            foreign_whole.insert(root);
        }
        foreign_parts.extend(part);
    }
    for claim in claims
        .iter()
        .filter(|claim| claim.origin == origin && run_tree_kind(&claim.kind))
    {
        // A local extension is the exception: the write path does not set the step's blocked
        // reason the way a replay does, so its tree is still rebuilt (extensions are rare).
        if foreign_parts.is_empty() && claim.kind != "work.extended" {
            continue;
        }
        let Some(root) = run_tree_of_tx(transaction, &claim.subject)
            .map_err(|error| incremental_claim_error(error, claim, "run-tree-owner"))?
        else {
            continue;
        };
        let Some(part) = run_part_of_tx(transaction, &claim.subject)
            .map_err(|error| incremental_claim_error(error, claim, "run-tree-owner"))?
        else {
            continue;
        };
        // The label means a local claim that forces a rebuild: a work extension, or a local
        // claim in a part of a tree that another writer's claims reached in the same range.
        if foreign_whole.contains(&root) {
            mark_dirty(&mut dirty, Aggregate::RunTree(root), "local claim");
        } else if claim.kind == "work.extended" || foreign_parts.contains(&part) {
            mark_dirty(&mut dirty, part, "local claim");
        }
    }
    for claim in &claims {
        let has_operation = claim.body.get("_operation").is_some();
        // Supported work transitions already share canonical ordering and affected-tree
        // repair with replay. An operation identity alone changes neither the work fields
        // nor that ordering. Validate its shape here and its digest/state in the registry
        // below; do not force a full-store replay based on the identity's prefix.
        if claim.kind.starts_with("planning-session.") {
            rebuild_planning = true;
        }
        if replay_projects_kind(&claim.kind)
            && !Store::simple_replication_kind(&claim.kind)
            && !matches!(
                claim.kind.as_str(),
                "intent.desired"
                    | "agent.retired"
                    | "work.person-asked"
                    | "work.person-done"
                    | "work.person-cancelled"
                    | "doc.bound"
                    | "mission.published"
                    | "mission-run.created"
                    | "mission-run.state"
                    | "run-generation.created"
                    | "run-generation.state"
                    | "run-generation.superseded"
                    | "revision-proposal.created"
                    | "revision-proposal.approved"
                    | "revision-proposal.cancelled"
                    | "revision-proposal.applied"
                    | "step-run.carried"
                    | "step-run.state"
                    | "step-run.retried"
            )
            || (has_operation && operation_parts(&claim.body).is_none())
        {
            let reason = if operation_parts(&claim.body).is_none() && has_operation {
                "malformed-operation"
            } else {
                "non-incremental-kind"
            };
            crate::profile::note(&format!("replay: {reason}"));
            return Ok(IncrementalProjection::ReplayWithContext {
                reason,
                details: incremental_claim_error(
                    St3Error::new(reason, "claim requires canonical replay"),
                    claim,
                    "incremental-guard",
                ).details,
            });
        }
        // Person asks add steps (and sometimes a whole run) in their own claim. Their response
        // also resumes an originating step. Rebuild the affected tree so a response received
        // before its ask, or a later local update, has the same result as canonical replay.
        if matches!(
            claim.kind.as_str(),
            "work.person-asked" | "work.person-done" | "work.person-cancelled"
        ) && let Some(aggregate) = aggregate_of_tx(transaction, claim)
            .map_err(|error| incremental_claim_error(error, claim, "aggregate-owner"))?
        {
            mark_dirty(&mut dirty, aggregate, "person ask, answer or cancel");
        }
        if !Store::simple_replication_kind(&claim.kind) {
            let key = canonical::claim_key(transaction, &claim.id).map_err(|error| {
                incremental_claim_error(internal(error), claim, "canonical-key")
            })?;
            let out_of_order = last_key.as_ref().is_some_and(|last| key < *last);
            if last_key.as_ref().is_none_or(|last| key > *last) {
                last_key = Some(key);
            }
            let repaired: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=?1 AND state='repaired')",
                    [&claim.id],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            // A claim about a run tree not projected yet waits in the claim log: the claim that
            // creates its run or generation rebuilds the tree with it.
            // A document keeps the earliest binding of each version, whatever order the
            // bindings arrive in, so only a repair rebuilds one. Rebuilding every version of a
            // document that is republished all day costs more with each version.
            if ((out_of_order && claim.kind != "doc.bound") || repaired)
                && let Some(aggregate) = aggregate_of_tx(transaction, claim)
                    .map_err(|error| incremental_claim_error(error, claim, "aggregate-owner"))?
            {
                let reason = if repaired {
                    "repaired claim"
                } else {
                    "structural claim sorting before projected ones"
                };
                mark_dirty(&mut dirty, aggregate, reason);
            }
            if claim.kind == "mission-run.created" {
                let generation = claim
                    .body
                    .get("fields")
                    .unwrap_or(&claim.body)
                    .get("current_generation")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let generation_id = generation_id_from_subject(generation);
                let prior_dependents: bool = transaction
                    .query_row(
                        RUN_HAS_PRIOR_CLAIMS,
                        params![
                            frontier,
                            claim.subject,
                            generation,
                            format!("step-run/{generation_id}/"),
                            format!("step-run/{generation_id}0"),
                        ],
                        |row| row.get(0),
                    )
                    .map_err(internal)?;
                if prior_dependents
                    && let Some(aggregate) = aggregate_of_tx(transaction, claim)
                        .map_err(|error| incremental_claim_error(error, claim, "aggregate-owner"))?
                {
                    mark_dirty(
                        &mut dirty,
                        aggregate,
                        "run created after claims that depend on it",
                    );
                }
            }
            if claim.kind == "run-generation.created" {
                // A generation whose steps already have claims, from a peer that sent them first,
                // is rebuilt with its tree so those claims apply in the replay's order.
                let prior_dependents: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM claims WHERE store_index<=?1
                         AND subject>=?2 AND subject<?3)",
                        params![
                            frontier,
                            format!("step-run/{}/", generation_id_from_subject(&claim.subject)),
                            format!("step-run/{}0", generation_id_from_subject(&claim.subject)),
                        ],
                        |row| row.get(0),
                    )
                    .map_err(internal)?;
                if prior_dependents
                    && let Some(aggregate) = aggregate_of_tx(transaction, claim)
                        .map_err(|error| incremental_claim_error(error, claim, "aggregate-owner"))?
                {
                    mark_dirty(
                        &mut dirty,
                        aggregate,
                        "generation created after its steps' claims",
                    );
                }
            }
            if claim.kind == "mission.published" {
                let revision = claim
                    .body
                    .get("revision")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // A run or generation that arrived before the revision it runs, and a proposal to
                // adopt it, wait for it; their trees are rebuilt once the revision is projected.
                let mut waiting = Vec::new();
                for query in [
                    "SELECT pending.subject FROM claims AS pending
                     WHERE pending.kind='mission-run.created'
                       AND json_extract(pending.body, '$.fields.revision')=?2
                       AND json_extract(pending.body, '$.fields.mission')=?1
                       AND NOT EXISTS(SELECT 1 FROM mission_runs AS projected
                                      WHERE projected.id=substr(pending.subject, 13))",
                    "SELECT pending.subject FROM claims AS pending
                     WHERE pending.kind='run-generation.created'
                       AND json_extract(pending.body, '$.fields.revision')=?2
                       AND ?1 IS NOT NULL
                       AND NOT EXISTS(SELECT 1 FROM run_generations AS projected
                                      WHERE projected.id=substr(pending.subject, 16))",
                    "SELECT pending.subject FROM claims AS pending
                     WHERE pending.kind='revision-proposal.created'
                       AND json_extract(pending.body, '$.fields.candidate_revision')=?2
                       AND ?1 IS NOT NULL",
                ] {
                    let mut statement = transaction.prepare_cached(query).map_err(internal)?;
                    waiting.extend(
                        statement
                            .query_map(params![claim.subject, revision], |row| {
                                row.get::<_, String>(0)
                            })
                            .map_err(internal)?
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map_err(internal)?,
                    );
                }
                for run in waiting {
                    if let Some(part) = run_part_of_tx(transaction, &run)? {
                        mark_dirty(
                            &mut dirty,
                            part,
                            "mission published after runs waiting for its revision",
                        );
                    }
                }
            }
        }
    }
    // Event-only claims do not change the structural graph. Their operation
    // registry can advance with the frontier. Message sends reconcile an ambiguous
    // receipt from that operation's indexed history, using the full replay's reducer.
    // Other kinds retain the full replay guard for ambiguous IDs.
    let mut reconciled_message_operations = BTreeSet::new();
    for claim in &claims {
        let Some((operation_id, request_digest)) = operation_parts(&claim.body) else {
            continue;
        };
        if claim.kind == "message.sent" && reconciled_message_operations.contains(operation_id) {
            continue;
        }
        if let Some((stored_digest, _, state)) =
            operation_tx(transaction, operation_id).map_err(|error| {
                incremental_claim_error(internal(error), claim, "operation-registry")
            })?
            && (stored_digest != request_digest || state != "active")
        {
            if claim.kind == "message.sent" {
                repair_operations_tx(transaction, &[operation_id.to_owned()]).map_err(|error| {
                    incremental_claim_error(internal(error), claim, "operation-registry")
                })?;
                reconciled_message_operations.insert(operation_id.to_owned());
                continue;
            }
            crate::profile::note("replay: operation-conflict");
            return Ok(IncrementalProjection::ReplayWithContext {
                reason: "operation-conflict",
                details: incremental_claim_error(St3Error::new("operation-conflict",
                    "operation digest differs or registry state is not active"), claim,
                    "operation-registry").with_detail("projection_error", format!(
                        "operation registry refuses incoming claim: digest_matches={}; stored state {}", stored_digest == request_digest, state.chars().take(32).collect::<String>()
                    )).details,
            });
        }
        register_operation_tx(transaction, claim).map_err(|error| {
            incremental_claim_error(internal(error), claim, "operation-registry")
        })?;
    }
    let mut latest_work_by_subject = BTreeMap::new();
    for claim in &work_claims {
        let previous = if let Some(previous) = latest_work_by_subject.get(&claim.subject) {
            Some(*previous)
        } else {
            transaction
                .query_row(
                    "SELECT updated_at_unix_ms FROM step_runs WHERE subject=?1",
                    [&claim.subject],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| incremental_claim_error(internal(error), claim, "work-frontier"))?
                .and_then(|value: String| value.parse::<u128>().ok())
        };
        if previous.is_none_or(|value| claim.accepted_at_unix_ms <= value) {
            // A work claim for a step that is not projected yet waits in the claim log; the
            // generation that creates the step rebuilds its tree and applies it.
            if let Some(part) = run_part_of_tx(transaction, &claim.subject)
                .map_err(|error| incremental_claim_error(error, claim, "run-tree-owner"))?
            {
                mark_dirty(
                    &mut dirty,
                    part,
                    "another writer's work claim not newer than the step",
                );
            }
            continue;
        }
        latest_work_by_subject.insert(&claim.subject, claim.accepted_at_unix_ms);
    }
    for claim in &claims {
        insert_event(
            transaction,
            claim.store_index,
            &claim.kind,
            &claim.subject,
            &claim.body,
        )
        .map_err(|error| incremental_claim_error(internal(error), claim, "event-insert"))?;
    }
    for claim in &claims {
        for projector in projectors::REPLICATED.iter().filter(|p| p.takes.takes(&claim.kind)) {
            (projector.project)(transaction, claim).map_err(|error| {
                incremental_claim_error(internal(error), claim, projector.name)
            })?;
        }
    }
    let mut aggregates = BTreeMap::<String, Option<Aggregate>>::new();
    // A claim is left to its aggregate's rebuild when that aggregate is dirty, and left waiting
    // when it is about a run tree that nothing has created yet.
    let mut in_dirty = |claim: &ClaimRecord| -> Result<bool, St3Error> {
        if Store::simple_replication_kind(&claim.kind) && !claim.kind.starts_with("work.") {
            return Ok(false);
        }
        let aggregate = match aggregates.get(claim.subject.as_str()) {
            Some(aggregate) => aggregate.clone(),
            None => {
                let aggregate = aggregate_of_tx(transaction, claim)
                    .map_err(|error| incremental_claim_error(error, claim, "aggregate-owner"))?;
                aggregates.insert(claim.subject.clone(), aggregate.clone());
                aggregate
            }
        };
        Ok(match aggregate {
            Some(aggregate) => dirty.contains(&aggregate),
            None => claim.kind != "mission-run.created" && run_tree_kind(&claim.kind),
        })
    };
    for claim in &claims {
        if in_dirty(claim)? {
            continue;
        }
        match claim.kind.as_str() {
            "agent.retired" => retirements::project_subject(transaction, &claim.subject)?,
            "intent.desired" => {
                let desired = serde_json::from_value::<DesiredSubject>(claim.body.clone())
                    .map_err(|error| {
                        incremental_claim_error(internal(error), claim, "desired-decode")
                    })?;
                select_replicated_desired(transaction, claim, &desired)
                    .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?;
            }
            "doc.bound" => select_replicated_document(transaction, claim, claim.store_index)
                .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?,
            "mission.published" => {
                select_replicated_mission(transaction, claim, claim.store_index)
                    .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?;
            }
            "mission-run.created" => project_mission_run_created(transaction, claim)
                .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?,
            _ => {}
        }
    }
    let mut run_updates = claims
        .iter()
        .filter(|claim| {
            claim.kind.starts_with("work.")
                || matches!(
                    claim.kind.as_str(),
                    "mission-run.state"
                        | "run-generation.created"
                        | "run-generation.state"
                        | "run-generation.superseded"
                        | "revision-proposal.created"
                        | "revision-proposal.approved"
                        | "revision-proposal.cancelled"
                        | "revision-proposal.applied"
                        | "step-run.carried"
                        | "step-run.state"
                        | "step-run.retried"
                )
        })
        .collect::<Vec<_>>();
    run_updates.sort_by_key(|claim| &order_keys[&claim.id]);
    for claim in run_updates {
        // This node's own claims are in the graph already, whether or not their tree is rebuilt.
        if claim.origin == origin || in_dirty(claim)? {
            continue;
        }
        project_mission_run_update(transaction, claim)
            .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?;
    }
    for claim in claims
        .iter()
        .filter(|claim| claim.kind == "step-run.carried")
    {
        if claim.origin == origin || in_dirty(claim)? {
            continue;
        }
        reconcile_carried_step_tx(transaction, claim)
            .map_err(|error| incremental_claim_error(error, claim, "claim-project"))?;
    }
    // Missions first, since a rebuilt run reads the revision it runs.
    let (trees, bases): (Vec<_>, Vec<_>) = dirty
        .iter()
        .partition(|aggregate| matches!(aggregate, Aggregate::RunTree(_) | Aggregate::RunPart(_)));
    for aggregate in bases {
        rebuild_base_aggregate_tx(transaction, aggregate).map_err(|error| {
            let subject = match aggregate {
                Aggregate::Desired(subject)
                | Aggregate::Document(subject)
                | Aggregate::Mission(subject) => subject.clone(),
                Aggregate::RunTree(root) | Aggregate::RunPart(root) => format!("mission-run/{root}"),
            };
            error
                .with_detail("projection_stage", "base-aggregate-rebuild")
                .with_detail("projection_subject", subject)
        })?;
    }
    for aggregate in trees {
        let (rebuilt, root) = match aggregate {
            Aggregate::RunTree(root) => (rebuild_run_tree_tx(transaction, root), root),
            Aggregate::RunPart(top) => (rebuild_run_part_tx(transaction, top), top),
            _ => continue,
        };
        rebuilt.map_err(|error| {
            error
                .with_detail("projection_stage", "run-tree-rebuild")
                .with_detail("projection_subject", format!("mission-run/{root}"))
        })?;
    }
    if rebuild_planning {
        rebuild_planning_tx(transaction)
            .map_err(|error| internal(error).with_detail("projection_stage", "planning-rebuild"))?;
    }
    owned_sets::project_tx(transaction)
        .map_err(|error| error.with_detail("projection_stage", "owned-set-project"))?;
    Ok(IncrementalProjection::Projected)
}

/// The graph tables a replay derives from claims alone, children before the tables their
/// foreign keys name. Local tables, such as lease renewals, are not among them.
const REPLAYED_GRAPH_TABLES: [&str; 8] = [
    "desired",
    "mission_definitions",
    "mission_run_deadlines",
    "mission_run_after",
    "step_runs",
    "revision_proposals",
    "run_generations",
    "mission_runs",
];

/// Clear the graph and replay every claim into it, as a node that joins late does. A replay
/// over the rows an earlier projection left behind depends on how that projection ran: it
/// skipped a generation it had already projected, so it re-applied a terminal state that the
/// generation had reopened. Two nodes holding the same claims then showed different graphs.
pub fn replay_graph_from_nothing_tx(transaction: &Transaction<'_>) -> Result<(), St3Error> {
    replay_graph_from_nothing_with_progress_tx(transaction, &mut |_| {})
}

fn replay_graph_from_nothing_with_progress_tx(
    transaction: &Transaction<'_>,
    progress: &mut dyn FnMut(ReplayProgress),
) -> Result<(), St3Error> {
    let mut stage = |phase| {
        progress(ReplayProgress {
            phase,
            processed: None,
            total: None,
        })
    };
    stage("full-replay/clear");
    for table in REPLAYED_GRAPH_TABLES {
        transaction
            .execute(&format!("DELETE FROM {table}"), [])
            .map_err(internal)?;
    }
    stage("full-replay/operations");
    rebuild_operations_tx(transaction).map_err(internal)?;
    project_replicated_base_claims_with_progress(transaction, progress)?;
    let mut stage = |phase| {
        progress(ReplayProgress {
            phase,
            processed: None,
            total: None,
        })
    };
    stage("full-replay/mission-runs");
    project_replicated_mission_runs(transaction)?;
    for replay in projectors::GRAPH_REPLAY {
        stage(replay.phase);
        (replay.run)(transaction).map_err(internal)?;
    }
    Ok(())
}

fn project_replicated_base_claims(transaction: &Transaction<'_>) -> Result<(), St3Error> {
    project_replicated_base_claims_with_progress(transaction, &mut |_| {})
}

fn project_replicated_base_claims_with_progress(
    transaction: &Transaction<'_>,
    progress: &mut dyn FnMut(ReplayProgress),
) -> Result<(), St3Error> {
    const BATCH: usize = 256;
    // Event positions do not consume bodies or canonical fold order. Preserve the same
    // admitted/repaired and case-sensitive glass exclusions without decoding unrelated
    // observation, message or work bodies just to insert their positions.
    transaction.execute(
        "INSERT OR IGNORE INTO event_positions(store_index, subject)
         SELECT claims.store_index, claims.subject FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE substr(claims.subject,1,6) <> 'glass/' COLLATE BINARY
           AND NOT EXISTS (SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')",
        [],
    ).map_err(internal)?;
    let selection = "FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.kind IN ('intent.desired','doc.bound','mission.published')
               AND NOT EXISTS (
                 SELECT 1 FROM replica_records
                 WHERE replica_records.claim_id=claims.id
                   AND replica_records.state='repaired'
             )";
    // Sort selected IDs once. Re-evaluating the canonical key for every body page
    // would scan/sort the remaining selected history once per page. This temporary
    // order contains no bodies and lives only on this serialized writer connection.
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS heal_replay_base_order(
             ordinal INTEGER PRIMARY KEY, claim_id TEXT NOT NULL);
         DELETE FROM temp.heal_replay_base_order;",
        )
        .map_err(internal)?;
    transaction
        .execute(
            &canonical_sql(&format!(
                "INSERT INTO temp.heal_replay_base_order(claim_id)
             SELECT claims.id {selection} ORDER BY CANONICAL_ASC(claims)"
            )),
            [],
        )
        .map_err(internal)?;
    let total: u64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(ordinal),0) FROM temp.heal_replay_base_order",
            [],
            |row| row.get(0),
        )
        .map_err(internal)?;
    clear_quarantined_claims_tx(transaction, "projection:base")?;

    progress(ReplayProgress {
        phase: "full-replay/base-claims",
        processed: Some(0),
        total: Some(total),
    });
    let mut cursor: u64 = 0;
    let mut processed = 0;
    loop {
        // Finish/drop the read before any per-claim savepoint can roll back. Bound retained
        // bodies, and use the same total canonical key as the full fold, including ties.
        let claims = {
            let mut statement = transaction
                .prepare_cached(BASE_REPLAY_PAGE_SQL)
                .map_err(internal)?;
            statement
                .query_map(params![cursor, BATCH as i64], |row| {
                    Ok((claim_from_row(row)?, row.get::<_, u64>(10)?))
                })
                .map_err(internal)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(internal)?
        };
        if claims.is_empty() {
            break;
        }
        cursor = claims.last().expect("nonempty replay page").1;
        for (claim, _) in claims {
            project_claim_isolated_tx(transaction, "projection:base", &claim, || {
                match claim.kind.as_str() {
                    "intent.desired" => {
                        let desired = serde_json::from_value::<DesiredSubject>(claim.body.clone())
                            .map_err(internal)?;
                        select_replicated_desired(transaction, &claim, &desired)?;
                    }
                    "doc.bound" => {
                        select_replicated_document(transaction, &claim, claim.store_index)?
                    }
                    "mission.published" => {
                        select_replicated_mission(transaction, &claim, claim.store_index)?
                    }
                    _ => {}
                }
                Ok(())
            })?;
            processed += 1;
        }
        progress(ReplayProgress {
            phase: "full-replay/base-claims",
            processed: Some(processed),
            total: Some(total),
        });
    }
    transaction
        .execute("DELETE FROM temp.heal_replay_base_order", [])
        .map_err(internal)?;
    owned_sets::project_tx(transaction)
}

const BASE_REPLAY_PAGE_SQL: &str =
    "SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind,
            claims.origin, claims.actor, claims.body, claims.predecessors,
            claims.accepted_at_unix_ms, ordering.ordinal
     FROM temp.heal_replay_base_order ordering
     JOIN claims ON claims.id=ordering.claim_id
     WHERE ordering.ordinal>?1 ORDER BY ordering.ordinal LIMIT ?2";

/// Project one replicated claim on its own. A claim whose projection fails is rolled back alone
/// and recorded as an unhealthy projection that names it, so one claim a build cannot project
/// never holds every later claim from every peer out of the graph.
fn project_claim_isolated_tx(
    transaction: &Transaction<'_>,
    aggregate: &str,
    claim: &ClaimRecord,
    project: impl FnOnce() -> Result<(), St3Error>,
) -> Result<(), St3Error> {
    transaction
        .execute_batch("SAVEPOINT project_claim")
        .map_err(internal)?;
    match project() {
        Ok(()) => transaction
            .execute_batch("RELEASE project_claim")
            .map_err(internal),
        Err(error) => {
            transaction
                .execute_batch("ROLLBACK TO project_claim; RELEASE project_claim")
                .map_err(internal)?;
            transaction
                .execute(
                    "INSERT INTO projection_health(aggregate, status, error_code, error_message, updated_at_unix_ms)
                     VALUES (?1, 'stale', ?2, ?3, ?4)
                     ON CONFLICT(aggregate) DO UPDATE SET status='stale', error_code=excluded.error_code,
                        error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![
                        format!("{aggregate}:{}", claim.id),
                        error.code,
                        format!("{} on {}: {}", claim.kind, claim.subject, error.message),
                        now_ms().to_string()
                    ],
                )
                .map_err(internal)?;
            Ok(())
        }
    }
}

/// A full replay decides every claim again, so it first forgets the claims it quarantined before.
fn clear_quarantined_claims_tx(
    transaction: &Transaction<'_>,
    aggregate: &str,
) -> Result<(), St3Error> {
    transaction
        .execute(
            "DELETE FROM projection_health WHERE aggregate LIKE ?1",
            [format!("{aggregate}:%")],
        )
        .map_err(internal)?;
    Ok(())
}

fn select_replicated_desired(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
    desired: &DesiredSubject,
) -> Result<(), St3Error> {
    if claim.body.get("owned_set").is_some() || owned_sets::owner(transaction, &claim.subject, None)?.is_some() {
        return Ok(());
    }
    let current = current_desired_row_tx(transaction, &claim.subject).map_err(internal)?;
    let revision = desired_revision(desired);
    let select = if let Some(row) = &current {
        if claim.id == row.claim_id
            || claim_descends_from(transaction, &claim.id, &row.claim_id).map_err(internal)?
        {
            true
        } else if claim_descends_from(transaction, &row.claim_id, &claim.id).map_err(internal)? {
            false
        } else {
            (revision.as_str(), claim.id.as_str()) > (row.revision.as_str(), row.claim_id.as_str())
        }
    } else {
        true
    };
    if !select {
        return Ok(());
    }
    message_subscriptions::validate_replicated(transaction, claim, desired)?;
    transaction
        .execute(
            "INSERT INTO desired(subject, kind, revision, claim_id, body, member, owner_run, owner_generation, owner_step) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(subject) DO UPDATE SET kind=excluded.kind, revision=excluded.revision, claim_id=excluded.claim_id, body=excluded.body, member=excluded.member, owner_run=excluded.owner_run, owner_generation=excluded.owner_generation, owner_step=excluded.owner_step",
            params![
                claim.subject,
                desired.kind,
                revision,
                claim.id,
                canonical_json_text(&desired.desired).map_err(internal)?,
                desired
                    .member
                    .as_ref()
                    .map(canonical_serialized_json_text)
                    .transpose()
                    .map_err(internal)?,
                desired.owner_run,
                desired.owner_generation,
                desired.owner_step,
            ],
        )
        .map_err(internal)?;
    Ok(())
}

fn select_desired_repair_tx(
    transaction: &Transaction<'_>,
    repaired_claim_id: &str,
    replacement_claim_id: &str,
) -> Result<()> {
    let selected = transaction
        .query_row(
            "SELECT subject FROM desired WHERE claim_id=?1",
            [repaired_claim_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(subject) = selected else {
        return Ok(());
    };
    let replacement = transaction
        .query_row(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors,
                    accepted_at_unix_ms
             FROM claims WHERE id=?1",
            [replacement_claim_id],
            claim_from_row,
        )
        .optional()?;
    let Some(replacement) = replacement else {
        return Ok(());
    };
    if replacement.subject != subject || replacement.kind != "intent.desired" {
        return Ok(());
    }
    let desired = serde_json::from_value::<DesiredSubject>(replacement.body.clone())?;
    transaction.execute(
        "INSERT INTO desired(subject, kind, revision, claim_id, body, member, owner_run, owner_generation, owner_step)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(subject) DO UPDATE SET kind=excluded.kind, revision=excluded.revision,
            claim_id=excluded.claim_id, body=excluded.body, member=excluded.member,
            owner_run=excluded.owner_run, owner_generation=excluded.owner_generation,
            owner_step=excluded.owner_step",
        params![
            replacement.subject,
            desired.kind,
            desired_revision(&desired),
            replacement.id,
            canonical_json_text(&desired.desired)?,
            desired
                .member
                .as_ref()
                .map(canonical_serialized_json_text)
                .transpose()?,
            desired.owner_run,
            desired.owner_generation,
            desired.owner_step,
        ],
    )?;
    Ok(())
}

fn project_replicated_mission_runs(transaction: &Transaction<'_>) -> Result<(), St3Error> {
    clear_quarantined_claims_tx(transaction, "projection:runs")?;
    for pass in 0..3 {
        let filter = match pass {
            0 => "claims.kind='mission-run.created'",
            1 => "claims.kind IN ('mission-run.state','run-generation.created','run-generation.state','run-generation.superseded',
                  'revision-proposal.created','revision-proposal.approved','revision-proposal.cancelled','revision-proposal.applied',
                  'step-run.carried','step-run.state','step-run.retried',
                  'work.claimed','work.renewed','work.progress','work.submitted','work.failed','work.released','work.extended','work.person-asked','work.person-done','work.person-cancelled')",
            _ => "claims.kind='step-run.carried'",
        };
        let mut statement = transaction
            .prepare(&canonical_sql(&format!(
                "SELECT claims.id
                 FROM claims JOIN batches ON batches.id=claims.batch_id WHERE {filter}
                 ORDER BY CANONICAL_ASC(claims)"
            )))
            .map_err(internal)?;
        let claim_ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        drop(statement);
        for id in claim_ids {
            let claim = claim_by_id_tx(transaction, &id)
                .map_err(internal)?
                .context("a mission run claim disappeared during replay")
                .map_err(internal)?;
            project_claim_isolated_tx(transaction, "projection:runs", &claim, || match pass {
                0 => project_mission_run_created(transaction, &claim),
                1 => project_mission_run_update(transaction, &claim),
                _ => reconcile_carried_step_tx(transaction, &claim),
            })?;
        }
    }
    Ok(())
}

fn reconcile_carried_step_tx(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<(), St3Error> {
    // A replay may be repairing an existing projection. A later step claim
    // owns its state; otherwise the carried claim is the last direct state
    // observation for this successor step.
    let later_step_claim = transaction
        .query_row(
            "SELECT 1 FROM claims WHERE subject=?1 AND kind<>'step-run.carried' LIMIT 1",
            [&claim.subject],
            |_| Ok(()),
        )
        .optional()
        .map_err(internal)?
        .is_some();
    if later_step_claim || step_owner_is_terminal_tx(transaction, &claim.subject)? {
        return Ok(());
    }
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let Some(status) = fields.get("status").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(attempt) = fields.get("attempt").and_then(Value::as_u64) else {
        return Ok(());
    };
    let worker_reported = fields
        .get("worker_reported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A carried claim keeps the worker lease it records. A carried `ready` step names the seat
    // whose claim the revision dropped, which is not a lease.
    let lease = |name: &str| {
        fields
            .get(name)
            .filter(|_| matches!(status, "claimed" | "working" | "verifying" | "blocked"))
    };
    transaction
        .execute(
            "UPDATE step_runs SET status=?2, attempt=?3, worker_reported=?4,
                    blocked_reason=CASE WHEN status=?2 THEN blocked_reason ELSE NULL END,
                    not_before_unix_ms=CASE WHEN status=?2 THEN not_before_unix_ms ELSE NULL END,
                    lease_owner=?6, lease_incarnation=?7, lease_expires_at_unix_ms=?8,
                    updated_at_unix_ms=?5
                 WHERE subject=?1 AND status<>?2",
            params![
                claim.subject,
                status,
                attempt,
                worker_reported,
                claim.accepted_at_unix_ms.to_string(),
                lease("claimant").and_then(Value::as_str),
                lease("claim_incarnation").and_then(Value::as_str),
                lease("claim_expires_at_unix_ms")
                    .and_then(Value::as_u64)
                    .map(|expiry| expiry.to_string()),
            ],
        )
        .map_err(internal)?;
    Ok(())
}

fn project_mission_run_created(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<(), St3Error> {
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let run_id = claim.subject.strip_prefix("mission-run/").ok_or_else(|| {
        St3Error::new(
            "invalid-mission-run-claim",
            format!("claim `{}` has an invalid mission run subject", claim.id),
        )
    })?;
    if fields.get("ad_hoc_title").is_some() {
        return adhoc_work::project_start(transaction, claim);
    }
    // Creation claims fold in canonical order. A second admission of the same occurrence must
    // not append steps from another child revision to the winning initial generation.
    if transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM mission_runs WHERE id=?1)",
            [run_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(internal)?
    {
        return Ok(());
    }
    let mission_subject = fields
        .get("mission")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-mission-run-claim",
                format!("claim `{}` has no mission", claim.id),
            )
        })?;
    let mission_id = mission_subject
        .strip_prefix("mission/")
        .unwrap_or(mission_subject);
    let revision = fields
        .get("revision")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-mission-run-claim",
                format!("claim `{}` has no mission revision", claim.id),
            )
        })?;
    let mission_body = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission_id, revision],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?;
    let Some(mission_body) = mission_body else {
        return Ok(());
    };
    let mission = serde_json::from_str::<MissionSpec>(&mission_body).map_err(internal)?;
    let workspace = fields
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let requester = fields
        .get("requester")
        .and_then(Value::as_str)
        .unwrap_or("person/requester");
    let inputs = fields
        .get("inputs")
        .cloned()
        .map(serde_json::from_value::<BTreeMap<String, MissionRunInput>>)
        .transpose()
        .map_err(internal)?
        .unwrap_or_default();
    let mode = fields.get("mode").and_then(Value::as_str).unwrap_or("run");
    let root_revision = fields
        .get("root_revision")
        .and_then(Value::as_str)
        .unwrap_or(revision);
    let root_mission_run = fields
        .get("root_mission_run")
        .and_then(Value::as_str)
        .unwrap_or(&claim.subject);
    let root_run_id = root_mission_run
        .strip_prefix("mission-run/")
        .unwrap_or(root_mission_run);
    let parent_step_run = fields.get("parent_step_run").and_then(Value::as_str);
    let default_selector = fields
        .get("default_selector")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value::<WorkSelector>)
        .transpose()
        .map_err(internal)?;
    let generation_subject = fields
        .get("current_generation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-mission-run-claim",
                format!("claim `{}` has no initial generation", claim.id),
            )
        })?;
    let generation_id = generation_id_from_subject(generation_subject);
    let after = fields.get("after").and_then(Value::as_str);
    let mission = crate::mission::run_mission(mission, after)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO mission_runs(id, mission_id, initial_revision, current_generation_id, root_revision, root_run_id, parent_step_run, workspace, requester, inputs, mode, status, phase, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running', 'normal', ?12, ?12)",
            params![run_id, mission_id, revision, generation_id, root_revision, root_run_id, parent_step_run, workspace, requester, serde_json::to_string(&inputs).map_err(internal)?, mode, mission_run_creation_time(fields, claim.accepted_at_unix_ms).to_string()],
        )
        .map_err(internal)?;
    if let Some(after) = after {
        transaction
            .execute(
                "INSERT OR IGNORE INTO mission_run_after(run_id, after_run) VALUES (?1, ?2)",
                params![run_id, after],
            )
            .map_err(internal)?;
    }
    if let Some(timeout_ms) = fields.get("timeout_ms").and_then(Value::as_u64) {
        // Older creation claims armed this while waiting. Admission, rather than that
        // obsolete creation deadline, determines the clock on a rebuilt or joining node.
        let deadline_at_unix_ms = if after.is_none() {
            fields.get("deadline_at_unix_ms").and_then(Value::as_u64)
        } else {
            None
        };
        transaction
            .execute(
                "INSERT OR IGNORE INTO mission_run_deadlines(run_id, timeout_ms, deadline_at_unix_ms)
                 VALUES (?1, ?2, ?3)",
                params![run_id, timeout_ms, deadline_at_unix_ms.map(|deadline| deadline.to_string())],
            )
            .map_err(internal)?;
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, NULL, 'running', ?4, 'initial mission run', ?5, ?5)",
            params![generation_id, run_id, revision, requester, mission_run_creation_time(fields, claim.accepted_at_unix_ms).to_string()],
        )
        .map_err(internal)?;
    let view = mission_run_view_tx(transaction, run_id).map_err(internal)?;
    let variables = mission_run_variables(&view, revision);
    let mut steps = Vec::new();
    flatten_steps(&mission, default_selector, &[], &mut steps);
    for (step, selector, constraints) in steps {
        let (assignee, available_to, agentless) = interpolate_selector(&selector, &variables)?;
        let subject = format!("step-run/{generation_id}/{}", step.path);
        let mut step_variables = variables.clone();
        step_variables.insert("ST_STEP".into(), step.path.clone());
        step_variables.insert("ST_STEP_RUN".into(), subject.clone());
        step_variables.insert("ST_ATTEMPT".into(), "1".into());
        step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
        step_variables.insert(
            "ST_PARENT_STEP_RUN".into(),
            crate::mission::parent_step_path(&mission, &step.path)
                .map(|path| format!("step-run/{generation_id}/{path}"))
                .or_else(|| parent_step_run.map(str::to_owned))
                .unwrap_or_default(),
        );
        let title = step
            .title
            .as_deref()
            .map(|value| crate::mission::interpolate(value, &step_variables))
            .transpose()?;
        let goals = interpolate_goals(&step.goals, &step_variables)?;
        let constraints = interpolate_goals(&constraints, &step_variables)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, created_at_unix_ms, updated_at_unix_ms, constraints)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 1, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
                params![subject, run_id, generation_id, step.path, step.definition_hash, assignee, serde_json::to_string(&available_to).map_err(internal)?, agentless, title, goals, mission_run_creation_time(fields, claim.accepted_at_unix_ms).to_string(), constraints],
            )
            .map_err(internal)?;
    }
    let root_terminal = transaction
        .query_row(
            "SELECT status, phase FROM mission_runs WHERE id=?1",
            [root_run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(internal)?
        .is_some_and(|(status, phase)| is_terminal_run_state(&status) || phase == "terminal");
    if root_terminal {
        terminalize_projected_run_tree_tx(
            transaction,
            run_id,
            "the root mission run is terminal",
            claim.accepted_at_unix_ms,
        )?;
    }
    Ok(())
}

fn project_mission_run_update(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<(), St3Error> {
    // Nudges are retained events, never work-state transitions. This also matches
    // full replay for historical bodies that bypassed today's field admission.
    if claim.kind == WORK_NUDGED_KIND {
        return Ok(());
    }
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    if person_work::project(transaction, claim)? {
        return Ok(());
    }
    if claim.kind.starts_with("revision-proposal.") {
        project_revision_proposal(transaction, claim, fields)?;
        return Ok(());
    }
    if claim.kind == "run-generation.created" {
        project_run_generation_created(transaction, claim)?;
        return Ok(());
    }
    if claim.kind == "run-generation.superseded" || claim.kind == "run-generation.state" {
        let Some(generation_id) = claim.subject.strip_prefix("run-generation/") else {
            return Ok(());
        };
        let status = fields
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("superseded");
        let current_status = transaction
            .query_row(
                "SELECT status FROM run_generations WHERE id=?1",
                [generation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?;
        if current_status
            .as_deref()
            .is_some_and(is_terminal_generation_state)
            && !is_terminal_generation_state(status)
        {
            return Ok(());
        }
        transaction
            .execute(
                "UPDATE run_generations SET status=?2, updated_at_unix_ms=?3 WHERE id=?1",
                params![generation_id, status, claim.accepted_at_unix_ms.to_string()],
            )
            .map_err(internal)?;
        if is_terminal_generation_state(status) {
            terminalize_projected_generation_steps_tx(
                transaction,
                generation_id,
                fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the owning run generation is terminal"),
                claim.accepted_at_unix_ms,
            )?;
        }
        return Ok(());
    }
    if claim.kind == "mission-run.state" {
        let Some(run_id) = claim.subject.strip_prefix("mission-run/") else {
            return Ok(());
        };
        let Some(status) = fields.get("status").and_then(Value::as_str) else {
            return Ok(());
        };
        let phase = fields
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("normal");
        let current = transaction
            .query_row(
                "SELECT status, phase FROM mission_runs WHERE id=?1",
                [run_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(internal)?;
        if current.is_some_and(|(current_status, current_phase)| {
            (is_terminal_run_state(&current_status) || current_phase == "terminal")
                && !(is_terminal_run_state(status) || phase == "terminal")
        }) {
            return Ok(());
        }
        transaction
            .execute(
                "UPDATE mission_runs SET status=?2, phase=?3, updated_at_unix_ms=?4 WHERE id=?1",
                params![run_id, status, phase, claim.accepted_at_unix_ms.to_string()],
            )
            .map_err(internal)?;
        if is_terminal_run_state(status) || phase == "terminal" {
            terminalize_projected_run_tree_tx(
                transaction,
                run_id,
                fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the owning mission run is terminal"),
                claim.accepted_at_unix_ms,
            )?;
        }
        return Ok(());
    }
    if !claim.subject.starts_with("step-run/") {
        return Ok(());
    }
    let owner_is_terminal = step_owner_is_terminal_tx(transaction, &claim.subject)?;
    if claim.kind == "step-run.retried" {
        if owner_is_terminal {
            return Ok(());
        }
        let Some(attempt) = fields.get("attempt").and_then(Value::as_u64) else {
            return Ok(());
        };
        let not_before = fields
            .get("not_before_unix_ms")
            .and_then(Value::as_u64)
            .map(|value| value.to_string());
        let goals = fields
            .get("goals")
            .map(serde_json::to_string)
            .transpose()
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE step_runs SET status='pending', attempt=?2, worker_reported=0, lease_owner=NULL,
                        lease_incarnation=NULL, lease_expires_at_unix_ms=NULL, blocked_reason=?3,
                        not_before_unix_ms=?4, activated_at_unix_ms=NULL, updated_at_unix_ms=?5,
                        goals=COALESCE(?6, goals) WHERE subject=?1",
                params![claim.subject, attempt, fields.get("reason").and_then(Value::as_str), not_before, claim.accepted_at_unix_ms.to_string(), goals],
            )
            .map_err(internal)?;
        return Ok(());
    }
    let Some(status) = fields.get("status").and_then(Value::as_str) else {
        return Ok(());
    };
    if owner_is_terminal && !matches!(status, "completed" | "failed" | "cancelled") {
        return Ok(());
    }
    if claim.kind == "step-run.state" {
        // A retained completed projection can outlive its admission claim. A duplicate
        // completion is not a new admission and must not manufacture a fresh budget.
        let previously_completed = if status == "completed"
            && claim.subject.rsplit('/').next() == Some(crate::mission::AFTER_RUN_STEP)
        {
            transaction.query_row("SELECT status='completed' FROM step_runs WHERE subject=?1",
                [&claim.subject], |row| row.get::<_, bool>(0))
                .optional().map_err(internal)?.unwrap_or(false)
        } else { false };
        transaction
            .execute(
                "UPDATE step_runs SET status=?2, blocked_reason=?3,
                    lease_owner=CASE WHEN ?2 IN ('ready','orphaned','completed','failed','cancelled') THEN NULL ELSE lease_owner END,
                    lease_incarnation=CASE WHEN ?2 IN ('ready','orphaned','completed','failed','cancelled') THEN NULL ELSE lease_incarnation END,
                    lease_expires_at_unix_ms=CASE WHEN ?2 IN ('ready','orphaned','completed','failed','cancelled') THEN NULL ELSE lease_expires_at_unix_ms END,
                        not_before_unix_ms=CASE WHEN ?2='ready' THEN NULL ELSE not_before_unix_ms END,
                        activated_at_unix_ms=CASE WHEN ?2='ready' THEN ?4 ELSE activated_at_unix_ms END,
                        readiness_epoch=COALESCE(?5, readiness_epoch), updated_at_unix_ms=?4 WHERE subject=?1",
                params![claim.subject, status, fields.get("reason").and_then(Value::as_str), claim.accepted_at_unix_ms.to_string(), fields.get("readiness_epoch").and_then(Value::as_u64)],
            )
            .map_err(internal)?;
        if status == "completed" {
            arm_waiting_mission_deadline_tx(transaction, &claim.subject, claim.accepted_at_unix_ms, previously_completed)?;
        }
        return Ok(());
    }
    if claim.kind.starts_with("work.") {
        if owner_is_terminal {
            return Ok(());
        }
        let claim_attempt = fields.get("attempt").and_then(Value::as_u64).unwrap_or(1);
        let current_attempt = transaction
            .query_row(
                "SELECT attempt FROM step_runs WHERE subject=?1",
                [&claim.subject],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(internal)?;
        if current_attempt != Some(claim_attempt) {
            return Ok(());
        }
        if claim.kind == "work.released"
            && let Some(to) = fields.get("handoff_to").and_then(Value::as_str)
        {
            transaction
                .execute(
                    "UPDATE step_runs SET assignee=?2, available_to='[]' WHERE subject=?1",
                    params![claim.subject, to],
                )
                .map_err(internal)?;
        }
        let lease_expiry = fields
            .get("claim_expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(|value| value.to_string());
        transaction
            .execute(
                "UPDATE step_runs SET status=?2, worker_reported=?3, lease_owner=?4,
                        lease_incarnation=?5, lease_expires_at_unix_ms=?6, blocked_reason=?7,
                        readiness_epoch=COALESCE(?8, readiness_epoch), updated_at_unix_ms=?9 WHERE subject=?1",
                params![
                    claim.subject,
                    status,
                    fields
                        .get("worker_reported")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    fields.get("claimant").and_then(Value::as_str),
                    fields.get("claim_incarnation").and_then(Value::as_str),
                    lease_expiry,
                    fields.get("reason").and_then(Value::as_str),
                    fields.get("readiness_epoch").and_then(Value::as_u64),
                    claim.accepted_at_unix_ms.to_string(),
                ],
            )
            .map_err(internal)?;
    }
    Ok(())
}

fn project_revision_proposal(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
    fields: &Value,
) -> Result<(), St3Error> {
    let Some(proposal_id) = claim.subject.strip_prefix("revision-proposal/") else {
        return Ok(());
    };
    let now = claim.accepted_at_unix_ms.to_string();
    if claim.kind == "revision-proposal.created" {
        let Some(run) = fields.get("run").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(source) = fields.get("source_generation").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(revision) = fields.get("candidate_revision").and_then(Value::as_str) else {
            return Ok(());
        };
        let actor = claim.actor.as_deref().unwrap_or("requester");
        let reason = fields
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = fields
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending-approval");
        let cutover = fields
            .get("cutover")
            .and_then(Value::as_str)
            .unwrap_or("restart-active");
        let compatible = fields
            .get("compatible_steps")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let reviewers = fields
            .get("reviewers")
            .cloned()
            .unwrap_or_else(|| json!([]));
        transaction
            .execute(
                "INSERT OR IGNORE INTO revision_proposals(
                   id, run_id, source_generation_id, candidate_revision, actor, reason, status,
                   cutover, compatible_steps, reviewers, approvals, preview_hash,
                   created_at_unix_ms, updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '[]', ?11, ?12, ?12)",
                params![
                    proposal_id,
                    run.strip_prefix("mission-run/").unwrap_or(run),
                    generation_id_from_subject(source),
                    revision,
                    actor,
                    reason,
                    status,
                    cutover,
                    serde_json::to_string(&compatible).map_err(internal)?,
                    serde_json::to_string(&reviewers).map_err(internal)?,
                    fields.get("preview_hash").and_then(Value::as_str),
                    now,
                ],
            )
            .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET phase='revision-draining', updated_at_unix_ms=?2
                     WHERE id=?1 AND current_generation_id=?3 AND phase='normal'
                       AND status IN ('running','standing','blocked')
                       AND EXISTS (SELECT 1 FROM revision_proposals
                                   WHERE id=?4 AND status='draining')",
                    params![
                        run.strip_prefix("mission-run/").unwrap_or(run),
                        now,
                        generation_id_from_subject(source),
                        proposal_id
                    ],
                )
                .map_err(internal)?;
        }
        return Ok(());
    }
    if claim.kind == "revision-proposal.approved" {
        let Some(reviewer) = fields.get("reviewer").and_then(Value::as_str) else {
            return Ok(());
        };
        let mut approvals = transaction
            .query_row(
                "SELECT approvals FROM revision_proposals WHERE id=?1",
                [proposal_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?
            .map(|value| serde_json::from_str::<BTreeSet<String>>(&value))
            .transpose()
            .map_err(internal)?
            .unwrap_or_default();
        approvals.insert(reviewer.to_owned());
        let terminal = transaction
            .query_row(
                "SELECT status IN ('applied','cancelled') FROM revision_proposals WHERE id=?1",
                [proposal_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .map_err(internal)?
            .unwrap_or(false);
        let draining = !terminal
            && fields
                .get("all_approved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && transaction
                .query_row(
                    "SELECT cutover='when-idle' FROM revision_proposals WHERE id=?1",
                    [proposal_id],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or(false);
        transaction
            .execute(
                "UPDATE revision_proposals SET approvals=?2,
                   status=CASE WHEN ?3 THEN 'draining' ELSE status END,
                   updated_at_unix_ms=?4 WHERE id=?1",
                params![
                    proposal_id,
                    serde_json::to_string(&approvals).map_err(internal)?,
                    draining,
                    now
                ],
            )
            .map_err(internal)?;
        if draining {
            transaction
                .execute(
                    "UPDATE mission_runs SET phase='revision-draining', updated_at_unix_ms=?2
                     WHERE id=(SELECT run_id FROM revision_proposals WHERE id=?1)
                       AND current_generation_id=(SELECT source_generation_id FROM revision_proposals WHERE id=?1)
                       AND phase='normal' AND status IN ('running','standing','blocked')",
                    params![proposal_id, now],
                )
                .map_err(internal)?;
        }
        return Ok(());
    }
    let (status, successor) = match claim.kind.as_str() {
        "revision-proposal.cancelled" => ("cancelled", None),
        "revision-proposal.applied" => (
            "applied",
            fields
                .get("successor_generation")
                .and_then(Value::as_str)
                .map(generation_id_from_subject),
        ),
        _ => return Ok(()),
    };
    transaction
        .execute(
            "UPDATE revision_proposals SET status=?2, successor_generation_id=?3,
               updated_at_unix_ms=?4 WHERE id=?1",
            params![proposal_id, status, successor, now],
        )
        .map_err(internal)?;
    if status == "cancelled" {
        transaction
            .execute(
                "UPDATE mission_runs SET phase='normal', updated_at_unix_ms=?2
                 WHERE id=(SELECT run_id FROM revision_proposals WHERE id=?1)
                   AND current_generation_id=(SELECT source_generation_id FROM revision_proposals WHERE id=?1)
                   AND phase='revision-draining'",
                params![proposal_id, now],
            )
            .map_err(internal)?;
    }
    Ok(())
}

fn project_run_generation_created(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<(), St3Error> {
    let Some(generation_id) = claim.subject.strip_prefix("run-generation/") else {
        return Ok(());
    };
    if transaction
        .query_row(
            "SELECT 1 FROM run_generations WHERE id=?1",
            [generation_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(internal)?
        .is_some()
    {
        return Ok(());
    }
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let Some(run_subject) = fields.get("run").and_then(Value::as_str) else {
        return Ok(());
    };
    let run_id = run_subject
        .strip_prefix("mission-run/")
        .unwrap_or(run_subject);
    let Some(revision) = fields.get("revision").and_then(Value::as_str) else {
        return Ok(());
    };
    let predecessor = fields
        .get("predecessor")
        .and_then(Value::as_str)
        .map(generation_id_from_subject);
    let reason = fields
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("replicated generation");
    // Generation replay needs effective predecessor step state, not presentation-only
    // wake history. Avoid scanning every sent message for every carried step.
    let current = match mission_run_view_for_projection_tx(transaction, run_id) {
        Ok(current) => current,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
        Err(error) => return Err(internal(error)),
    };
    let mission_id = current
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&current.mission);
    let body = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission_id, revision],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?;
    let Some(body) = body else {
        return Ok(());
    };
    let mission = crate::mission::run_mission(
        serde_json::from_str::<MissionSpec>(&body).map_err(internal)?,
        current.after.as_deref(),
    )?;
    let mut variables = mission_run_variables(&current, revision);
    variables.insert("ST_RUN_GENERATION".into(), generation_id.to_owned());
    let compatible = fields
        .get("compatible_steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    transaction
        .execute(
            "INSERT INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?7)",
            params![generation_id, run_id, revision, predecessor, claim.actor, reason, claim.accepted_at_unix_ms.to_string()],
        )
        .map_err(internal)?;
    let mut new_steps = Vec::new();
    flatten_steps(&mission, None, &[], &mut new_steps);
    for (step, selector, constraints) in new_steps {
        let (assignee, available_to, agentless) = interpolate_selector(&selector, &variables)?;
        let subject = format!("step-run/{generation_id}/{}", step.path);
        let carried = compatible
            .contains(step.path.as_str())
            .then(|| current.steps.iter().find(|old| old.step == step.path))
            .flatten();
        // The predecessor is already terminal when its successor is replayed.
        // Its projected step may have been cancelled after the carried claim was
        // written, so use that claim as the authority for the successor state. Only this
        // generation carries onto its own step subjects, but a cutover may write each carried
        // claim in a batch of its own, so find it by subject rather than by this batch.
        let carried_body = transaction
            .query_row(
                &format!(
                    "SELECT claims.body FROM claims JOIN batches ON batches.id=claims.batch_id
                     WHERE claims.subject=?1 AND claims.kind='step-run.carried'
                     ORDER BY {CANONICAL_ORDER} LIMIT 1"
                ),
                [&subject],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?
            .map(|body| serde_json::from_str::<Value>(&body).map_err(internal))
            .transpose()?;
        let carried_fields = carried_body
            .as_ref()
            .map(|body| body.get("fields").unwrap_or(body));
        let status = carried_fields
            .and_then(|fields| fields.get("status"))
            .and_then(Value::as_str)
            .or_else(|| {
                carried.map(|old| match old.status.as_str() {
                    "claimed" | "working" | "verifying" => "ready",
                    status => status,
                })
            })
            .unwrap_or("pending");
        let attempt = carried_fields
            .and_then(|fields| fields.get("attempt"))
            .and_then(Value::as_u64)
            .and_then(|attempt| u32::try_from(attempt).ok())
            .or_else(|| carried.map(|old| old.attempt))
            .unwrap_or(1);
        let worker_reported = carried_fields
            .and_then(|fields| fields.get("worker_reported"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| carried.is_some_and(|old| old.worker_reported && status != "ready"));
        let matching_predecessor = carried.filter(|old| old.status == status);
        // A carried `ready` step names the seat whose claim the revision dropped, not a lease.
        let leased = matches!(status, "claimed" | "working" | "verifying" | "blocked");
        let carried_lease = |name: &str| {
            carried_fields
                .filter(|_| leased)
                .and_then(|fields| fields.get(name))
                .and_then(|value| match value {
                    Value::String(value) => Some(value.clone()),
                    Value::Number(value) => Some(value.to_string()),
                    _ => None,
                })
        };
        let mut step_variables = variables.clone();
        step_variables.insert("ST_STEP".into(), step.path.clone());
        step_variables.insert("ST_STEP_RUN".into(), subject.clone());
        step_variables.insert("ST_ATTEMPT".into(), attempt.to_string());
        step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
        step_variables.insert(
            "ST_PARENT_STEP_RUN".into(),
            crate::mission::parent_step_path(&mission, &step.path)
                .map(|path| format!("step-run/{generation_id}/{path}"))
                .or_else(|| current.parent_step_run.clone())
                .unwrap_or_default(),
        );
        let title = step
            .title
            .as_deref()
            .map(|value| crate::mission::interpolate(value, &step_variables))
            .transpose()?;
        let goals = interpolate_goals(&step.goals, &step_variables)?;
        let constraints = interpolate_goals(&constraints, &step_variables)?;
        transaction
            .execute(
                "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported, blocked_reason, not_before_unix_ms, readiness_epoch, created_at_unix_ms, updated_at_unix_ms, constraints, lease_owner, lease_incarnation, lease_expires_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17, ?18, ?19, ?20, ?21)",
                params![subject, run_id, generation_id, step.path, step.definition_hash, status, attempt, assignee, serde_json::to_string(&available_to).map_err(internal)?, agentless, title, goals, worker_reported, matching_predecessor.and_then(|old| old.blocked_reason.as_deref()), matching_predecessor.and_then(|old| old.not_before_unix_ms).map(|value| value.to_string()), carried.map(|old| old.readiness_epoch).unwrap_or(0), claim.accepted_at_unix_ms.to_string(), constraints, carried_lease("claimant"), carried_lease("claim_incarnation"), carried_lease("claim_expires_at_unix_ms")],
            )
            .map_err(internal)?;
    }
    transaction
        .execute(
            "UPDATE mission_runs SET current_generation_id=?2, status='running', phase='normal', updated_at_unix_ms=?3 WHERE id=?1",
            params![run_id, generation_id, claim.accepted_at_unix_ms.to_string()],
        )
        .map_err(internal)?;
    Ok(())
}

fn claim_descends_from(
    transaction: &Transaction<'_>,
    descendant: &str,
    ancestor: &str,
) -> Result<bool> {
    if descendant == ancestor {
        return Ok(true);
    }
    let mut pending = vec![descendant.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(claim_id) = pending.pop() {
        if !seen.insert(claim_id.clone()) {
            continue;
        }
        // A checkpoint may have dropped a claim on the path; its tombstone keeps the links.
        let predecessors = transaction
            .query_row(
                "SELECT predecessors FROM claims WHERE id=?1
                 UNION ALL SELECT predecessors FROM checkpoint_claims WHERE id=?1
                 LIMIT 1",
                [&claim_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(predecessors) = predecessors else {
            continue;
        };
        for predecessor in serde_json::from_str::<Vec<String>>(&predecessors)? {
            if predecessor == ancestor {
                return Ok(true);
            }
            pending.push(predecessor);
        }
    }
    Ok(false)
}

fn select_replicated_mission(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
    created_index: u64,
) -> Result<(), St3Error> {
    let mission = serde_json::from_value::<MissionSpec>(claim.body.clone()).map_err(|error| {
        St3Error::new(
            "invalid-mission-claim",
            format!(
                "replicated mission claim `{}` is invalid: {error}",
                claim.id
            ),
        )
    })?;
    if claim.subject != mission.subject || claim.subject != format!("mission/{}", mission.id) {
        return Err(St3Error::new(
            "invalid-mission-claim",
            format!(
                "replicated mission claim `{}` has mismatched identity",
                claim.id
            ),
        ));
    }
    let mut unhashed = mission.clone();
    let revision = std::mem::take(&mut unhashed.revision);
    let expected = hex::encode(Sha256::digest(
        serde_json::to_vec(&unhashed).map_err(internal)?,
    ));
    if revision != expected {
        return Err(St3Error::new(
            "invalid-mission-claim",
            format!(
                "replicated mission claim `{}` has an invalid revision",
                claim.id
            ),
        ));
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO mission_revisions(mission_id, revision, state, body, claim_id, created_index) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![mission.id, mission.revision, mission_state_name(&mission.state), serde_json::to_string(&mission).map_err(internal)?, claim.id, created_index],
        )
        .map_err(internal)?;
    let previous: String = transaction
        .query_row(
            "SELECT claim_id FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission.id, mission.revision],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if canonical::claim_key(transaction, &claim.id).map_err(internal)?
        < canonical::claim_key(transaction, &previous).map_err(internal)?
    {
        transaction
            .execute(
                "UPDATE mission_revisions SET claim_id=?3 WHERE mission_id=?1 AND revision=?2",
                params![mission.id, mission.revision, claim.id],
            )
            .map_err(internal)?;
    }
    if claim.body.get("owned_set").is_some() || owned_sets::owner(transaction, &claim.subject, None)?.is_some() {
        return Ok(());
    }
    let current: Option<(String, String)> = transaction
        .query_row(
            "SELECT revision, claim_id FROM mission_definitions WHERE mission_id=?1",
            [&mission.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let select = if let Some((current_revision, current_claim)) = current {
        // A runtime-created run's definition (person_work::project_minimal_run) yields to a
        // publication of the same mission id, which that projection never replaces, so the
        // publication wins whichever arrives first.
        let runtime: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1
                     AND kind IN ('work.person-asked','mission-run.created'))",
                [&current_claim],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if runtime
            || claim_descends_from(transaction, &claim.id, &current_claim).map_err(internal)?
        {
            true
        } else if claim_descends_from(transaction, &current_claim, &claim.id).map_err(internal)? {
            false
        } else {
            (mission.revision.as_str(), claim.id.as_str())
                > (current_revision.as_str(), current_claim.as_str())
        }
    } else {
        true
    };
    if select {
        transaction
            .execute(
                "INSERT INTO mission_definitions(mission_id, revision, state, claim_id) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(mission_id) DO UPDATE SET revision=excluded.revision, state=excluded.state, claim_id=excluded.claim_id",
                params![mission.id, mission.revision, mission_state_name(&mission.state), claim.id],
            )
            .map_err(internal)?;
    }
    Ok(())
}

fn mission_state_name(state: &MissionState) -> &'static str {
    match state {
        MissionState::Draft => "draft",
        MissionState::Ready => "ready",
        MissionState::Retired => "retired",
    }
}

fn normalize_step_run(value: &str) -> String {
    if value.starts_with("step-run/") {
        value.to_owned()
    } else {
        format!("step-run/{value}")
    }
}

fn normalize_seat(value: &str) -> String {
    if value.starts_with("agent/") {
        value.to_owned()
    } else {
        format!("agent/{value}")
    }
}

fn normalize_mission_run(value: &str) -> String {
    if value.starts_with("mission-run/") {
        value.to_owned()
    } else {
        format!("mission-run/{value}")
    }
}

struct RosterStepRow {
    subject: String,
    run: String,
    step: String,
    status: String,
    assignee: Option<String>,
    claimant: Option<String>,
    available_to: Vec<String>,
    created_at_unix_ms: u128,
    carried_claimant: Option<String>,
}

impl RosterStepRow {
    fn seat_step(&self) -> SeatStep<'_> {
        SeatStep {
            subject: &self.subject,
            run: &self.run,
            step: &self.step,
            status: &self.status,
            assignee: self.assignee.as_deref(),
            claimant: self.claimant.as_deref(),
            available_to: &self.available_to,
            created_at_unix_ms: self.created_at_unix_ms,
            carried_claimant: self.carried_claimant.as_deref(),
        }
    }
}

/// The held and ready agent steps of seat `?1`, or of every seat when null. The unfinished-step
/// predicate, word for word that of `step_runs_open_index`, reads the fleet's open steps rather
/// than every step the store has run.
const SEAT_STEP_ROWS: &str = "SELECT subject, run_id, step_path, status, assignee, lease_owner,
            available_to, created_at_unix_ms, lease_expires_at_unix_ms
     FROM step_runs
     WHERE agentless=0
       AND status IN ('ready', 'claimed', 'working', 'verifying')
       AND status NOT IN ('completed','failed','cancelled')
       AND (?1 IS NULL OR assignee=?1 OR lease_owner=?1)
       AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=step_runs.run_id)
       AND NOT ((status='ready' OR (status IN ('claimed','working','verifying')
                  AND lease_expires_at_unix_ms IS NOT NULL
                  AND CAST(lease_expires_at_unix_ms AS INTEGER)<=?2))
                AND (SELECT phase FROM mission_runs WHERE id=step_runs.run_id)='revision-draining')
     ORDER BY length(created_at_unix_ms), created_at_unix_ms, subject";

struct RecordedSeatMove {
    movement: QueueMove,
    view: SeatQueueMoveView,
}

/// The durable facts that order one seat's queue: when each run joined, every
/// recorded move in graph order, and which joined runs are still live.
#[derive(Default)]
struct SeatQueueInputs {
    joins: Vec<QueueJoin>,
    moves: Vec<RecordedSeatMove>,
    statuses: BTreeMap<String, String>,
    live: BTreeSet<String>,
}

impl SeatQueueInputs {
    fn live_order(&self) -> Vec<String> {
        let moves = self
            .moves
            .iter()
            .map(|recorded| recorded.movement.clone())
            .collect::<Vec<_>>();
        seat_queue::replay(&self.joins, &moves)
            .into_iter()
            .filter(|run| self.live.contains(run))
            .collect()
    }
}

/// When each seat `?1` (every seat when null) joined each open run it has a step in, and the
/// run's status. It reads from the open runs to their steps, in that order: from the steps,
/// SQLite reads every step any seat was ever assigned before it finds the open runs among them.
/// Every seat's queue moves (`?2` null), or one seat's, that no repair replaced, in graph order.
pub fn seat_queue_moves_query() -> String {
    canonical_sql(
        "SELECT claims.id, claims.subject, claims.actor, claims.body, claims.accepted_at_unix_ms
         FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.kind=?1 AND (?2 IS NULL OR claims.subject=?2)
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY CANONICAL_ASC(claims)",
    )
}

pub fn seat_queue_joins_query() -> String {
    format!(
        "SELECT step_runs.assignee, step_runs.run_id,
                MIN(CAST(step_runs.created_at_unix_ms AS INTEGER)), mission_runs.status
         FROM mission_runs CROSS JOIN step_runs ON step_runs.run_id=mission_runs.id
         WHERE {OPEN_MISSION_RUN}
           AND mission_runs.phase<>'terminal'
           AND step_runs.agentless=0
           AND step_runs.assignee IS NOT NULL
           AND (?1 IS NULL OR step_runs.assignee=?1)
         GROUP BY step_runs.assignee, step_runs.run_id
         HAVING MAX(step_runs.generation_id=mission_runs.current_generation_id)=1
         ORDER BY step_runs.assignee, step_runs.run_id"
    )
}

/// The current generation's steps of actor `?1` (every actor when null), with finished and
/// pending ones when `?2`, and agentless ones when `?3` and no actor is named. With `open_only`
/// the finished steps are left out in SQL, word for word the predicate of
/// `step_runs_open_index`, so the read covers the fleet's open steps and not every step it ran.
fn work_at_snapshot_query(open_only: bool) -> String {
    work_at_snapshot_query_of(open_only, "")
}

/// [`work_at_snapshot_query`] of only the step subjects in the JSON array `?4`, by their key.
fn selected_work_at_snapshot_query(open_only: bool) -> String {
    work_at_snapshot_query_of(open_only, "subject IN (SELECT value FROM json_each(?4)) AND")
}

fn work_at_snapshot_query_of(open_only: bool, selected: &str) -> String {
    let open = if open_only {
        "AND status NOT IN ('completed','failed','cancelled')"
    } else {
        ""
    };
    format!(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs
         WHERE {selected} (agentless=0 OR (?3 AND ?1 IS NULL))
           AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=step_runs.run_id)
           AND (?1 IS NULL
                OR assignee=?1
                OR lease_owner=?1
                OR EXISTS (SELECT 1 FROM json_each(step_runs.available_to) WHERE value=?1))
           AND (?2 OR status NOT IN ('pending','completed','failed','cancelled'))
           {open}
         ORDER BY created_at_unix_ms, step_path"
    )
}

/// [`work_at_snapshot_query`] for one named actor `?1`. The reconciler reads each running seat's
/// steps on every pass; the actor's steps are found through the assignee and lease indexes, and
/// the few steps offered to several seats through their own index, instead of scanning every
/// step the store ever ran.
fn seat_work_at_snapshot_query(open_only: bool) -> String {
    let open = if open_only {
        "AND status NOT IN ('completed','failed','cancelled')"
    } else {
        ""
    };
    format!(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs
         WHERE rowid IN (
                 SELECT rowid FROM step_runs WHERE assignee=?1
                 UNION SELECT rowid FROM step_runs WHERE lease_owner=?1
                 UNION SELECT rowid FROM step_runs WHERE available_to!='[]'
                   AND EXISTS (SELECT 1 FROM json_each(step_runs.available_to) WHERE value=?1)
               )
           AND (agentless=0 OR (?3 AND ?1 IS NULL))
           AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=step_runs.run_id)
           AND (?2 OR status NOT IN ('pending','completed','failed','cancelled'))
           {open}
         ORDER BY created_at_unix_ms, step_path"
    )
}

/// Read seat-queue inputs for one seat or the whole roster. A live run is queued
/// while its current generation has a step for the seat, and it keeps the join
/// time of its first such step in any generation. Runs that are no longer
/// queued are read only when a move names them, because a run that never
/// anchors or receives a move cannot change the relative order of the others.
fn seat_queue_inputs_tx(
    connection: &Connection,
    agent: Option<&str>,
) -> Result<BTreeMap<String, SeatQueueInputs>> {
    seat_queue_inputs_of_tx(connection, agent, None)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Named-run probes `seat_queue_inputs_of_tx` made on this thread, for tests.
    static NAMED_RUN_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many named-run probes this thread's seat-queue reads have made, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn named_run_probes() -> usize {
    NAMED_RUN_PROBES.with(std::cell::Cell::get)
}

/// [`seat_queue_inputs_tx`], keeping only `seats` when given: every move and join is still read
/// in one pass, but only those seats' named runs are probed, one query each.
fn seat_queue_inputs_of_tx(
    connection: &Connection,
    agent: Option<&str>,
    only: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, SeatQueueInputs>> {
    let mut seats = BTreeMap::<String, SeatQueueInputs>::new();
    let mut statement = connection.prepare(&seat_queue_moves_query())?;
    let rows = statement.query_map(params![seat_queue::MOVED_CLAIM, agent], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (claim_id, subject, actor, body, accepted_at) = row?;
        let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        let fields = body.get("fields").unwrap_or(&body);
        let text = |name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
        let (Some(run), Some(placement)) = (
            text("run"),
            text("placement").as_deref().and_then(Placement::parse),
        ) else {
            continue;
        };
        let anchor = text("anchor");
        let at_unix_ms = accepted_at.parse().unwrap_or_default();
        seats
            .entry(subject)
            .or_default()
            .moves
            .push(RecordedSeatMove {
                movement: QueueMove {
                    run: run.clone(),
                    placement,
                    anchor: anchor.clone(),
                    at_unix_ms,
                },
                view: SeatQueueMoveView {
                    claim_id,
                    run,
                    placement: placement.as_str().into(),
                    anchor,
                    actor,
                    reason: text("reason"),
                    moved_at_unix_ms: at_unix_ms,
                },
            });
    }
    drop(statement);

    let mut statement = connection.prepare(&seat_queue_joins_query())?;
    let rows = statement.query_map(params![agent], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (seat, run_id, joined_at, status) = row?;
        let run = format!("mission-run/{run_id}");
        let inputs = seats.entry(seat).or_default();
        inputs.joins.push(QueueJoin {
            run: run.clone(),
            at_unix_ms: u128::try_from(joined_at).unwrap_or_default(),
        });
        inputs.statuses.insert(run.clone(), status);
        inputs.live.insert(run);
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT MIN(CAST(step_runs.created_at_unix_ms AS INTEGER)), mission_runs.status
         FROM step_runs JOIN mission_runs ON mission_runs.id=step_runs.run_id
         WHERE step_runs.assignee=?1 AND step_runs.run_id=?2 AND step_runs.agentless=0",
    )?;
    if let Some(only) = only {
        seats.retain(|seat, _| only.contains(seat));
    }
    for (seat, inputs) in &mut seats {
        let named = inputs
            .moves
            .iter()
            .flat_map(|recorded| {
                std::iter::once(recorded.movement.run.clone())
                    .chain(recorded.movement.anchor.clone())
            })
            .filter(|run| !inputs.live.contains(run))
            .collect::<BTreeSet<_>>();
        for run in named {
            let Some(run_id) = run.strip_prefix("mission-run/") else {
                continue;
            };
            #[cfg(any(test, feature = "test-support"))]
            NAMED_RUN_PROBES.with(|probes| probes.set(probes.get() + 1));
            let joined = statement
                .query_row(params![seat, run_id], |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })
                .optional()?;
            if let Some((Some(joined_at), status)) = joined {
                inputs.joins.push(QueueJoin {
                    run: run.clone(),
                    at_unix_ms: u128::try_from(joined_at).unwrap_or_default(),
                });
                inputs.statuses.insert(run, status.unwrap_or_default());
            }
        }
    }
    Ok(seats)
}

fn flatten_steps<'a>(
    mission: &'a MissionSpec,
    inherited_selector: Option<WorkSelector>,
    inherited_constraints: &[String],
    output: &mut Vec<(&'a crate::model::StepSpec, WorkSelector, Vec<String>)>,
) {
    let agent_constraints = run_agent_constraints(mission);
    flatten_run_steps(
        mission,
        inherited_selector,
        inherited_constraints,
        &agent_constraints,
        output,
    );
}

fn flatten_run_steps<'a>(
    mission: &'a MissionSpec,
    inherited_selector: Option<WorkSelector>,
    inherited_constraints: &[String],
    agent_constraints: &BTreeMap<String, Vec<String>>,
    output: &mut Vec<(&'a crate::model::StepSpec, WorkSelector, Vec<String>)>,
) {
    let mut mission_constraints = inherited_constraints.to_vec();
    mission_constraints.extend(mission.constraints.clone());
    let mission_selector = mission
        .work_selector
        .clone()
        .or(inherited_selector)
        .unwrap_or(WorkSelector::Agentless);
    for id in &mission.display_order {
        let step = &mission.steps[id];
        let selector = step
            .work_selector
            .clone()
            .unwrap_or_else(|| mission_selector.clone());
        let mut constraints = mission_constraints.clone();
        constraints.extend(step.constraints.clone());
        let mut effective = constraints.clone();
        for agent in selector_agents(&selector) {
            for constraint in agent_constraints.get(agent).into_iter().flatten() {
                if !effective.contains(constraint) {
                    effective.push(constraint.clone());
                }
            }
        }
        output.push((step, selector.clone(), effective));
        if let Some(nested) = &step.nested_mission {
            flatten_run_steps(
                nested,
                Some(selector),
                &constraints,
                agent_constraints,
                output,
            );
        }
    }
}

/// Agent blocks anywhere in one run declare run-scoped agents, so their constraints apply to every
/// step of that run that selects the agent. Loop rounds are separate runs and keep their own.
fn run_agent_constraints(mission: &MissionSpec) -> BTreeMap<String, Vec<String>> {
    fn collect(
        agent_constraints: &BTreeMap<String, Vec<String>>,
        output: &mut BTreeMap<String, Vec<String>>,
    ) {
        for (agent, constraints) in agent_constraints {
            let entry = output.entry(agent.clone()).or_default();
            for constraint in constraints {
                if !entry.contains(constraint) {
                    entry.push(constraint.clone());
                }
            }
        }
    }
    fn append(mission: &MissionSpec, output: &mut BTreeMap<String, Vec<String>>) {
        collect(&mission.agent_constraints, output);
        for id in &mission.display_order {
            let step = &mission.steps[id];
            collect(&step.agent_constraints, output);
            if let Some(nested) = &step.nested_mission {
                append(nested, output);
            }
        }
    }
    let mut output = BTreeMap::new();
    append(mission, &mut output);
    output
}

fn selector_agents(selector: &WorkSelector) -> &[String] {
    match selector {
        WorkSelector::Assigned { agent } => std::slice::from_ref(agent),
        WorkSelector::Available { agents } => agents,
        WorkSelector::Agentless => &[],
    }
}

fn flatten_mission_step_specs(mission: &MissionSpec) -> Vec<&crate::model::StepSpec> {
    fn append<'a>(mission: &'a MissionSpec, output: &mut Vec<&'a crate::model::StepSpec>) {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            output.push(step);
            if let Some(nested) = &step.nested_mission {
                append(nested, output);
            }
        }
    }
    let mut output = Vec::new();
    append(mission, &mut output);
    output
}

fn interpolate_selector(
    selector: &WorkSelector,
    variables: &BTreeMap<String, String>,
) -> Result<(Option<String>, Vec<String>, bool), St3Error> {
    match selector {
        WorkSelector::Assigned { agent } => Ok((
            Some(crate::mission::interpolate(agent, variables)?),
            Vec::new(),
            false,
        )),
        WorkSelector::Available { agents } => {
            let mut output = agents
                .iter()
                .map(|agent| crate::mission::interpolate(agent, variables))
                .collect::<Result<Vec<_>, _>>()?;
            output.sort();
            output.dedup();
            Ok((None, output, false))
        }
        WorkSelector::Agentless => Ok((None, Vec::new(), true)),
    }
}

pub fn mission_run_variables(
    run: &MissionRunView,
    revision: &str,
) -> BTreeMap<String, String> {
    let mut variables = BTreeMap::from([
        (
            "ST_MISSION".into(),
            run.mission
                .strip_prefix("mission/")
                .unwrap_or(&run.mission)
                .into(),
        ),
        ("ST_MISSION_REVISION".into(), revision.into()),
        ("ST_MISSION_RUN".into(), run.id.clone()),
        (
            "ST_RUN_GENERATION".into(),
            generation_id_from_subject(&run.generation).into(),
        ),
        ("ST_WORKSPACE".into(), run.workspace.clone()),
        ("ST_REQUESTER".into(), run.requester.clone()),
        ("PATH".into(), "${PATH}".into()),
        (
            "ST_PARENT_STEP_RUN".into(),
            run.parent_step_run.clone().unwrap_or_default(),
        ),
        ("ST_ROOT_MISSION_RUN".into(), run.root_mission_run.clone()),
        (
            "ST_ROOT_MISSION_RUN_ID".into(),
            run.root_mission_run
                .strip_prefix("mission-run/")
                .unwrap_or(&run.root_mission_run)
                .into(),
        ),
    ]);
    variables.extend(
        run.inputs
            .iter()
            .map(|(name, input)| (format!("input.{name}"), input.value.clone())),
    );
    extend_loop_variables(&mut variables, &run.inputs);
    variables
}

fn extend_loop_variables(
    variables: &mut BTreeMap<String, String>,
    inputs: &BTreeMap<String, MissionRunInput>,
) {
    if let Some(round) = inputs.get(crate::mission::LOOP_ROUND_INPUT) {
        variables.insert("ST_LOOP_ROUND".into(), round.value.clone());
        variables.insert("loop.round".into(), round.value.clone());
    }
    if let Some(feedback) = inputs.get(crate::mission::LOOP_FEEDBACK_INPUT) {
        variables.insert("ST_LOOP_FEEDBACK".into(), feedback.value.clone());
        variables.insert("loop.feedback".into(), feedback.value.clone());
    }
    if let Some(candidate) = inputs.get(crate::mission::CANDIDATE_INDEX_INPUT) {
        variables.insert("ST_CANDIDATE_INDEX".into(), candidate.value.clone());
        variables.insert("candidate.index".into(), candidate.value.clone());
    }
    if let Some(item) = inputs.get(crate::mission::LOOP_ITEM_INPUT)
        && let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&item.value)
    {
        for (name, value) in fields {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            variables.insert(format!("loop.item.{name}"), value);
        }
        let id = variables.get("loop.item.id").cloned().unwrap_or_default();
        variables.insert("ST_LOOP_ITEM_ID".into(), id);
    }
}

fn interpolate_goals(
    goals: &[String],
    variables: &BTreeMap<String, String>,
) -> Result<String, St3Error> {
    let goals = goals
        .iter()
        .map(|goal| crate::mission::interpolate(goal, variables))
        .collect::<Result<Vec<_>, _>>()?;
    serde_json::to_string(&goals).map_err(internal)
}

pub fn analyze_mission_revision(
    old: &MissionSpec,
    new: &MissionSpec,
    requester: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>), St3Error> {
    let old_hashes = step_hashes(old);
    let new_hashes = step_hashes(new);
    let mut changed = old_hashes
        .keys()
        .chain(new_hashes.keys())
        .filter(|path| old_hashes.get(*path) != new_hashes.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    if mission_header_hash(old)? != mission_header_hash(new)? {
        changed.insert(String::new());
    }
    if changed.is_empty() {
        return Err(St3Error::new(
            "unchanged-mission-revision",
            "the proposed mission revision does not change the mission",
        ));
    }

    let requester = normalize_actor(requester, "person");
    let metadata = revision_reviewers(old, &requester);
    // Free mode: a person or an agent may revise any part of a run. Human-only protection still
    // selects its reviewers.
    let mut reviewers = BTreeSet::new();
    for path in &changed {
        reviewers.extend(reviewers_for_changed_path(&metadata, path).iter().cloned());
    }
    Ok((compatible_step_paths(old, new), reviewers))
}

/// The reviewers each mission or step path's human-only revision protection selects.
fn revision_reviewers(
    mission: &MissionSpec,
    requester: &str,
) -> BTreeMap<String, BTreeSet<String>> {
    fn collect(
        mission: &MissionSpec,
        requester: &str,
        inherited: BTreeSet<String>,
        output: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut mission_reviewers = inherited;
        if mission.revisions_human_only {
            mission_reviewers.insert(
                mission
                    .revision_reviewer
                    .as_deref()
                    .unwrap_or(requester)
                    .to_owned(),
            );
        }
        output.insert(String::new(), mission_reviewers.clone());
        for id in &mission.display_order {
            let step = &mission.steps[id];
            let mut step_reviewers = mission_reviewers.clone();
            if step.revisions_human_only {
                step_reviewers.insert(
                    step.revision_reviewer
                        .as_deref()
                        .unwrap_or(requester)
                        .to_owned(),
                );
            }
            output.insert(step.path.clone(), step_reviewers.clone());
            if let Some(nested) = &step.nested_mission {
                let mut nested_reviewers = BTreeMap::new();
                collect(nested, requester, step_reviewers, &mut nested_reviewers);
                for (path, reviewers) in nested_reviewers {
                    if !path.is_empty() {
                        output.insert(path, reviewers);
                    }
                }
            }
        }
    }
    let mut output = BTreeMap::new();
    collect(mission, requester, BTreeSet::new(), &mut output);
    output
}

fn reviewers_for_changed_path<'a>(
    metadata: &'a BTreeMap<String, BTreeSet<String>>,
    path: &str,
) -> &'a BTreeSet<String> {
    metadata
        .iter()
        .filter(|(candidate, _)| {
            candidate.is_empty()
                || path == candidate.as_str()
                || path.starts_with(&format!("{candidate}/"))
        })
        .max_by_key(|(candidate, _)| candidate.len())
        .map(|(_, meta)| meta)
        .expect("the mission metadata is always present")
}

fn step_hashes(mission: &MissionSpec) -> BTreeMap<String, String> {
    let mut flat = Vec::new();
    flatten_steps(mission, None, &[], &mut flat);
    flat.into_iter()
        .map(|(step, selector, constraints)| {
            let bytes = serde_json::to_vec(&(step.definition_hash.as_str(), selector, constraints))
                .expect("a step compatibility value serializes");
            (step.path.clone(), hex::encode(Sha256::digest(bytes)))
        })
        .collect()
}

fn mission_header_hash(mission: &MissionSpec) -> Result<String, St3Error> {
    let value = json!({
        "id": mission.id,
        "state": mission.state,
        "inputs": mission.inputs,
        "max_active_runs": mission.max_active_runs,
        "owners": mission.revision_owners,
        "human_only": mission.revisions_human_only,
        "reviewer": mission.revision_reviewer,
        "cutover": mission.revision_cutover,
        "declarations": mission.declarations_kdl,
        "work_selector": mission.work_selector,
        "completion": mission.completion,
        "goals": mission.goals,
        "constraints": mission.constraints,
        "agent_constraints": mission.agent_constraints,
        "baselines": mission.baselines,
        "products": mission.products,
        "gates": mission.gates,
    });
    serde_json::to_vec(&value)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .map_err(internal)
}

fn compatible_step_paths(old: &MissionSpec, new: &MissionSpec) -> BTreeSet<String> {
    let old_hashes = step_hashes(old);
    let new_hashes = step_hashes(new);
    let dependencies = flattened_dependencies(new);
    let mut incompatible = new_hashes
        .iter()
        .filter(|(path, hash)| old_hashes.get(*path) != Some(*hash))
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    loop {
        let additions = dependencies
            .iter()
            .filter(|(path, dependencies)| {
                !incompatible.contains(*path)
                    && dependencies
                        .iter()
                        .any(|dependency| incompatible.contains(dependency))
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if additions.is_empty() {
            break;
        }
        incompatible.extend(additions);
    }
    new_hashes
        .keys()
        .filter(|path| !incompatible.contains(*path))
        .cloned()
        .collect()
}

/// Preserve work when the step itself and every step it depends on are
/// unchanged. Mission-level constraints and selectors affect future execution,
/// so an active lease is released back to ready, but they must not silently
/// erase a completed result or a submission already awaiting verification.
fn carried_revision_step_paths(
    old: &MissionSpec,
    new: &MissionSpec,
    current: &[StepRunView],
    mut compatible: BTreeSet<String>,
) -> BTreeSet<String> {
    let old_definitions = flattened_step_definition_hashes(old);
    let new_definitions = flattened_step_definition_hashes(new);
    let regated = regated_submitted_steps(old, new, current);
    let dependencies = flattened_dependencies(new);
    let mut unstable = new_definitions
        .iter()
        .filter(|(path, hash)| {
            old_definitions.get(*path) != Some(*hash) && !regated.contains(*path)
        })
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    loop {
        let additions = dependencies
            .iter()
            .filter(|(path, dependencies)| {
                !unstable.contains(*path)
                    && dependencies
                        .iter()
                        .any(|dependency| unstable.contains(dependency))
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if additions.is_empty() {
            break;
        }
        unstable.extend(additions);
    }
    compatible.extend(
        current
            .iter()
            .filter(|step| {
                new_definitions.contains_key(&step.step) && !unstable.contains(&step.step)
            })
            .map(|step| step.step.clone()),
    );
    compatible
}

/// The steps a revision changes only in their gates while their worker's submitted work waits
/// on those gates, as it does behind a broken gate. They keep the submission: the successor
/// generation checks the work against the revised gates rather than asking for it again.
fn regated_submitted_steps(
    old: &MissionSpec,
    new: &MissionSpec,
    current: &[StepRunView],
) -> BTreeSet<String> {
    let old_steps = flatten_mission_step_specs(old)
        .into_iter()
        .map(|step| (step.path.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    flatten_mission_step_specs(new)
        .into_iter()
        .filter(|step| {
            current.iter().any(|view| {
                view.step == step.path && view.status == "verifying" && view.worker_reported
            }) && old_steps.get(step.path.as_str()).is_some_and(|old| {
                old.definition_hash != step.definition_hash
                    && step_definition_without_gates(old) == step_definition_without_gates(step)
            })
        })
        .map(|step| step.path.clone())
        .collect()
}

/// A step's definition with its gates, and the gates and definition hashes of any steps nested
/// in it, left out.
fn step_definition_without_gates(step: &crate::model::StepSpec) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.remove("gates");
                fields.remove("definition_hash");
                fields.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(step).unwrap_or(Value::Null);
    strip(&mut value);
    value
}

fn flattened_step_definition_hashes(mission: &MissionSpec) -> BTreeMap<String, String> {
    flatten_mission_step_specs(mission)
        .into_iter()
        .map(|step| (step.path.clone(), step.definition_hash.clone()))
        .collect()
}

/// The seat whose claim the carried projection gives up: an unreported verifying step
/// becomes ready in the new generation. A carried `ready` claim records it as `claimant`;
/// other carried statuses record the lease they keep under the same field.
fn dropped_claimant<'a>(old: &'a StepRunView, status: &str) -> Option<&'a str> {
    (status == "ready" && matches!(old.status.as_str(), "claimed" | "working" | "verifying"))
        .then_some(old.claimant.as_deref())
        .flatten()
}

/// For each named ready step, the seat whose claim a revision dropped when it
/// carried the step, while that seat has not claimed it since. Its seat queue
/// keeps the step first, as a reorder never takes a held step's place. The
/// lookup starts from the named steps, so its cost follows current work rather
/// than every carried step in history.
fn carried_claimants_tx<'a>(
    connection: &Connection,
    ready: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, String>> {
    let ready = ready.into_iter().collect::<Vec<_>>();
    if ready.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(connection
        .prepare_cached(&format!(
            "SELECT carried.subject, json_extract(carried.body, '$.fields.claimant')
             FROM json_each(?1) AS ready
             CROSS JOIN claims AS carried
               ON carried.subject=ready.value AND carried.kind='step-run.carried'
             WHERE json_extract(carried.body, '$.fields.status')='ready'
               AND json_extract(carried.body, '$.fields.claimant') IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM claims AS later
                   WHERE later.subject=carried.subject AND later.kind='work.claimed'
                     AND {}
               )",
            canonical::after_sql("later", "carried")
        ))?
        .query_map([serde_json::to_string(&ready)?], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn is_failed_terminal(status: &str, phase: &str) -> bool {
    status == "failed" && phase == "terminal"
}

/// A reopened run keeps its completed normal work and retries its failed steps. Work that the
/// failure cancelled and every final step start again.
fn retain_reopened_step_paths(
    mission: &MissionSpec,
    current: &[StepRunView],
    compatible: &mut BTreeSet<String>,
) {
    let finals = flatten_mission_step_specs(mission)
        .into_iter()
        .filter(|step| step.finally)
        .map(|step| step.path.as_str())
        .collect::<BTreeSet<_>>();
    compatible.retain(|path| {
        current.iter().any(|step| {
            &step.step == path
                && (step.status == "failed"
                    || (step.status == "completed" && !finals.contains(path.as_str())))
        })
    });
}

fn carried_step_projection(step: &StepRunView) -> (&str, bool) {
    match step.status.as_str() {
        "verifying" if step.worker_reported => ("verifying", true),
        "verifying" => ("ready", false),
        status => (status, step.worker_reported),
    }
}

/// Return the current-generation step that took the place of a superseded step, whoever holds it.
fn current_generation_successor_tx(
    connection: &Connection,
    step: &StepRunView,
) -> rusqlite::Result<Option<StepRunView>> {
    let run_id = step.run.strip_prefix("mission-run/").unwrap_or(&step.run);
    connection
        .query_row(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                    lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
             FROM step_runs
             WHERE run_id=?1 AND step_path=?2 AND generation_id<>?3
               AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)",
            params![run_id, step.step, generation_id_from_subject(&step.generation)],
            step_run_from_row,
        )
        .optional()
}

/// A carried step keeps the worker lease of the status it carries, so its worker continues in
/// the successor generation without claiming again.
fn carried_step_lease(
    carried: Option<&StepRunView>,
    status: &str,
) -> (Option<String>, Option<String>, Option<u128>) {
    let Some(old) = carried.filter(|old| {
        old.status == status
            && old.claimant.is_some()
            && matches!(status, "claimed" | "working" | "verifying" | "blocked")
    }) else {
        return (None, None, None);
    };
    (
        old.claimant.clone(),
        old.claim_incarnation.clone(),
        old.claim_expires_at_unix_ms,
    )
}

fn flattened_dependencies(mission: &MissionSpec) -> BTreeMap<String, BTreeSet<String>> {
    fn collect(
        mission: &MissionSpec,
        prefix: &str,
        parent: Option<&str>,
        output: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            let mut dependencies = BTreeSet::new();
            if let Some(parent) = parent {
                dependencies.insert(parent.into());
            }
            for dependency in &step.dependencies {
                if let DependencySpec::Step { step: target, .. } = dependency {
                    dependencies.insert(if prefix.is_empty() {
                        target.clone()
                    } else {
                        format!("{prefix}/{target}")
                    });
                }
            }
            output.insert(step.path.clone(), dependencies);
            if let Some(nested) = &step.nested_mission {
                collect(
                    nested,
                    &format!("{}/{}", step.path, nested.id),
                    Some(&step.path),
                    output,
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    collect(mission, "", None, &mut output);
    output
}

pub fn step_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepRunView> {
    let lease: Option<String> = row.get(14)?;
    let not_before: Option<String> = row.get(16)?;
    let created: String = row.get(17)?;
    let updated: String = row.get(18)?;
    let subject: String = row.get(0)?;
    let generation = subject
        .split('/')
        .nth(1)
        .map(|id| format!("run-generation/{id}"))
        .unwrap_or_default();
    Ok(StepRunView {
        subject,
        run: format!("mission-run/{}", row.get::<_, String>(1)?),
        generation,
        step: row.get(2)?,
        fresh_context: false,
        queue: None,
        queue_position: None,
        definition_hash: row.get(3)?,
        status: row.get(4)?,
        attempt: row.get::<_, u32>(5)?,
        assigned_to: row.get(6)?,
        available_to: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
        agentless: row.get(8)?,
        title: row.get(9)?,
        goals: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
        constraints: serde_json::from_str(&row.get::<_, String>(20)?).unwrap_or_default(),
        under: Vec::new(),
        worker_reported: row.get::<_, bool>(11)?,
        claimant: row.get(12)?,
        claim_incarnation: row.get(13)?,
        submitted_incarnation: None,
        claim_expires_at_unix_ms: lease.and_then(|value| value.parse().ok()),
        carried_claimant: None,
        execution_started_at_unix_ms: None,
        execution_paused_until_unix_ms: None,
        execution_waited_ms: 0,
        execution_elapsed_ms: 0,
        timeout_ms: None,
        timeout_extension_ms: 0,
        ready_age_ms: None,
        wake: None,
        progress_summary: None,
        progress_at_unix_ms: None,
        completion_summary: None,
        readiness_epoch: row.get(19)?,
        blocked_reason: row.get(15)?,
        blockers: Vec::new(),
        not_before_unix_ms: not_before.and_then(|value| value.parse().ok()),
        nudged_at_unix_ms: None,
        created_at_unix_ms: created.parse().unwrap_or(0),
        updated_at_unix_ms: updated.parse().unwrap_or(0),
        person_answers: Vec::new(),
    })
}

/// What `work extend` has added to one step attempt's execution budget as of `snapshot_unix_ms`.
pub fn step_timeout_extension_at(
    connection: &Connection,
    subject: &str,
    attempt: u32,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<u64> {
    Ok(connection
        .query_row(
            "SELECT COALESCE(SUM(CAST(json_extract(body, '$.fields.extend_ms') AS INTEGER)), 0)
             FROM claims
             WHERE subject=?1 AND kind='work.extended'
               AND json_extract(body, '$.fields.attempt')=?2
               AND CAST(accepted_at_unix_ms AS INTEGER)<=?3",
            params![subject, attempt, snapshot_unix_ms as i64],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u64)
}

/// The episode of one attempt's timeout fault. An extension starts a new episode, so a step that
/// runs out of its extended budget faults again.
pub fn step_timeout_episode(subject: &str, attempt: u32, extension_ms: u64) -> String {
    let key = format!("step-timeout:{subject}:{attempt}:{extension_ms}");
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    format!("attention/{}", &digest[..32])
}

fn enrich_step_queue(connection: &Connection, view: &mut StepRunView) -> rusqlite::Result<()> {
    enrich_step_queue_at(connection, view, now_ms())
}

fn enrich_step_queue_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    enrich_step_queue_cached(connection, view, snapshot_unix_ms, &mut PinnedDefinitions::default())
}

/// [`enrich_step_queue_at`] for the steps of one read: the pinned definitions already parsed for
/// an earlier step of it are reused, not parsed again for each step.
fn enrich_step_queue_cached(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
    definitions: &mut PinnedDefinitions,
) -> rusqlite::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    STEPS_ENRICHED.with(|enriched| enriched.set(enriched.get() + 1));
    apply_effective_step_state(connection, view, snapshot_unix_ms)?;
    let timing = step_execution_clock_at(
        connection,
        &view.subject,
        view.attempt,
        snapshot_unix_ms,
        matches!(view.status.as_str(), "claimed" | "working"),
    )?;
    view.execution_started_at_unix_ms = timing.started;
    view.execution_paused_until_unix_ms = timing.paused_until;
    view.execution_waited_ms = timing.waited;
    view.execution_elapsed_ms = timing.elapsed;
    view.timeout_extension_ms =
        step_timeout_extension_at(connection, &view.subject, view.attempt, snapshot_unix_ms)?;
    enrich_step_summaries_at(connection, view, snapshot_unix_ms)?;
    enrich_step_nudge_at(connection, view, snapshot_unix_ms)?;
    enrich_step_wake_at(connection, view, snapshot_unix_ms)?;
    enrich_step_definition_cached(connection, view, definitions)?;
    adhoc_work::enrich_handoff(connection, view, snapshot_unix_ms)?;
    person_work::enrich_responses(connection, view)
}

/// When st last nudged a held step's holder in this attempt: one indexed lookup of the step's
/// newest `work.nudged` claim.
fn enrich_step_nudge_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    if !matches!(view.status.as_str(), "claimed" | "working") {
        return Ok(());
    }
    let snapshot = snapshot_unix_ms.to_string();
    view.nudged_at_unix_ms = connection
        .prepare_cached(
            "SELECT accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='work.nudged'
               AND json_extract(body, '$.fields.attempt')=?2
               AND (length(accepted_at_unix_ms), accepted_at_unix_ms) <= (length(?3), ?3)
             ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1",
        )?
        .query_row(params![view.subject, view.attempt, snapshot], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|at| at.parse().ok());
    Ok(())
}

/// Reads the current attempt's summaries, following revision carry links back to
/// the cutover snapshot. Successor reports supersede inherited reports, while a
/// new attempt cannot inherit the predecessor attempt's notes.
fn enrich_step_summaries_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare_cached(&canonical_sql(
        "SELECT kind, body, accepted_at_unix_ms FROM claims
         WHERE subject=?1 AND kind IN ('work.progress','work.submitted','step-run.carried')
         ORDER BY CANONICAL_DESC(claims)",
    ))?;
    view.progress_summary = None;
    view.progress_at_unix_ms = None;
    view.completion_summary = None;
    view.submitted_incarnation = None;
    let mut submitted = false;
    let mut subject = view.subject.clone();
    let mut snapshot = snapshot_unix_ms;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(subject.clone()) {
            // Replicated carry links can form a cycle. Keep the reports already
            // read and stop at the repeated subject, without following it again.
            break;
        }
        let mut predecessor = None;
        let mut events = statement.query([&subject])?;
        while let Some(row) = events.next()? {
            let kind = row.get::<_, String>(0)?;
            let body = row.get::<_, String>(1)?;
            let accepted = row.get::<_, String>(2)?.parse::<u128>().unwrap_or(0);
            if accepted > snapshot {
                continue;
            }
            let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
            let fields = body.get("fields").unwrap_or(&body);
            if fields.get("attempt").and_then(Value::as_u64) != Some(u64::from(view.attempt)) {
                continue;
            }
            if kind == "step-run.carried" {
                if predecessor.is_none() {
                    predecessor = fields
                        .get("source_step_run")
                        .and_then(Value::as_str)
                        .map(|source| (source.to_owned(), accepted));
                }
                continue;
            }
            if fields.get("handoff_acknowledged").is_some() {
                continue;
            }
            if kind == "work.submitted" && !submitted {
                // A newer submission without an incarnation must not borrow an older one.
                submitted = true;
                view.submitted_incarnation = fields
                    .get("submitted_incarnation")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            let Some(summary) = fields
                .get("summary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
            else {
                continue;
            };
            if kind == "work.progress" && view.progress_summary.is_none() {
                view.progress_summary = Some(summary.to_owned());
                view.progress_at_unix_ms = Some(accepted);
            } else if kind == "work.submitted" && view.completion_summary.is_none() {
                view.completion_summary = Some(summary.to_owned());
            }
        }
        if view.progress_summary.is_some() && view.completion_summary.is_some() && submitted {
            break;
        }
        let Some((source, cutover)) = predecessor else {
            break;
        };
        subject = source;
        snapshot = cutover;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn enrich_step_queue_for_reconcile_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    enrich_step_queue_for_reconcile_cached(
        connection,
        view,
        snapshot_unix_ms,
        &mut PinnedDefinitions::default(),
    )
}

fn enrich_step_queue_for_reconcile_cached(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
    definitions: &mut PinnedDefinitions,
) -> rusqlite::Result<()> {
    apply_effective_step_state(connection, view, snapshot_unix_ms)?;
    enrich_step_definition_cached(connection, view, definitions)?;
    adhoc_work::enrich_handoff(connection, view, snapshot_unix_ms)?;
    person_work::enrich_responses(connection, view)
}

fn pinned_generation_mission_tx(
    connection: &Connection,
    generation: &str,
) -> rusqlite::Result<Option<MissionSpec>> {
    let body = connection
        .query_row(
            "SELECT mission_revisions.body
             FROM run_generations
             JOIN mission_runs ON mission_runs.id=run_generations.run_id
             JOIN mission_revisions
               ON mission_revisions.mission_id=mission_runs.mission_id
              AND mission_revisions.revision=run_generations.revision
             WHERE run_generations.id=?1",
            [generation_id_from_subject(generation)],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    body.map(|body| {
        serde_json::from_str::<MissionSpec>(&body).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                body.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    })
    .transpose()
}

fn enrich_step_definition(connection: &Connection, view: &mut StepRunView) -> rusqlite::Result<()> {
    enrich_step_definition_cached(connection, view, &mut PinnedDefinitions::default())
}

/// The pinned definitions one read has parsed, by generation. A generation's definition is the
/// whole mission, so parsing it again for each of the run's steps costs the square of the run's
/// size (4,200 steps: 170 s in a debug build).
#[derive(Default)]
struct PinnedDefinitions(BTreeMap<String, Option<MissionSpec>>);

impl PinnedDefinitions {
    fn of(
        &mut self,
        connection: &Connection,
        generation: &str,
    ) -> rusqlite::Result<Option<&MissionSpec>> {
        if !self.0.contains_key(generation) {
            #[cfg(any(test, feature = "test-support"))]
            DEFINITIONS_PARSED.with(|parsed| parsed.set(parsed.get() + 1));
            let mission = pinned_generation_mission_tx(connection, generation)?;
            self.0.insert(generation.to_owned(), mission);
        }
        Ok(self.0[generation].as_ref())
    }
}

fn enrich_step_definition_cached(
    connection: &Connection,
    view: &mut StepRunView,
    definitions: &mut PinnedDefinitions,
) -> rusqlite::Result<()> {
    if let Some(mission) = definitions.of(connection, &view.generation)? {
        apply_step_definition(view, mission);
    }
    Ok(())
}

fn apply_step_definition(view: &mut StepRunView, mission: &MissionSpec) {
    if let Some(step) = crate::mission::find_step(mission, &view.step) {
        view.queue.clone_from(&step.queue);
        view.queue_position = step.queue_position;
        view.timeout_ms = step.timeout_ms;
        view.fresh_context = step.fresh_context;
    }
}

fn fresh_context_ready_tx(
    connection: &Connection,
    step: &StepRunView,
    actor: &str,
    incarnation: &str,
) -> rusqlite::Result<bool> {
    let member: Option<String> = connection
        .query_row(
            "SELECT member FROM desired WHERE subject=?1",
            [actor],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let seat_fresh = member
        .as_deref()
        .and_then(|member| serde_json::from_str::<Value>(member).ok())
        .is_some_and(|member| {
            member
                .pointer("/tags/st3.fresh_context")
                .and_then(Value::as_str)
                == Some("true")
        });
    if !step.fresh_context && !seat_fresh {
        return Ok(true);
    }
    let operation = crate::model::fresh_context_operation(step);
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_observations
         WHERE subject=?1 AND kind='runtime.action.succeeded'
           AND json_extract(body, '$.fields.action')='fresh-context'
           AND json_extract(body, '$.fields.operation')=?2
           AND json_extract(body, '$.fields.incarnation_id')=?3)
         OR EXISTS(SELECT 1 FROM claims
         WHERE subject=?1 AND kind='runtime.action.succeeded'
           AND json_extract(body, '$.fields.action')='fresh-context'
           AND json_extract(body, '$.fields.operation')=?2
           AND json_extract(body, '$.fields.incarnation_id')=?3)",
        params![actor, operation, incarnation],
        |row| row.get(0),
    )
}

fn enrich_step_wake_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    view.ready_age_ms =
        (view.status == "ready").then(|| snapshot_unix_ms.saturating_sub(view.updated_at_unix_ms));
    let Some(assignee) = view.assigned_to.as_deref() else {
        return Ok(());
    };
    let harness = current_harness_at(connection, assignee, None).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })?;
    let assignee_state = harness
        .as_ref()
        .map(|harness| harness.state.clone())
        .unwrap_or_else(|| "unavailable".into());
    let current_incarnation_id = harness
        .as_ref()
        .map(|harness| harness.incarnation_id.clone())
        .unwrap_or_else(|| "unknown".into());
    let current_incarnation_key =
        hex::encode(Sha256::digest(current_incarnation_id.as_bytes()))[..12].to_owned();
    let mut attempts = Vec::new();
    let wake_tag_prefix = format!(
        "st3-work:{}@{}@{}@",
        view.subject, view.attempt, view.readiness_epoch
    );
    let bare_assignee = assignee
        .strip_prefix("agent/")
        .filter(|suffix| !suffix.contains('/'))
        .unwrap_or(assignee);
    let mut statement = connection.prepare(&canonical_sql(
        "SELECT claims.body, claims.accepted_at_unix_ms,
                EXISTS(SELECT 1 FROM claims consumed
                       WHERE consumed.subject=claims.subject
                         AND consumed.kind IN ('message.read','message.closed')
                         AND consumed.accepted_at_unix_ms<=?1),
                EXISTS(SELECT 1 FROM claims delivered
                       WHERE delivered.subject=claims.subject
                         AND delivered.kind='message.delivered'
                         AND delivered.accepted_at_unix_ms<=?1),
                EXISTS(SELECT 1 FROM claims staged
                       WHERE staged.subject=claims.subject
                         AND staged.kind='message.staged'
                         AND staged.accepted_at_unix_ms<=?1)
         FROM claims INDEXED BY claims_message_to_index
         WHERE claims.kind='message.sent' AND claims.accepted_at_unix_ms<=?1
           AND instr(claims.body, ?2)>0
           AND json_extract(claims.body, '$.fields.to') IN (?3, ?4)
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let rows = statement.query_map(
        params![
            snapshot_unix_ms.to_string(),
            wake_tag_prefix,
            assignee,
            bare_assignee
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
            ))
        },
    )?;
    for row in rows {
        let (body, accepted, consumed, delivered, staged) = row?;
        let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        let fields = body.get("fields").unwrap_or(&body);
        if fields
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| {
                tags.iter()
                    .any(|tag| tag.as_str() == Some("st3-wake-source:manual"))
            })
        {
            continue;
        }
        let matching_incarnation = fields
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find_map(|tag| {
                work_wake_tag_incarnation(tag, &view.subject, view.attempt, view.readiness_epoch)
                    .map(str::to_owned)
            });
        if let Some(incarnation) = matching_incarnation {
            attempts.push((
                accepted.parse::<u128>().unwrap_or(0),
                incarnation,
                consumed,
                delivered,
                staged,
            ));
        }
    }
    let first_attempt = attempts.first().map(|(accepted, ..)| *accepted);
    let last_attempt_at_unix_ms = attempts.last().map(|(accepted, ..)| *accepted);
    let wake_incarnation_key = attempts
        .last()
        .map(|(_, incarnation, ..)| incarnation.as_str())
        .unwrap_or(current_incarnation_key.as_str());
    let wake_incarnation_id = harness_incarnation_for_key_at(
        connection,
        assignee,
        wake_incarnation_key,
        snapshot_unix_ms,
    )?
    .unwrap_or_else(|| {
        if wake_incarnation_key == current_incarnation_key {
            current_incarnation_id.clone()
        } else {
            wake_incarnation_key.to_owned()
        }
    });
    let acknowledged_by = if !attempts.is_empty()
        && matches!(
            view.status.as_str(),
            "claimed" | "working" | "verifying" | "completed" | "failed" | "cancelled"
        ) {
        Some("claim".into())
    } else if attempts
        .iter()
        .any(|(_, incarnation, consumed, _, _)| *consumed && incarnation == wake_incarnation_key)
    {
        Some("consumed".into())
    } else if harness.as_ref().is_some_and(|harness| {
        harness.state == "working" && current_incarnation_key == wake_incarnation_key
    }) && attempts
        .iter()
        .any(|(_, incarnation, _, delivered, staged)| {
            incarnation == wake_incarnation_key
                && (*delivered
                    || *staged
                        && harness.as_ref().is_some_and(|harness| {
                            matches!(harness.driver.as_deref(), Some("pi" | "omp"))
                        }))
        })
    {
        Some("delivery".into())
    } else if first_attempt.is_some_and(|requested| {
        harness.as_ref().is_some_and(|harness| {
            current_incarnation_key == wake_incarnation_key
                && harness.state == "working"
                && harness.observed_at_unix_ms >= requested
        })
    }) {
        Some("turn".into())
    } else {
        None
    };
    let failure = connection
        .query_row(
            &canonical_sql(
                "SELECT body FROM claims
             WHERE subject=?1 AND kind='harness.diagnostic' AND accepted_at_unix_ms<=?2
               AND json_extract(body, '$.fields.code')='work-wake-exhausted'
               AND json_extract(body, '$.fields.step_run')=?3
               AND json_extract(body, '$.fields.incarnation_id')=?4
               AND json_extract(body, '$.fields.attempt')=?5
               AND json_extract(body, '$.fields.readiness_epoch')=?6
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ),
            params![
                assignee,
                snapshot_unix_ms.to_string(),
                view.subject,
                wake_incarnation_id,
                view.attempt,
                view.readiness_epoch
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .and_then(|body| {
            let fields = body.get("fields").unwrap_or(&body);
            (fields.get("status").and_then(Value::as_str) == Some("failed"))
                .then(|| {
                    fields
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        });
    if view.status == "ready" || !attempts.is_empty() || failure.is_some() {
        view.wake = Some(WorkWakeView {
            assignee: assignee.into(),
            assignee_state,
            incarnation_id: wake_incarnation_id,
            attempts: attempts.len().try_into().unwrap_or(u32::MAX),
            last_attempt_at_unix_ms,
            acknowledged_by,
            failure,
        });
    }
    Ok(())
}

fn work_wake_tag_incarnation<'a>(
    tag: &'a str,
    step_subject: &str,
    attempt: u32,
    readiness_epoch: u32,
) -> Option<&'a str> {
    let tag = tag.strip_prefix("st3-work:")?;
    let mut parts = tag.rsplitn(4, '@');
    let incarnation = parts.next()?;
    (parts.next().and_then(|value| value.parse::<u32>().ok()) == Some(readiness_epoch)
        && parts.next().and_then(|value| value.parse::<u32>().ok()) == Some(attempt)
        && parts.next() == Some(step_subject))
    .then_some(incarnation)
}

fn harness_incarnation_for_key_at(
    connection: &Connection,
    subject: &str,
    incarnation_key: &str,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<Option<String>> {
    let mut statement = connection.prepare(&canonical_sql(
        "SELECT body FROM claims
         WHERE subject=?1 AND kind='harness.observed' AND accepted_at_unix_ms<=?2
         ORDER BY CANONICAL_DESC(claims)",
    ))?;
    let rows = statement.query_map(params![subject, snapshot_unix_ms.to_string()], |row| {
        row.get::<_, String>(0)
    })?;
    for row in rows {
        let body = serde_json::from_str::<Value>(&row?).unwrap_or(Value::Null);
        let fields = body.get("fields").unwrap_or(&body);
        let Some(incarnation) = fields.get("incarnation_id").and_then(Value::as_str) else {
            continue;
        };
        let key = hex::encode(Sha256::digest(incarnation.as_bytes()));
        if key.get(..12) == Some(incarnation_key) {
            return Ok(Some(incarnation.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(any(test, feature = "test-support"))]
pub fn step_execution_timing_at(
    connection: &Connection,
    subject: &str,
    attempt: u32,
    snapshot_unix_ms: u128,
    currently_active: bool,
) -> rusqlite::Result<(Option<u128>, u128)> {
    Ok(fold_step_timing(
        &step_timing_events(connection, subject)?,
        attempt,
        snapshot_unix_ms,
        currently_active,
    ))
}

pub fn step_execution_clock_at(
    connection: &Connection,
    subject: &str,
    attempt: u32,
    snapshot_unix_ms: u128,
    currently_active: bool,
) -> rusqlite::Result<StepTiming> {
    Ok(fold_step_clock(
        &step_timing_events(connection, subject)?,
        attempt,
        snapshot_unix_ms,
        currently_active,
    ))
}

/// The lease that bounds the open execution interval of `subject`'s `attempt` as of
/// `snapshot_unix_ms`, as its claims last named it, when an interval is open under one. A
/// quiet renewal moves the step's lease without a claim, never this one.
pub fn step_timing_lease_at(
    connection: &Connection,
    subject: &str,
    attempt: u32,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<Option<u128>> {
    let (started, _, lease) =
        fold_step_timing_open(&step_timing_events(connection, subject)?, attempt, snapshot_unix_ms);
    Ok(started.and(lease))
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static STEP_TIMING_FOLD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static STEP_TIMING_ROWS_PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A step's step and work events in canonical order, as its execution timing folds them.
fn step_timing_events(
    connection: &Connection,
    subject: &str,
) -> rusqlite::Result<Vec<(String, Value, u128)>> {
    #[cfg(any(test, feature = "test-support"))]
    STEP_TIMING_FOLD_CALLS.with(|calls| calls.set(calls.get() + 1));
    let mut statement = connection.prepare(&canonical_sql(
        "SELECT claims.kind, claims.body, claims.accepted_at_unix_ms
         FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1
           AND claims.kind IN ('step-run.state','step-run.carried','work.claimed','work.renewed',
                               'work.progress','work.submitted','work.failed','work.released','work.extended','work.person-asked','work.person-done','work.person-cancelled')
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let events = statement
        .query_map([subject], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    #[cfg(test)]
    STEP_TIMING_ROWS_PARSED.with(|rows| rows.set(rows.get() + events.len()));
    Ok(events
        .into_iter()
        .map(|(kind, body, accepted)| {
            (
                kind,
                serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
                accepted.parse::<u128>().unwrap_or(0),
            )
        })
        .collect())
}

/// A step's execution timing as of one snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepTiming {
    /// The start of the interval that is running now.
    pub started: Option<u128>,
    /// What earlier intervals and the running one have used.
    pub elapsed: u128,
    /// When a declared wait ends on its own, while the clock is stopped for one.
    pub paused_until: Option<u128>,
    /// The time this attempt has spent in declared waits so far, a wait still running included.
    pub waited: u128,
}

/// Close the wait that began at `pause_started` and add what it spent to `waited`.
fn end_wait(
    waited: &mut u128,
    pause_started: &mut Option<u128>,
    paused_until: &mut Option<u128>,
    at: u128,
) {
    if let Some(began) = pause_started.take() {
        *waited = waited.saturating_add(at.saturating_sub(began));
    }
    *paused_until = None;
}

/// Fold a step's execution timing from its step and work events in canonical order: the active
/// interval of `attempt` as of `snapshot_unix_ms`, and the time elapsed in earlier intervals.
/// An interval closes when the next event arrives after its lease expiry. The checkpoint
/// planner replays the same fold, so it keeps every renewal the answer depends on.
pub fn fold_step_timing(
    events: &[(String, Value, u128)],
    attempt: u32,
    snapshot_unix_ms: u128,
    currently_active: bool,
) -> (Option<u128>, u128) {
    let timing = fold_step_clock(events, attempt, snapshot_unix_ms, currently_active);
    (timing.started, timing.elapsed)
}

/// [`fold_step_timing`] with the end of a declared wait. A `work.progress` record with
/// `clock: "paused"` closes the running interval and stops the clock until a record with
/// `clock: "running"` reopens it, or until its `wait_until_unix_ms`, whichever comes first, so a
/// seat that never resumes cannot hide a hung step. Renewals and ordinary progress during a wait
/// keep the lease and leave the clock stopped.
pub fn fold_step_clock(
    events: &[(String, Value, u128)],
    attempt: u32,
    snapshot_unix_ms: u128,
    currently_active: bool,
) -> StepTiming {
    let ClockFold {
        mut started,
        mut elapsed,
        lease_expires,
        paused_until,
        waited,
    } = fold_step_clock_open(events, attempt, snapshot_unix_ms);
    if let Some(interval_start) = started {
        let interval_end =
            lease_expires.map_or(snapshot_unix_ms, |expiry| snapshot_unix_ms.min(expiry));
        let expired = lease_expires.is_some_and(|expiry| expiry <= snapshot_unix_ms);
        if expired || currently_active {
            elapsed = elapsed.saturating_add(interval_end.saturating_sub(interval_start));
        }
        if expired || !currently_active {
            started = None;
        }
    }
    StepTiming {
        started,
        elapsed,
        paused_until: paused_until.filter(|_| currently_active),
        waited,
    }
}

/// [`fold_step_timing`]'s events folded up to `snapshot_unix_ms`: the open interval's start,
/// if any, the time elapsed in closed intervals, and the lease the claims last named.
fn fold_step_timing_open(
    events: &[(String, Value, u128)],
    attempt: u32,
    snapshot_unix_ms: u128,
) -> (Option<u128>, u128, Option<u128>) {
    let fold = fold_step_clock_open(events, attempt, snapshot_unix_ms);
    (fold.started, fold.elapsed, fold.lease_expires)
}

/// The events folded up to the snapshot, before the interval that is running is settled.
struct ClockFold {
    started: Option<u128>,
    elapsed: u128,
    lease_expires: Option<u128>,
    paused_until: Option<u128>,
    waited: u128,
}

fn fold_step_clock_open(
    events: &[(String, Value, u128)],
    attempt: u32,
    snapshot_unix_ms: u128,
) -> ClockFold {
    let mut elapsed = 0_u128;
    let mut started = None;
    let mut lease_expires = None;
    let mut paused_until: Option<u128> = None;
    let mut pause_started: Option<u128> = None;
    let mut waited = 0_u128;
    for (kind, body, accepted) in events {
        let accepted = *accepted;
        if accepted > snapshot_unix_ms {
            break;
        }
        let fields = body.get("fields").unwrap_or(body);
        if kind.starts_with("work.")
            && fields.get("attempt").and_then(Value::as_u64) != Some(u64::from(attempt))
        {
            continue;
        }

        // A wait ends at its deadline whether or not anyone resumed.
        if let Some(deadline) = paused_until
            && accepted > deadline
        {
            started = Some(deadline);
            end_wait(&mut waited, &mut pause_started, &mut paused_until, deadline);
        }
        if let (Some(interval_start), Some(expiry)) = (started, lease_expires)
            && accepted > expiry
        {
            elapsed = elapsed.saturating_add(expiry.saturating_sub(interval_start));
            started = None;
            lease_expires = None;
        }

        match kind.as_str() {
            "step-run.state"
                if fields
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "claimed" | "working")) =>
            {
                // Agentless execution has no `work.claimed` event. Its reconciler-owned
                // `working` transition is therefore the authoritative opening edge. Worker
                // claims may also carry this projection; keeping the earliest edge is
                // idempotent and their following work event supplies the lease. A declared
                // wait keeps the clock stopped.
                if started.is_none() && paused_until.is_none() {
                    started = Some(accepted);
                }
            }
            "work.claimed" => {
                // A new claim ends any wait the previous holder declared.
                end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
                if started.is_none() {
                    started = Some(accepted);
                }
                lease_expires = fields
                    .get("claim_expires_at_unix_ms")
                    .and_then(Value::as_u64)
                    .map(u128::from);
            }
            // A claim carried into a successor generation opens its interval at the cutover.
            "step-run.carried"
                if fields
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "claimed" | "working"))
                    && fields
                        .get("claimant")
                        .is_some_and(|claimant| !claimant.is_null()) =>
            {
                end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
                if started.is_none() {
                    started = Some(accepted);
                }
                lease_expires = fields
                    .get("claim_expires_at_unix_ms")
                    .and_then(Value::as_u64)
                    .map(u128::from);
            }
            "work.renewed" | "work.progress" | "work.extended"
                if started.is_some() || paused_until.is_some() =>
            {
                let clock = if kind == "work.progress" {
                    fields.get("clock").and_then(Value::as_str)
                } else {
                    None
                };
                let wait_until = fields
                    .get("wait_until_unix_ms")
                    .and_then(Value::as_u64)
                    .map(u128::from)
                    .filter(|deadline| *deadline > accepted);
                match (clock, wait_until) {
                    (Some("paused"), Some(deadline)) => {
                        // Declaring again while a wait runs spends the time so far and starts
                        // the next span, so chained declarations draw on one budget.
                        end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
                        pause_started = Some(accepted);
                        if let Some(interval_start) = started {
                            let interval_end =
                                lease_expires.map_or(accepted, |expiry| accepted.min(expiry));
                            elapsed = elapsed
                                .saturating_add(interval_end.saturating_sub(interval_start));
                        }
                        started = None;
                        paused_until = Some(deadline);
                    }
                    (Some("running"), _) if paused_until.is_some() => {
                        end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
                        started = Some(accepted);
                    }
                    _ => {}
                }
                lease_expires = fields
                    .get("claim_expires_at_unix_ms")
                    .and_then(Value::as_u64)
                    .map(u128::from);
            }
            "work.submitted" | "work.failed" | "work.released" => {
                if let Some(interval_start) = started {
                    let interval_end =
                        lease_expires.map_or(accepted, |expiry| accepted.min(expiry));
                    elapsed = elapsed.saturating_add(interval_end.saturating_sub(interval_start));
                }
                started = None;
                lease_expires = None;
                end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
            }
            "step-run.state"
                if fields
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| !matches!(status, "claimed" | "working")) =>
            {
                if let Some(interval_start) = started {
                    let interval_end =
                        lease_expires.map_or(accepted, |expiry| accepted.min(expiry));
                    elapsed = elapsed.saturating_add(interval_end.saturating_sub(interval_start));
                }
                started = None;
                lease_expires = None;
                end_wait(&mut waited, &mut pause_started, &mut paused_until, accepted);
            }
            _ => {}
        }
    }

    if let Some(deadline) = paused_until
        && snapshot_unix_ms > deadline
    {
        started = Some(deadline);
        end_wait(&mut waited, &mut pause_started, &mut paused_until, deadline);
    }
    if let Some(began) = pause_started {
        waited = waited.saturating_add(snapshot_unix_ms.saturating_sub(began));
    }
    ClockFold {
        started,
        elapsed,
        lease_expires,
        paused_until,
        waited,
    }
}

fn apply_effective_step_state(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    let owner = connection
        .query_row(
            "SELECT mission_runs.status, mission_runs.phase,
                    mission_runs.current_generation_id, run_generations.status,
                    root_runs.status, root_runs.phase
             FROM mission_runs JOIN run_generations
               ON run_generations.id=?2 AND run_generations.run_id=mission_runs.id
             JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
             WHERE mission_runs.id=?1",
            params![
                view.run.strip_prefix("mission-run/").unwrap_or(&view.run),
                generation_id_from_subject(&view.generation),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    apply_effective_step_state_with_owner(connection, view, snapshot_unix_ms, owner)
}

type EffectiveStepOwner = (String, String, String, String, String, String);

fn apply_effective_step_state_with_owner(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
    owner: Option<EffectiveStepOwner>,
) -> rusqlite::Result<()> {
    let Some((
        run_status,
        run_phase,
        current_generation,
        generation_status,
        root_status,
        root_phase,
    )) = owner
    else {
        return Ok(());
    };
    if view
        .assigned_to
        .as_deref()
        .is_some_and(|person| person.starts_with("person/"))
    {
        if let Some(ask) = person_work::request(connection, &view.subject)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?
        {
            if matches!(view.status.as_str(), "ready" | "pending")
                && !person_work::current(connection, &ask, snapshot_unix_ms)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?
            {
                view.status = "cancelled".into();
                view.blocked_reason =
                    Some("the requester, originating attempt or owning run ended".into());
            }
        }
    }
    let generation_id = generation_id_from_subject(&view.generation);
    if is_terminal_run_state(&run_status)
        || run_phase == "terminal"
        || is_terminal_generation_state(&generation_status)
        || generation_id != current_generation
        || is_terminal_run_state(&root_status)
        || root_phase == "terminal"
    {
        if !matches!(view.status.as_str(), "completed" | "failed" | "cancelled") {
            view.status = "cancelled".into();
            view.blocked_reason = Some("the owning mission run or generation is terminal".into());
        }
        view.claimant = None;
        view.claim_incarnation = None;
        view.claim_expires_at_unix_ms = None;
        return Ok(());
    }
    if matches!(
        view.status.as_str(),
        "claimed" | "working" | "verifying" | "blocked"
    ) && view
        .claim_expires_at_unix_ms
        .is_some_and(|expiry| expiry <= snapshot_unix_ms)
    {
        view.status = "ready".into();
        view.blocked_reason = Some("the worker lease expired".into());
        // Expiry is a new readiness episode even before a repair writes it. Old
        // consumed wakes must not acknowledge this newly unclaimed work. A claim
        // persists this effective epoch, so later expiries advance it once more.
        view.readiness_epoch = view.readiness_epoch.saturating_add(1);
        view.claimant = None;
        view.claim_incarnation = None;
        view.claim_expires_at_unix_ms = None;
    }
    if view.status == "waiting-person" {
        view.blockers = active_step_blockers_tx(connection, &view.subject, snapshot_unix_ms)?;
        if view.blockers.is_empty() {
            view.status = "ready".into();
            view.blocked_reason = None;
            view.readiness_epoch += 1;
        }
    }
    if view.status == "ready" && view.blocked_reason.is_some() {
        view.blockers = active_step_blockers_tx(connection, &view.subject, snapshot_unix_ms)?;
        if view.blockers.is_empty() {
            view.blocked_reason = None;
        } else {
            view.status = "blocked".into();
            view.claimant = None;
            view.claim_incarnation = None;
            view.claim_expires_at_unix_ms = None;
        }
    }
    Ok(())
}

fn active_step_blockers_tx(
    connection: &Connection,
    subject: &str,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<Vec<String>> {
    let mut query = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
        FROM claims WHERE kind='work.person-asked' AND json_extract(body,'$.fields.origin_step')=?1 ORDER BY CANONICAL_ASC(claims)"))?;
    let asks = query
        .query_map([subject], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut blockers = Vec::new();
    for ask in asks {
        if person_work::current(connection, &ask, snapshot_unix_ms)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?
        {
            blockers.push(ask.subject);
        }
    }
    Ok(blockers)
}

fn planning_session_view_tx(
    connection: &Connection,
    id: &str,
) -> Result<Option<PlanningSessionView>> {
    let row = connection
        .query_row(
            "SELECT mission_id, request_ref, workspace, requester, planner, status, published_revision,
                    target_run_id, source_generation_id, created_at_unix_ms, updated_at_unix_ms, planner_spec_json
             FROM planning_sessions WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        mission,
        request_ref,
        workspace,
        requester,
        planner,
        status,
        published_revision,
        target_run_id,
        source_generation_id,
        created,
        updated,
        planner_spec_json,
    )) = row
    else {
        return Ok(None);
    };
    let mut candidate_statement = connection.prepare(
        "SELECT candidate.variant, candidate.revision, candidate.markdown_ref, candidate.kdl_ref,
                candidate.mission_revision, candidate.submitted_at_unix_ms
         FROM planning_candidates candidate
         JOIN (
           SELECT variant, MAX(revision) revision FROM planning_candidates
           WHERE session_id=?1 GROUP BY variant
         ) latest ON latest.variant=candidate.variant AND latest.revision=candidate.revision
         WHERE candidate.session_id=?1 ORDER BY candidate.variant",
    )?;
    let candidates = candidate_statement
        .query_map([id], |row| {
            let variant: String = row.get(0)?;
            let submitted: String = row.get(5)?;
            Ok(PlanningCandidateView {
                variant,
                revision: row.get(1)?,
                markdown: row.get(2)?,
                kdl: row.get(3)?,
                mission: format!("mission/{mission}"),
                mission_revision: row.get(4)?,
                submitted_at_unix_ms: submitted.parse().unwrap_or(0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut variants = Vec::new();
    for candidate in candidates {
        let preview = connection
            .query_row(
                "SELECT candidate_revision, hash, store_index, graph, diff, mission_response, created_at_unix_ms
                 FROM planning_previews WHERE session_id=?1 AND variant=?2",
                params![id, candidate.variant],
                |row| {
                    let created: String = row.get(6)?;
                    Ok(PlanningPreviewView {
                        variant: candidate.variant.clone(),
                        candidate_revision: row.get(0)?,
                        hash: row.get(1)?,
                        store_index: row.get(2)?,
                        graph: row.get(3)?,
                        diff: row.get(4)?,
                        mission: serde_json::from_str(&row.get::<_, String>(5)?).unwrap(),
                        created_at_unix_ms: created.parse().unwrap_or(0),
                    })
                },
            )
            .optional()?;
        variants.push(PlanningVariantView {
            name: candidate.variant.clone(),
            candidate,
            preview,
        });
    }
    let selected = variants
        .iter()
        .find(|variant| variant.name == "default")
        .or_else(|| variants.first());
    let candidate = selected.map(|variant| variant.candidate.clone());
    let preview = selected.and_then(|variant| variant.preview.clone());
    Ok(Some(PlanningSessionView {
        subject: format!("planning-session/{id}"),
        id: id.to_owned(),
        mission,
        request: request_ref,
        workspace,
        requester,
        planner,
        planner_config: serde_json::from_str(&planner_spec_json)?,
        status,
        target_mission_run: target_run_id.map(|id| format!("mission-run/{id}")),
        source_generation: source_generation_id.map(|id| format!("run-generation/{id}")),
        candidate,
        preview,
        variants,
        published_revision,
        created_at_unix_ms: created.parse().unwrap_or(0),
        updated_at_unix_ms: updated.parse().unwrap_or(0),
    }))
}

fn descendant_mission_run_ids_tx(
    connection: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "WITH RECURSIVE descendant_runs(id) AS (
           SELECT child.id
           FROM mission_runs child
           JOIN step_runs parent_step ON parent_step.subject=child.parent_step_run
           WHERE parent_step.generation_id=?1
           UNION
           SELECT child.id
           FROM mission_runs child
           JOIN step_runs parent_step ON parent_step.subject=child.parent_step_run
           JOIN descendant_runs parent_run ON parent_step.run_id=parent_run.id
         )
         SELECT id FROM descendant_runs ORDER BY id",
    )?;
    statement
        .query_map([generation_id], |row| row.get::<_, String>(0))?
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn terminalize_generation_steps_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    generation_id: &str,
    reason: &str,
    actor: Option<&str>,
    batch_id: Option<&str>,
    now: u128,
) -> Result<Vec<String>, St3Error> {
    let mut statement = transaction
        .prepare(
            "SELECT subject FROM step_runs
             WHERE generation_id=?1 AND status NOT IN ('completed','failed','cancelled')
             ORDER BY subject",
        )
        .map_err(internal)?;
    let subjects = statement
        .query_map([generation_id], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    drop(statement);

    let mut claim_ids = Vec::with_capacity(subjects.len());
    for subject in subjects {
        transaction
            .execute(
                "UPDATE step_runs
                 SET status='cancelled', blocked_reason=?2, lease_owner=NULL,
                     lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                     updated_at_unix_ms=?3
                 WHERE subject=?1",
                params![subject, reason, now.to_string()],
            )
            .map_err(internal)?;
        let claim = append_claim_tx(
            transaction,
            origin,
            &subject,
            "step-run.state",
            actor,
            &json!({"fields": {"status": "cancelled", "reason": reason}}),
            &[],
            batch_id,
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
    }
    Ok(claim_ids)
}

#[allow(clippy::too_many_arguments)]
fn terminalize_run_steps_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    run_id: &str,
    reason: &str,
    actor: Option<&str>,
    batch_id: Option<&str>,
    now: u128,
) -> Result<Vec<String>, St3Error> {
    let mut statement = transaction
        .prepare("SELECT id FROM run_generations WHERE run_id=?1 ORDER BY created_at_unix_ms, id")
        .map_err(internal)?;
    let generations = statement
        .query_map([run_id], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    drop(statement);
    let mut claim_ids = Vec::new();
    for generation in generations {
        claim_ids.extend(terminalize_generation_steps_tx(
            transaction,
            origin,
            &generation,
            reason,
            actor,
            batch_id,
            now,
        )?);
    }
    Ok(claim_ids)
}

fn terminalize_projected_generation_steps_tx(
    transaction: &Transaction<'_>,
    generation_id: &str,
    reason: &str,
    updated_at_unix_ms: u128,
) -> Result<(), St3Error> {
    transaction
        .execute(
            "UPDATE step_runs
             SET status='cancelled', blocked_reason=?2, lease_owner=NULL,
                 lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                 updated_at_unix_ms=?3
             WHERE generation_id=?1
               AND (status NOT IN ('completed','failed','cancelled')
                    OR (status='cancelled' AND blocked_reason=?4))",
            params![
                generation_id,
                reason,
                updated_at_unix_ms.to_string(),
                "the root mission run is terminal"
            ],
        )
        .map_err(internal)?;
    Ok(())
}

fn terminalize_projected_run_steps_tx(
    transaction: &Transaction<'_>,
    run_id: &str,
    reason: &str,
    updated_at_unix_ms: u128,
) -> Result<(), St3Error> {
    transaction
        .execute(
            "UPDATE step_runs
             SET status='cancelled', blocked_reason=?2, lease_owner=NULL,
                 lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                 updated_at_unix_ms=?3
             WHERE run_id=?1
               AND (status NOT IN ('completed','failed','cancelled')
                    OR (status='cancelled' AND blocked_reason=?4))",
            params![
                run_id,
                reason,
                updated_at_unix_ms.to_string(),
                "the root mission run is terminal"
            ],
        )
        .map_err(internal)?;
    Ok(())
}

fn terminalize_projected_run_tree_tx(
    transaction: &Transaction<'_>,
    run_id: &str,
    reason: &str,
    updated_at_unix_ms: u128,
) -> Result<(), St3Error> {
    terminalize_projected_run_steps_tx(transaction, run_id, reason, updated_at_unix_ms)?;
    let generation = transaction
        .query_row(
            "SELECT current_generation_id FROM mission_runs WHERE id=?1",
            [run_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?;
    let Some(generation) = generation else {
        return Ok(());
    };
    transaction
        .execute(
            "UPDATE mission_runs
             SET status=CASE WHEN status IN ('running','standing','blocked') THEN 'cancelled' ELSE status END,
                 phase=CASE WHEN status IN ('running','standing','blocked') THEN 'terminal' ELSE phase END,
                 updated_at_unix_ms=?2
             WHERE id=?1 AND status IN ('running','standing','blocked')",
            params![run_id, updated_at_unix_ms.to_string()],
        )
        .map_err(internal)?;
    transaction
        .execute(
            "UPDATE run_generations
             SET status=CASE WHEN status IN ('running','standing','blocked') THEN 'cancelled' ELSE status END,
                 updated_at_unix_ms=?2
             WHERE id=?1 AND status IN ('running','standing','blocked')",
            params![generation, updated_at_unix_ms.to_string()],
        )
        .map_err(internal)?;
    for descendant in descendant_mission_run_ids_tx(transaction, &generation).map_err(internal)? {
        terminalize_projected_run_steps_tx(transaction, &descendant, reason, updated_at_unix_ms)?;
        let child_generation = transaction
            .query_row(
                "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                [&descendant],
                |row| row.get::<_, String>(0),
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE mission_runs
                 SET status=CASE WHEN status IN ('running','standing','blocked') THEN 'cancelled' ELSE status END,
                     phase=CASE WHEN status IN ('running','standing','blocked') THEN 'terminal' ELSE phase END,
                     updated_at_unix_ms=?2
                 WHERE id=?1 AND status IN ('running','standing','blocked')",
                params![descendant, updated_at_unix_ms.to_string()],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE run_generations
                 SET status=CASE WHEN status IN ('running','standing','blocked') THEN 'cancelled' ELSE status END,
                     updated_at_unix_ms=?2
                 WHERE id=?1 AND status IN ('running','standing','blocked')",
                params![child_generation, updated_at_unix_ms.to_string()],
            )
            .map_err(internal)?;
    }
    Ok(())
}

fn step_owner_is_terminal_tx(
    transaction: &Transaction<'_>,
    subject: &str,
) -> Result<bool, St3Error> {
    let owner = transaction
        .query_row(
            "SELECT mission_runs.status, mission_runs.phase,
                    mission_runs.current_generation_id, step_runs.generation_id,
                    run_generations.status, root_runs.status, root_runs.phase
             FROM step_runs
             JOIN mission_runs ON mission_runs.id=step_runs.run_id
             JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
             JOIN run_generations ON run_generations.id=step_runs.generation_id
             WHERE step_runs.subject=?1",
            [subject],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    Ok(owner.is_some_and(
        |(
            run_status,
            run_phase,
            current_generation,
            generation,
            generation_status,
            root_status,
            root_phase,
        )| {
            is_terminal_run_state(&run_status)
                || run_phase == "terminal"
                || is_terminal_generation_state(&generation_status)
                || current_generation != generation
                || is_terminal_run_state(&root_status)
                || root_phase == "terminal"
        },
    ))
}

fn cancel_mission_run_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    run_subject: &str,
    reason: &str,
    forced_batch: Option<&str>,
    now: u128,
) -> Result<Vec<String>, St3Error> {
    let run_id = run_subject
        .strip_prefix("mission-run/")
        .unwrap_or(run_subject);
    let current = transaction
        .query_row(
            "SELECT status, phase, current_generation_id, mission_id
             FROM mission_runs WHERE id=?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| {
            St3Error::new(
                "missing-mission-run",
                format!("mission run `mission-run/{run_id}` does not exist"),
            )
        })?;
    let (status, phase, generation_id, mission_id) = current;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        let mut claim_ids = cancel_descendant_mission_runs_tx(
            transaction,
            origin,
            &generation_id,
            "daemon/runtime",
            reason,
            now,
        )?;
        claim_ids.extend(terminalize_run_steps_tx(
            transaction,
            origin,
            run_id,
            reason,
            None,
            forced_batch,
            now,
        )?);
        return Ok(claim_ids);
    }
    let descendants =
        descendant_mission_run_ids_tx(transaction, &generation_id).map_err(internal)?;
    let mut claim_ids = Vec::new();
    for descendant in descendants.into_iter().rev() {
        claim_ids.extend(cancel_mission_run_tx(
            transaction,
            origin,
            &format!("mission-run/{descendant}"),
            reason,
            forced_batch,
            now,
        )?);
    }
    let revision: String = transaction
        .query_row(
            "SELECT revision FROM run_generations WHERE id=?1",
            [&generation_id],
            |row| row.get(0),
        )
        .map_err(internal)?;
    let mission: MissionSpec = transaction
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission_id, revision],
            |row| row.get::<_, String>(0),
        )
        .map_err(internal)
        .and_then(|body| serde_json::from_str(&body).map_err(internal))?;
    let final_paths = flatten_mission_step_specs(&mission)
        .into_iter()
        .filter(|step| step.finally)
        .map(|step| step.path.clone())
        .collect::<BTreeSet<_>>();
    let mut statement = transaction
        .prepare(
            "SELECT subject, step_path, lease_owner FROM step_runs
             WHERE generation_id=?1 AND status NOT IN ('completed','failed','cancelled')
             ORDER BY subject",
        )
        .map_err(internal)?;
    let steps = statement
        .query_map([&generation_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    drop(statement);
    for (subject, path, claimant) in steps {
        if final_paths.contains(&path) {
            continue;
        }
        if let Some(claimant) = claimant {
            let stable = canonical_hash(&(run_id, &generation_id, &subject, "cancelled"))
                .map_err(internal)?;
            append_claim_tx(
                transaction,
                origin,
                &format!("message/mission-cancelled-{}", &stable[..20]),
                "message.sent",
                None,
                &json!({"fields": {
                    "from": "daemon/runtime",
                    "to": claimant,
                    "title": "Mission work cancelled",
                    "content": format!("Mission run mission-run/{run_id} cancelled step {subject}. Stop this work. Reason: {reason}"),
                    "status": "sent",
                    "tags": [format!("mission-run:mission-run/{run_id}"), format!("step-run:{subject}")],
                }}),
                &[],
                forced_batch,
            )
            .map_err(internal)?;
        }
        transaction
            .execute(
                "UPDATE step_runs
                 SET status='cancelled', blocked_reason=?2, lease_owner=NULL,
                     lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                     updated_at_unix_ms=?3
                 WHERE subject=?1",
                params![subject, reason, now.to_string()],
            )
            .map_err(internal)?;
        let claim = append_claim_tx(
            transaction,
            origin,
            &subject,
            "step-run.state",
            None,
            &json!({"fields": {"status": "cancelled", "reason": reason}}),
            &[],
            forced_batch,
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
    }
    let (next_status, next_phase) = if final_paths.is_empty() {
        ("running", "cleanup-cancelled")
    } else {
        ("running", "final-cancelled")
    };
    transaction
        .execute(
            "UPDATE mission_runs SET status=?2, phase=?3, updated_at_unix_ms=?4 WHERE id=?1",
            params![run_id, next_status, next_phase, now.to_string()],
        )
        .map_err(internal)?;
    transaction
        .execute(
            "UPDATE run_generations SET status=?2, updated_at_unix_ms=?3 WHERE id=?1",
            params![generation_id, next_status, now.to_string()],
        )
        .map_err(internal)?;
    let body = json!({"fields": {
        "status": next_status,
        "phase": next_phase,
        "reason": reason,
        "previous_phase": phase,
    }});
    for (subject, kind) in [
        (format!("mission-run/{run_id}"), "mission-run.state"),
        (
            format!("run-generation/{generation_id}"),
            "run-generation.state",
        ),
    ] {
        let claim = append_claim_tx(
            transaction,
            origin,
            &subject,
            kind,
            None,
            &body,
            &[],
            forced_batch,
        )
        .map_err(internal)?;
        claim_ids.push(claim.id);
    }
    Ok(claim_ids)
}

fn cancel_descendant_mission_runs_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    generation_id: &str,
    actor: &str,
    reason: &str,
    now: u128,
) -> Result<Vec<String>, St3Error> {
    let mut claim_ids = Vec::new();
    for run_id in descendant_mission_run_ids_tx(transaction, generation_id).map_err(internal)? {
        let (status, child_generation): (String, String) = transaction
            .query_row(
                "SELECT status, current_generation_id FROM mission_runs WHERE id=?1",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(internal)?;
        claim_ids.extend(terminalize_run_steps_tx(
            transaction,
            origin,
            &run_id,
            reason,
            Some(actor),
            None,
            now,
        )?);
        if !matches!(status.as_str(), "running" | "standing" | "blocked") {
            continue;
        }
        transaction
            .execute(
                "UPDATE mission_runs
                 SET status='cancelled', phase='terminal', updated_at_unix_ms=?2
                 WHERE id=?1",
                params![run_id, now.to_string()],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE run_generations
                 SET status='cancelled', updated_at_unix_ms=?2 WHERE id=?1",
                params![child_generation, now.to_string()],
            )
            .map_err(internal)?;
        let body = json!({"fields": {
            "status": "cancelled",
            "phase": "terminal",
            "reason": reason,
        }});
        let run_claim = append_claim_tx(
            transaction,
            origin,
            &format!("mission-run/{run_id}"),
            "mission-run.state",
            Some(actor),
            &body,
            &[],
            None,
        )
        .map_err(internal)?;
        claim_ids.push(run_claim.id);
        let generation_claim = append_claim_tx(
            transaction,
            origin,
            &format!("run-generation/{child_generation}"),
            "run-generation.state",
            Some(actor),
            &body,
            &[],
            None,
        )
        .map_err(internal)?;
        claim_ids.push(generation_claim.id);
    }
    Ok(claim_ids)
}

fn mission_run_view_tx(connection: &Connection, run_id: &str) -> rusqlite::Result<MissionRunView> {
    mission_run_view_with_enrichment_tx(connection, run_id, true)
}

fn mission_run_view_for_reconcile_tx(
    connection: &Connection,
    run_id: &str,
) -> rusqlite::Result<MissionRunView> {
    mission_run_view_with_enrichment_tx(connection, run_id, false)
}

/// Replay only needs effective predecessor states and run variables. Presentation
/// enrichment re-parses the complete mission and wake history for every step.
fn mission_run_view_for_projection_tx(
    connection: &Connection,
    run_id: &str,
) -> rusqlite::Result<MissionRunView> {
    let mut view = mission_run_header_tx(connection, run_id)?;
    let mut statement = connection.prepare(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs WHERE generation_id=?1 ORDER BY created_at_unix_ms, step_path",
    )?;
    view.steps = statement
        .query_map(
            [generation_id_from_subject(&view.generation)],
            step_run_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot_unix_ms = now_ms();
    for step in &mut view.steps {
        apply_effective_step_state(connection, step, snapshot_unix_ms)?;
    }
    Ok(view)
}

fn first_readiness_fault_tx(
    connection: &Connection,
    subject: &str,
) -> rusqlite::Result<Option<String>> {
    let body: Option<String> = connection
        .query_row(
            &canonical_sql(
                "SELECT body FROM claims WHERE subject=?1 AND kind='reconcile.fault'
            AND json_extract(body, '$.fields.scope')='scheduler/first-readiness'
            ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ),
            [subject],
            |row| row.get(0),
        )
        .optional()?;
    Ok(body
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .and_then(|body| {
            (body["fields"]["status"] == "faulted")
                .then(|| body["fields"]["reason"].as_str().map(str::to_owned))
                .flatten()
        }))
}

fn mission_run_view_with_enrichment_tx(
    connection: &Connection,
    run_id: &str,
    presentation: bool,
) -> rusqlite::Result<MissionRunView> {
    let enrichment = if presentation {
        MissionRunEnrichment::Presentation
    } else {
        MissionRunEnrichment::Reconcile
    };
    mission_run_view_and_definition_tx(connection, run_id, enrichment).map(|(view, _)| view)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MissionRunEnrichment {
    Presentation,
    Reconcile,
    Evaluation,
}

fn mission_run_view_and_definition_tx(
    connection: &Connection,
    run_id: &str,
    enrichment: MissionRunEnrichment,
) -> rusqlite::Result<(MissionRunView, Option<MissionSpec>)> {
    let presentation = enrichment == MissionRunEnrichment::Presentation;
    let mut view = mission_run_header_tx(connection, run_id)?;
    let mut statement = connection.prepare(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs WHERE generation_id=?1 ORDER BY created_at_unix_ms, step_path",
    )?;
    view.steps = statement
        .query_map(
            [generation_id_from_subject(&view.generation)],
            step_run_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mission = if presentation || view.steps.is_empty() {
        None
    } else {
        pinned_generation_mission_tx(connection, &view.generation)?
    };
    let mut definitions = PinnedDefinitions::default();
    for step in &mut view.steps {
        if presentation {
            enrich_step_queue_cached(connection, step, now_ms(), &mut definitions)?;
        } else {
            let snapshot_unix_ms = now_ms();
            apply_effective_step_state(connection, step, snapshot_unix_ms)?;
            if let Some(mission) = &mission {
                apply_step_definition(step, mission);
            }
            adhoc_work::enrich_handoff(connection, step, snapshot_unix_ms)?;
            person_work::enrich_responses(connection, step)?;
            // Untimed inactive steps cannot time out or have a clock-driven wake. Active
            // steps still need their wait deadline for `incremental::step_due`, even without
            // a timeout. A missing definition is not proof that the step is untimed.
            if enrichment != MissionRunEnrichment::Evaluation
                || mission.is_none()
                || step.timeout_ms.is_some()
                || matches!(step.status.as_str(), "claimed" | "working")
            {
                let timing = step_execution_clock_at(
                    connection,
                    &step.subject,
                    step.attempt,
                    snapshot_unix_ms,
                    matches!(step.status.as_str(), "claimed" | "working"),
                )?;
                step.execution_started_at_unix_ms = timing.started;
                step.execution_paused_until_unix_ms = timing.paused_until;
                step.execution_waited_ms = timing.waited;
                step.execution_elapsed_ms = timing.elapsed;
                step.timeout_extension_ms = step_timeout_extension_at(
                    connection, &step.subject, step.attempt, snapshot_unix_ms,
                )?;
            }
        }
    }
    view.provenance = crate::provenance::read(connection, &view.mission, &view.revision)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
    view.loops = loop_run_views_tx(connection, &view)?;
    view.outcome = mission_run_outcome_tx(connection, &view)?;
    if presentation {
        view.scheduler_fault = first_readiness_fault_tx(connection, &view.subject)?;
    }
    Ok((view, mission))
}

/// The outcome someone set on a finished run: its latest state claim when that claim moved it
/// from one terminal state to another, together with the state it replaced.
pub fn mission_run_outcome_tx(
    connection: &Connection,
    run: &MissionRunView,
) -> rusqlite::Result<Option<MissionRunOutcomeView>> {
    if run.phase != "terminal" {
        return Ok(None);
    }
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='mission-run.state'
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 2"
    ))?;
    let claims = statement
        .query_map([&run.subject], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(claims
        .first()
        .and_then(|latest| run_outcome_from_claim(latest, claims.get(1))))
}

fn run_outcome_from_claim(
    claim: &ClaimRecord,
    previous: Option<&ClaimRecord>,
) -> Option<MissionRunOutcomeView> {
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let text =
        |fields: &Value, name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
    if text(fields, "phase").as_deref() != Some("terminal")
        || text(fields, "previous_phase").as_deref() != Some("terminal")
    {
        return None;
    }
    Some(MissionRunOutcomeView {
        status: text(fields, "status")?,
        // `mission-run.state` claims are durable, so the claim before is the state replaced.
        previous_status: previous.and_then(|previous| {
            text(
                previous.body.get("fields").unwrap_or(&previous.body),
                "status",
            )
        }),
        reason: text(fields, "reason").unwrap_or_default(),
        actor: claim.actor.clone()?,
        at_unix_ms: claim.accepted_at_unix_ms,
    })
}

const MISSION_RUN_HEADER_SELECT: &str =
        "SELECT mission_runs.id, mission_runs.mission_id, mission_runs.initial_revision,
                mission_runs.current_generation_id, run_generations.revision,
                mission_runs.root_revision, mission_runs.root_run_id, mission_runs.parent_step_run,
                mission_runs.workspace, mission_runs.requester, mission_runs.inputs, mission_runs.mode,
                mission_run_deadlines.timeout_ms, mission_run_deadlines.deadline_at_unix_ms,
                mission_runs.status, mission_runs.phase, mission_runs.created_at_unix_ms,
                mission_runs.updated_at_unix_ms, mission_run_after.after_run
         FROM mission_runs JOIN run_generations
           ON run_generations.id=mission_runs.current_generation_id
         LEFT JOIN mission_run_deadlines ON mission_run_deadlines.run_id=mission_runs.id
         LEFT JOIN mission_run_after ON mission_run_after.run_id=mission_runs.id
         ORDER BY mission_runs.created_at_unix_ms, mission_runs.id";

/// The headers of the runs still running, standing or blocked, read through the open runs index.
fn open_mission_run_headers_query() -> String {
    MISSION_RUN_HEADER_SELECT.replace(
        "ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
        &format!(
            "WHERE {OPEN_MISSION_RUN}
               AND mission_runs.status IN ('running','standing','blocked')
             ORDER BY mission_runs.created_at_unix_ms, mission_runs.id"
        ),
    )
}

pub fn mission_run_header_tx(
    connection: &Connection,
    run_id: &str,
) -> rusqlite::Result<MissionRunView> {
    let query = MISSION_RUN_HEADER_SELECT.replace(
        "ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
        "WHERE mission_runs.id=?1",
    );
    connection.query_row(&query, [run_id], mission_run_header_from_row)
}

fn mission_run_header_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MissionRunView> {
    let id: String = row.get(0)?;
    let generation_id: String = row.get(3)?;
    let root_run_id: String = row.get(6)?;
    let deadline: Option<String> = row.get(13)?;
    let created: String = row.get(16)?;
    let updated: String = row.get(17)?;
    Ok(MissionRunView {
        provenance: None,
        subject: format!("mission-run/{id}"),
        id,
        mission: format!("mission/{}", row.get::<_, String>(1)?),
        generation: format!("run-generation/{generation_id}"),
        initial_revision: row.get(2)?,
        revision: row.get(4)?,
        root_revision: row.get(5)?,
        root_mission_run: format!("mission-run/{root_run_id}"),
        parent_step_run: row.get(7)?,
        workspace: row.get(8)?,
        requester: row.get(9)?,
        inputs: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
        mode: row.get(11)?,
        timeout_ms: row.get(12)?,
        deadline_at_unix_ms: deadline.and_then(|value| value.parse().ok()),
        after: row.get(18)?,
        status: row.get(14)?,
        phase: row.get(15)?,
        outcome: None,
        created_at_unix_ms: created.parse().unwrap_or(0),
        updated_at_unix_ms: updated.parse().unwrap_or(0),
        steps: Vec::new(),
        loops: Vec::new(),
        stuck_gates: Vec::new(),
        scheduler_fault: None,
    })
}

pub fn loop_run_views_tx(
    connection: &Connection,
    run: &MissionRunView,
) -> rusqlite::Result<Vec<LoopRunView>> {
    let mission_id = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
    let body = connection
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission_id, run.revision],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(mission) = body.and_then(|body| serde_json::from_str::<MissionSpec>(&body).ok())
    else {
        return Ok(Vec::new());
    };
    let generation = generation_id_from_subject(&run.generation);
    let mut loops = Vec::new();
    for step_id in &mission.display_order {
        let Some(step) = mission.steps.get(step_id) else {
            continue;
        };
        let Some(spec) = step.loop_spec.as_deref() else {
            continue;
        };
        let subject = format!("loop-run/{generation}/{}", spec.path);
        let state = connection
            .query_row(
                &canonical_sql("SELECT body FROM claims WHERE subject=?1 AND kind='loop.state' ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                [&subject],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .unwrap_or(Value::Null);
        let fields = state.get("fields").unwrap_or(&state);
        let step_view = run.steps.iter().find(|view| view.step == spec.path);
        let status = fields
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| step_view.map(|view| view.status.as_str()))
            .unwrap_or("pending")
            .to_owned();
        let mut results_statement = connection.prepare(&canonical_sql(
            "SELECT id, body, accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='loop.round-result' ORDER BY CANONICAL_ASC(claims)",
        ))?;
        let results = results_statement
            .query_map([&subject], |row| {
                let body =
                    serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap_or(Value::Null);
                let fields = body.get("fields").unwrap_or(&body);
                let metrics = fields
                    .get("metrics")
                    .and_then(Value::as_object)
                    .map(|metrics| {
                        metrics
                            .iter()
                            .filter_map(|(name, value)| {
                                value.as_f64().map(|value| (name.clone(), value))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let recorded: String = row.get(2)?;
                Ok(LoopRoundView {
                    claim: row.get(0)?,
                    round: fields.get("round").and_then(Value::as_u64).unwrap_or(0) as u32,
                    status: fields
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    mission_run: fields
                        .get("mission_run")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    metrics,
                    feedback: fields
                        .get("feedback")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    candidate: fields
                        .get("candidate")
                        .and_then(Value::as_u64)
                        .map(|value| value as u32),
                    item: fields.get("item").cloned(),
                    reason: fields
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    token_usage: fields
                        .get("token_usage")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    recorded_at_unix_ms: recorded.parse().unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let (mode, max_parallel, candidate_count) = if let Some(candidates) = &spec.candidates {
            (
                "best-of-n".to_owned(),
                Some(candidates.max_parallel),
                Some(candidates.count),
            )
        } else if let Some(for_each) = &spec.for_each {
            ("for-each".to_owned(), Some(for_each.max_parallel), None)
        } else {
            ("rounds".to_owned(), None, None)
        };
        let best_metrics = fields
            .get("best_metrics")
            .and_then(Value::as_object)
            .map(|metrics| {
                metrics
                    .iter()
                    .filter_map(|(name, value)| value.as_f64().map(|value| (name.clone(), value)))
                    .collect()
            })
            .unwrap_or_default();
        loops.push(LoopRunView {
            subject: subject.clone(),
            id: spec.id.clone(),
            path: spec.path.clone(),
            step_run: format!("step-run/{generation}/{}", spec.path),
            mode,
            status,
            round: fields.get("round").and_then(Value::as_u64).unwrap_or(0) as u32,
            max_rounds: spec.max_rounds,
            timeout_ms: spec.timeout_ms,
            max_parallel,
            item_count: fields.get("items").and_then(Value::as_array).map(Vec::len),
            candidate_count,
            best_round: fields
                .get("best_round")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            best_metrics,
            feedback: fields
                .get("feedback")
                .and_then(Value::as_str)
                .map(str::to_owned),
            winner: fields
                .get("winner")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            reason: fields
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            results,
        });
    }
    Ok(loops)
}

fn revision_proposal_view_tx(
    connection: &Connection,
    proposal_id: &str,
) -> rusqlite::Result<RevisionProposalView> {
    connection.query_row(
        "SELECT id, run_id, source_generation_id, candidate_revision, actor, reason, status,
                cutover, compatible_steps, reviewers, approvals, preview_hash,
                successor_generation_id, created_at_unix_ms, updated_at_unix_ms
         FROM revision_proposals WHERE id=?1",
        [proposal_id],
        |row| {
            let id: String = row.get(0)?;
            let run_id: String = row.get(1)?;
            let source: String = row.get(2)?;
            let successor: Option<String> = row.get(12)?;
            let created: String = row.get(13)?;
            let updated: String = row.get(14)?;
            Ok(RevisionProposalView {
                subject: format!("revision-proposal/{id}"),
                id,
                run: format!("mission-run/{run_id}"),
                source_generation: format!("run-generation/{source}"),
                candidate_revision: row.get(3)?,
                actor: row.get(4)?,
                reason: row.get(5)?,
                status: row.get(6)?,
                cutover: match row.get::<_, String>(7)?.as_str() {
                    "when-idle" => RevisionCutover::WhenIdle,
                    _ => RevisionCutover::RestartActive,
                },
                compatible_steps: serde_json::from_str(&row.get::<_, String>(8)?)
                    .unwrap_or_default(),
                reviewers: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
                approvals: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
                preview_hash: row.get(11)?,
                successor_generation: successor.map(|id| format!("run-generation/{id}")),
                created_at_unix_ms: created.parse().unwrap_or(0),
                updated_at_unix_ms: updated.parse().unwrap_or(0),
            })
        },
    )
}

fn revision_cutover_name(cutover: &RevisionCutover) -> &'static str {
    match cutover {
        RevisionCutover::RestartActive => "restart-active",
        RevisionCutover::WhenIdle => "when-idle",
    }
}

pub fn generation_id_from_subject(subject: &str) -> &str {
    subject.strip_prefix("run-generation/").unwrap_or(subject)
}

#[cfg(any(test, feature = "test-support"))]
pub fn expire_work_lease_by_claim(store: &Store, subject: &str, actor: &str, incarnation: &str) {
    let current = store.step_run(subject).unwrap().unwrap();
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    let claim = append_claim_tx(
        &transaction,
        &store.origin,
        subject,
        "work.renewed",
        Some(actor),
        &json!({"fields": {
            "attempt": current.attempt,
            "status": current.status,
            "worker_reported": current.worker_reported,
            "claimant": actor,
            "claim_incarnation": incarnation,
            "claim_expires_at_unix_ms": 0,
            "readiness_epoch": current.readiness_epoch,
        }}),
        &[],
        None,
    )
    .unwrap();
    project_mission_run_update(&transaction, &claim).unwrap();
    transaction.commit().unwrap();
}

#[cfg(test)]
pub mod tests;

/// The projected tables the graph digest commits: digest label, table, digested columns in
/// order, and row order. Triggers built from the same list bump `graph_generation` whenever one
/// of these columns changes.
const GRAPH_DIGEST_TABLES: [smallclaims::store::runtime::LegacyDigestTable; 6] = [
    (
        "desired",
        "desired",
        &[
            "subject",
            "kind",
            "revision",
            "claim_id",
            "body",
            "member",
            "owner_run",
            "owner_generation",
            "owner_step",
        ],
        "subject",
    ),
    (
        "missions",
        "mission_definitions",
        &["mission_id", "revision", "state", "claim_id"],
        "mission_id",
    ),
    (
        "runs",
        "mission_runs",
        &[
            "id",
            "mission_id",
            "current_generation_id",
            "status",
            "phase",
        ],
        "id",
    ),
    (
        "generations",
        "run_generations",
        &["id", "run_id", "revision", "predecessor_id", "status"],
        "id",
    ),
    (
        "steps",
        "step_runs",
        &[
            "subject",
            "generation_id",
            "step_path",
            "status",
            "attempt",
            "lease_owner",
            "blocked_reason",
        ],
        "subject",
    ),
    (
        "proposals",
        "revision_proposals",
        &[
            "id",
            "run_id",
            "source_generation_id",
            "candidate_revision",
            "status",
            "approvals",
            "successor_generation_id",
        ],
        "id",
    ),
];

#[cfg(any(test, feature = "test-support"))]
impl Store {
    #[cfg(any(test, feature = "test-support"))]
    pub fn import_replication(
        &self,
        relay: &str,
        input: &ReplicationBatch,
    ) -> Result<ReplicationResponse, St3Error> {
        if relay != input.peer {
            return Err(St3Error::new(
                "peer-label-mismatch",
                "the configured relay label does not match the batch label",
            ));
        }
        for (hash, bytes) in &input.blobs {
            if hex::encode(Sha256::digest(bytes)) != *hash {
                return Err(St3Error::new(
                    "blob-hash-mismatch",
                    format!("replicated blob `{hash}` failed verification"),
                ));
            }
        }
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        for (hash, bytes) in &input.blobs {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO blobs(hash, bytes, size) VALUES (?1, ?2, ?3)",
                    params![hash, bytes, bytes.len() as u64],
                )
                .map_err(internal)?;
        }
        let mut missing_ranges = Vec::new();
        let mut blocked_origins = BTreeSet::new();
        for batch in &input.batches {
            if blocked_origins.contains(&batch.origin) {
                continue;
            }
            let accepted_through = transaction
                .query_row(
                    "SELECT MAX(replica_sequence) FROM batches WHERE origin=?1",
                    [&batch.origin],
                    |row| row.get::<_, Option<u64>>(0),
                )
                .map_err(internal)?
                .unwrap_or(0);
            if batch.replica_sequence <= accepted_through {
                let stored_hash = transaction
                    .query_row(
                        "SELECT hash FROM batches WHERE origin=?1 AND replica_sequence=?2",
                        params![batch.origin, batch.replica_sequence],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if stored_hash.as_deref() != Some(batch.hash.as_str()) {
                    return Err(St3Error::new(
                        "replica-sequence-conflict",
                        format!(
                            "replica `{}` sequence {} has another hash",
                            batch.origin, batch.replica_sequence
                        ),
                    ));
                }
                continue;
            }
            if batch.replica_sequence != accepted_through.saturating_add(1) {
                missing_ranges.push(ReplicaRange {
                    origin: batch.origin.clone(),
                    from: accepted_through.saturating_add(1),
                    through: batch.replica_sequence.saturating_sub(1),
                });
                blocked_origins.insert(batch.origin.clone());
                continue;
            }
            let expected_previous = transaction
                .query_row(
                    "SELECT hash FROM batches WHERE origin=?1 ORDER BY replica_sequence DESC LIMIT 1",
                    [&batch.origin],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?;
            if expected_previous != batch.previous_hash {
                return Err(St3Error::new(
                    "broken-replica-chain",
                    format!(
                        "replica sequence {} does not cite the current chain head",
                        batch.replica_sequence
                    ),
                ));
            }
            verify_replica_batch(batch)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![batch.id, batch.origin, batch.replica_sequence, batch.previous_hash, batch.hash, batch.accepted_at_unix_ms.to_string()],
                )
                .map_err(internal)?;
            for claim in &batch.claims {
                ensure_claim_blobs(&transaction, claim)?;
                let inserted = transaction
                    .prepare_cached(
                        "INSERT OR IGNORE INTO claims(id, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )
                    .map_err(internal)?
                    .execute(params![
                        claim.id,
                        claim.batch_id,
                        claim.subject,
                        claim.kind,
                        claim.origin,
                        claim.actor,
                        canonical_json_text(&claim.body).map_err(internal)?,
                        serde_json::to_string(&claim.predecessors).map_err(internal)?,
                        claim.accepted_at_unix_ms.to_string(),
                    ])
                    .map_err(internal)?;
                if inserted != 0 {
                    let index = transaction.last_insert_rowid() as u64;
                    let effective = register_operation_tx(&transaction, claim).map_err(internal)?;
                    if effective {
                        insert_event(
                            &transaction,
                            index,
                            &claim.kind,
                            &claim.subject,
                            &claim.body,
                        )
                        .map_err(internal)?;
                    }
                    if effective && claim.kind == "intent.desired" {
                        if let Ok(desired) =
                            serde_json::from_value::<DesiredSubject>(claim.body.clone())
                        {
                            select_replicated_desired(&transaction, claim, &desired)?;
                        }
                    } else if effective && claim.kind == retirements::KIND {
                        retirements::project_subject(&transaction, &claim.subject)?;
                    } else if effective && claim.kind == "doc.bound" {
                        select_replicated_document(&transaction, claim, index)?;
                    } else if effective && claim.kind == "mission.published" {
                        select_replicated_mission(&transaction, claim, index)?;
                    }
                }
            }
            transaction
                .execute(
                    "INSERT INTO peer_replica_cursors(peer, origin, accepted_through) VALUES (?1, ?2, ?3)
                     ON CONFLICT(peer, origin) DO UPDATE SET accepted_through=MAX(accepted_through, excluded.accepted_through)",
                    params![relay, batch.origin, batch.replica_sequence],
                )
                .map_err(internal)?;
        }
        rebuild_operations_tx(&transaction).map_err(internal)?;
        owned_sets::project_tx(&transaction)?;
        project_replicated_mission_runs(&transaction)?;
        rebuild_planning_tx(&transaction).map_err(internal)?;
        custom::flush(&transaction).map_err(internal)?;
        let accepted_heads = replica_heads(&transaction).map_err(internal)?;
        let accepted_through = accepted_heads.get(&input.peer).copied().unwrap_or(0);
        let missing_sequences = missing_ranges
            .iter()
            .filter(|range| range.origin == input.peer)
            .flat_map(|range| range.from..=range.through)
            .collect::<Vec<_>>();
        transaction
            .execute(
                "INSERT INTO peer_cursors(peer, accepted_through) VALUES (?1, ?2)
                 ON CONFLICT(peer) DO UPDATE SET accepted_through=excluded.accepted_through",
                params![relay, accepted_through],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(ReplicationResponse {
            accepted_through,
            missing_sequences,
            accepted_heads,
            missing_ranges,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn verify_replica_batch(batch: &ReplicaBatch) -> Result<(), St3Error> {
    verify_replica_batch_header(batch)?;
    for claim in &batch.claims {
        if claim.batch_id != batch.id || claim.origin != batch.origin {
            return Err(St3Error::new(
                "claim-batch-mismatch",
                format!(
                    "replicated claim `{}` names another batch or origin",
                    claim.id
                ),
            ));
        }
        if !claim_id_is_content_hash(claim).map_err(internal)? {
            return Err(St3Error::new(
                "claim-hash-mismatch",
                format!("replicated claim `{}` failed verification", claim.id),
            ));
        }
        if known_replicated_claim_kind(&claim.kind) {
            let fields = schema_fields_for_body(&claim.kind, &claim.body).map_err(|error| {
                St3Error::new(
                    "invalid-replicated-claim",
                    format!(
                        "replicated claim `{}` has an invalid body: {error}",
                        claim.id
                    ),
                )
            })?;
            st3_schema::registry()
                .validate_claim(&claim.subject, &claim.kind, &fields)
                .map_err(|error| {
                    St3Error::new(
                        "invalid-replicated-claim",
                        format!(
                            "replicated claim `{}` violates {}: {}",
                            claim.id, error.code, error.message
                        ),
                    )
                })?;
        }
    }
    Ok(())
}

fn check_observer_completion(
    transaction: &Transaction<'_>,
    observer: &str,
    desired_revision: &str,
    current: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<(), St3Error> {
    if current.is_some_and(|current| !current()) {
        return Err(St3Error::new(
            "observer-operation-retired",
            format!("observer `{observer}` operation retired before its completion was recorded"),
        ));
    }
    let selected = current_desired_row_tx(transaction, observer).map_err(internal)?;
    if selected.as_ref().map(|desired| desired.revision.as_str()) != Some(desired_revision) {
        return Err(St3Error::new(
            "stale-observer-revision",
            format!("observer `{observer}` changed before its observation completed"),
        ));
    }
    Ok(())
}

pub fn append_claim_fenced_outcome(
    graph: &GraphStore,
    input: &ClaimInput,
    fence: Option<&crate::mailbox::Fence>,
) -> Result<(ClaimRecord, bool), St3Error> {
    append_claim_with_fences(graph, input, fence, None)
}

fn append_claim_with_fences(
    graph: &GraphStore,
    input: &ClaimInput,
    fence: Option<&crate::mailbox::Fence>,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool), St3Error> {
    append_claim_with_subject_fences(graph, input, fence, event_runtime, None, None)
}

fn append_claim_with_subject_fences(
    graph: &GraphStore,
    input: &ClaimInput,
    fence: Option<&crate::mailbox::Fence>,
    event_runtime: Option<&str>,
    expected_subjects: Option<&BTreeMap<String, String>>,
    observer_completion: Option<(&str, &str, &(dyn Fn() -> bool + Sync))>,
) -> Result<(ClaimRecord, bool), St3Error> {
    append_claim_with_commit_context(graph, input, fence, event_runtime, expected_subjects, observer_completion, Default::default())
}

type ClaimAdmission<'a> = dyn Fn(&Connection) -> Result<(), St3Error> + Sync + 'a;

#[derive(Default)]
struct ClaimCommitContext<'a> {
    signature: Option<&'a str>,
    // Only retire_agent supplies the declaration, owner and rollout admission together.
    retirement_writer: bool,
    admission: Option<&'a ClaimAdmission<'a>>,
    assistant: Option<&'a assistant_write::AssistantWriteAuthorization>,
}

fn append_claim_with_commit_context(
    graph: &GraphStore,
    input: &ClaimInput,
    fence: Option<&crate::mailbox::Fence>,
    event_runtime: Option<&str>,
    expected_subjects: Option<&BTreeMap<String, String>>,
    observer_completion: Option<(&str, &str, &(dyn Fn() -> bool + Sync))>,
    context: ClaimCommitContext<'_>,
) -> Result<(ClaimRecord, bool), St3Error> {
    let admission = |connection: &Connection| {
        if input.kind == retirements::KIND && !context.retirement_writer {
            return Err(retirements::writer_required());
        }
        if let Some(admission) = context.admission {
            admission(connection)?;
        }
        if input.kind == "message.sent" {
            retirements::admit_recipient(
                connection,
                input.fields.get("to").and_then(Value::as_str),
            )?;
        }
        Ok(())
    };
    if context.assistant.is_none() {
        assistant_write::refuse_unverified(&input.kind, input.actor.as_deref(),
            input.fields.get("from").and_then(Value::as_str),
            ["sender_class", "assistant_grant_id", "assistant_owner", "assistant_session_actor"].iter().any(|key| input.fields.contains_key(*key)))?;
    }
    validate_claim_input(input)?;
    if local_retention(&input.kind)
        || (input.actor.is_none() && system_local_retention(&input.kind))
    {
        return append_local_observation_fenced(graph, input, event_runtime);
    }
    if latest_retention(&input.kind) {
        return append_latest_observation_fenced(graph, input, now_ms(), event_runtime);
    }
    // One claim in a savepoint of the writer's next batch; its caller hears back once that
    // batch commits.
    graph.connection
        .batched(|transaction| -> Result<(ClaimRecord, bool), St3Error> {
            let mut assistant_input;
            let input = if let Some(authorization) = context.assistant {
                let grant_id = assistant_write::authorize_grant(transaction, &graph.origin, authorization)?;
                assistant_input = input.clone();
                assistant_input.fields.insert("sender_class".into(), json!("external-assistant"));
                assistant_input.fields.insert("assistant_grant_id".into(), json!(grant_id));
                assistant_input.fields.insert("assistant_owner".into(), json!(authorization.owner));
                assistant_input.fields.insert("assistant_session_actor".into(), json!(authorization.actor));
                &assistant_input
            } else { input };
            let operation = claim_operation(input)?;
            if let Some((subject, revision, current)) = observer_completion {
                check_observer_completion(transaction, subject, revision, Some(current))?;
            }
            if let Some(signature) = context.signature {
                transaction.execute(
                    "INSERT OR REPLACE INTO expected_claim_signatures(subject,kind,actor,signature) VALUES (?1,?2,?3,?4)",
                    params![input.subject,input.kind,input.actor,signature],
                ).map_err(internal)?;
            }
            let outcome = (|| {
            check_harness_event_runtime(transaction, &input.subject, event_runtime)?;
            let settled_receipt = if let Some(fence) = fence {
                check_mailbox_fence(transaction, fence, &graph.origin)?;
                let index = transaction.query_row(
                    "SELECT MIN(store_index) FROM claims WHERE subject=?1 AND subject LIKE 'message/%'",
                    [&input.subject], |row| row.get::<_, Option<u64>>(0),
                ).map_err(internal)?.ok_or_else(|| St3Error::new("missing-message", "message does not exist"))?;
                let message = message_view_tx(transaction, &input.subject, index).map_err(internal)?;
                if message.to != fence.subject || input.actor.as_deref() != Some(&fence.subject) {
                    return Err(St3Error::new("wrong-message-recipient", "receipt belongs to another seat"));
                }
                if input.kind == "message.staged" && !rollouts::message_allowed(transaction, &message)? {
                    return Err(St3Error::new("seat-rollout-draining", "new independent delivery waits for the seat rollout"));
                }
                matches!((input.kind.as_str(), message.status.as_str()),
                    ("message.staged", "delivered" | "read" | "closed") | ("message.delivered", "read" | "closed") | ("message.read", "closed"))
            } else { false };
            if let Some((operation_id, request_digest)) = &operation
                && let Some((stored_digest, canonical_claim, state)) =
                    operation_tx(transaction, operation_id).map_err(internal)?
            {
                if state == "conflict" {
                    return Err(St3Error::new(
                        "idempotency-conflict",
                        "the replicated operation has conflicting requests",
                    )
                    .with_detail("operation_id", operation_id.clone()));
                }
                if stored_digest != *request_digest {
                    return Err(St3Error::new(
                        "idempotency-mismatch",
                        "the idempotency key already identifies a different request",
                    )
                    .with_detail("operation_id", operation_id.clone())
                    .with_detail("stored_digest", stored_digest)
                    .with_detail("request_digest", request_digest.clone()));
                }
                return claim_by_id_tx(transaction, &canonical_claim)
                    .map_err(internal)?
                    .map(|claim| (claim, false))
                    .ok_or_else(|| {
                        St3Error::new("internal", "the operation claim is missing")
                    });
            }
            if let Some((operation_id, request_digest)) = &operation {
                checkpointed_operation_outcome(transaction, operation_id, request_digest)?;
            }
            // Replays and key/body mismatches settle before lifecycle admission. A new send
            // and retirement serialize here, in the same savepoint as their durable claims.
            admission(transaction)?;
            if let Some(expected_subjects) = expected_subjects {
                for (subject, expected) in expected_subjects {
                    // Layout edits merge, but retiring a folded duplicate must not lose
                    // an edit accepted after the client read it.
                    if subject == &input.subject
                        && !(input.kind == "arrangement.edited"
                            && input.fields.get("operations").and_then(Value::as_array)
                                .is_some_and(|operations| operations.iter().any(|operation| operation["op"] == "retire")))
                    { continue; }
                    let actual = if let Some(id) = subject.strip_prefix("launch/") {
                        transaction.query_row(
                            "SELECT updated_at_unix_ms FROM planning_sessions WHERE id=?1",
                            [id], |row| row.get::<_, String>(0),
                        ).optional().map_err(internal)?.map(|time| format!("launch/{time}"))
                    } else if subject.starts_with("arrangement/") {
                        transaction.query_row(
                            "SELECT revision FROM arrangements WHERE subject=?1 AND created=1 AND retired=0",
                            [subject], |row| row.get::<_, String>(0),
                        ).optional().map_err(internal)?
                    } else {
                        latest_claim_id_tx(transaction, subject).map_err(internal)?
                    };
                    if actual.as_deref() != Some(expected.as_str()) {
                        return Err(St3Error::new("stale-subject", format!("subject `{subject}` changed"))
                            .with_detail("subject", subject.clone())
                            .with_detail("expected_head", json!(expected))
                            .with_detail("current_head", json!(actual)));
                    }
                }
            }
            // A native adoption may import old DND only before any explicit graph decision.
            // Check inside the writer transaction so a concurrent release always wins.
            if input.kind == "delivery.hold"
                && input.fields.get("legacy_adoption") == Some(&Value::Bool(true))
                && let Some(existing) = latest_claim_of_kind_tx(transaction, &input.subject, "delivery.hold")?
            {
                return Ok((existing, false));
            }
            if let Some(existing) = subagents::check_subagent_claim_tx(transaction, input)? {
                return Ok((existing, false));
            }
            for evidence in &input.evidence {
                // Evidence may cite a claim that a checkpoint has since dropped.
                let exists = checkpoint::claim_or_tombstone_exists(transaction, evidence)
                    .map_err(internal)?;
                if !exists {
                    return Err(St3Error::new(
                        "missing-evidence",
                        format!("evidence claim `{evidence}` is not stored"),
                    ));
                }
            }
            if let Some(expected) = &input.expected_subject {
                let actual = if input.kind == "arrangement.edited" {
                    transaction.query_row("SELECT revision FROM arrangements WHERE subject=?1", [&input.subject], |row| row.get::<_,String>(0)).optional().map_err(internal)?
                } else {
                    latest_claim_id_tx(transaction, &input.subject).map_err(internal)?
                };
                if &actual != expected {
                    return Err(St3Error::new(
                        "stale-subject",
                        format!("subject `{}` changed", input.subject),
                    )
                    .with_detail("subject", input.subject.clone())
                    .with_detail("expected_head", json!(expected))
                    .with_detail("current_head", json!(actual)));
                }
            }
            if input.kind == crate::model::PLACEMENT_SOURCE_OFFLINE_KIND {
                let current: Option<String> = transaction.query_row(
                    "SELECT claim_id FROM desired WHERE subject=?1 AND kind='agent'",
                    [&input.subject], |row| row.get(0),
                ).optional().map_err(internal)?;
                if current.as_deref() != input.fields.get("desired_token").and_then(Value::as_str) {
                    return Err(St3Error::new("stale-placement",
                        "placement changed; read the current handoff before overriding its sources"));
                }
            }
            // Check under the writer lock: concurrent captures must not append a second opener
            // or replace an existing observation with another attribution-only claim.
            if input.kind == "resource.observed"
                && input.fields.get("attribution_only") == Some(&Value::Bool(true))
                && let Some(previous) = latest_actual(transaction, &input.subject).map_err(internal)?
                && previous.get("facts").unwrap_or(&previous).as_object().is_some_and(|facts| {
                    facts.contains_key("opened_by") || facts.contains_key("opened_by_run")
                })
                && let Some(existing) = latest_claim_of_kind_tx(transaction, &input.subject, "resource.observed")?
            {
                return Ok((existing, false));
            }
            let mut stored_fields = custom::prepare(transaction, input)?;
            if let Some(fields) = normalize_resource_observation(transaction, input)? {
                stored_fields = Some(fields);
            }
            if let Some(fields) = glasses::prepare(transaction, input)? {
                stored_fields = Some(fields);
            }
            arrangements::prepare(transaction, input)?;
            // A leave names its own batch as the last sequence of its window. The sequence is only
            // known here, under the writer lock, so a zero high water stands for "this batch".
            if input.kind == "fleet.member-left"
                && input.fields.get("high_water").and_then(Value::as_u64) == Some(0)
            {
                let mut fields = stored_fields
                    .clone()
                    .unwrap_or_else(|| input.fields.clone());
                fields.insert(
                    "high_water".into(),
                    Value::from(
                        next_replica_sequence(transaction, &graph.origin).map_err(internal)?,
                    ),
                );
                stored_fields = Some(fields);
            }
            if settled_receipt || validate_message_transition(transaction, input)? {
                let latest_id = latest_claim_id_tx(transaction, &input.subject)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        St3Error::new(
                            "internal",
                            "an idempotent message transition has no head",
                        )
                    })?;
                let latest = claim_by_id_tx(transaction, &latest_id)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        St3Error::new(
                            "internal",
                            "an idempotent message transition head is missing",
                        )
                    })?;
                return Ok((latest, false));
            }
            let predecessor = if input.kind == "arrangement.edited" {
                transaction.query_row("SELECT revision FROM arrangements WHERE subject=?1", [&input.subject], |row| row.get::<_, String>(0)).optional().map_err(internal)?
            } else {
                latest_claim_id_tx(transaction, &input.subject).map_err(internal)?
            };
            let mut predecessors = predecessor.into_iter().collect::<Vec<_>>();
            // A placement handoff explicitly cites its local runtime and peer stop proof.
            // Keep same-subject evidence as causal links even when a newer declaration
            // displaced the runtime branch from the selected subject head.
            if input.kind == "runtime.observed" || input.kind == crate::model::PLACEMENT_SOURCE_OFFLINE_KIND {
                for evidence in &input.evidence {
                    if claim_by_id_tx(transaction, evidence).map_err(internal)?
                        .is_some_and(|claim| claim.subject == input.subject)
                    {
                        predecessors.push(evidence.clone());
                    }
                }
                if input.kind == crate::model::PLACEMENT_SOURCE_OFFLINE_KIND {
                    // The operator explicitly supersedes these sources' known runtime records.
                    // Retain those branches so destination routing is usable while sources are
                    // offline. Choose them under the writer lock; idempotent retries keep the
                    // original claim rather than changing the request with newer observations.
                    for source in input.fields.get("sources").and_then(Value::as_array)
                        .into_iter().flatten().filter_map(Value::as_str) {
                        let prior: Option<String> = transaction.query_row(&format!(
                            "SELECT claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
                             WHERE claims.subject=?1 AND claims.kind='runtime.observed' AND claims.origin=?2
                             ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
                        ), params![input.subject, source], |row| row.get(0)).optional().map_err(internal)?;
                        predecessors.extend(prior);
                    }
                }
                predecessors.sort();
                predecessors.dedup();
            }
            let mut body = json!({
                "fields": stored_fields.as_ref().unwrap_or(&input.fields),
                "evidence": input.evidence,
            });
            if let Some((operation_id, request_digest)) = &operation {
                body.as_object_mut()
                    .expect("the claim body is an object")
                    .insert(
                        "_operation".into(),
                        json!({ "id": operation_id, "request_digest": request_digest }),
                    );
            }
            let record = append_admitted_claim_tx(
                transaction,
                &graph.origin,
                &input.subject,
                &input.kind,
                input.actor.as_deref(),
                &body,
                &predecessors,
                None,
            )
            .map_err(claim_append_error)?;
            if operation.is_some() {
                register_operation_tx(transaction, &record).map_err(internal)?;
            }
            Ok((record, true))
            })()?;
            if let Some((subject, revision, current)) = observer_completion {
                check_observer_completion(transaction, subject, revision, Some(current))?;
            }
            if context.signature.is_some() {
                // An idempotent repeat does not append/consume a signature. Remove its
                // temporary expectation before commit; any failure rolls it back with the message.
                transaction.execute(
                    "DELETE FROM expected_claim_signatures WHERE subject=?1 AND kind=?2 AND actor=?3",
                    params![input.subject,input.kind,input.actor],
                ).map_err(internal)?;
            }
            Ok(outcome)
        })
        .map_err(|error| St3Error::new("internal", error))?
}

pub fn validate_claim_input(input: &ClaimInput) -> Result<(), St3Error> {
    validate_claim_kind(&input.kind)?;
    validate_claim_subject(&input.subject)?;
    if let Some(actor) = &input.actor {
        validate_actor(actor)?;
    }
    validate_claim_fields(input)?;
    st3_schema::owned_terminals::validate_declaration_owner(
        &input.subject,
        &input.kind,
        input.actor.as_deref(),
    )
    .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::glasses::validate_owner(&input.subject, input.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::arrangements::validate_actor(&input.subject, input.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    st3_schema::directive_notes::validate_actor(&input.subject, &input.kind, input.actor.as_deref())
        .map_err(|e| St3Error::new(e.code, e.message))?;
    claim_operation(input)?;
    Ok(())
}

/// Record an observation of `local` retention on this node only. It gets no batch, no
/// envelope and no idempotency row, and it never replicates. A repeated idempotency key
/// returns the first observation. The record's ID starts with `local-observation/`.
fn check_harness_event_runtime(
    connection: &Connection,
    subject: &str,
    runtime: Option<&str>,
) -> Result<(), St3Error> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let body: Option<String> = connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query("claims.body", "runtime.observed")
        ))
        .map_err(internal)?
        .query_row(params![subject, i64::MAX], |r| r.get(0))
        .optional()
        .map_err(internal)?;
    let body: Value = serde_json::from_str(body.as_deref().unwrap_or("null")).map_err(internal)?;
    let fields = body.get("fields").unwrap_or(&body);
    if fields["status"] != "running" || fields["incarnation_id"].as_str() != Some(runtime) {
        return Err(St3Error::new(
            "stale-harness-event-session",
            "the publishing driver is not this seat's running incarnation",
        ));
    }
    Ok(())
}

fn append_local_observation_fenced(
    graph: &GraphStore,
    input: &ClaimInput,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool), St3Error> {
    validate_local_observation(input)?;
    graph
        .connection
        .batched(|transaction| {
            check_harness_event_runtime(transaction, &input.subject, event_runtime)?;
            insert_local_observation_tx(transaction, &graph.origin, input, now_ms())
        })
        .map_err(|error| St3Error::new("internal", error))?
}

/// Record an observation of `latest` retention. The local observation log keeps every
/// one. The replicated claim log gets a claim only when the observed state changes; the
/// returned record is that claim, or the local observation when nothing replicated.
#[cfg(any(test, feature = "test-support"))]
pub fn append_latest_observation(
    graph: &GraphStore,
    input: &ClaimInput,
    now: u128,
) -> Result<(ClaimRecord, bool), St3Error> {
    append_latest_observation_fenced(graph, input, now, None)
}

fn append_latest_observation_fenced(
    graph: &GraphStore,
    input: &ClaimInput,
    now: u128,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool), St3Error> {
    append_latest_observation_publication(graph, input, now, event_runtime)
        .map(|(record, appended, _)| (record, appended))
}

fn append_latest_observation_publication(
    graph: &GraphStore,
    input: &ClaimInput,
    now: u128,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool, bool), St3Error> {
    validate_local_observation(input)?;
    graph
        .connection
        .batched(|transaction| {
            check_harness_event_runtime(transaction, &input.subject, event_runtime)?;
            let (mut local, appended) =
                insert_local_observation_tx(transaction, &graph.origin, input, now)?;
            if !appended {
                return Ok((local, false, false));
            }
            let published = match input.kind.as_str() {
                "harness.observed" => {
                    publish_changed_harness_state_tx(transaction, &graph.origin, input, now)?
                }
                "harness.usage" => {
                    publish_due_usage_tx(transaction, &graph.origin, input, &local, now)?
                }
                "harness.todo.observed" | "workspace.observed" => Some(publish_latest_claim_tx(
                    transaction,
                    &graph.origin,
                    &input.subject,
                    &input.kind,
                    input.actor.as_deref(),
                    &json!(input.fields),
                )?),
                _ => None,
            };
            let status_transition = published.as_ref().is_some_and(|claim| {
                input.kind == "harness.observed"
                    && claim.body["fields"]["status_transition"] == true
            });
            if input.kind == "harness.observed" && event_runtime.is_some() {
                // Native acknowledgements echo the admitted producer event, including on
                // replay. Derived history metadata belongs to its replicated publication.
                return Ok((local, true, status_transition));
            }
            if input.kind == "harness.observed" {
                let source = match published.as_ref() {
                    Some(claim) => Some(claim.clone()),
                    None => latest_harness_of_incarnation_tx(
                        transaction,
                        &input.subject,
                        input.fields.get("incarnation_id").and_then(Value::as_str),
                    )?,
                };
                if let Some(source) = source {
                    local.body["fields"]["observed_since_ms"] = source.body["fields"]["observed_since_ms"].clone();
                    local.body["fields"]["status_transition"] = published.as_ref()
                        .map_or(json!(false), |claim| claim.body["fields"]["status_transition"].clone());
                    transaction.execute("UPDATE local_observations SET body=?1 WHERE id=?2", params![
                        canonical_json_text(&local.body).map_err(internal)?,
                        local_observation_position(&local).expect("a local observation has a position"),
                    ]).map_err(internal)?;
                }
            }
            Ok((published.unwrap_or(local), true, status_transition))
        })
        .map_err(|error| St3Error::new("internal", error))?
}

/// Shared tables the projection digest covers, with the columns each leaves out.
const PROJECTION_DIGEST_TABLES: &[(&str, &[&str])] = &[
    ("operations", &[]),
    ("blobs", &[]),
    ("documents", &["created_index"]),
    ("desired", &[]),
    ("arrangements", &["changed_index"]),
    ("arrangement_registers", &[]),
    ("message_index", &["created_index"]),
    ("resource_observations", &[]),
    ("glass_heads", &[]),
    ("person_directive_notes", &[]),
    ("custom_registrations", &[]),
    ("custom_sources", &[]),
    ("custom_dependencies", &[]),
    ("mission_revisions", &["created_index"]),
    ("mission_definitions", &[]),
    ("mission_runs", &[]),
    ("mission_run_deadlines", &[]),
    ("mission_run_after", &[]),
    ("run_generations", &[]),
    // These effective timestamps include member-local lease renewals. The underlying durable
    // claim identity, including its lease timestamps, remains covered by claim_sources.
    (
        "step_runs",
        &["lease_expires_at_unix_ms", "updated_at_unix_ms"],
    ),
    ("revision_proposals", &[]),
    ("planning_sessions", &[]),
    ("planning_candidates", &[]),
    ("planning_previews", &[]),
];

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn unchanged_replication_snapshot_reuses_inventory() {
    let store = Store::open_memory("node").unwrap();
    let first = store.replication_snapshot().unwrap();
    let second = store.replication_snapshot().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.authority_digest, first.inventory.digest);
    let first_hash = replica_envelope_hash("node", 1, None, 1, b"first");
    let second_hash = replica_envelope_hash("node", 1, None, 1, b"second");
    assert_ne!(
        first_hash, second_hash,
        "the inventory must commit payload bytes"
    );
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn replication_snapshot_moves_an_exclusive_inventory_into_its_successor() {
    let store = Store::open_memory("node").unwrap();
    for index in 0..5 {
        store
            .append_client_claim(&ClaimInput {
                subject: format!("resource/envelope-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let first = store.replication_snapshot().unwrap();
    assert!(first.inventory.envelopes.capacity() > first.inventory.envelopes.len());
    let buffer = first.inventory.envelopes.as_ptr();
    drop(first);
    store
        .append_client_claim(&ClaimInput {
            subject: "resource/last-envelope".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([(
                "kind".into(),
                Value::String("custom.test.replication".into()),
            )]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let successor = store.replication_snapshot().unwrap();
    assert_eq!(successor.inventory.envelopes.len(), 6);
    assert_eq!(successor.inventory.envelopes.as_ptr(), buffer);
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn replication_snapshot_inserts_new_envelopes_in_canonical_order() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let target = Store::open_memory("z").unwrap();
    let initial = target.replication_snapshot().unwrap();
    target
        .append_client_claim(&ClaimInput {
            subject: "resource/local-envelope".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([(
                "kind".into(),
                Value::String("custom.test.replication".into()),
            )]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("local-envelope".into()),
        })
        .unwrap();
    let local = target.replication_snapshot().unwrap();
    assert_eq!(
        local.inventory.envelopes.len(),
        initial.inventory.envelopes.len() + 1
    );

    let source = Store::open_memory("a").unwrap();
    source
        .append_client_claim(&ClaimInput {
            subject: "resource/remote-envelope".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([(
                "kind".into(),
                Value::String("custom.test.replication".into()),
            )]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("remote-envelope".into()),
        })
        .unwrap();
    source.bind_fleet(FLEET).unwrap();
    let exchange = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    target.bind_fleet(FLEET).unwrap();
    target
        .receive_replication_exchange("a", FLEET, &exchange)
        .unwrap();
    let incremental = target.replication_snapshot().unwrap();
    let connection = target.connection.lock().unwrap();
    let (full, max_rowid) = full_replication_inventory_rows(&connection).unwrap();
    assert_eq!(incremental.inventory.public().envelopes, full);
    assert_eq!(incremental.max_envelope_rowid, max_rowid);
    assert_eq!(
        incremental.inventory.digest,
        replication_inventory_digest(&full)
    );
    assert_eq!(incremental.buckets, test_replication_buckets(&full));
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn replication_snapshot_keeps_compact_envelope_identifiers() {
    assert_eq!(std::mem::size_of::<CompactEnvelopeId>(), 48);
    let store = Store::open_memory("node").unwrap();
    for index in 0..100 {
        store
            .append_client_claim(&ClaimInput {
                subject: format!("resource/compact-envelope-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let snapshot = store.replication_snapshot().unwrap();
    assert!(snapshot.inventory.envelopes.len() >= 100);
    assert_eq!(snapshot.inventory.writers, ["node"]);
    assert!(
        snapshot.inventory.irregular_hashes.is_empty(),
        "every local envelope hash is stored as SHA-256 bytes"
    );
}

#[cfg(any(test, feature = "test-support"))]
#[test]
pub fn a_malformed_peer_hash_does_not_stop_replication() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    let target = Store::open_memory("target").unwrap();
    source.bind_fleet(FLEET).unwrap();
    target.bind_fleet(FLEET).unwrap();
    let mut exchange = source.export_replication_summary(FLEET).unwrap();
    exchange.envelopes = vec![ReplicaEnvelope {
        writer: "source".into(),
        sequence: 1,
        previous_hash: None,
        hash: "NOT-A-SHA256".into(),
        accepted_at_unix_ms: 1,
        payload: "{}".into(),
        member_key: None,
        signature: None,
    }];
    target
        .receive_replication_exchange("source", FLEET, &exchange)
        .unwrap();

    let inventory = target.replication_inventory().unwrap();
    assert!(
        inventory
            .envelopes
            .iter()
            .any(|id| id.writer == "source" && id.hash == "NOT-A-SHA256"),
        "the stored identity keeps its exact hash text"
    );
    assert_eq!(
        inventory.digest,
        replication_inventory_digest(&inventory.envelopes)
    );
    // The stored row is still found by its identity when a peer asks for it.
    let fresh = Store::open_memory("fresh").unwrap();
    fresh.bind_fleet(FLEET).unwrap();
    let forwarded = target
        .export_replication_exchange(
            FLEET,
            &fresh.export_replication_summary(FLEET).unwrap().inventory,
        )
        .unwrap();
    assert!(
        forwarded
            .envelopes
            .iter()
            .any(|envelope| envelope.hash == "NOT-A-SHA256")
    );
}

#[cfg(test)]
pub mod harness_event_tests;
