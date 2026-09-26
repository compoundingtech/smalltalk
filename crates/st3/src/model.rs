use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_EVAL_TIMEOUT_MS: u64 = 20 * 60 * 1_000;

#[derive(Debug)]
pub struct St3Error {
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Map<String, Value>,
}

impl St3Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: serde_json::Map::new(),
        }
    }

    pub fn with_detail(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.details.insert(name.into(), value.into());
        self
    }
}

impl fmt::Display for St3Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for St3Error {}

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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageSummary {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    pub title: Option<String>,
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnderSpec {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionAuthority {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revise: Vec<String>,
}

impl MissionAuthority {
    pub fn allows(&self, action: &str, mission: &str) -> bool {
        let patterns = match action {
            "publish" => &self.publish,
            "start" => &self.start,
            "revise" => &self.revise,
            _ => return false,
        };
        patterns.iter().any(|pattern| {
            pattern == mission
                || pattern
                    .strip_suffix("/*")
                    .is_some_and(|prefix| mission.starts_with(&format!("{prefix}/")))
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScheduleSpec {
    pub stopped: bool,
    pub host: String,
    pub at_unix_ms: Option<i64>,
    pub every_ms: Option<u64>,
    pub anchor_unix_ms: Option<i64>,
    pub catch_up: String,
    pub max_catch_up: Option<u32>,
    pub work: Option<ScheduledWork>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScheduledWork {
    pub mission: String,
    pub revision: String,
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
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        review_targets: Vec<String>,
    },
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverSpec {
    pub resource: String,
    pub provider: String,
    pub locator: String,
    pub fields: Vec<String>,
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
    pub stopped: bool,
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
pub struct DocumentVersion {
    pub name: String,
    pub hash: String,
    pub size: u64,
    pub created_index: u64,
    pub latest: bool,
    pub binding_claim_id: String,
    #[serde(default)]
    pub created_at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
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
pub struct ClaimInput {
    pub subject: String,
    pub kind: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub expected_subject: Option<Option<String>>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimRecord {
    pub id: String,
    pub store_index: u64,
    pub batch_id: String,
    pub subject: String,
    pub kind: String,
    pub origin: String,
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    pub body: Value,
    pub predecessors: Vec<String>,
    pub accepted_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimsPage {
    pub claims: Vec<ClaimRecord>,
    pub next_cursor: Option<u64>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttentionItemView {
    pub kind: String,
    pub subject: String,
    pub person: String,
    pub title: String,
    pub detail: String,
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
    pub created_index: u64,
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
    pub prompt: Option<String>,
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
    pub status: String,
    pub checks: Vec<DoctorCheck>,
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
    pub status: String,
    pub phase: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    #[serde(default)]
    pub steps: Vec<StepRunView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<LoopRunView>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub under: Vec<UnderSpec>,
    pub worker_reported: bool,
    pub claimant: Option<String>,
    pub claim_incarnation: Option<String>,
    pub claim_expires_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_started_at_unix_ms: Option<u128>,
    #[serde(default)]
    pub execution_elapsed_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_age_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WorkWakeView>,
    pub readiness_epoch: u32,
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    pub not_before_unix_ms: Option<u128>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkWakeRequest {
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

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ReplicaEnvelopeId {
    pub writer: String,
    pub sequence: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaEnvelope {
    pub writer: String,
    pub sequence: u64,
    pub previous_hash: Option<String>,
    pub hash: String,
    pub accepted_at_unix_ms: u128,
    /// Base64-encoded CBOR. Receipt does not decode this field.
    pub payload: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplicationInventory {
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub envelopes: Vec<ReplicaEnvelopeId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExchange {
    pub peer: String,
    pub fleet_id: String,
    pub schema_digest: String,
    pub authority_digest: String,
    pub graph_digest: String,
    pub inventory: ReplicationInventory,
    #[serde(default)]
    pub envelopes: Vec<ReplicaEnvelope>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceipt {
    pub received: usize,
    pub duplicate: usize,
    pub inventory: ReplicationInventory,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExportRequest {
    pub fleet_id: String,
    pub inventory: ReplicationInventory,
    #[serde(default)]
    pub summary_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationExportResponse {
    pub exchange: ReplicationExchange,
    pub store_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceiveRequest {
    pub peer: String,
    pub fleet_id: String,
    pub exchange: ReplicationExchange,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationReceiveResponse {
    pub receipt: ReplicationReceipt,
    pub changed: bool,
    pub store_index: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationPeerFailureRequest {
    pub peer: String,
    pub status: String,
    pub error: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaRecordView {
    pub record_ref: String,
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub position: u64,
    pub state: String,
    pub claim_id: Option<String>,
    pub subject: Option<String>,
    pub kind: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub replacement_claim_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplicationStatus {
    pub configured: bool,
    pub fleet_id: Option<String>,
    pub authority_digest: String,
    pub graph_digest: String,
    pub received_envelopes: u64,
    pub pending_records: u64,
    pub valid_records: u64,
    pub unknown_records: u64,
    pub invalid_records: u64,
    pub repaired_records: u64,
    pub unhealthy_projections: u64,
    pub peers: Vec<ReplicationPeerStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationPeerStatus {
    pub peer: String,
    pub status: String,
    pub last_success_at_unix_ms: Option<u128>,
    pub last_error: Option<String>,
    pub schema_digest: Option<String>,
    pub authority_digest: Option<String>,
    pub graph_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationRepairRequest {
    pub record_ref: String,
    pub replacement_claim_id: String,
    pub reason: String,
    pub actor: String,
    pub idempotency_key: String,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationBatch {
    pub peer: String,
    #[serde(default)]
    pub replica_heads: BTreeMap<String, u64>,
    pub batches: Vec<ReplicaBatch>,
    pub blobs: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaBatch {
    pub id: String,
    pub origin: String,
    pub replica_sequence: u64,
    pub previous_hash: Option<String>,
    pub hash: String,
    pub accepted_at_unix_ms: u128,
    pub claims: Vec<ClaimRecord>,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicationResponse {
    pub accepted_through: u64,
    pub missing_sequences: Vec<u64>,
    #[serde(default)]
    pub accepted_heads: BTreeMap<String, u64>,
    #[serde(default)]
    pub missing_ranges: Vec<ReplicaRange>,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplicaRange {
    pub origin: String,
    pub from: u64,
    pub through: u64,
}
