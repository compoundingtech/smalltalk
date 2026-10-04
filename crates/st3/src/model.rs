use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use smallclaims::Error as St3Error;
pub use smallclaims::claim::{
    ClaimInput, ClaimRecord, ClaimsPage, DocumentVersion, ReplicaBatch, ReplicaEnvelope,
    ReplicaEnvelopeId, ReplicaEnvelopeSignature,
};
#[cfg(test)]
pub use smallclaims::replication::{ReplicaRange, ReplicationBatch, ReplicationResponse};
pub use smallclaims::replication::{
    ClaimRange, ClaimRangeDigest, ClaimSubjectDigest, HealClaim, InventoryCheckpoint,
    ReplicaRecordView, ReplicationExchange, ReplicationExportRequest, ReplicationExportResponse,
    ReplicationFirstSync, ReplicationHealAnswer, ReplicationHealAnswerRequest,
    ReplicationHealNextRequest, ReplicationHealQuery, ReplicationHealReport,
    ReplicationHealRequest, ReplicationHealStep, ReplicationInventory, ReplicationInventoryBucket,
    ReplicationPeerFailureRequest, ReplicationPeerStatus, ReplicationPeerSync, ReplicationReceipt,
    ReplicationReceiveRequest, ReplicationReceiveResponse, ReplicationRepairRequest,
    ReplicationStatus, ReplicationTimings, UnhealthyProjection,
};

