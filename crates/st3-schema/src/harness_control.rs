//! Owner-held input queues and exact native control settlement. No client-side queue state.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub desired_revision: String,
    pub incarnation_id: String,
    pub session_id: String,
    /// A driver-observed native activity generation, not a provider request identifier.
    pub turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane { FollowUp, Steer }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome { Accepted, Dispatched, Applied, Rejected, Indeterminate, Cancelled }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEntry {
    pub id: String,
    pub actor: String,
    pub content: String,
    pub lane: Lane,
    pub binding: Binding,
    pub status: Outcome,
    pub operation_id: String,
    pub reason: Option<String>,
    pub result: Option<NativeResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    pub authority: String,
    pub revision: u64,
    pub entries: Vec<QueueEntry>,
}
impl Default for Queue {
    fn default() -> Self { Self { authority: "owner".into(), revision: 0, entries: Vec::new() } }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueMutation {
    Enqueue { content: String, lane: Lane },
    Move { entry_id: String, before_id: Option<String> },
    Cancel { entry_id: String },
    Replace { entry_id: String, content: String },
    Promote { entry_id: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueRequest {
    pub subject: String,
    pub actor: String,
    pub idempotency_key: String,
    pub binding: Binding,
    pub queue_revision: u64,
    pub mutation: QueueMutation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    pub subject: String,
    pub entry_id: Option<String>,
    pub status: Outcome,
    pub queue_revision: u64,
    pub reason: Option<String>,
    pub result: Option<NativeResult>,
    pub binding: Binding,
}

/// Steering requires a public native consumption fence, absent in the supported runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SteerCapability {
    Unsupported { reason: SteerUnsupportedReason },
}
impl Default for SteerCapability {
    fn default() -> Self {
        Self::Unsupported { reason: SteerUnsupportedReason::NativePreDequeueApiUnavailable }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteerUnsupportedReason {
    NativePreDequeueApiUnavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeState {
    pub subject: String,
    pub binding: Binding,
    pub idle: bool,
    pub input_supported: bool,
    pub steer: SteerCapability,
    pub models: Models,
    pub approval: Approval,
    #[serde(default)]
    pub pending_ask: Option<PendingAsk>,
    #[serde(default)]
    pub ask_supported: bool,
    #[serde(default)]
    pub ask_reason: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputCommand {
    pub operation_id: String,
    pub entry_id: String,
    pub actor: String,
    pub content: String,
    pub lane: Lane,
    pub binding: Binding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReceipt {
    pub subject: String,
    pub binding: Binding,
    pub operation_id: String,
    pub status: Outcome,
    pub reason: Option<String>,
    pub result: Option<NativeResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub reasoning: bool,
    pub supported_efforts: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedModel {
    pub provider: String,
    pub id: String,
    pub effective_effort: Option<String>,
    pub configured_effort: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Models {
    pub choices: Vec<ModelChoice>,
    pub selected: Option<SelectedModel>,
    pub atomic_model_effort: bool,
    pub revision: String,
    pub available: bool,
    pub complete: bool,
    pub source: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub supported: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObservation {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub idle: bool,
    pub input_supported: bool,
    pub steer: SteerCapability,
    pub models: Models,
    pub approval: Approval,
    #[serde(default)]
    pub pending_ask: Option<PendingAsk>,
    #[serde(default)]
    pub ask_supported: bool,
    #[serde(default)]
    pub ask_reason: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueParameters {
    pub subject: String,
    pub binding: Binding,
    pub queue_revision: u64,
    pub mutation: QueueMutation,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueView {
    pub schema: String,
    pub subject: String,
    pub queue: Queue,
    pub native: Option<ControlState>,
    pub cursor: Option<String>,
    pub total: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum NativeResult {
    Input(InputResult),
    Model(ModelResult),
    Ask(AskResult),
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputResult {
    pub native_event: String,
    pub turn_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResult {
    pub provider: Option<String>,
    pub id: Option<String>,
    pub effective_effort: Option<String>,
    pub atomic_model_effort: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelParameters {
    pub subject: String,
    pub binding: Binding,
    pub model_revision: String,
    pub provider: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub actor: String,
    pub idempotency_key: String,
    pub parameters: ModelParameters,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCommand {
    pub operation_id: String,
    pub binding: Binding,
    pub model_revision: String,
    pub provider: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand { Input(InputCommand), SetModel(ModelCommand), AnswerAsk(AskCommand) }
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReceipt {
    pub operation_id: String,
    pub subject: String,
    pub binding: Binding,
    pub model_revision: String,
    pub status: Outcome,
    pub reason: Option<String>,
    pub result: Option<ModelResult>,
}

/// Durable owner operation lookup returns the original queue or model receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ControlOperationReceipt {
    Queue(Receipt),
    Model(ModelReceipt),
    Ask(AskReceipt),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsSummary {
    pub selected: Option<SelectedModel>,
    pub revision: String,
    pub atomic_model_effort: bool,
    pub available: bool,
    pub complete: bool,
    pub source: String,
    pub catalog_path: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlState {
    pub subject: String,
    pub binding: Binding,
    pub idle: bool,
    pub input_supported: bool,
    pub steer: SteerCapability,
    pub models: ModelsSummary,
    pub approval: Approval,
    #[serde(default)]
    pub pending_ask: Option<PendingAskView>,
    #[serde(default)]
    pub ask_supported: bool,
    #[serde(default)]
    pub ask_reason: Option<String>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogPage {
    pub schema: String,
    pub subject: String,
    pub model_revision: String,
    pub selected: Option<SelectedModel>,
    pub atomic_model_effort: bool,
    pub available: bool,
    pub complete: bool,
    pub source: String,
    pub choices: Vec<ModelChoice>,
    pub cursor: Option<String>,
    pub total: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AskOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingAsk {
    pub tool_call_id: String,
    pub questions: Vec<AskQuestion>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingAskView {
    pub ask_ref: String,
    pub tool_call_id: String,
    pub questions: Vec<AskQuestion>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicAskAnswer {
    #[serde(rename = "questionId")]
    pub question_id: String,
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "_tag", deny_unknown_fields)]
pub enum PublicAskParameters {
    Selection { #[serde(rename = "askRef")] ask_ref: String, answers: Vec<PublicAskAnswer> },
    Text { #[serde(rename = "askRef")] ask_ref: String, text: String },
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskAnswer {
    pub id: String,
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskParameters {
    pub subject: String,
    pub binding: Binding,
    pub tool_call_id: String,
    pub answers: Vec<AskAnswer>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    pub actor: String,
    pub idempotency_key: String,
    pub parameters: AskParameters,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskCommand {
    pub operation_id: String,
    pub binding: Binding,
    pub tool_call_id: String,
    pub answers: Vec<AskAnswer>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskResult {
    pub native_event: String,
    pub tool_call_id: String,
    pub answers: Vec<AskAnswer>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AskIndeterminateReason {
    #[serde(rename = "terminal-input-conflict")]
    TerminalInputConflict,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "_tag", deny_unknown_fields)]
pub enum AskOutcome {
    Indeterminate { reason: AskIndeterminateReason },
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskReceipt {
    pub operation_id: String,
    pub subject: String,
    pub binding: Binding,
    pub tool_call_id: String,
    pub status: Outcome,
    pub reason: Option<String>,
    pub result: Option<AskResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AskOutcome>,
}
/// A one-use opaque terminal input token. The native public input hook translates
/// it into a single key only while the exact ask operation owns its input guard.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskTerminalInput {
    pub operation_id: String,
    pub binding: Binding,
    pub tool_call_id: String,
    pub token: String,
    pub question_index: usize,
    pub surface: AskSurface,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AskSurface { Question, Custom, Review }
