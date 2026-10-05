//! Closed client disclosure policy. Authorization and canonical special projection execute
//! in the gateway; these selections never confer a grant or permit raw body fallback.
use serde::Serialize;
use serde_json::{json, Value};

use crate::{canonical_json_sha256, is_custom_claim_kind, registry, FieldSpec};

mod contract;
pub use contract::Contract;

pub const WIRE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Shared, Attention, Message, Person, Account, PersonRequest, PersonAnswer,
    ReviewerRequest, ReviewerResult, OperationalAudience, Glass, RecordedActor, Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialProjector {
    AgentDesired, AccountDesired, ResourceObservation, GlassBody, CustomFields,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyPolicy {
    pub identity: Audience,
    pub special: &'static [SpecialProjector],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ClaimPolicy {
    pub audience: Audience,
    /// Native FieldSpec names only. Unlisted and newly added fields are withheld.
    pub fields: &'static [&'static str],
    /// Requires canonical projection, never copying arbitrary JSON bags.
    pub special: Option<SpecialProjector>,
}

pub fn family_policy(family: &str) -> Option<FamilyPolicy> {
    let (identity, special): (Audience, &'static [SpecialProjector]) = match family {
        "message" => (Audience::Message, &[]),
        "attention" => (Audience::Attention, &[]),
        "person" => (Audience::Person, &[]),
        "account" => (Audience::Account, &[SpecialProjector::AccountDesired]),
        "agent" => (Audience::Shared, &[SpecialProjector::AgentDesired]),
        "glass" => (Audience::Glass, &[SpecialProjector::GlassBody]),
        "custom" => (Audience::RecordedActor, &[SpecialProjector::CustomFields]),
        "resource" => (Audience::Shared, &[SpecialProjector::ResourceObservation]),
        "checkpoint" | "checkpoint-excusal" | "daemon" | "doc" | "exec" |
        "file" | "gate-operation" | "fleet-invite" | "github-post" | "host" |
        "lane" | "observer" | "mission" | "mission-run" | "loop-run" |
        "owned-set" | "planning-session" | "pty" | "repair" | "rule" |
        "revision-proposal" | "run-generation" | "schedule" | "step-run" |
        "subscription" => (Audience::Shared, &[]),
        _ if registry().subjects.contains_key(family) => (Audience::Denied, &[]),
        _ => return None,
    };
    Some(FamilyPolicy { identity, special })
}

/// Categorical denial precedes subject visibility, provenance and payload reads.
pub fn family_ref_allowed(family: &str, reference: &str) -> bool {
    family_policy(family).is_some()
        && !(reference == "custom/client" || reference.starts_with("custom/client/"))
}