pub const MAX_EVAL_TIMEOUT_MS: u64 = 20 * 60 * 1_000;

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiResponse<T> {
    pub api_version: String,
    pub request_id: String,
    pub snapshot_host: String,
    pub store_index: u64,
    pub value: T,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiErrorResponse {
    pub api_version: String,
    pub request_id: String,
    pub snapshot_host: String,
    pub store_index: u64,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberKind {
    Agent,
    Exec,
    Pty,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartType {
    #[default]
    Always,
    OnFailure,
    Never,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberLifecycle {
    #[default]
    Service,
    AdoptOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum LaunchSpec {
    Shell(String),
    Argv(Vec<String>),
}

impl From<&LaunchSpec> for st_runtime::Launch {
    fn from(value: &LaunchSpec) -> Self {
        match value {
            LaunchSpec::Shell(source) => Self::Shell(source.clone()),
            LaunchSpec::Argv(argv) => Self::Argv(argv.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RestartIntensity {
    pub attempts: u32,
    pub interval_ms: u64,
    pub delay_ms: u64,
    pub mode: String,
}

impl Default for RestartIntensity {
    fn default() -> Self {
        Self {
            attempts: 3,
            interval_ms: 60_000,
            delay_ms: 0,
            mode: "delay".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemberSpec {
    pub kind: MemberKind,
    pub host: String,
    pub runtime_id: String,
    pub workspace: String,
    #[serde(default)]
    pub workspace_create: bool,
    pub cwd: String,
    pub terminal: bool,
    pub launch: LaunchSpec,
    pub environment: BTreeMap<String, String>,
    pub tags: BTreeMap<String, String>,
    pub display_name: Option<String>,
    pub lifecycle: MemberLifecycle,
    pub restart: RestartType,
    pub restart_intensity: RestartIntensity,
    pub shutdown_timeout_ms: u64,
    pub driver: Option<String>,
}

impl MemberSpec {
    /// What differs between how this member is declared to launch and how `launched` was:
    /// `host`, `workspace`, `harness`, `terminal` or `launch` (the command, or a typed harness's
    /// model, effort and arguments). The argv st adds to a typed harness itself (its channel and
    /// hook settings), the environment and the restart policy are left out, so a new st build or
    /// a mission run's new generation is not a launch change.
    pub fn launch_changes(&self, launched: &MemberSpec) -> Vec<&'static str> {
        let mut changes = Vec::new();
        if self.host != launched.host {
            changes.push("host");
        }
        if self.workspace != launched.workspace || self.cwd != launched.cwd {
            changes.push("workspace");
        }
        if self.driver != launched.driver {
            changes.push("harness");
        }
        if self.terminal != launched.terminal {
            changes.push("terminal");
        }
        if authored_launch(&self.launch) != authored_launch(&launched.launch) {
            changes.push("launch");
        }
        changes
    }
}

/// A launch without the arguments st puts right after a typed harness's program, past the
/// wrapper's `--`: its channel and its hook settings, which follow st's build, not the author.
pub(crate) fn authored_launch(launch: &LaunchSpec) -> Vec<&str> {
    let argv = match launch {
        LaunchSpec::Shell(source) => return vec![source.as_str()],
        LaunchSpec::Argv(argv) => argv,
    };
    let Some(separator) = argv.iter().position(|argument| argument == "--") else {
        return argv.iter().map(String::as_str).collect();
    };
    let mut authored = argv[..=separator]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut provider = argv[separator + 1..].iter().map(String::as_str);
    authored.extend(provider.next());
    let mut provider = provider.peekable();
    while provider
        .peek()
        .is_some_and(|flag| matches!(*flag, "--channels" | "--settings"))
    {
        provider.next();
        provider.next();
    }
    authored.extend(provider);
    authored
}

/// Presentation is independent of the durable seat identity.
pub fn effective_agent_name<'a>(subject: &'a str, desired: Option<&'a Value>) -> &'a str {
    desired.and_then(|desired| {
        desired.get("display_name").and_then(Value::as_str).or_else(|| {
            desired.get("children")?.as_array()?.iter()
                .find(|child| child.get("name").and_then(Value::as_str) == Some("name"))?
                .get("arguments")?.as_array()?.first()?.as_str()
        })
    })
        .unwrap_or_else(|| subject.strip_prefix("agent/").unwrap_or(subject))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DesiredSubject {
    pub subject: String,
    pub kind: String,
    pub desired: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_step: Option<String>,
}

impl DesiredSubject {
    /// Keep the authored KDL name and its normalized member projection identical.
    pub fn set_display_name(&mut self, name: Option<&str>) -> Result<(), St3Error> {
        let body = self.desired.as_object_mut()
            .ok_or_else(|| St3Error::new("invalid-agent-declaration", "agent has no canonical body"))?;
        if !body.contains_key("children") {
            body.insert("children".into(), serde_json::json!([]));
        }
        let children = body.get_mut("children").and_then(Value::as_array_mut)
            .ok_or_else(|| St3Error::new("invalid-agent-declaration", "agent body must be an array"))?;
        if let Some(name) = name {
            if let Some(child) = children.iter_mut()
                .find(|child| child.get("name").and_then(Value::as_str) == Some("name"))
            {
                child["arguments"] = serde_json::json!([name]);
            } else {
                children.push(serde_json::json!({ "name": "name", "arguments": [name] }));
            }
        } else {
            children.retain(|child| child.get("name").and_then(Value::as_str) != Some("name"));
        }
        if let Some(member) = &mut self.member {
            member.display_name = name.map(str::to_owned);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageSummary {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub incarnation_count: usize,
    pub aggregation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUsage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub compactions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_trigger: Option<String>,
    pub observed_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionState {
    Draft,
    Ready,
    Retired,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RevisionCutover {
    #[default]
    RestartActive,
    WhenIdle,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "dependency", rename_all = "kebab-case")]
pub enum DependencySpec {
    Step { step: String, state: String },
    Predicate { gate: GateSpec },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BaselineSpec {
    pub name: String,
    pub gates: Vec<GateSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProductSpec {
    pub subject: String,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum UsedMissionSpec {
    Revision { mission: String, revision: String },
    StepOutput { step: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetrySpec {
    pub attempts: u32,
    pub backoff_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum MetricSource {
    Gate {
        gate: String,
    },
    Field {
        subject: String,
        path: String,
    },
    Exec {
        command: String,
        host: String,
        workspace: String,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        time_limit_ms: u64,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MetricSpec {
    pub name: String,
    pub direction: String,
    pub min_improvement: f64,
    pub source: MetricSource,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LoopStopSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plateau_metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plateau_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeated_failure: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
#[derive(Default)]
pub enum LoopExhaustionSpec {
    #[default]
    Fail,
    Succeed,
    Human {
        gate: GateSpec,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LoopAttentionSpec {
    pub title: String,
    pub reviewer: String,
    pub severity: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LoopForEachSpec {
    pub resource: String,
    pub field: String,
    pub max_parallel: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "selector", rename_all = "kebab-case")]
pub enum LoopCandidateSelector {
    Metric { metric: String },
    Llm { gate: GateSpec },
    Human { gate: GateSpec },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LoopCandidatesSpec {
    pub count: u32,
    pub max_parallel: u32,
    pub select: LoopCandidateSelector,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LoopSpec {
    pub id: String,
    pub path: String,
    pub max_rounds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<LoopForEachSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<LoopCandidatesSpec>,
    #[serde(default)]
    pub metrics: Vec<MetricSpec>,
    #[serde(default)]
    pub stop: LoopStopSpec,
    #[serde(default)]
    pub until: Vec<GateSpec>,
    pub round: Box<MissionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_keep: Option<Box<MissionSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_discard: Option<Box<MissionSpec>>,
    #[serde(default)]
    pub on_exhausted: LoopExhaustionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhaustion_attention: Option<LoopAttentionSpec>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkSelector {
    Assigned { agent: String },
    Available { agents: Vec<String> },
    Agentless,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CompletionSpec {
    AllStepsExhausted,
    Dependencies { dependencies: Vec<DependencySpec> },
}

impl Default for RetrySpec {
    fn default() -> Self {
        Self {
            attempts: 1,
            backoff_ms: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StepSpec {
    pub id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fresh_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    pub title: Option<String>,
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Constraints from agent blocks declared in this step, keyed by the agent subject template.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_constraints: BTreeMap<String, Vec<String>>,
    pub timeout_ms: Option<u64>,
    pub retry: RetrySpec,
    pub finally: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_selector: Option<WorkSelector>,
    #[serde(default)]
    pub revision_owners: Vec<String>,
    #[serde(default)]
    pub revisions_human_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_reviewer: Option<String>,
    pub dependencies: Vec<DependencySpec>,
    #[serde(default)]
    pub baselines: Vec<BaselineSpec>,
    #[serde(default)]
    pub documents: Vec<String>,
    pub declarations_kdl: Option<String>,
    pub products: Vec<ProductSpec>,
    #[serde(default)]
    pub produces_mission: Option<String>,
    #[serde(default)]
    pub uses_mission: Option<UsedMissionSpec>,
    /// The mission run this agentless step waits for. The step completes when that run
    /// completes and fails when it fails or is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_run: Option<String>,
    pub gates: Vec<GateSpec>,
    pub nested_mission: Option<Box<MissionSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_spec: Option<Box<LoopSpec>>,
    pub definition_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MissionSpec {
    pub id: String,
    pub subject: String,
    pub state: MissionState,
    pub revision: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, MissionInputSpec>,
    #[serde(default = "default_mission_run_limit")]
    pub max_active_runs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub revision_owners: Vec<String>,
    #[serde(default)]
    pub revisions_human_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_reviewer: Option<String>,
    #[serde(default)]
    pub revision_cutover: RevisionCutover,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declarations_kdl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_selector: Option<WorkSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionSpec>,
    pub goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Constraints from agent blocks declared in this mission, keyed by the agent subject template.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_constraints: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub baselines: Vec<BaselineSpec>,
    #[serde(default)]
    pub products: Vec<ProductSpec>,
    #[serde(default)]
    pub gates: Vec<GateSpec>,
    pub steps: BTreeMap<String, StepSpec>,
    pub display_order: Vec<String>,
}

fn default_mission_run_limit() -> Option<u32> {
    Some(1)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionInputKind {
    Text,
    Resource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionInputSpec {
    pub name: String,
    pub kind: MissionInputKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionRunInput {
    pub kind: MissionInputKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GateContext {
    pub subject: String,
    pub name: String,
    pub started_at_unix_ms: u128,
    /// The owner's attempt. A later attempt gets its own mechanical and LLM gate results.
    #[serde(default)]
    pub attempt: u32,
    /// The mission run and generation the gate decides for. A broken gate's attention item names
    /// them and closes when the generation is replaced.
    #[serde(default)]
    pub run: String,
    #[serde(default)]
    pub generation: String,
    /// Whether the gate decides for an eval run, whose exec gates keep their verdicts: any
    /// status but 0 fails the boundary.
    #[serde(default)]
    pub eval: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnderSpec {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScheduleSpec {
    pub stopped: bool,
    pub host: String,
    pub at_unix_ms: Option<i64>,
    pub every_ms: Option<u64>,
    pub anchor_unix_ms: Option<i64>,
    pub calendar: Option<CalendarSchedule>,
    pub catch_up: String,
    pub max_catch_up: Option<u32>,
    pub work: Option<ScheduledWork>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CalendarSchedule {
    /// Minutes since local midnight.
    pub at_minute: u16,
    /// ISO weekday (Monday = 1), or none for daily.
    pub weekday: Option<u8>,
    pub timezone: String,
}


#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScheduledWork {
    pub mission: String,
    pub revision: Option<String>,
    pub workspace: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuantifiedFieldSpec {
    pub path: String,
    pub operator: String,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "predicate", rename_all = "kebab-case")]
pub enum SubscriptionConditionSpec {
    Field {
        path: String,
        operator: String,
        value: Value,
    },
    Every {
        path: String,
        fields: Vec<QuantifiedFieldSpec>,
    },
    NotEvery {
        path: String,
        fields: Vec<QuantifiedFieldSpec>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "predicate", rename_all = "kebab-case")]
pub enum GateSpec {
    Exists {
        name: String,
        subject: String,
    },
    Empty {
        name: String,
        subject: String,
    },
    Field {
        name: String,
        path: String,
        subject: String,
        operator: String,
        value: Value,
    },
    Every {
        name: String,
        path: String,
        subject: String,
        fields: Vec<QuantifiedFieldSpec>,
    },
    NotEvery {
        name: String,
        path: String,
        subject: String,
        fields: Vec<QuantifiedFieldSpec>,
    },
    Has {
        name: String,
        subject: String,
        text: String,
    },
    Lacks {
        name: String,
        subject: String,
        text: String,
    },
    Deadline {
        name: String,
        duration_ms: u64,
    },
    Mechanical {
        name: String,
        command: String,
        host: String,
        workspace: String,
        environment: BTreeMap<String, String>,
        time_limit_ms: u64,
    },
    Llm {
        name: String,
        model: String,
        host: String,
        workspace: String,
        tools: Vec<String>,
        environment: BTreeMap<String, String>,
        token_budget: u64,
        time_limit_ms: u64,
        prompt: String,
    },
    Human {
        name: String,
        reviewer: String,
        // Preserve absence in older mission claims: their revision was hashed before
        // human gates carried a mode. Runtime evaluation treats None as approve.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        review_targets: Vec<String>,
    },
}

fn default_human_gate_mode() -> String {
    "approve".into()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NormalizedIntent {
    pub schema: String,
    pub source_hash: String,
    pub subjects: BTreeMap<String, DesiredSubject>,
    #[serde(default)]
    pub missions: BTreeMap<String, MissionSpec>,
    #[serde(default)]
    pub mission_runs: BTreeMap<String, MissionRunDeclaration>,
    #[serde(default)]
    pub planning_sessions: BTreeMap<String, PlanningSessionDeclaration>,
    #[serde(default)]
    pub resource_refreshes: Vec<ResourceRefreshOperation>,
    #[serde(default)]
    pub replica_repairs: Vec<ReplicaRepairDeclaration>,
    pub document_refs: BTreeSet<String>,
    /// Public input spellings accepted only during a bounded vocabulary transition.
    #[serde(default)]
    pub deprecated_syntax: BTreeSet<String>,
    pub normalized: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplicaRepairDeclaration {
    pub record_ref: String,
    pub replacement_claim_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedCancellation {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionRunCreation {
    pub mission: String,
    pub revision: String,
    pub workspace: String,
    pub requester: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub mode: String,
    /// The mission run that must complete before this run's work starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionRevisionOperation {
    pub id: String,
    pub mission: String,
    pub revision: String,
    pub from_generation: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<NamedCancellation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeResetOperation {
    pub id: String,
    pub runtime: String,
    pub from_generation: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionRunDeclaration {
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation: Option<MissionRunCreation>,
    #[serde(default)]
    pub revisions: BTreeMap<String, MissionRevisionOperation>,
    #[serde(default)]
    pub resets: BTreeMap<String, RuntimeResetOperation>,
    #[serde(default)]
    pub cancellations: BTreeMap<String, NamedCancellation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannerSpec {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Default for PlannerSpec {
    fn default() -> Self {
        Self {
            provider: "codex".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("medium".into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanningSessionCreation {
    pub mission: String,
    pub request: String,
    pub workspace: String,
    pub requester: String,
    pub planner: PlannerSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_generation: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanningFeedbackOperation {
    pub id: String,
    pub document: String,
    pub variant: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanningSessionDeclaration {
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation: Option<PlanningSessionCreation>,
    #[serde(default)]
    pub feedback: BTreeMap<String, PlanningFeedbackOperation>,
    #[serde(default)]
    pub cancellations: BTreeMap<String, NamedCancellation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResourceRefreshOperation {
    pub resource: String,
    pub id: String,
    pub timeout_ms: u64,
}

/// A lane's declared settings, read from its `intent.desired` body.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct LaneSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    pub stopped: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverSpec {
    pub resource: String,
    pub provider: String,
    pub locator: String,
    pub fields: Vec<String>,
    #[serde(default)]
    pub every_ms: Option<u64>,
    pub stopped: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionSpec {
    pub observer: String,
    #[serde(default)]
    pub to: String,
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<SubscriptionConditionSpec>,
    pub delivery: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    /// An item that a live agent owns goes to that agent as one message instead (`owner
    /// "message"`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_message: bool,
    /// The GitHub logins whose mentions the subscription hears.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// The mission's text input that carries one observation's new items (`text "NAME"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_input: Option<String>,
    /// A message delivery that collects new items and sends them together at most this often
    /// (`every "30m"`), and only when something arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_every_ms: Option<u64>,
    /// A seat's watch on one issue or pull request (`delivery "watch"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchSpec>,
    pub stopped: bool,
}

/// What a watch hears: one item of the observed repository, from when the watch began until its
/// optional deadline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WatchSpec {
    pub item: u64,
    pub since_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_unix_ms: Option<u128>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceWatchRequest {
    pub provider: String,
    pub locator: String,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub to: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceUnwatchRequest {
    #[serde(default)]
    pub actor: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceWatchView {
    pub resource: String,
    pub observer: String,
    pub subscription: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceObservationOutcome {
    pub baseline: bool,
    pub changed_fields: Vec<String>,
    #[serde(default)]
    pub observation_claim: Option<String>,
    pub message_subjects: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IntentInput {
    pub kdl: String,
    #[serde(default)]
    pub source_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRequest {
    pub intent: IntentInput,
    #[serde(default)]
    pub at_index: Option<u64>,
}

/// Run each exec gate of a mission file once, now, the way a run would: `st missions check`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GateCheckRequest {
    pub intent: IntentInput,
    /// The workspace `${ST_WORKSPACE}` and relative gate workspaces stand for.
    pub workspace: String,
    /// Values for the mission's inputs. A gate that reads an input without one is unchecked.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct GateCheckView {
    pub id: String,
    /// The host that ran the checks. A gate declared for another host is `unchecked`.
    pub host: String,
    /// Whether every gate has its answer.
    pub finished: bool,
    pub gates: Vec<GateCheckItemView>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct GateCheckItemView {
    pub mission: String,
    /// What the gate decides for: `mission`, `step PATH`, or `loop PATH`.
    pub owner: String,
    pub gate: String,
    pub host: String,
    pub workspace: String,
    pub command: String,
    /// `waiting`, `running`, `pass`, `not-yet`, `broken`, or `unchecked`.
    pub answer: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// Why the gate is broken or unchecked.
    #[serde(default)]
    pub reason: Option<String>,
    /// The end of the check's output.
    #[serde(default)]
    pub output: String,
    /// The refusals and partial listings the check's `st` commands reported.
    #[serde(default)]
    pub calls: Vec<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubjectChange {
    pub subject: String,
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_revision: Option<String>,
    pub new_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlannedAction {
    pub subject: String,
    pub action: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionResponse {
    pub store_index: u64,
    pub source_hash: String,
    pub normalized: Value,
    pub resolved_intent: IntentInput,
    pub changes: Vec<SubjectChange>,
    pub predicted_actions: Vec<PlannedAction>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub subject_tokens: BTreeMap<String, Vec<String>>,
    pub mission_revisions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningSessionStartRequest {
    pub mission: String,
    #[serde(default)]
    pub run: Option<String>,
    pub request: Vec<u8>,
    pub workspace: String,
    #[serde(default)]
    pub requester: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningCandidateSubmitRequest {
    pub actor: String,
    pub markdown: Vec<u8>,
    pub kdl: Vec<u8>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningRevisionRequest {
    pub actor: String,
    pub feedback: Vec<u8>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningApprovalRequest {
    pub actor: String,
    pub preview_hash: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchDecisionType {
    Boolean,
    SingleChoice,
    MultipleChoice,
    Rank,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaunchDecisionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum LaunchDecisionResponse {
    Boolean(bool),
    SingleChoice(String),
    MultipleChoice(Vec<String>),
    Rank(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchDecisionRequest {
    pub actor: String,
    pub question: String,
    pub decision_type: LaunchDecisionType,
    #[serde(default)]
    pub options: Vec<LaunchDecisionOption>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchDecisionAnswerRequest {
    pub actor: String,
    pub response: LaunchDecisionResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub expected_revision: u32,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchStartRequest {
    pub actor: String,
    pub workspace: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchApproveAndStartRequest {
    pub actor: String,
    pub preview_hash: String,
    pub workspace: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchApproveAndStartView {
    pub launch: PlanningSessionView,
    pub mission_run: MissionRunView,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningCancelRequest {
    pub actor: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningCandidateView {
    pub variant: String,
    pub revision: u32,
    pub markdown: String,
    pub kdl: String,
    pub mission: String,
    pub mission_revision: String,
    pub submitted_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningPreviewView {
    pub variant: String,
    pub hash: String,
    pub candidate_revision: u32,
    pub store_index: u64,
    pub graph: String,
    pub diff: String,
    pub mission: MissionResponse,
    pub created_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningSessionView {
    pub subject: String,
    pub id: String,
    pub mission: String,
    pub request: String,
    pub workspace: String,
    pub requester: String,
    pub planner: String,
    /// Immutable harness choice made when this launch was created.
    #[serde(default)]
    pub planner_config: PlannerSpec,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_mission_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<PlanningCandidateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PlanningPreviewView>,
    #[serde(default)]
    pub variants: Vec<PlanningVariantView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision: Option<String>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningVariantView {
    pub name: String,
    pub candidate: PlanningCandidateView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PlanningPreviewView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanningProposalRequest {
    pub actor: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApplyRequest {
    pub intent: IntentInput,
    pub expected_subjects: BTreeMap<String, Vec<String>>,
    pub idempotency_key: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApplyResponse {
    pub changed: bool,
    pub store_index: u64,
    pub batch_id: Option<String>,
    pub claim_ids: Vec<String>,
    pub subject_tokens: BTreeMap<String, Vec<String>>,
    pub reconcile_subjects: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolved_kdl: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<PlannedAction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DocumentPutRequest {
    pub name: String,
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub expected_document: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DocumentListResponse {
    pub items: Vec<DocumentVersion>,
    pub has_more: bool,
    pub limit: usize,
    pub history: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GateResultRequest {
    pub operation_capability: String,
    pub verdict: String,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvalStatus {
    pub mission_run: String,
    pub lifecycle: String,
    pub phase: String,
    pub active_steps: Vec<String>,
    pub verdict: Option<String>,
    pub cleanup: String,
    pub store_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusResponse {
    pub store_index: u64,
    pub subjects: Vec<SubjectStatus>,
    pub pending_actions: Vec<PlannedAction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationalAnnotation {
    pub layer: String,
    pub actionable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_incarnation: Option<String>,
}

impl Default for OperationalAnnotation {
    fn default() -> Self {
        Self {
            layer: "current".into(),
            actionable: true,
            reasons: Vec::new(),
            owner_generation: None,
            runtime_incarnation: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubjectStatus {
    pub subject: String,
    pub kind: Option<String>,
    pub desired_token: Option<String>,
    pub desired_revision: Option<String>,
    pub desired: Option<Value>,
    pub actual: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_claim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<CurrentHarnessView>,
    pub conflicts: Vec<String>,
    pub claims: Vec<String>,
    pub owner_run: Option<String>,
    pub gap: Option<String>,
    pub reachability: String,
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub under: Vec<UnderSpec>,
    #[serde(default)]
    pub projection: OperationalAnnotation,
}

/// The status conditions `st trace wait --for` accepts, which an attention request can also use
/// as its `until` condition.
pub const STATUS_WAIT_CONDITIONS: &[&str] = &[
    "running",
    "ready",
    "standing",
    "completed",
    "failed",
    "cancelled",
    "delivered",
    "terminal",
    "exited",
    "stopped",
];

/// The status a subject's actual projection reports.
pub fn projected_actual_status(actual: Option<&Value>) -> Option<&str> {
    let fields = actual.map(|actual| actual.get("fields").unwrap_or(actual))?;
    fields
        .get("status")
        .or_else(|| fields.pointer("/facts/status"))
        .and_then(Value::as_str)
}

/// Whether a subject's status meets one of [`STATUS_WAIT_CONDITIONS`]. `None` is a subject the
/// graph does not know.
pub fn status_wait_condition_holds(condition: &str, status: Option<&SubjectStatus>) -> bool {
    let actual_status = projected_actual_status(status.and_then(|item| item.actual.as_ref()));
    match condition {
        "running" => matches!(actual_status, Some("running" | "ready")),
        "ready" => actual_status == Some("ready"),
        "standing" => actual_status == Some("standing"),
        "completed" => actual_status == Some("completed"),
        "failed" => actual_status == Some("failed"),
        "cancelled" => actual_status == Some("cancelled"),
        "delivered" => actual_status == Some("delivered"),
        "terminal" => matches!(actual_status, Some("completed" | "failed" | "cancelled")),
        "exited" => actual_status == Some("exited"),
        "stopped" => {
            status.is_none_or(|item| item.actual.is_none())
                || matches!(actual_status, Some("stopped" | "removed"))
        }
        _ => false,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientPageInfo {
    pub limit: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_expires_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientResourcePage {
    pub kind: String,
    pub collection: String,
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    pub items: Vec<Value>,
    pub page: ClientPageInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<ClientSyncNotice>,
    /// Present on a list another host owns: whether the owner answered, or this host's replica
    /// stood in for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicated: Option<ClientReplicated>,
}

/// Where a page about another host's agent came from and whether it can be missing recent
/// items.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientReplicated {
    pub owner_host_id: String,
    /// `owner` when the owner answered, `replica` when this host's copy stood in.
    pub source: String,
    /// False when the page is a replica that may lack what the owner holds.
    pub complete: bool,
    /// `current` (the owner answered), `lagging` (this host is catching up with the owner's
    /// fleet) or `unverified` (the owner could not be asked).
    pub state: String,
    /// Why the owner could not be asked, such as `no-route` or `timed-out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Present on every page while this host is catching up with a peer, because its projections
/// can then show early history as current, and while its graph has diverged from a peer's,
/// because they can then be wrong.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientSyncNotice {
    /// `diverged` when any peer has diverged, else `catching-up`.
    pub state: String,
    pub peers: Vec<ClientSyncPeer>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientSyncPeer {
    pub host_id: String,
    pub peer_only_envelopes: u64,
    pub local_only_envelopes: u64,
    pub last_exchange_at: Option<String>,
    pub estimated_catch_up_seconds: Option<u64>,
    /// Since when this host and the peer hold the same envelopes but project different graphs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diverged_since: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CurrentHarnessView {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    pub incarnation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_buffer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
    pub claim: String,
    pub observed_at_unix_ms: u128,
}

impl CurrentHarnessView {
    pub fn is_ready(&self) -> bool {
        matches!(self.state.as_str(), "ready" | "working" | "idle")
            && self.reason.as_deref() != Some("providerAuth")
    }
}

#[cfg(test)]
mod current_harness_view_tests {
    use super::CurrentHarnessView;

    fn harness(reason: Option<&str>) -> CurrentHarnessView {
        CurrentHarnessView {
            state: "idle".into(),
            driver: Some("claude".into()),
            incarnation_id: "worker-one".into(),
            transport: Some("claude-channel".into()),
            reason: reason.map(str::to_owned),
            blocked_on: None,
            ask: None,
            input_buffer: None,
            exit: None,
            claim: "claim/one".into(),
            observed_at_unix_ms: 1,
        }
    }

    #[test]
    fn provider_auth_idle_is_not_ready_for_delivery() {
        assert!(!harness(Some("providerAuth")).is_ready());
        assert!(harness(Some("channelInitialized")).is_ready());
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeResetRequest {
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeResetView {
    pub subject: String,
    pub desired_token: String,
    pub incarnation_id: String,
    pub reset_claim: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceRefreshRequest {
    pub timeout_ms: u64,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceRefreshView {
    pub resource: String,
    pub observers: Vec<String>,
    pub changed: bool,
    pub completed_at_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventRecord {
    pub store_index: u64,
    pub kind: String,
    pub subject: String,
    pub body: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewRequest {
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub expected_subject: Option<Option<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HumanReviewView {
    pub operation: String,
    pub request: String,
    pub owner: String,
    pub mission: String,
    pub mission_run: String,
    pub generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub reviewer: String,
    #[serde(default = "default_human_gate_mode")]
    pub mode: String,
    pub question: String,
    #[serde(default)]
    pub review_targets: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    pub attempt: u32,
    pub requested_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionActionView {
    pub label: String,
    pub argv: Vec<String>,
}

/// A current fault and the agent that owns it. No fault waits on a person.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FaultView {
    /// The agent assigned to the failed step, else the run's requester when that is an agent,
    /// else the fleet's fault agent: a live agent declared with `handles-faults`. None when no
    /// agent can take it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(flatten)]
    pub item: AttentionItemView,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionItemView {
    #[serde(default)]
    pub episode: String,
    #[serde(default)]
    pub priority: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_mode: Option<String>,
    pub subject: String,
    pub person: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub title: String,
    pub detail: String,
    /// The structured request a person-step ask carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    pub requested_at_unix_ms: u128,
    #[serde(default)]
    pub actions: Vec<AttentionActionView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionRequest {
    pub reviewer: String,
    pub title: String,
    pub reason: String,
    pub severity: String,
    #[serde(default)]
    pub targets: Vec<String>,
    pub actor: String,
    pub idempotency_key: String,
}

/// An attention request as it is posted, with what closes it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionRequestPost {
    #[serde(flatten)]
    pub request: AttentionRequest,
    #[serde(flatten)]
    pub closing: AttentionClosing,
}

/// What closes an attention request besides a target that can end. A request names at least one
/// of these or such a target; an agent's request that names none closes when the step that agent
/// has claimed ends.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AttentionClosing {
    /// One of [`STATUS_WAIT_CONDITIONS`]. The daemon resolves the request once every target
    /// meets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// A `step-run/` subject. The daemon resolves the request once that step completes, fails or
    /// is cancelled, starts another attempt, or leaves the run's current generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `person` when only a person closes the request. `st` when the daemon closes it once the
    /// condition that raised it clears; only the daemon itself raises those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<String>,
}

/// One mission request that a subscription recorded, with its current disposition: `pending`,
/// legacy `held` awaiting automatic migration, `started`, `cancelled`, or `failed` when its run
/// could not be created.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SubscriptionRequestView {
    pub request: String,
    pub subscription: String,
    pub resource: String,
    pub mission: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_run: Option<String>,
    pub requested_at_unix_ms: u128,
}

/// A person's decision to release or cancel one subscription request.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubscriptionRequestDecision {
    pub actor: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionResolveRequest {
    pub outcome: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionWithdrawRequest {
    pub reason: String,
    pub actor: String,
    pub idempotency_key: String,
}

/// What one target of a fault is doing now, so a person can recognize a leftover request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AttentionTargetState {
    pub id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix_ms: Option<u128>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionRequestView {
    pub subject: String,
    pub request: String,
    pub reviewer: String,
    pub title: String,
    pub reason: String,
    pub severity: String,
    #[serde(default)]
    pub targets: Vec<String>,
    pub actor: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
    pub requested_at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The step whose end closes the request, and its attempt when the request was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_attempt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<String>,
}

/// One file a message carries, by reference. The claim holds this record and never the bytes:
/// they stay on `origin`, the member that took the upload, and travel only to the members that
/// read or deliver the message, one direct hop at a time. `sha256` is their hash in hex.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MessageAttachment {
    pub sha256: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub size: u64,
    /// The member holding the bytes, such as `host/laptop`.
    pub origin: String,
}

/// An attachment as a sender names it: an upload it made, `blob/<sha256>`, with the type it gave.
/// The daemon completes it into a [`MessageAttachment`] with the size and the member holding it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttachmentInput {
    pub blob: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessageSendRequest {
    pub idempotency_key: String,
    pub from: String,
    pub to: String,
    pub content: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentInput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessageLifecycleRequest {
    pub lifecycle: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub expected_subject: Option<Option<String>>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessageView {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub content: String,
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageAttachment>,
    pub created_index: u64,
}

/// The daemon's answer to a message send, and what a send's idempotency key landed as. Older
/// daemons answer a send with the bare message, which reads as a new send with no time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessageSendReceipt {
    #[serde(flatten)]
    pub message: MessageView,
    /// The key that names this message: a request with the same key and content returns it again.
    #[serde(default)]
    pub idempotency_key: String,
    /// The key already named this message when the request came, so nothing new was sent.
    #[serde(default)]
    pub already_sent: bool,
    /// When this daemon first accepted the message, in RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessagePage {
    pub items: Vec<MessageView>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub limit: usize,
}

#[cfg(test)]
mod message_view_compatibility_tests {
    use super::MessageView;
    use serde_json::json;

    #[test]
    fn reads_older_optional_fields_and_newer_unknown_fields_without_losing_large_indices() {
        let view: MessageView = serde_json::from_value(json!({
            "subject": "message/compat",
            "from": "agent/sender",
            "to": "agent/receiver",
            "content": "hello",
            "status": "sent",
            "created_index": u64::MAX,
            "future_server_field": { "enabled": true }
        }))
        .unwrap();
        assert_eq!(view.created_index, u64::MAX);
        assert_eq!(view.title, None);
        assert_eq!(view.in_reply_to, None);
        assert!(view.tags.is_empty());
        assert_eq!(
            serde_json::to_value(view).unwrap()["created_index"],
            json!(u64::MAX)
        );
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuickAgentRequest {
    pub subject: String,
    pub worktree: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub expected_subject: Vec<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuickAgentResponse {
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    pub runtime_id: String,
    pub event_cursor: u64,
    pub incarnation_id: Option<String>,
    pub ready: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attachment {
    pub subject: String,
    pub runtime_id: String,
    pub incarnation_id: Option<String>,
    pub capability: String,
    pub websocket_path: String,
    pub expires_at_unix_ms: u128,
}

/// A running terminal that this daemon owns on its own host: the PTY session a local attach
/// connects to directly, with no WebSocket bridge through the daemon and no graph write.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalTerminal {
    pub subject: String,
    pub runtime_id: String,
    /// The graph's incarnation, `DAEMON_PID:CREATED_AT`, which the PTY itself must prove.
    pub incarnation_id: String,
    /// The daemon's PTY root as an absolute path.
    pub pty_root: std::path::PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AttachRequest {
    #[serde(default = "default_terminal_rows")]
    pub rows: u16,
    #[serde(default = "default_terminal_columns")]
    pub columns: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextClearRequest {
    pub expected_incarnation: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSignalRequest {
    pub expected_incarnation: String,
    pub signal: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionInputMode {
    Line,
    Raw,
    Key,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionInputRequest {
    pub expected_incarnation: String,
    pub mode: SessionInputMode,
    pub value: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionLogChunk {
    pub subject: String,
    pub runtime_id: String,
    pub generation_id: String,
    pub previous: bool,
    pub start_offset: u64,
    pub next_offset: u64,
    pub data_base64: String,
    pub eof: bool,
    pub status: String,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionScreen {
    pub subject: String,
    pub runtime_id: String,
    pub incarnation_id: String,
    pub screen: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DoctorReport {
    /// Build identity of the responding daemon, absent on older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_version: Option<String>,
    pub status: String,
    pub checks: Vec<DoctorCheck>,
    #[serde(default)]
    pub performance: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationalRepairItem {
    pub id: String,
    pub class: String,
    pub subject: String,
    pub affected_subjects: Vec<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationalRepairPlan {
    pub api_version: String,
    pub token: String,
    pub snapshot_index: u64,
    pub status: String,
    pub items: Vec<OperationalRepairItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationalRepairApplyRequest {
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationalRepairResult {
    pub api_version: String,
    pub token: String,
    pub applied: usize,
    pub already_applied: bool,
    pub affected_subjects: Vec<String>,
    pub claim_ids: Vec<String>,
    pub receipt_claim_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionControlResponse {
    pub subject: String,
    pub request_claim_id: String,
    pub result_claim_id: String,
    pub event_cursor: u64,
}

fn default_terminal_rows() -> u16 {
    24
}

fn default_terminal_columns() -> u16 {
    80
}

#[derive(Clone, Debug)]
pub struct Capability {
    pub kind: String,
    pub subject: String,
    pub incarnation_id: Option<String>,
    pub expires_at_unix_ms: u128,
    pub used: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvalStartRequest {
    pub name: String,
    pub bundle_hash: String,
    pub bundle: Vec<u8>,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvalStartResponse {
    pub event_cursor: u64,
    pub mission_run: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRunRequest {
    pub mission: String,
    #[serde(default)]
    pub revision: Option<String>,
    pub workspace: String,
    #[serde(default)]
    pub requester: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionDefinitionView {
    pub mission: MissionSpec,
    pub updated_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRunView {
    pub subject: String,
    pub id: String,
    pub mission: String,
    pub generation: String,
    pub initial_revision: String,
    pub revision: String,
    pub root_revision: String,
    pub root_mission_run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_step_run: Option<String>,
    pub workspace: String,
    pub requester: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, MissionRunInput>,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at_unix_ms: Option<u128>,
    /// The mission run that must complete before this run's work starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub status: String,
    pub phase: String,
    /// The outcome a person or an authorized agent set after the run finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<MissionRunOutcomeView>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    #[serde(default)]
    pub steps: Vec<StepRunView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<LoopRunView>,
    /// Unresolved exit-code field gates on terminal execs, computed for mission details.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stuck_gates: Vec<String>,
}

/// Who set a finished run's outcome, from what, and why.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionRunOutcomeView {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_status: Option<String>,
    pub reason: String,
    pub actor: String,
    pub at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoopRunView {
    pub subject: String,
    pub id: String,
    pub path: String,
    pub step_run: String,
    pub mode: String,
    pub status: String,
    pub round: u32,
    pub max_rounds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_round: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub best_metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<LoopRoundView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoopRoundView {
    pub claim: String,
    pub round: u32,
    pub status: String,
    pub mission_run: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub token_usage: u64,
    pub recorded_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepRunView {
    pub subject: String,
    pub run: String,
    pub generation: String,
    pub step: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fresh_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    pub definition_hash: String,
    pub status: String,
    pub attempt: u32,
    pub assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_to: Vec<String>,
    pub agentless: bool,
    pub title: Option<String>,
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Responses to person asks: this attempt's asks on a step that asked, or the step's own
    /// response on a person ask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub person_answers: Vec<PersonAnswerView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub under: Vec<UnderSpec>,
    pub worker_reported: bool,
    pub claimant: Option<String>,
    pub claim_incarnation: Option<String>,
    pub claim_expires_at_unix_ms: Option<u128>,
    /// The seat whose claim a revision dropped when it carried this ready step
    /// into the current generation, until that seat claims it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_claimant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_started_at_unix_ms: Option<u128>,
    #[serde(default)]
    pub execution_elapsed_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// What `work extend` has added to this attempt's execution budget.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timeout_extension_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_age_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WorkWakeView>,
    /// The latest `work progress` summary for the current attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_at_unix_ms: Option<u128>,
    /// The `work complete` summary for the current attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_summary: Option<String>,
    pub readiness_epoch: u32,
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    pub not_before_unix_ms: Option<u128>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
}

pub fn fresh_context_operation(step: &StepRunView) -> String {
    format!(
        "fresh-context:{}:{}:{}",
        step.subject, step.attempt, step.readiness_epoch
    )
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkWakeView {
    pub assignee: String,
    pub assignee_state: String,
    pub incarnation_id: String,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// One agent seat's current claim and its ordered queue of mission runs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeatQueueView {
    pub agent: String,
    pub current_work_ids: Vec<String>,
    pub next_work_id: Option<String>,
    pub runs: Vec<SeatQueueRunView>,
    /// The most recent moves, newest first.
    pub moves: Vec<SeatQueueMoveView>,
    pub move_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeatQueueRunView {
    pub run: String,
    pub position: u32,
    /// `claimed` when the seat holds a step in this run, `ready` when it has a
    /// ready step for the seat, and `waiting` otherwise.
    pub state: String,
    pub run_status: String,
    pub joined_at_unix_ms: u128,
    pub claimed_work_ids: Vec<String>,
    pub ready_work_ids: Vec<String>,
    pub waiting_work_ids: Vec<String>,
    /// The mission run this run waits for before any of its work can start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeatQueueMoveView {
    pub claim_id: String,
    pub run: String,
    pub placement: String,
    pub anchor: Option<String>,
    pub actor: Option<String>,
    pub reason: Option<String>,
    pub moved_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SeatQueueMoveRequest {
    pub agent: String,
    pub run: String,
    pub placement: String,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

/// One lane as st shows it: its declaration and its entries in order.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LaneView {
    pub subject: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    /// False once its run ended or a revision dropped it.
    pub open: bool,
    /// The newest lane claim, or `empty` before the first one; it changes with every claim.
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_unix_ms: Option<u128>,
    pub entries: Vec<crate::lane::Entry>,
    /// Joins, leaves, moves, and approvals, newest first.
    pub recent: Vec<crate::lane::Recent>,
}

/// One change to a lane. `change` is `join`, `leave`, `move`, `mark`, or `approve`; the other
/// optional fields belong to the change that uses them.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaneChangeRequest {
    pub lane: String,
    pub change: String,
    pub entry: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub placement: Option<String>,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

/// The claim a lane change recorded, or none when it changed nothing, and the lane after it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaneChangeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimRecord>,
    pub lane: LaneView,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkRequest {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub incarnation: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub idempotency_key: String,
}

/// `work extend`: add `by_ms` to the execution budget of the attempt the actor holds.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkExtendRequest {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub incarnation: Option<String>,
    pub by_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersonAskRequest {
    #[serde(skip)]
    #[doc(hidden)]
    pub legacy_request: Option<String>,
    pub person: String,
    pub title: String,
    pub reason: String,
    pub actor: String,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub new_run: Option<String>,
    #[serde(default)]
    pub incarnation: Option<String>,
    /// A structured request (`crate::person_request::StructuredRequest`). Without one, the ask
    /// is free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersonStepResponse {
    pub subject: String,
    pub actor: String,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub episode: Option<String>,
    /// A named answer or text for a structured request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<crate::person_request::AnswerInput>,
    pub idempotency_key: String,
}

/// A person's response to an ask, as data: the typed answer when the ask was structured.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PersonAnswerView {
    pub ask: String,
    /// `completed` when the person answered, `cancelled` when the ask was withdrawn.
    pub status: String,
    pub summary: String,
    pub respondent: String,
    pub answered_at_unix_ms: u128,
    /// The typed answer: `type`, `outcome`, and `id`, `label` and `text` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkWakeRequest {
    pub actor: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRunOutcomeRequest {
    pub actor: String,
    pub status: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRetireRequest {
    pub actor: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkRetryRequest {
    pub actor: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionRevisionRequest {
    pub intent: IntentInput,
    pub actor: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunGenerationView {
    pub subject: String,
    pub id: String,
    pub run: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<String>,
    pub status: String,
    pub actor: String,
    pub reason: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    #[serde(default)]
    pub steps: Vec<StepRunView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RevisionProposalView {
    pub subject: String,
    pub id: String,
    pub run: String,
    pub source_generation: String,
    pub candidate_revision: String,
    pub actor: String,
    pub reason: String,
    pub status: String,
    pub cutover: RevisionCutover,
    #[serde(default)]
    pub compatible_steps: Vec<String>,
    #[serde(default)]
    pub reviewers: Vec<String>,
    #[serde(default)]
    pub approvals: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_generation: Option<String>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RevisionSubmissionView {
    pub status: String,
    pub mission_run: MissionRunView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<RevisionProposalView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RevisionApprovalRequest {
    pub actor: String,
    pub preview_hash: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RevisionCancelRequest {
    pub actor: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionProductionRequest {
    pub intent: IntentInput,
    pub actor: String,
    #[serde(default)]
    pub incarnation: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MissionOutputView {
    pub step: String,
    pub mission: String,
    pub revision: String,
    pub claim_id: String,
}

#[cfg(test)]
mod launch_change_tests {
    use super::{LaunchSpec, MemberSpec};

    fn claude(settings: &str, model: &str) -> MemberSpec {
        let intent = crate::graph::parse_intent(
            &format!(
                "version 2\nagent \"example/worker\" {{ workspace \"/work\"; harness \"claude\" {{ model {model:?}; }} }}"
            ),
            "example-host",
        )
        .unwrap();
        let mut member = intent.subjects["agent/example/worker"]
            .member
            .clone()
            .unwrap();
        // Stand in for another st build's hook registration.
        if let LaunchSpec::Argv(argv) = &mut member.launch {
            let at = argv.iter().position(|item| item == "--settings").unwrap();
            argv[at + 1] = settings.into();
        }
        member
    }

    #[test]
    fn only_what_the_author_declares_changes_a_launch() {
        let launched = claude("{\"hooks\":{}}", "example-model");
        // Another st build's hook settings, a new run generation's environment and a new
        // restart policy launch the same harness.
        let mut same = claude("{\"hooks\":{\"Stop\":[]}}", "example-model");
        same.environment
            .insert("ST_RUN_GENERATION".into(), "next".into());
        same.restart = super::RestartType::Never;
        assert!(same.launch_changes(&launched).is_empty());

        assert_eq!(
            claude("{\"hooks\":{}}", "another-model").launch_changes(&launched),
            ["launch"]
        );
        let mut moved = launched.clone();
        moved.workspace = "/elsewhere".into();
        moved.cwd = "/elsewhere".into();
        assert_eq!(moved.launch_changes(&launched), ["workspace"]);
        let mut switched = launched.clone();
        switched.driver = Some("codex".into());
        assert_eq!(switched.launch_changes(&launched), ["harness"]);
        let mut command = launched.clone();
        command.launch = LaunchSpec::Shell("sleep 1".into());
        let mut other = command.clone();
        other.launch = LaunchSpec::Shell("sleep 2".into());
        assert_eq!(other.launch_changes(&command), ["launch"]);
    }
}