pub fn claim_policy(kind: &str) -> Option<ClaimPolicy> {
    if kind.starts_with("custom.client.") || kind == "custom.client" {
        return Some(ClaimPolicy { audience: Audience::Denied, fields: &[], special: None });
    }
    if is_custom_claim_kind(kind) {
        return Some(ClaimPolicy {
            audience: Audience::RecordedActor, fields: &[],
            special: Some(SpecialProjector::CustomFields),
        });
    }
    let (audience, fields, special): (Audience, &'static [&'static str], Option<SpecialProjector>) = match kind {
        "agent.account" => (Audience::Shared, &["account"], None),
        "agent.placement.source-offline" => (Audience::Shared, &["destination"], None),
        "agent.presence" => (Audience::Shared, &["presence", "reachability", "reason"], None),
        "agent.queue.moved" => (Audience::Shared, &["run", "placement", "anchor", "reason"], None),
        "attention.requested" => (Audience::Attention, &["reviewer", "title", "reason", "severity", "targets", "until", "step", "step_attempt", "closed_by"], None),
        "attention.resolved" => (Audience::Attention, &["request", "outcome", "reason"], None),
        "checkpoint.excused" => (Audience::Shared, &["writer", "reason"], None),
        "checkpoint.sealed" => (Audience::Shared, &["cut_unix_ms", "sealed_digest", "sealed_count", "rules_digest", "checkpoint_protocol", "build"], None),
        "checkpoint.verified" => (Audience::Shared, &["cut_unix_ms", "sealed_digest", "rules_digest", "drop_digest", "dropped_envelopes", "dropped_claims", "retained_digest", "graph_digest", "reader_digest", "checkpoint_protocol", "build"], None),
        "daemon.diagnostic" => (Audience::Shared, &["severity", "code", "status", "reason"], None),
        "daemon.started" => (Audience::Shared, &["status", "pid", "version", "schema", "schema_digest"], None),
        "delivery.hold" => (Audience::Shared, &["held", "until_unix_ms", "reason", "legacy_adoption"], None),
        "doc.bound" => (Audience::Shared, &["name", "hash", "size", "executable"], None),
        "eval.verdict" => (Audience::Shared, &["verdict", "reason"], None),
        "file.observed" => (Audience::Shared, &["status", "path", "content_hash", "mode", "reason"], None),
        "fleet.invite-created" => (Audience::Shared, &["sponsor", "name", "expires_at_unix_ms", "created_by"], None),
        "fleet.invite-redeemed" => (Audience::Shared, &["name"], None),
        "fleet.invite-revoked" => (Audience::Shared, &["reason", "revoked_by"], None),
        "fleet.member-admitted" => (Audience::Shared, &["fleet_id", "via", "sponsor", "invite", "mode", "writer_floor", "admitted_by"], None),
        "fleet.member-endpoints" => (Audience::Shared, &["mode", "build"], None),
        "fleet.member-left" => (Audience::Shared, &["high_water"], None),
        "fleet.member-removed" => (Audience::Shared, &["high_water", "reason", "removed_by"], None),
        "gate.requested" => (Audience::ReviewerRequest, &["status", "owner", "reviewer", "mode", "question", "operation", "mission_revision", "step_definition", "attempt", "runner", "model", "token_budget", "gate", "baseline"], None),
        "gate.result" => (Audience::ReviewerResult, &["verdict", "decision", "reason", "operation", "request", "gate", "baseline", "field", "token_usage", "stage"], None),
        "github.posted" => (Audience::Shared, &["agent", "repository", "item", "kind", "id", "url", "login"], None),
        "glass.deleted" => (Audience::Glass, &["base_revision", "replaced_revision"], None),
        "glass.upserted" => (Audience::Glass, &["base_revision", "replaced_revision"], Some(SpecialProjector::GlassBody)),
        "harness.context-clear.requested" => (Audience::RecordedActor, &["runtime_id", "incarnation_id", "context_epoch", "operation_status"], None),
        "harness.context-clear.result" => (Audience::RecordedActor, &["result", "context_epoch", "reason", "incarnation_id", "runtime_id"], None),
        "harness.diagnostic" => (Audience::Shared, &["driver", "severity", "status", "code", "reason", "incarnation_id", "step_run", "wake_attempts", "attempt", "readiness_epoch", "observed_since_ms", "retry_attempt", "retry_after_unix_ms"], None),
        "harness.limits" => (Audience::Shared, &["driver", "incarnation_id", "account", "account_ref", "plan", "five_hour_percent", "five_hour_resets_at_unix_ms", "weekly_percent", "weekly_resets_at_unix_ms", "measured_at_unix_ms"], None),
        "harness.observed" => (Audience::Shared, &["state", "background_jobs", "driver", "reason", "incarnation_id", "transport", "exit", "observed_since_ms", "observed_at_ms", "ownership_sequence", "transition_sequence", "evidence_incarnation", "rollout_operation", "quiescent"], None),
        "harness.usage" => (Audience::Shared, &["input_tokens", "output_tokens", "total_tokens", "cached_tokens", "cache_write_tokens", "owner_run", "owner_step", "host", "observed_at_unix_ms", "context_used_tokens", "context_window_tokens", "context_used_percent", "compactions", "last_compaction_ms", "last_compaction_trigger", "cost", "currency", "account", "cache_write_1h_tokens", "cost_microusd", "reported_cost_microusd", "unpriced_tokens", "pricing", "semantics", "driver", "model", "incarnation_id"], None),
        "intent.desired" => (Audience::Shared, &["kind", "revision"], None),
        "lane.approved" => (Audience::Shared, &["entry", "reason"], None),
        "lane.joined" => (Audience::Shared, &["entry", "reason"], None),
        "lane.left" => (Audience::Shared, &["entry", "outcome", "reason"], None),
        "lane.marked" => (Audience::Shared, &["entry", "state", "detail", "head"], None),
        "lane.moved" => (Audience::Shared, &["entry", "placement", "anchor", "reason"], None),
        "loop.round-dispatch" => (Audience::Shared, &["round", "dispatch", "status", "mission_run", "candidate", "item_id", "reason"], None),
        "loop.round-result" => (Audience::Shared, &["round", "status", "mission_run", "feedback", "candidate", "reason", "token_usage"], None),
        "loop.state" => (Audience::Shared, &["status", "round", "reason", "best_round", "feedback", "winner"], None),
        "message.closed" => (Audience::Message, &["status"], None),
        "message.delivered" => (Audience::Message, &["status", "recipient", "transport", "runtime_id"], None),
        "message.read" => (Audience::Message, &["status"], None),
        "message.sent" => (Audience::Message, &["from", "to", "content", "status", "session_id", "title", "in_reply_to", "tags"], None),
        "message.staged" => (Audience::Message, &["status", "recipient", "transport", "runtime_id"], None),
        "mission-run.created" => (Audience::Shared, &["status", "mission", "revision", "generation", "initial_revision", "current_generation", "root_revision", "root_mission_run", "workspace", "requester", "mode", "timeout_ms", "deadline_at_unix_ms", "parent_step_run", "after"], None),
        "mission-run.state" => (Audience::Shared, &["status", "phase", "previous_phase", "reason", "completion", "finally"], None),
        "mission.produced" => (Audience::Shared, &["name", "mission", "revision", "step_definition", "attempt"], None),
        "mission.published" => (Audience::Shared, &["revision", "state"], None),
        "observer.observed" => (Audience::Shared, &["status", "revision", "attempt", "changed", "next_check_unix_ms", "resource", "provider", "locator", "observation"], None),
        "observer.refresh-requested" => (Audience::Shared, &["revision", "attempt"], None),
        "observer.state" => (Audience::Shared, &["state", "reason", "error_code", "revision", "attempt", "next_check_unix_ms"], None),
        "operational.failure" => (Audience::OperationalAudience, &["episode", "condition", "reviewer", "title", "reason", "severity", "source_revision", "incarnation"], None),
        "operational.recovered" => (Audience::OperationalAudience, &["episode", "failure", "reason"], None),
        "owned-set.revised" => (Audience::Shared, &["revision"], None),
        "planning-session.approved" => (Audience::Shared, &["variant", "candidate_revision", "mission_revision", "markdown", "kdl", "requester"], None),
        "planning-session.cancelled" => (Audience::Shared, &["reason", "requester"], None),
        "planning-session.candidate-submitted" => (Audience::Shared, &["variant", "revision", "candidate_revision", "markdown", "kdl", "mission_revision"], None),
        "planning-session.previewed" => (Audience::Shared, &["variant", "candidate_revision"], None),
        "planning-session.question-answered" => (Audience::PersonAnswer, &["decision_id", "expected_revision", "explanation", "requester"], None),
        "planning-session.question-requested" => (Audience::PersonRequest, &["decision_id", "revision", "question", "decision_type", "requester", "planner"], None),
        "planning-session.revision-requested" => (Audience::Shared, &["variant", "candidate_revision", "feedback", "requester"], None),
        "planning-session.started" => (Audience::Shared, &["mission", "request", "workspace", "requester", "planner", "target_run", "target_generation"], None),
        "principal.key-granted" => (Audience::Shared, &["role", "label"], None),
        "principal.key-revoked" => (Audience::Shared, &["reason"], None),
        "publication.operation" => (Audience::Shared, &["operation", "action", "status"], None),
        "reconcile.fault" => (Audience::Shared, &["scope", "status", "reason"], None),
        "record.repaired" => (Audience::Shared, &["record", "replacement", "reason"], None),
        "repair.applied" => (Audience::Shared, &["item_count", "reason"], None),
        "resource.observed" => (Audience::Shared, &["kind", "observed_at"], Some(SpecialProjector::ResourceObservation)),
        "revision-proposal.applied" => (Audience::Shared, &["status", "successor_generation", "reason"], None),
        "revision-proposal.approved" => (Audience::ReviewerResult, &["reviewer", "all_approved"], None),
        "revision-proposal.cancelled" => (Audience::Shared, &["status", "reason"], None),
        "revision-proposal.created" => (Audience::Shared, &["run", "source_generation", "candidate_revision", "reason", "status", "cutover"], None),
        "rule.audited" => (Audience::Shared, &["rule", "actor", "action", "target"], None),
        "rule.set" => (Audience::Shared, &["mode", "description"], None),
        "run-generation.created" => (Audience::Shared, &["run", "revision", "status", "predecessor", "reason"], None),
        "run-generation.state" => (Audience::Shared, &["status", "phase", "previous_phase", "reason", "successor"], None),
        "run-generation.superseded" => (Audience::Shared, &["status", "phase", "previous_phase", "reason", "successor"], None),
        "runtime.action.deadline-reached" => (Audience::RecordedActor, &["action", "operation", "runtime_id", "terminal", "incarnation_id", "reason", "signal", "operation_status", "code", "harness", "native_session_id"], None),
        "runtime.action.failed" => (Audience::RecordedActor, &["action", "operation", "runtime_id", "terminal", "incarnation_id", "reason", "signal", "operation_status", "code", "harness", "native_session_id"], None),
        "runtime.action.requested" => (Audience::RecordedActor, &["action", "operation", "runtime_id", "terminal", "incarnation_id", "deadline_unix_ms", "signal", "reason"], None),
        "runtime.action.succeeded" => (Audience::RecordedActor, &["action", "operation", "runtime_id", "terminal", "incarnation_id", "reason", "signal", "operation_status", "code", "harness", "native_session_id"], None),
        "runtime.observed" => (Audience::Shared, &["status", "runtime_id", "terminal", "reachability", "reason", "exit_code", "exit_signal", "incarnation_id", "adopted", "driver", "host", "shutdown_timeout_ms"], None),
        "runtime.readiness-deadline-reached" => (Audience::Shared, &["runtime_id", "driver", "incarnation_id", "deadline_unix_ms", "reason"], None),
        "runtime.reconcile-decision" => (Audience::Shared, &["decision", "reachability", "reason", "restart_at_unix_ms", "gate", "input_number"], None),
        "runtime.restart-window-reset" => (Audience::Shared, &["incarnation_id", "reason"], None),
        "schedule.occurrence-cancelled" => (Audience::Shared, &["occurrence", "revision", "reason"], None),
        "schedule.occurrence-reached" => (Audience::Shared, &["occurrence", "revision", "at_unix_ms", "scheduled_at_unix_ms", "scheduled"], None),
        "schedule.occurrence-scheduled" => (Audience::Shared, &["occurrence", "revision", "at_unix_ms", "scheduled_at_unix_ms"], None),
        "schedule.work-failed" => (Audience::Shared, &["request", "code", "reason"], None),
        "schedule.work-requested" => (Audience::Shared, &["revision", "occurrence", "mission", "mission_revision", "workspace"], None),
        "schedule.work-started" => (Audience::Shared, &["request", "mission_run"], None),
        "step-run.carried" => (Audience::Shared, &["source", "source_step_run", "source_generation", "definition_hash", "status", "attempt", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms"], None),
        "step-run.retried" => (Audience::Shared, &["status", "attempt", "reason", "not_before_unix_ms"], None),
        "step-run.state" => (Audience::Shared, &["status", "reason", "readiness_epoch", "attempt"], None),
        "subagent.appeared" => (Audience::Shared, &["subagent_id", "subagent_type", "description", "driver", "incarnation_id", "step_run", "started_at_unix_ms", "lease_expires_at_unix_ms"], None),
        "subagent.ended" => (Audience::Shared, &["subagent_id", "outcome", "reason", "ended_at_unix_ms", "duration_ms", "input_tokens", "output_tokens", "cache_write_tokens", "cached_tokens", "total_tokens"], None),
        "subagent.renewed" => (Audience::Shared, &["subagent_id", "incarnation_id", "lease_expires_at_unix_ms"], None),
        "subscription.batch-sent" => (Audience::Shared, &["through", "message", "entries"], None),
        "subscription.mission-deferred" => (Audience::Shared, &["request", "not_before_unix_ms"], None),
        "subscription.mission-failed" => (Audience::Shared, &["request", "code", "reason"], None),
        "subscription.mission-request-cancelled" => (Audience::Shared, &["request", "reason"], None),
        "subscription.mission-request-released" => (Audience::Shared, &["request", "reason"], None),
        "subscription.mission-requested" => (Audience::Shared, &["mission", "mission_revision", "resource", "resource_input", "workspace", "discovery", "requester", "held"], None),
        "subscription.mission-started" => (Audience::Shared, &["request", "mission_run"], None),
        "subscription.state" => (Audience::Shared, &["state", "reason", "observer", "to"], None),
        "subscription.watch-ended" => (Audience::Shared, &["reason", "since_unix_ms", "message"], None),
        "terminal.input.requested" => (Audience::RecordedActor, &["mode", "byte_count", "runtime_id", "incarnation_id", "intent", "sequence"], None),
        "terminal.input.result" => (Audience::RecordedActor, &["result", "reason", "incarnation_id", "runtime_id", "sequence"], None),
        "transport.observed" => (Audience::Shared, &["status", "reason", "protocol", "last_success_at"], None),
        "work.claimed" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.extended" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.failed" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.person-asked" => (Audience::PersonRequest, &["run", "generation", "origin_step", "origin_attempt", "person", "title", "reason", "key", "attempt", "status", "owner_run", "owner_generation", "waiting_since"], None),
        "work.person-cancelled" => (Audience::PersonAnswer, &["attempt", "status", "summary", "key", "episode"], None),
        "work.person-done" => (Audience::PersonAnswer, &["attempt", "status", "summary", "key", "episode"], None),
        "work.progress" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.released" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.renewed" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "work.submitted" => (Audience::Shared, &["attempt", "status", "summary", "reason", "worker_reported", "claimant", "claim_incarnation", "claim_expires_at_unix_ms", "readiness_epoch", "extend_ms"], None),
        "workspace.observed" => (Audience::Shared, &["host", "workspace", "repository"], None),
        _ => return None,
    };
    Some(ClaimPolicy { audience, fields, special })
}

/// Resolves a positive selection against native authority, never a shadow type.
pub fn disclosed_field(kind: &str, field: &str) -> Option<&'static FieldSpec> {
    let policy = claim_policy(kind)?;
    if policy.audience == Audience::Denied || !policy.fields.contains(&field) {
        return None;
    }
    registry().claims.get(kind)?.fields.get(field)
}

/// Positive canonical resource facts selection. This applies only after the
/// gateway recognizes the current canonical source format, never legacy bags.
pub fn resource_field_policy(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "vcs.repository" => &["url", "vcs", "default_ref", "head", "state", "repository_id", "github_http_requests_since_start"],
        "vcs.commit" => &["repository", "sha", "tree", "author", "committer", "message", "committed_at", "url", "state"],
        "vcs.ref" => &["repository", "name", "target", "head", "ref_type", "url"],
        "vcs.pull-request" => &["repository", "number", "url", "title", "author", "head", "head_sha", "branch", "opened_by", "opened_by_run", "base", "base_branch", "state", "draft", "merged", "created_at", "updated_at", "checks_state", "review_decision", "comments"],
        "vcs.issue" => &["repository", "number", "url", "title", "author", "opened_by", "opened_by_run", "state", "state_reason", "created_at", "updated_at", "comments"],
        "ci.run" => &["repository", "commit", "pull_request", "provider", "external_id", "url", "name", "status", "conclusion", "started_at", "completed_at"],
        "human.review" => &["target", "document", "reviewer", "decision", "reason", "submitted_at"],
        "filesystem.file" => &["status", "path", "content_hash", "size", "mode", "reason"],
        "harness.session-file" => &["harness", "agent", "incarnation_id", "status", "modified_at"],
        _ => return None,
    })
}

pub fn resource_facts_descriptor() -> Value {
    let kinds: std::collections::BTreeMap<_, _> = registry().resources.iter()
        .filter_map(|(kind, spec)| {
            let selection = resource_field_policy(kind)?;
            let fields: std::collections::BTreeMap<_, _> = selection.iter()
                .filter_map(|name| spec.fields.get(*name).map(|field| {
                    let mut projected = field.clone();
                    projected.required = false;
                    (*name, projected)
                }))
                .collect();
            Some((kind, json!({"fields": fields, "additional_fields": false})))
        })
        .collect();
    json!({"field": "facts", "source_format": "canonical_fields_facts", "historical_extra_fields": "withheld", "additional_fields": false, "by_kind": kinds})
}

/// Effective descriptor: only exposed native FieldSpecs participate. Hashing uses
/// recursively sorted JSON keys independently of serde_json's map feature flags.
/// Optional keys remain absent or null; JSON values must be finite JSON-safe data.
pub fn family_descriptor(family: &str) -> Option<Value> {
    let policy = family_policy(family)?;
    let subject = registry().subjects.get(family)?;
    let mut claims = std::collections::BTreeMap::new();
    for (kind, spec) in &registry().claims {
        if !spec.subjects.iter().any(|s| s == family || s == "*") {
            continue;
        }
        let Some(selected) = claim_policy(kind) else { continue };
        if selected.audience == Audience::Denied { continue; }
        let mut fields: std::collections::BTreeMap<_, _> = selected.fields.iter()
            .filter_map(|name| spec.fields.get(*name).map(|field| {
                let mut projected = field.clone();
                projected.required = false;
                (*name, projected)
            }))
            .collect();
        // Desired special projection is family-specific; other arbitrary desired
        // bags remain withheld even when the identity is shared.
        let special = if kind == "intent.desired" {
            policy.special.iter().copied().find(|p| matches!(p,
                SpecialProjector::AgentDesired | SpecialProjector::AccountDesired))
        } else { selected.special };
        let special_field = match special {
            Some(SpecialProjector::AgentDesired | SpecialProjector::AccountDesired) => Some("desired"),
            Some(SpecialProjector::GlassBody) => Some("body"),
            _ => None,
        };
        if let Some(name) = special_field {
            if let Some(native) = spec.fields.get(name) {
                let mut projected = native.clone();
                projected.required = false;
                fields.insert(name, projected);
            }
        }
        if special == Some(SpecialProjector::ResourceObservation) {
            // The nested object is constructed from native ResourceSpec fields;
            // special_output supplies its closed per-kind properties.
            fields.insert("facts", FieldSpec {
                value_type: crate::ValueType::Object,
                required: false,
                values: Vec::new(),
                immutable: false,
                reference: false,
                reference_families: Vec::new(),
            });
        }
        claims.insert(kind.clone(), json!({
            "audience": selected.audience, "fields": fields,
            "special": special, "retention": spec.retention,
            "special_output": if special == Some(SpecialProjector::ResourceObservation) {
                Some(resource_facts_descriptor())
            } else { None },
            "additional_fields": false,
        }));
    }
    if family == "custom" {
        claims.insert("custom.*".to_owned(), json!({"audience": Audience::RecordedActor, "fields": {}, "special": SpecialProjector::CustomFields, "additional_fields": true}));
    }
    Some(json!({
        "wire_version": WIRE_VERSION,
        "family": subject.family, "pattern": subject.pattern,
        "reference_contract": crate::subject_reference_contract(),
        "identity": policy.identity, "special": policy.special,
        "claims": claims,
        "custom_payload": if family == "custom" { Some(json!({
            "audience": Audience::RecordedActor, "special": SpecialProjector::CustomFields,
            "registry_valid_kind": true, "denied_kind_prefix": "custom.client.",
            "denied_ref_prefix": "custom/client", "additional_fields": true,
        })) } else { None },
        "value_semantics": {
            "optional_keys": "absent_or_null", "numbers": "finite_json",
            "special_payloads": "canonical_projector_only",
            "unknown_kinds_and_fields": "withheld",
        },
    }))
}

pub fn family_schema_id(family: &str) -> Option<String> {
    let descriptor = family_descriptor(family)?;
    Some(format!("subject-schema/{}/{family}", canonical_json_sha256(&descriptor)))
}

/// Claim descriptor identity includes the concrete kind, family audience and
/// wire semantics. Custom instances use the registry-valid open-map template.
pub fn claim_schema_id(family: &str, kind: &str) -> Option<String> {
    let policy = claim_policy(kind)?;
    if policy.audience == Audience::Denied { return None; }
    let descriptor = family_descriptor(family)?;
    let key = if is_custom_claim_kind(kind) {
        if family != "custom" { return None; }
        "custom.*"
    } else { kind };
    let claim = descriptor["claims"].get(key)?;
    let effective = json!({
        "wire_version": WIRE_VERSION, "family": family, "kind": kind,
        "identity": descriptor["identity"], "claim": claim,
        "value_semantics": descriptor["value_semantics"],
        "custom_payload": if key == "custom.*" { descriptor["custom_payload"].clone() } else { Value::Null },
    });
    Some(format!("subject-claim-schema/{}/{family}/{kind}", canonical_json_sha256(&effective)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_namespaces_and_unregistered_selections_fail_closed() {
        assert!(!family_ref_allowed("custom", "custom/client"));
        assert!(!family_ref_allowed("custom", "custom/client/private"));
        assert!(family_ref_allowed("custom", "custom/clientish/item"));
        assert_eq!(claim_policy("custom.client.private").unwrap().audience, Audience::Denied);
        assert!(claim_policy("custom.bad").is_none());
        assert!(claim_policy("custom..bad").is_none());
        assert!(claim_policy("future.claim").is_none());
        assert!(family_policy("future").is_none());
        assert!(disclosed_field("message.sent", "future_scalar").is_none());
        assert_eq!(claim_policy("custom.team.record").unwrap().special, Some(SpecialProjector::CustomFields));
        assert_eq!(claim_policy("custom.team.record").unwrap().audience, Audience::RecordedActor);
    }

    #[test]
    fn sensitive_native_fields_and_unstructured_bags_are_withheld() {
        for (kind, fields) in [
            ("gate.requested", &["capability_hash", "capability_expires_at"][..]),
            ("planning-session.approved", &["preview_token", "preview_hash"][..]),
            ("principal.key-granted", &["key", "issuer", "issuer_key"][..]),
            ("intent.desired", &["desired"][..]),
            ("mission.published", &["body"][..]),
            ("owned-set.revised", &["body"][..]),
            ("planning-session.started", &["planner_config"][..]),
            ("resource.observed", &["state", "credentials", "legacy_extra"][..]),
            ("harness.observed", &["input_buffer", "ask"][..]),
        ] {
            for field in fields {
                assert!(disclosed_field(kind, field).is_none(), "{kind}.{field}");
            }
        }
        assert_eq!(claim_policy("resource.observed").unwrap().fields, &["kind", "observed_at"]);
        assert!(claim_policy("private.note").is_none());
    }

    #[test]
    fn episode_claims_keep_their_distinct_audiences() {
        for (kind, audience) in [
            ("work.person-asked", Audience::PersonRequest),
            ("work.person-done", Audience::PersonAnswer),
            ("gate.requested", Audience::ReviewerRequest),
            ("gate.result", Audience::ReviewerResult),
            ("operational.failure", Audience::OperationalAudience),
            ("operational.recovered", Audience::OperationalAudience),
            ("message.sent", Audience::Message),
            ("glass.upserted", Audience::Glass),
        ] {
            assert_eq!(claim_policy(kind).unwrap().audience, audience);
        }
    }

    #[test]
    fn every_native_family_has_real_typed_disclosure_or_canonical_payload() {
        let native = registry();
        assert_eq!(native.subjects.len(), 33);
        for family in native.subjects.keys() {
            let descriptor = family_descriptor(family).unwrap();
            let claims = descriptor["claims"].as_object().unwrap();
            if family == "custom" {
                assert_eq!(descriptor["custom_payload"]["additional_fields"], true);
                assert_eq!(descriptor["custom_payload"]["audience"], "recorded_actor");
            } else {
                // More than identity metadata: each family exposes a real native
                // claim field with its native type, not an empty placeholder.
                assert!(claims.values().any(|claim|
                    claim["fields"].as_object().is_some_and(|fields|
                        fields.values().any(|field| field.get("value_type").is_some())
                    )
                ), "{family}");
            }
            for (kind, spec) in &native.claims {
                if let Some(policy) = claim_policy(kind) {
                    for field in policy.fields {
                        assert!(spec.fields.contains_key(*field), "{kind}.{field}");
                    }
                }
            }
        }
        assert_eq!(family_descriptor("agent").unwrap()["claims"]["intent.desired"]["special"], "agent_desired");
        assert_eq!(family_descriptor("account").unwrap()["claims"]["intent.desired"]["special"], "account_desired");
        assert!(family_descriptor("mission").unwrap()["claims"]["intent.desired"]["special"].is_null());
        assert!(family_descriptor("resource").unwrap()["claims"]["resource.observed"]["fields"].get("legacy_extra").is_none());
        assert!(family_descriptor("resource").unwrap()["claims"]["resource.observed"]["fields"].get("state").is_none());
    }

    #[test]
    fn schema_identity_hashes_the_effective_descriptor() {
        let descriptor = family_descriptor("message").unwrap();
        let digest = canonical_json_sha256(&descriptor);
        assert_eq!(family_schema_id("message").unwrap(), format!("subject-schema/{digest}/message"));
        let mut changed = descriptor;
        changed["claims"]["message.sent"]["audience"] = json!("shared");
        assert_ne!(canonical_json_sha256(&changed), digest);
        assert!(family_schema_id("unknown").is_none());
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    #[test]
    fn canonical_special_outputs_are_included_without_requiring_payload_availability() {
        for (family, kind, field) in [
            ("agent", "intent.desired", "desired"),
            ("account", "intent.desired", "desired"),
            ("glass", "glass.upserted", "body"),
        ] {
            let descriptor = family_descriptor(family).unwrap();
            assert_eq!(descriptor["claims"][kind]["fields"][field]["value_type"], "object");
            assert_eq!(descriptor["claims"][kind]["fields"][field]["required"], false);
        }
        assert!(family_descriptor("mission").unwrap()["claims"]["intent.desired"]["fields"].get("desired").is_none());
        assert_eq!(family_descriptor("message").unwrap()["claims"]["message.sent"]["fields"]["status"]["required"], false);
    }

    #[test]
    fn claim_ids_reject_wrong_family_and_reserved_custom_and_bind_actual_kind() {
        assert!(claim_schema_id("agent", "message.sent").is_none());
        assert!(claim_schema_id("custom", "custom.client.private").is_none());
        assert!(claim_schema_id("custom", "custom.bad").is_none());
        assert!(claim_schema_id("agent", "custom.team.record").is_none());
        assert_ne!(claim_schema_id("custom", "custom.team.first"), claim_schema_id("custom", "custom.team.second"));
        assert_ne!(claim_schema_id("agent", "intent.desired"), claim_schema_id("account", "intent.desired"));
    }
}

#[cfg(test)]
mod resource_descriptor_tests {
    use super::*;

    #[test]
    fn resource_facts_are_nested_closed_and_intersect_native_fields() {
        let descriptor = family_descriptor("resource").unwrap();
        let observation = &descriptor["claims"]["resource.observed"];
        assert_eq!(observation["fields"]["facts"]["value_type"], "object");
        assert_eq!(observation["special_output"]["field"], "facts");
        assert_eq!(observation["special_output"]["additional_fields"], false);
        assert_eq!(observation["special_output"]["by_kind"]["ci.run"]["fields"]["status"]["values"],
            json!(["queued", "in-progress", "completed"]));
        assert!(observation["special_output"]["by_kind"]["harness.session-file"]["fields"].get("path").is_none());
        assert!(resource_field_policy("custom.team.resource").is_none());
        for (kind, spec) in &registry().resources {
            for name in resource_field_policy(kind).unwrap() {
                assert!(spec.fields.contains_key(*name), "{kind}.{name}");
            }
        }
    }
}
