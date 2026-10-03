//! Controlled Codex app-server launch and persistent thread ownership.
//!
//! Native delivery cannot infer a thread from cwd, process, PTY, or `thread/list`. This module
//! starts a dedicated provider daemon, initializes an observer connection before the interactive
//! client starts, and binds a typed start notification or successful resume response to the exact
//! wrapper process incarnation that owns the PTY launch. On resume, the owning TUI must first make
//! the preserved thread visible in the provider's loaded-thread inventory. Its control watcher persists
//! delivery-relevant thread and turn state. The native delivery layer selects one durable FIFO
//! inbox head and submits typed input only when that state proves an idle or one exact regular
//! active turn.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tungstenite::{Message as WebSocketMessage, WebSocket};

// Bound unexpected provider frames independently of the size of a saved thread.
const CODEX_CONTROL_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

use crate::provider_session::ProviderProcess;
use crate::{
    delivery_ledger, ding, driver_diagnostic, harness_context, harness_state, message, run, status,
};

const REQUIRED_CODEX_CLIENT_REQUESTS: &[&str] = &[
    "hooks/list",
    "initialize",
    "thread/loaded/list",
    "thread/read",
    "thread/resume",
    "turn/start",
    "turn/steer",
];
const REQUIRED_CODEX_CLIENT_NOTIFICATIONS: &[&str] = &["initialized"];
const REQUIRED_CODEX_SERVER_NOTIFICATIONS: &[&str] = &[
    "item/completed",
    "item/started",
    "thread/started",
    "thread/status/changed",
    "turn/completed",
    "turn/started",
];
// The control observer does not answer server requests. A listed request is reviewed and safe to
// ignore. An unlisted request creates a delivery hold until the thread reports a safe status.
const CLASSIFIED_CODEX_SERVER_REQUESTS: &[&str] = &[
    "account/chatgptAuthTokens/refresh",
    "applyPatchApproval",
    "attestation/generate",
    "currentTime/read",
    "execCommandApproval",
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/call",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];
// A listed item is reviewed and safe to ignore unless `observe` handles it explicitly. An unlisted
// item creates a delivery hold until the thread reports a safe status.
const CLASSIFIED_CODEX_THREAD_ITEMS: &[&str] = &[
    "agentMessage",
    "collabAgentToolCall",
    "commandExecution",
    "contextCompaction",
    "dynamicToolCall",
    "enteredReviewMode",
    "exitedReviewMode",
    "fileChange",
    "functionCallOutput",
    "hookPrompt",
    "imageGeneration",
    "imageView",
    "mcpToolCall",
    "plan",
    "reasoning",
    "sleep",
    "subAgentActivity",
    "userMessage",
    "webSearch",
];
const RUNTIME_SCHEMA: &str = "st.codex-runtime.v1";
const BINDING_SCHEMA: &str = "st.codex-thread-binding.v1";
const CONTROL_STATE_SCHEMA: &str = "st.codex-control-state.v1";
const RESIDENCY_CHECKPOINT_SCHEMA: &str = "st.codex-residency-checkpoint.v1";
const RESIDENCY_CHECKPOINT_FILE: &str = "residency-checkpoint.json";
const WRAPPER_DIAGNOSTIC_SCHEMA: &str = "st.codex-wrapper-diagnostic.v1";
const CONTROL_TUI_LOADED_REQUEST_ID: u64 = 0;
const CONTROL_SUBSCRIBE_REQUEST_ID: u64 = 1;
const FIRST_DELIVERY_REQUEST_ID: u64 = 2;
/// A string ID can never collide with the numeric subscription and delivery requests.
const ACCOUNT_READ_REQUEST_ID: &str = "st-account-read";
const HOOK_TRUST_PREFLIGHT_REQUEST_ID: u64 = 1;
// The inner provider result must reach the wrapper before the outer ownership wait expires.
const TUI_LOADED_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_POLL: Duration = Duration::from_millis(100);
const TRANSCRIPT_TURN_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);
const TRANSCRIPT_CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SNAPSHOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const DELIVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REJECTED_DELIVERY_BACKOFF: Duration = Duration::from_secs(5);
const SNAPSHOT_REQUEST_ATTEMPTS: u8 = 3;
const TRANSCRIPT_TURN_RECOVERY_BYTES: u64 = 2 * 1024 * 1024;
const TRANSCRIPT_DISCOVERY_FILE_LIMIT: usize = 10_000;
const INBOX_REFRESH_FALLBACK: Duration = Duration::from_secs(15);
const SOCKET_PATH_BUDGET: usize = 96;

#[derive(Debug)]
struct AppServerExitedBeforeControl(ExitStatus);

impl fmt::Display for AppServerExitedBeforeControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Codex app-server exited before control connected: {}",
            self.0
        )
    }
}

impl std::error::Error for AppServerExitedBeforeControl {}

struct WrapperDiagnostics {
    file: File,
    agent: String,
    runtime_id: String,
}

impl WrapperDiagnostics {
    fn open(state_dir: &Path, agent: &str, runtime_id: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(state_dir.join("wrapper.log"))?;
        Ok(Self {
            file,
            agent: agent.to_string(),
            runtime_id: runtime_id.to_string(),
        })
    }

    fn record(&mut self, stage: &str, detail: Value) -> Result<()> {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis();
        serde_json::to_writer(
            &mut self.file,
            &json!({
                "schema": WRAPPER_DIAGNOSTIC_SCHEMA,
                "unixMs": unix_ms,
                "agent": self.agent,
                "runtimeId": self.runtime_id,
                "stage": stage,
                "detail": detail,
            }),
        )?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexRuntime {
    schema: String,
    agent: String,
    runtime_id: String,
    incarnation: String,
}

impl CodexRuntime {
    fn fresh(agent: String, runtime_id: String) -> Result<Self> {
        Self::with_incarnation(agent, runtime_id, random_token()?)
    }

    fn with_incarnation(agent: String, runtime_id: String, incarnation: String) -> Result<Self> {
        anyhow::ensure!(
            !incarnation.is_empty(),
            "Codex runtime incarnation is empty"
        );
        Ok(Self {
            schema: RUNTIME_SCHEMA.to_string(),
            agent,
            runtime_id,
            incarnation,
        })
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexThreadBinding {
    schema: String,
    agent: String,
    runtime_id: String,
    runtime_incarnation: String,
    thread_id: String,
}

impl CodexThreadBinding {
    fn new(runtime: &CodexRuntime, thread_id: String) -> Self {
        Self {
            schema: crate::contracts::schema_for_owner(&runtime.schema, BINDING_SCHEMA),
            agent: runtime.agent.clone(),
            runtime_id: runtime.runtime_id.clone(),
            runtime_incarnation: runtime.incarnation.clone(),
            thread_id,
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexResidencyCheckpoint {
    schema: String,
    source_generation: crate::residency::Generation,
    resume_generation: crate::residency::Generation,
    binding: CodexThreadBinding,
}

impl CodexResidencyCheckpoint {
    pub fn thread_id(&self) -> &str {
        self.binding.thread_id()
    }
}

/// The latest delivery-relevant state observed on the bound app-server control stream.
///
/// `Active` permits `turn/steer`: its turn ID came from the latest unmatched `turn/started` event.
/// `Idle` and `TerminalError` permit `turn/start`. Every `Held` state blocks delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CodexObservedState {
    AwaitingStatus,
    Idle,
    TerminalError {
        reason: CodexTerminalError,
    },
    Active {
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    Held {
        reason: CodexHoldReason,
        #[serde(rename = "turnId")]
        turn_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexHoldReason {
    ActiveWithoutTurn,
    ConflictingTurn,
    Review,
    Compaction,
    UnknownProtocol,
    NotLoaded,
    SystemError,
    UnknownStatus,
    WaitingOnApproval,
    WaitingOnUserInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexTerminalError {
    SystemError,
    ProviderAuthRejected,
    /// A turn was rejected before useful work because its selected model had no capacity.
    ProviderCapacity,
}

/// A cause reported by Codex's `error` notification. Retryable failures may arrive without
/// a completion or thread-status change, so retain this evidence independently of delivery state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexTurnError {
    /// The turn the error belongs to. A later turn's start clears it; a foreign turn's does not.
    turn_id: String,
    /// st2's classification of `error.codexErrorInfo`, for the native-driver diagnostic.
    reason: driver_diagnostic::Reason,
    /// The word the observed-state record shows a reader. One word, from a closed set.
    word: &'static str,
    /// Codex's own `willRetry`. `true` means no `turn/completed` is coming until the retries are
    /// spent, so this error is the only evidence the seat is not progressing.
    will_retry: bool,
}

/// Classify one `CodexErrorInfo` value.
///
/// The value is either one of Codex's enum words or a single-key object (`httpConnectionFailed`
/// and its siblings carry an HTTP status). Both shapes reduce to one word here.
///
/// The classes are st2's, not Codex's, and they are chosen by WHAT A PERSON DOES about them: wait
/// for an allowance, pick another model, shorten the context, fix the network, take it up with
/// the provider, or fix st2's own request. A word this build does not know is `turnUnclassified`
/// — a real failure whose cause this version cannot name — never silently dropped and never
/// folded into a class it might not belong to.
fn codex_error_class(
    error_info: Option<&Value>,
) -> Option<(driver_diagnostic::Reason, &'static str, String)> {
    use driver_diagnostic::Reason;
    let word = match error_info {
        Some(Value::String(word)) => word.as_str(),
        Some(Value::Object(map)) if map.len() == 1 => map.keys().next()?.as_str(),
        _ => {
            return Some((
                Reason::TurnUnclassified,
                "unclassified",
                "unreadable".to_string(),
            ));
        }
    };
    let (reason, class) = match word {
        // The credential class is NOT here on purpose: it has its own stage, its own recovery
        // edge and its own repair text, and it outranks this one in the projection order.
        CODEX_PROVIDER_AUTH_REJECTED => return None,
        "usageLimitExceeded" | "rateLimitExceeded" | "sessionBudgetExceeded" => {
            (Reason::TurnUsageLimit, "usageLimit")
        }
        "serverOverloaded" => (Reason::TurnServerOverloaded, "serverOverloaded"),
        "contextWindowExceeded" => (Reason::TurnContextWindow, "contextWindow"),
        "httpConnectionFailed"
        | "responseStreamConnectionFailed"
        | "responseStreamDisconnected"
        | "responseTooManyFailedAttempts" => (Reason::TurnConnection, "connection"),
        "cyberPolicy" => (Reason::TurnPolicy, "policy"),
        "badRequest" | "activeTurnNotSteerable" => (Reason::TurnRejected, "rejected"),
        "internalServerError" | "threadRollbackFailed" | "sandboxError" => {
            (Reason::TurnInternal, "internal")
        }
        // Codex's own catch-all and every word added after this build was written land together:
        // both mean "a real failure this version cannot name", which is the honest report.
        _ => (Reason::TurnUnclassified, "unclassified"),
    };
    Some((reason, class, word.to_string()))
}

/// The `CodexErrorInfo` word that names a rejected provider credential.
///
/// It is the 401/invalid-credential arm of Codex's own closed error vocabulary and is distinct
/// from `usageLimitExceeded`. Some supported Codex releases do not expose the later
/// `rateLimitExceeded` word. The protocol gate pins the credential and stable quota words so a
/// release that merges them refuses the launch instead of reporting an exhausted allowance as a
/// rejected credential.
const CODEX_PROVIDER_AUTH_REJECTED: &str = "unauthorized";

/// What one `turn/completed` notification proves about this thread's provider credential.
///
/// `Turn.status` is required and `Turn.error` is populated only on `failed`, so both edges come
/// from the notification st2 already consumes — no second signal, and no inference from prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexTurnOutcome {
    /// `completed` — the provider accepted the credential for this turn.
    Accepted,
    /// `failed` with `error.codexErrorInfo: unauthorized`.
    ProviderAuthRejected,
    /// A typed allowance rejection or the captured transient model-capacity rejection.
    ProviderCapacity,
    /// `interrupted`, `inProgress`, or a failure this version does not classify: no evidence
    /// either way, so a standing rejection must stand.
    Indeterminate,
}

fn codex_turn_outcome(turn: Option<&Value>) -> CodexTurnOutcome {
    let Some(turn) = turn else {
        return CodexTurnOutcome::Indeterminate;
    };
    match turn.get("status").and_then(Value::as_str) {
        Some("completed") => CodexTurnOutcome::Accepted,
        Some("failed")
            if turn
                .pointer("/error/codexErrorInfo")
                .and_then(Value::as_str)
                == Some(CODEX_PROVIDER_AUTH_REJECTED) =>
        {
            CodexTurnOutcome::ProviderAuthRejected
        }
        Some("failed")
            if turn
                .pointer("/error/codexErrorInfo")
                .and_then(Value::as_str)
                == Some("usageLimitExceeded")
                || turn
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .is_some_and(is_provider_capacity_message) =>
        {
            CodexTurnOutcome::ProviderCapacity
        }
        _ => CodexTurnOutcome::Indeterminate,
    }
}

fn is_provider_capacity_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("selected model is at capacity")
        || normalized.contains("model is at capacity")
}

impl CodexObservedState {
    /// Driver-side projection into the generic observed-harness-state vocabulary (#162). `Held` is
    /// a delivery predicate — the complement of steerable — and never leaks into the published
    /// record: holds Codex positively reported as work project to `active` (with the human-blocking
    /// ones setting the blocked axis), while holds that only mean "st cannot currently prove
    /// anything" project to `None`, the indeterminate observation that writes nothing.
    pub fn harness_observation(&self) -> Option<harness_state::Observation> {
        use crate::harness_state::{Activity, Ask, BlockedOn, InputBuffer, Observation};
        let observation = |state, blocked_on| {
            // This producer reads the app-server control stream and cannot see the composer.
            Observation::new(state, blocked_on, InputBuffer::Unknown)
        };
        match self {
            CodexObservedState::AwaitingStatus => None,
            CodexObservedState::Idle => Some(observation(Activity::Idle, BlockedOn::None)),
            // Capacity is retryable in the same session, while authentication and system errors
            // end this harness incarnation.
            CodexObservedState::TerminalError {
                reason: CodexTerminalError::ProviderCapacity,
            } => Some(observation(Activity::Idle, BlockedOn::None).with_reason("providerCapacity")),
            CodexObservedState::TerminalError { reason } => Some(
                observation(Activity::Ended, BlockedOn::None).with_reason(match reason {
                    CodexTerminalError::SystemError => "systemError",
                    CodexTerminalError::ProviderAuthRejected => "providerAuth",
                    CodexTerminalError::ProviderCapacity => unreachable!(),
                }),
            ),
            CodexObservedState::Active { .. } => {
                Some(observation(Activity::Active, BlockedOn::None))
            }
            CodexObservedState::Held { reason, .. } => match reason {
                // Review's enter and exit are MODEL-emitted items inside a running turn
                // (`enteredReviewMode`/`exitedReviewMode`, released by `observe_hold_released`):
                // nothing awaits a human, so the observed record reports plain activity. The
                // delivery hold is untouched — `Held` still blocks steer — and `review` stays a
                // reserved ask word no producer emits.
                CodexHoldReason::Review => {
                    Some(observation(Activity::Active, BlockedOn::None).with_reason("review"))
                }
                CodexHoldReason::WaitingOnApproval => Some(
                    observation(Activity::Active, BlockedOn::Human)
                        .with_ask(Ask::Permission)
                        .with_reason("waitingOnApproval"),
                ),
                CodexHoldReason::WaitingOnUserInput => Some(
                    observation(Activity::Active, BlockedOn::Human)
                        .with_ask(Ask::Question)
                        .with_reason("waitingOnUserInput"),
                ),
                CodexHoldReason::Compaction => {
                    Some(observation(Activity::Active, BlockedOn::None).with_reason("compaction"))
                }
                CodexHoldReason::UnknownProtocol => Some(
                    observation(Activity::Active, BlockedOn::None).with_reason("unknownProtocol"),
                ),
                // Codex positively reported active; st2 merely cannot name a steerable turn.
                CodexHoldReason::ActiveWithoutTurn => Some(
                    observation(Activity::Active, BlockedOn::None).with_reason("activeWithoutTurn"),
                ),
                CodexHoldReason::ConflictingTurn => Some(
                    observation(Activity::Active, BlockedOn::None).with_reason("conflictingTurn"),
                ),
                CodexHoldReason::NotLoaded
                | CodexHoldReason::SystemError
                | CodexHoldReason::UnknownStatus => None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexControlState {
    schema: String,
    agent: String,
    runtime_id: String,
    runtime_incarnation: String,
    thread_id: String,
    subscribed: bool,
    observed: CodexObservedState,
}

#[derive(Debug, Clone)]
struct CodexDeliveryConfig {
    control: crate::session_control::SessionControl,
    catalog_root: PathBuf,
    agent_dir: PathBuf,
    inbox: PathBuf,
    identity: String,
    this_host: String,
    supervisor: Option<String>,
    /// The codex-cli version the protocol gate admitted, carried for the native-driver
    /// diagnostic's `producerVersion`. `None` only in tests that build a config without a gate.
    producer_version: Option<String>,
    model: Option<String>,
}

impl CodexDeliveryConfig {
    fn resolve(catalog_root: &Path, identity: &str) -> Result<Self> {
        let this_host = run::detect_host();
        let agent_dir = message::resolve_declared_dir(catalog_root, identity, &this_host)?
            .with_context(|| {
                format!(
                    "Codex native delivery agent '{identity}' is not declared in {}",
                    catalog_root.display()
                )
            })?;
        let supervisor = crate::discover(catalog_root)
            .specs
            .into_iter()
            .find(|spec| spec.path.parent() == Some(agent_dir.as_path()))
            .and_then(|spec| spec.supervisor);
        Ok(Self {
            control: crate::session_control::SessionControl::Catalog,
            catalog_root: catalog_root.to_path_buf(),
            inbox: message::inbox_dir(&agent_dir),
            agent_dir,
            identity: identity.to_string(),
            this_host,
            supervisor,
            producer_version: None,
            model: None,
        })
    }

    fn report_protocol_rejection(&self, codex: &str, error: &anyhow::Error) {
        let Some(supervisor) = self.supervisor.as_deref() else {
            eprintln!(
                "st codex: agent '{}' has no supervisor for a protocol rejection report",
                self.identity
            );
            return;
        };
        let subject = format!("Codex protocol rejected: {}", self.identity);
        let body = format!(
            "st rejected the installed Codex app-server protocol for agent '{}'. Native delivery did not start. Codex executable: '{}'. Error: {error:#}",
            self.identity, codex
        );
        let mut key_hash = Sha256::new();
        key_hash.update(b"st2.codex-protocol-rejection.v1");
        // Hash the original report wording so an upgrade does not publish the same failure twice.
        key_hash.update(body.replacen("st rejected", "st2 rejected", 1).as_bytes());
        let idempotency_key = format!("st2.codex-protocol-rejection.v1:{:x}", key_hash.finalize());
        let tags = ["codex-protocol".to_string(), "launch-rejected".to_string()];
        // Both endpoints are declaration keys, never routes: this runtime names itself by exact
        // key, and `supervisor` is the positional edge the org chart walks, so a parent that
        // declares an `address` still receives the report.
        let endpoints =
            message::declared_selector(&self.catalog_root, &self.identity, &self.this_host)
                .and_then(|sender| {
                    let recipient = message::declared_selector(
                        &self.catalog_root,
                        supervisor,
                        &self.this_host,
                    )?;
                    Ok((sender, recipient))
                });
        let (sender, recipient) = match endpoints {
            Ok(endpoints) => endpoints,
            Err(resolve_error) => {
                eprintln!(
                    "st codex: failed to resolve the endpoints of agent '{}' protocol rejection report: {resolve_error:#}",
                    self.identity
                );
                return;
            }
        };
        if let Err(report_error) = message::send_to_resolved_inbox(
            &self.catalog_root,
            &recipient,
            &self.this_host,
            &sender,
            Some(&subject),
            None,
            &tags,
            &body,
            Some(&idempotency_key),
            None,
        ) {
            eprintln!(
                "st codex: failed to report agent '{}' protocol rejection to supervisor '{}': {report_error:#}",
                self.identity, supervisor
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexDeliveryMethod {
    Start,
    Steer { turn_id: String },
}

#[derive(Debug, Clone)]
struct PendingCodexDelivery {
    request_id: u64,
    filename: String,
    method: CodexDeliveryMethod,
    requested_at: Instant,
}

#[derive(Debug, Clone)]
struct PendingCodexSnapshot {
    request_id: u64,
    filename: String,
    requested_at: Instant,
}

// One durable FIFO delivery attempt lives in the shared `crate::delivery_ledger`, which grades
// Codex's two receipts honestly: the JSON-RPC result of `turn/start`/`turn/steer` is
// `transportAccepted`, and only the exact completed typed user message — live, or found in a
// resumed thread's history — is `consumed`. Codex has no storage receipt and no scheduler
// admission signal, so it can never write the phases in between, and consumption is its true
// ceiling: reaching it releases FIFO ownership. Ordinary message archive precedence, which is the
// recipient agent's own act, still removes the inbox entry.

/// The exact codex-cli version whose Rust source settled the occupancy arithmetic below, read at
/// tag `rust-v0.151.0` (tag object `d8673cb68e349c208659b986697773d3145dbb14`) because the Nix
/// package ships a prebuilt musl tarball with no vendored source. HC-T03 calls Codex's baseline a
/// version-coupled constant — a property of a build, not of a documented contract — and HC-R13
/// bounds that with a fixture pinned to this literal, in the shape of `omp_session`'s
/// `admitted_versions_are_exactly_the_measured_set`. A codex bump that moves the numerator, the
/// denominator, or the baseline has to fail
/// [`tests::codex_context_recomputes_the_captured_reading_and_pins_its_verified_version`] rather
/// than silently publish a differently-meaning number.
///
/// Deliberately NOT a launch gate: this constant refuses nothing, it names what was measured.
/// Admitting 0.151.0 aligns the newest delivery-gated build with this measurement; a later Codex
/// admission must still re-read the version-coupled arithmetic rather than infer compatibility
/// from the unchanged literal.
pub const CODEX_CONTEXT_VERIFIED_VERSION: &str = "0.151.0";

mod context;
use self::context::*;

#[derive(Debug, Clone)]
struct RejectedCodexDelivery {
    filename: String,
    observed: CodexObservedState,
    rejected_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountRead {
    Due,
    Pending,
    Settled,
}

struct CodexInboxDelivery {
    config: CodexDeliveryConfig,
    runtime: CodexRuntime,
    wake: Receiver<()>,
    _watcher: Option<notify::RecommendedWatcher>,
    next_inbox_refresh: Instant,
    next_presence_refresh: Instant,
    next_context_transcript_refresh: Instant,
    head: Option<message::Message>,
    suppressed: bool,
    ledger: delivery_ledger::Ledger,
    pending: Option<PendingCodexDelivery>,
    pending_snapshot: Option<PendingCodexSnapshot>,
    snapshot_attempts: u8,
    verified_snapshot: Option<(String, CodexObservedState)>,
    require_snapshot: bool,
    rejected: Option<RejectedCodexDelivery>,
    next_request_id: u64,
    harness_writer: harness_state::Writer,
    /// Whether the latest projection carried evidence. Indeterminate observations write nothing
    /// and stop the heartbeat, so a state the pump can no longer see ages out instead of staying
    /// artificially fresh.
    harness_evidence: bool,
    /// A projected transition whose write failed, retried on the next pump pass before any
    /// heartbeat may re-stamp the contradicted on-disk state.
    pending_observation: Option<harness_state::Observation>,
    /// The numeric axis's producer, beside the categorical one. `None` only where the record has
    /// nowhere safe to stage — observability never blocks a launch.
    context: Option<CodexContextProducer>,
    /// Crash-safe normalized conversation operations for the client-v0 timeline.
    timeline: crate::harness_timeline::Writer,
    model_attempted: BTreeSet<String>,
    /// The paying account is read once the thread is bound and again after Codex reports an
    /// account change, so each response is recorded against the account that paid for it.
    account_read: AccountRead,
    /// Native-driver boundary diagnostics for launch fallback, provider credentials, delivery,
    /// and turn failures. Earlier protocol gates refuse admission before the session starts.
    diagnostics: driver_diagnostic::Publisher,
    /// Failure reported for the turn, retained until observed recovery.
    turn_error: Option<CodexTurnError>,
    safe_fallback_active: Arc<AtomicBool>,
    safe_fallback_diagnostic_published: bool,
}

impl CodexInboxDelivery {
    fn new(
        config: CodexDeliveryConfig,
        ledger_path: PathBuf,
        runtime: CodexRuntime,
        safe_fallback_active: Arc<AtomicBool>,
    ) -> Result<Self> {
        if !crate::push_mailbox::managed(&config.agent_dir) {
            fs::create_dir_all(&config.inbox).with_context(|| {
                format!(
                    "creating Codex native delivery inbox {}",
                    config.inbox.display()
                )
            })?;
        }
        let (wake_tx, wake) = mpsc::channel();
        crate::push_mailbox::watch(&config.agent_dir, wake_tx.clone());
        // Scoped to inbox + status: this pump's own process group writes runtime records (presence
        // refreshes, harness-state transitions) into the same agent dir, and those must not wake it.
        let watcher = crate::watch::watch_delivery_inputs_with_status(
            &config.agent_dir,
            wake_tx,
            matches!(
                config.control,
                crate::session_control::SessionControl::Catalog
            ),
        );
        let identity = config.identity.clone();
        let ledger = delivery_ledger::Ledger::open(
            &ledger_path,
            delivery_ledger::Harness::Codex.profile(),
            &config.identity,
            runtime.runtime_id(),
            |thread, filename| stable_client_user_message_id(&identity, thread, filename),
        )
        .with_owner_schema(&runtime.schema);
        // The pty session whose liveness vouches for the record is the wrapper's task: the
        // runtime ID names the pty registry entry, and only aliases the identity on
        // driver-expanded seats — a hand-authored seat may declare a different task ID.
        // The session token is the runtime incarnation the wrapper already minted: the pump and
        // the wrapper's terminal writer are the same session and must own the same records. The
        // claim is a WRITTEN act — it atomically supersedes whatever a predecessor left,
        // including a still-fresh live record the pty-name probe cannot distinguish.
        // Observability must never kill the launch: a claim that cannot be written degrades to
        // a token-only writer (refused by records it does not own, so it can only under-report)
        // with a warning, and delivery proceeds.
        let harness_writer = {
            let writer = harness_state::Writer::new(
                &config.agent_dir,
                config.identity.clone(),
                "codex",
                Some(runtime.runtime_id().to_string()),
            );
            match harness_state::claim(
                &config.agent_dir,
                config.identity.clone(),
                "codex",
                runtime.incarnation(),
            ) {
                Ok(claimed_seq) => writer.with_ownership(runtime.incarnation(), claimed_seq),
                Err(error) => {
                    tracing::warn!(
                        "st codex: observed-state claim failed; degrading to token-only: {error:#}"
                    );
                    writer.with_session(runtime.incarnation())
                }
            }
        };
        // The numeric record's writer, owned beside the state record's and carrying the same
        // incarnation so both name one session as their provenance. It takes no claim and no
        // sequence: HC-T04 leaves the numbers unfenced on purpose, because the worst a straggler
        // can publish here is a reading older than the reader thinks — which `observedAtMs`
        // already says — rather than a live state that is not live.
        let context_writer = if matches!(
            config.control,
            crate::session_control::SessionControl::Graph(_)
        ) {
            harness_context::Writer::new_paths(
                &config.agent_dir,
                config.identity.clone(),
                harness_context::Harness::Codex,
            )
        } else {
            harness_context::Writer::new(
                &config.agent_dir,
                config.identity.clone(),
                harness_context::Harness::Codex,
            )
        };
        let context = match context_writer {
            Ok(writer) => Some(CodexContextProducer::new(
                writer.with_session(runtime.incarnation()),
            )),
            Err(error) => {
                tracing::warn!(
                    "st codex: harness-context writer unavailable; context stays unpublished: {error:#}"
                );
                None
            }
        };
        let timeline =
            crate::harness_timeline::Writer::new(&config.agent_dir, "codex", runtime.incarnation())
                .with_model(config.model.clone());
        // The record belongs to this incarnation: the protocol gate already admitted the version
        // it names, so `support` is a measured fact rather than a probe result.
        let mut diagnostics = driver_diagnostic::Publisher::new(
            &config.agent_dir,
            driver_diagnostic::Driver::Codex,
            config.producer_version.clone(),
            driver_diagnostic::Support::Supported,
        );
        let safe_fallback_diagnostic_published = safe_fallback_active.load(Ordering::SeqCst);
        if safe_fallback_diagnostic_published {
            diagnostics.publish(
                driver_diagnostic::Stage::Launch,
                driver_diagnostic::Reason::LaunchConfigurationRejected,
                driver_diagnostic::Source::ProcessExit,
            );
        } else {
            // A later exact-config boot is the recovery boundary for a predecessor that had to
            // come up degraded. Do not leave that old advisory attached to the healthy session.
            diagnostics.clear(driver_diagnostic::Stage::Launch);
        }
        Ok(Self {
            config,
            runtime,
            wake,
            _watcher: watcher,
            next_inbox_refresh: Instant::now(),
            next_presence_refresh: Instant::now(),
            next_context_transcript_refresh: Instant::now(),
            head: None,
            suppressed: false,
            ledger,
            pending: None,
            pending_snapshot: None,
            snapshot_attempts: 0,
            verified_snapshot: None,
            require_snapshot: true,
            rejected: None,
            next_request_id: FIRST_DELIVERY_REQUEST_ID,
            harness_writer,
            harness_evidence: false,
            pending_observation: None,
            context,
            timeline,
            model_attempted: BTreeSet::new(),
            account_read: AccountRead::Due,
            diagnostics,
            turn_error: None,
            safe_fallback_active,
            safe_fallback_diagnostic_published,
        })
    }

    /// `account/read` when the account is due to be read and no read is outstanding.
    fn maybe_account_request(&mut self) -> Option<Value> {
        if self.account_read != AccountRead::Due {
            return None;
        }
        self.account_read = AccountRead::Pending;
        Some(json!({ "method": "account/read", "id": ACCOUNT_READ_REQUEST_ID, "params": {} }))
    }

    /// Consume the response to [`Self::maybe_account_request`], and schedule another read when
    /// Codex reports a login change. A refused read leaves later responses without an account.
    fn observe_account(&mut self, message: &Value) -> bool {
        if message.get("method").and_then(Value::as_str) == Some("account/updated") {
            if self.account_read == AccountRead::Settled {
                self.account_read = AccountRead::Due;
            }
            return false;
        }
        if message.get("method").is_some()
            || message.get("id").and_then(Value::as_str) != Some(ACCOUNT_READ_REQUEST_ID)
        {
            return false;
        }
        self.account_read = AccountRead::Settled;
        let account = message
            .get("result")
            .and_then(crate::account::codex_account_from_read);
        if account.is_none() {
            tracing::debug!("st codex: account/read named no account; usage stays unattributed");
        }
        if let Some(context) = self.context.as_mut() {
            context.set_account(account.clone());
        }
        self.timeline.set_account(account);
        true
    }

    fn sync_safe_fallback_diagnostic(&mut self) {
        if self.safe_fallback_diagnostic_published
            || !self.safe_fallback_active.load(Ordering::SeqCst)
        {
            return;
        }
        self.diagnostics.publish(
            driver_diagnostic::Stage::Launch,
            driver_diagnostic::Reason::LaunchConfigurationRejected,
            driver_diagnostic::Source::ProcessExit,
        );
        self.safe_fallback_diagnostic_published = true;
    }

    /// Publish the generic observed-harness-state projection of a control-state change. Best-effort
    /// like the presence refresh: a failed record write must not disturb delivery — but it must
    /// not count as evidence either. A transition whose write failed is retained as pending and
    /// retried before any heartbeat, so a stale on-disk state is never kept fresh in
    /// contradiction of the latest observation.
    fn observe_harness(&mut self, observed: &CodexObservedState) {
        match observed.harness_observation() {
            Some(observation) => self.publish_observation(self.name_turn_error(observation)),
            None => {
                // Evidence lost: stop heartbeating, drop anything pending (it predates the gap),
                // and mark the stream discontinuous so a state restated after the gap opens a
                // fresh transition instead of claiming continuity across an unobserved interval.
                self.harness_evidence = false;
                self.pending_observation = None;
                self.harness_writer.interrupt();
            }
        }
    }

    /// Hand one inbound control frame to the context producer. Best-effort in the same sense as the
    /// presence refresh and the state projection: a record write that fails must not disturb
    /// delivery. Unlike the state record there is nothing to retain and retry — the next model
    /// response carries another reading, and the record ages visibly through `ageMs` until it
    /// lands (HC-R06, HC-T05).
    fn observe_context(&mut self, message: &Value, thread_id: &str, active_turn_id: Option<&str>) {
        let is_usage =
            message.get("method").and_then(Value::as_str) == Some("thread/tokenUsage/updated");
        let track_usage = should_track_timeline_usage(message, active_turn_id);
        if is_usage && track_usage {
            if let Some(turn_id) = message.pointer("/params/turnId").and_then(Value::as_str)
                && !self.model_attempted.contains(turn_id)
            {
                if self.model_attempted.len() >= 128 {
                    self.model_attempted.pop_first();
                }
                self.model_attempted.insert(turn_id.into());
                match codex_turn_model(thread_id, turn_id) {
                    Ok(Some(model)) => self.timeline.remember_turn_model(turn_id, &model),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!("st codex: bounded turn model read failed: {error:#}")
                    }
                }
            }
        }
        if track_usage {
            if let Err(error) =
                crate::harness_timeline::observe_codex(&mut self.timeline, message, thread_id)
            {
                tracing::warn!("st codex: harness-timeline write failed: {error:#}");
            }
        }
        if let Some(context) = self.context.as_mut()
            && let Err(error) = context.observe(message, thread_id)
        {
            tracing::warn!("st codex: harness-context write failed: {error:#}");
        }
    }

    /// Codex 0.151 persists a manual `ContextCompaction` completion even when the secondary
    /// app-server subscriber receives no matching item notification. Sample the same bounded
    /// rollout tail used for turn recovery so that omission does not erase the context edge.
    fn refresh_transcript_context_if_due(&mut self, thread_id: &str) {
        if Instant::now() < self.next_context_transcript_refresh {
            return;
        }
        self.next_context_transcript_refresh = Instant::now() + TRANSCRIPT_CONTEXT_REFRESH_INTERVAL;
        let result = latest_codex_transcript(thread_id).and_then(|path| match path {
            Some(path) => codex_transcript_tail(&path).map(Some),
            None => Ok(None),
        });
        match result {
            Ok(Some(frames)) => {
                if let Some(context) = self.context.as_mut()
                    && let Err(error) = context.observe_transcript(&frames, thread_id)
                {
                    tracing::warn!(
                        "st codex: transcript harness-context recovery failed: {error:#}"
                    );
                }
                if let Err(error) = self.accept_transcript_receipts(&frames, thread_id) {
                    tracing::warn!(
                        "st codex: transcript delivery receipt recovery failed: {error:#}"
                    );
                }
            }
            Err(error) => {
                tracing::warn!("st codex: bounded transcript context discovery failed: {error:#}")
            }
            _ => {}
        }
    }

    /// The secondary app-server subscriber can miss a steered user-message notification even
    /// though Codex has appended it to the owning session. Its exact client ID in the durable
    /// rollout is the same consumption evidence as the live notification.
    fn accept_transcript_receipts(&mut self, frames: &[Value], thread_id: &str) -> Result<()> {
        for frame in frames {
            if frame.get("type").and_then(Value::as_str) != Some("event_msg")
                || frame.pointer("/payload/type").and_then(Value::as_str) != Some("item_completed")
                || frame.pointer("/payload/thread_id").and_then(Value::as_str) != Some(thread_id)
                || frame.pointer("/payload/item/type").and_then(Value::as_str)
                    != Some("UserMessage")
            {
                continue;
            }
            let Some(client_id) = frame
                .pointer("/payload/item/client_id")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let settled = self
                .ledger
                .correlated(client_id)
                .into_iter()
                .filter(|filename| {
                    self.ledger.entry(filename).is_some_and(|entry| {
                        entry.binding == thread_id
                            && entry.incarnation.as_deref() == Some(self.runtime.incarnation())
                            && entry.phase < delivery_ledger::Phase::Consumed
                    })
                })
                .collect::<Vec<_>>();
            for filename in settled {
                self.ledger
                    .record(&filename, delivery_ledger::Evidence::Consumed)?;
                self.next_inbox_refresh = Instant::now();
            }
        }
        Ok(())
    }

    /// Record what one inbound frame proves about this thread's provider credential.
    ///
    /// Frame-level like [`Self::observe_context`] and for the same reason: the credential is its
    /// own axis, and the earliest-boundary projection inside the publisher — not this call site —
    /// decides what a reader sees. Both edges come from `turn/completed`: a turn that reached
    /// `completed` is positive proof the account was accepted, and a `failed` turn whose typed
    /// error names `unauthorized` is the rejection. Anything else leaves a standing rejection
    /// alone; only positive evidence clears it.
    fn observe_provider_auth(&mut self, message: &Value, thread_id: &str) {
        if message.get("method").and_then(Value::as_str) != Some("turn/completed")
            || message.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id)
        {
            return;
        }
        match codex_turn_outcome(message.pointer("/params/turn")) {
            CodexTurnOutcome::ProviderAuthRejected => self.diagnostics.publish(
                driver_diagnostic::Stage::ProviderAuth,
                driver_diagnostic::Reason::ProviderAuthRejected,
                driver_diagnostic::Source::TurnResult,
            ),
            CodexTurnOutcome::Accepted => self
                .diagnostics
                .clear(driver_diagnostic::Stage::ProviderAuth),
            CodexTurnOutcome::ProviderCapacity => {}
            CodexTurnOutcome::Indeterminate => {}
        }
    }

    /// Let a standing turn error name itself in the observed record.
    ///
    /// The delivery-relevant [`CodexObservedState`] is untouched by this: `Held` stays exactly the
    /// complement of steerable (decision 0001), so nothing here can make a seat unreachable. What
    /// changes is the one field a reader looks at to find out why a seat is not moving.
    ///
    /// The rule is that the CAUSE outranks the ACTIVITY. `active` with no reason, `ended` with the
    /// bare word `systemError`, and every hold reason describe what kind of work the thread
    /// believes it is doing; the turn error describes why none of it is progressing, and that is
    /// what the reader needs. Two reasons are left alone: a human ask (`blockedOn: human`), which
    /// is a stronger and more actionable fact than a failed turn, and `providerAuth`, which names
    /// the same failure's more specific cause from a stage of its own.
    fn name_turn_error(
        &self,
        observation: harness_state::Observation,
    ) -> harness_state::Observation {
        use crate::harness_state::BlockedOn;
        let Some(error) = self.turn_error.as_ref() else {
            return observation;
        };
        if observation.blocked_on == BlockedOn::Human
            || observation.reason.as_deref() == Some("providerAuth")
        {
            return observation;
        }
        observation.with_reason(error.word)
    }

    /// Record what one inbound frame proves about the live turn, and return whether the standing
    /// error changed — the caller republishes the observed record when it did.
    ///
    /// Frame-level like [`Self::observe_context`] and [`Self::observe_provider_auth`], and for the
    /// same reason: it reads frames no delivery branch looks at, and every one of them may
    /// `continue`.
    ///
    /// A standing error is cleared only by POSITIVE proof that the turn recovered — a completed
    /// turn, an idle thread, or a different turn starting. Never by silence, and never by a
    /// `turn/completed` that itself reports `failed`: a failure is not its own recovery. That
    /// asymmetry is the whole point. Evidence that a seat is stuck must survive quiet, because
    /// quiet is exactly what a stuck seat produces.
    fn observe_turn_error(&mut self, message: &Value, thread_id: &str) -> bool {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return false;
        };
        if message.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id) {
            return false;
        }
        let before = self.turn_error.clone();
        match method {
            "error" => {
                // ErrorNotification requires the identity, retry flag, and error object.
                // Missing one proves nothing and must not clear a standing failure.
                let (Some(turn_id), Some(will_retry)) = (
                    message.pointer("/params/turnId").and_then(Value::as_str),
                    message
                        .pointer("/params/willRetry")
                        .and_then(Value::as_bool),
                ) else {
                    return false;
                };
                let Some(error) = message
                    .pointer("/params/error")
                    .filter(|error| error.is_object())
                else {
                    return false;
                };
                // TurnError.codexErrorInfo is optional and nullable. The notification proves a
                // failure even when the producer cannot name its cause.
                let Some((reason, word, codex_word)) =
                    codex_error_class(error.get("codexErrorInfo"))
                else {
                    // The credential arm. `observe_provider_auth` owns it from `turn/completed`,
                    // where the same rejection arrives with the turn's own typed result.
                    return false;
                };
                self.turn_error = Some(CodexTurnError {
                    turn_id: turn_id.to_string(),
                    reason,
                    word,
                    will_retry,
                });
                self.diagnostics.publish(
                    driver_diagnostic::Stage::Turn,
                    reason,
                    driver_diagnostic::Source::TurnError,
                );
                // The record carries a closed vocabulary a consumer can branch on. The log
                // carries Codex's own word beside it, so an operator reading the wrapper log
                // learns the exact cause even when this build classified it as `unclassified` —
                // which is the case where they need it most.
                tracing::warn!(
                    turn = turn_id,
                    class = word,
                    codex_error_info = codex_word,
                    will_retry = will_retry,
                    reason = reason.as_str(),
                    "st codex: the provider failed this turn"
                );
            }
            "turn/completed" => {
                // Only a turn that reached its ordinary end is proof of recovery, and only for
                // the turn that failed. A `failed` completion leaves the error standing, which is
                // what keeps a cause on the record after the thread goes terminal.
                let status = message
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str);
                if status != Some("completed") {
                    return false;
                }
                if let Some(turn_id) = message.pointer("/params/turn/id").and_then(Value::as_str) {
                    self.clear_turn_error(Some(turn_id));
                }
            }
            "turn/started" => {
                // A different turn beginning means the seat moved on; the same turn starting
                // twice is not evidence about the failure it already reported.
                let started = message.pointer("/params/turn/id").and_then(Value::as_str);
                if started.is_some_and(|started| {
                    self.turn_error
                        .as_ref()
                        .is_none_or(|error| error.turn_id != started)
                }) {
                    self.clear_turn_error(None);
                }
            }
            // A thread that reports itself idle has no failing turn by definition. This is the
            // one clear edge that does not name a turn, and it is deliberately narrow: `active`
            // and `systemError` prove nothing about the error, so they leave it alone.
            "thread/status/changed" => {
                if message
                    .pointer("/params/status/type")
                    .and_then(Value::as_str)
                    == Some("idle")
                {
                    self.clear_turn_error(None);
                }
            }
            _ => return false,
        }
        self.turn_error != before
    }

    /// Clear a standing turn error and its diagnostic stage. `turn_id` scopes the clear to one
    /// turn; `None` clears whatever stands.
    fn clear_turn_error(&mut self, turn_id: Option<&str>) {
        if turn_id.is_some_and(|turn_id| {
            self.turn_error
                .as_ref()
                .is_some_and(|error| error.turn_id != turn_id)
        }) {
            return;
        }
        self.turn_error = None;
        self.diagnostics.clear(driver_diagnostic::Stage::Turn);
    }

    fn publish_observation(&mut self, observation: harness_state::Observation) {
        match self.harness_writer.observe(observation.clone()) {
            Ok(()) => {
                self.harness_evidence = true;
                self.pending_observation = None;
            }
            Err(_) => {
                self.harness_evidence = false;
                self.harness_writer.interrupt();
                self.pending_observation = Some(observation);
            }
        }
    }

    /// Reconcile the ledger to what the recipient still has unread. Archive precedence is the
    /// recipient agent's act and the only settlement authority: an entry whose file left the inbox
    /// releases ownership, and this pump never moves a file.
    fn reconcile_inbox(&mut self, unread: &[message::Message]) -> Result<()> {
        self.ledger
            .prune(|filename| crate::push_mailbox::is_unread(&self.config.agent_dir, filename, unread))
    }

    fn refresh_if_due(&mut self) -> Result<()> {
        let now = Instant::now();
        // A pending transition retries on EVERY pump pass — its write failed once and the
        // on-disk record contradicts the latest observation until it lands; only the heartbeat
        // is presence-cadence work.
        if let Some(pending) = self.pending_observation.clone() {
            self.publish_observation(pending);
        }
        if now >= self.next_presence_refresh {
            // Session-owned observed state stays fresh only while this wrapper has evidence.
            // Catalog control also preserves its product's status lease; graph control has no file.
            self.config
                .control
                .refresh(&status::status_path(&self.config.agent_dir));
            if self.harness_evidence {
                let _ = self.harness_writer.heartbeat();
            }
            self.next_presence_refresh = now + crate::provider_session::SESSION_REFRESH;
        }
        let mut due = now >= self.next_inbox_refresh;
        while self.wake.try_recv().is_ok() {
            due = true;
        }
        if !due {
            return Ok(());
        }
        // Codex can bind its thread before the daemon's first mailbox replay lands. Until then
        // nothing is known, so nothing is pruned or delivered, and the next pass asks again.
        let unread = match crate::push_mailbox::messages(&self.config.agent_dir, &self.config.inbox)
        {
            Err(error) if crate::push_mailbox::is_not_replayed(&error) => return Ok(()),
            unread => unread?,
        };
        self.reconcile_inbox(&unread)?;
        if self.rejected.as_ref().is_some_and(|rejected| {
            unread
                .iter()
                .all(|message| message.filename != rejected.filename)
        }) {
            self.rejected = None;
        }
        // A consumed message remains unread until the recipient's normal archive precedence
        // settles it. It is history, not a FIFO lock: select the earliest unread message that has
        // not already reached Codex's consumption ceiling.
        let prior_head = self.head.as_ref().map(|message| message.filename.clone());
        self.head = unread
            .into_iter()
            .find(|message| !self.ledger.settled(&message.filename));
        if self.head.as_ref().map(|message| &message.filename) != prior_head.as_ref() {
            self.pending_snapshot = None;
            self.snapshot_attempts = 0;
            self.verified_snapshot = None;
        }
        self.suppressed = matches!(
            self.config.control,
            crate::session_control::SessionControl::Catalog
        ) && self
            .config
            .control
            .held(&status::status_path(&self.config.agent_dir));
        self.next_inbox_refresh = Instant::now() + INBOX_REFRESH_FALLBACK;
        Ok(())
    }

    fn delivery_held(&self) -> bool {
        match &self.config.control {
            crate::session_control::SessionControl::Catalog => self.suppressed,
            crate::session_control::SessionControl::Graph(gate) => gate.held(),
        }
    }

    fn maybe_request(&mut self, state: &CodexControlState) -> Result<Option<Value>> {
        self.refresh_if_due()?;
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.requested_at.elapsed() >= DELIVERY_REQUEST_TIMEOUT)
        {
            anyhow::bail!("Codex delivery request received no response within 30 seconds");
        }
        if self.pending.is_some()
            || self.pending_snapshot.is_some()
            || !state.subscribed
            || self.delivery_held()
        {
            return Ok(None);
        }
        // Fail closed: an unreadable ledger holds and surfaces rather than guessing. It never
        // refuses to start — a control connection that will not start delivers nothing at all.
        // The operator-visible surface is the existing typed boundary — the transport is
        // unavailable — and the raw reason stays in tracing, so no unbounded prose reaches the
        // record. Restating it is coalesced by the publisher, so a held pass costs no write.
        if let Some(reason) = self.ledger.quarantined().map(str::to_string) {
            tracing::warn!("st codex: delivery ledger is quarantined: {reason}");
            self.diagnostics.publish(
                driver_diagnostic::Stage::Delivery,
                driver_diagnostic::Reason::DeliveryUnavailable,
                driver_diagnostic::Source::PromptTransport,
            );
            return Ok(None);
        }
        // A newly selected thread is a different delivery binding. An old binding's receipt must
        // neither suppress nor acknowledge delivery to this thread.
        if self
            .ledger
            .binding()
            .is_some_and(|binding| binding != state.thread_id())
        {
            self.ledger.rebind(state.thread_id())?;
        }
        let Some(head) = self.head.clone() else {
            return Ok(None);
        };
        if self.require_snapshot
            && self
                .verified_snapshot
                .as_ref()
                .is_none_or(|(filename, _)| filename != &head.filename)
        {
            return Ok(None);
        }
        if self.rejected.as_ref().is_some_and(|rejected| {
            rejected.filename == head.filename
                && rejected.observed == state.observed
                && rejected.rejected_at.elapsed() < REJECTED_DELIVERY_BACKOFF
        }) {
            return Ok(None);
        }
        // Exactly one delivery is outstanding at a time on this transport: an entry bound to some
        // other file holds the pump until archive precedence resolves it, so a message arriving
        // out of filename order can never open a second concurrent delivery.
        if self.ledger.holds_other_than(&head.filename) {
            return Ok(None);
        }
        // An attempt this pump already owns is held until evidence settles or refuses it. Only an
        // authoritative "no" — a rejected request, or resumed history proving the client ID never
        // landed — authorizes sending the same identity again.
        if self.ledger.retry(&head.filename) != delivery_ledger::RetryDecision::Retry {
            return Ok(None);
        }
        let observed = self
            .verified_snapshot
            .as_ref()
            .map(|(_, observed)| observed)
            .unwrap_or(&state.observed);
        let method = match observed {
            CodexObservedState::Idle | CodexObservedState::TerminalError { .. } => {
                CodexDeliveryMethod::Start
            }
            CodexObservedState::Active { turn_id } => CodexDeliveryMethod::Steer {
                turn_id: turn_id.clone(),
            },
            CodexObservedState::AwaitingStatus | CodexObservedState::Held { .. } => {
                return Ok(None);
            }
        };
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("Codex delivery request ID overflow")?;
        let client_id =
            stable_client_user_message_id(&self.config.identity, state.thread_id(), &head.filename);
        let filename = head.filename.clone();
        let text = ding::poke_text(
            &self.config.catalog_root,
            &self.config.this_host,
            &self.config.identity,
            &head,
        );
        let request =
            codex_delivery_request(request_id, state.thread_id(), &client_id, &text, &method);
        // Durable ownership lands before transport.
        self.ledger.begin(delivery_ledger::Begin {
            filename: filename.clone(),
            binding: state.thread_id().to_string(),
            correlation: delivery_ledger::Correlation::native(client_id.clone()),
            // Codex's typed receipt is a live frame, so an attempt is acknowledged only by the
            // incarnation that made it; an older one is settled by the resume sweep instead.
            incarnation: Some(self.runtime.incarnation().to_string()),
        })?;
        self.pending = Some(PendingCodexDelivery {
            request_id,
            filename,
            method,
            requested_at: Instant::now(),
        });
        self.snapshot_attempts = 0;
        self.verified_snapshot = None;
        Ok(Some(request))
    }

    #[cfg(test)]
    fn without_snapshot_requirement(mut self) -> Self {
        self.require_snapshot = false;
        self
    }

    fn maybe_snapshot_request(&mut self, state: &CodexControlState) -> Result<Option<Value>> {
        self.refresh_if_due()?;
        // A graph hold blocks input, not a read-only status snapshot. Catalog DND retains
        // its historical suppression rule.
        let catalog_held = matches!(
            self.config.control,
            crate::session_control::SessionControl::Catalog
        ) && self.suppressed;
        if self.pending.is_some() || !state.subscribed || catalog_held {
            return Ok(None);
        }
        if self
            .pending_snapshot
            .as_ref()
            .is_some_and(|pending| pending.requested_at.elapsed() >= SNAPSHOT_REQUEST_TIMEOUT)
        {
            let pending = self
                .pending_snapshot
                .take()
                .context("Codex thread snapshot is not pending")?;
            self.finish_failed_snapshot(pending, state, "timed out");
        }
        if self.pending_snapshot.is_some() {
            return Ok(None);
        }
        let Some(head) = self.head.as_ref() else {
            return Ok(None);
        };
        // A successful turn/steer result is only transport acceptance. The live typed
        // notification or the bounded rollout tail supplies the consumption receipt; reading
        // all turns here makes each poll grow with the lifetime of the thread.
        if self
            .ledger
            .entry(&head.filename)
            .is_some_and(|entry| entry.phase == delivery_ledger::Phase::TransportAccepted)
        {
            return Ok(None);
        }
        if self
            .verified_snapshot
            .as_ref()
            .is_some_and(|(filename, _)| filename == &head.filename)
        {
            return Ok(None);
        }
        if self.ledger.holds_other_than(&head.filename)
            || self.ledger.retry(&head.filename) != delivery_ledger::RetryDecision::Retry
        {
            return Ok(None);
        }
        // A provider that rejects or silently drops thread/read must not hold an otherwise idle
        // inbox forever. Each head gets a bounded set of fresh requests; after they are exhausted,
        // fall back to the latest typed observer state. Delivery remains provider-fenced: stale
        // active state uses expectedTurnId, while a rejected turn request remains retryable.
        if self.snapshot_attempts >= SNAPSHOT_REQUEST_ATTEMPTS {
            self.verified_snapshot = Some((head.filename.clone(), state.observed.clone()));
            self.snapshot_attempts = 0;
            return Ok(None);
        }
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("Codex snapshot request ID overflow")?;
        self.snapshot_attempts += 1;
        self.pending_snapshot = Some(PendingCodexSnapshot {
            request_id,
            filename: head.filename.clone(),
            requested_at: Instant::now(),
        });
        Ok(Some(json!({
            "method": "thread/read",
            "id": request_id,
            "params": {
                "threadId": state.thread_id(),
                "includeTurns": false,
            }
        })))
    }

    fn accept_snapshot_response(
        &mut self,
        message: &Value,
        state: &mut CodexControlState,
    ) -> Result<bool> {
        let Some(pending) = self.pending_snapshot.as_ref() else {
            return Ok(false);
        };
        if message.get("method").is_some()
            || message.get("id") != Some(&Value::from(pending.request_id))
        {
            return Ok(false);
        }
        let pending = self
            .pending_snapshot
            .take()
            .context("Codex thread snapshot is not pending")?;
        if message.get("error").is_some() {
            self.finish_failed_snapshot(pending, state, "was rejected");
            return Ok(true);
        }
        let observed =
            match observed_from_thread_snapshot(message, state.thread_id(), &state.observed) {
                Ok(observed) => observed,
                Err(error) => {
                    tracing::warn!("st codex: invalid thread/read response; retrying: {error:#}");
                    self.finish_failed_snapshot(pending, state, "was invalid");
                    return Ok(true);
                }
            };
        state.observed = observed.clone();
        self.verified_snapshot = Some((pending.filename, observed));
        self.snapshot_attempts = 0;
        Ok(true)
    }

    fn finish_failed_snapshot(
        &mut self,
        pending: PendingCodexSnapshot,
        state: &CodexControlState,
        reason: &str,
    ) {
        tracing::warn!(
            "st codex: thread/read request {} {reason} (attempt {}/{}); delivery remains fenced until retry or bounded fallback",
            pending.request_id,
            self.snapshot_attempts,
            SNAPSHOT_REQUEST_ATTEMPTS,
        );
        if self.snapshot_attempts < SNAPSHOT_REQUEST_ATTEMPTS {
            return;
        }
        if self
            .head
            .as_ref()
            .is_some_and(|head| head.filename == pending.filename)
        {
            self.verified_snapshot = Some((pending.filename, state.observed.clone()));
        }
        self.snapshot_attempts = 0;
    }

    fn transcript_recovery_due(&self) -> bool {
        self.head.is_some()
            && (self
                .verified_snapshot
                .as_ref()
                .is_some_and(|(_, observed)| {
                    matches!(
                        observed,
                        CodexObservedState::Held {
                            reason: CodexHoldReason::ActiveWithoutTurn,
                            ..
                        }
                    )
                })
                || (self.verified_snapshot.is_none()
                    && self.pending_snapshot.as_ref().is_some_and(|pending| {
                        pending.requested_at.elapsed() >= TRANSCRIPT_TURN_RECOVERY_INTERVAL
                    })))
    }

    fn accept_transcript_recovery(&mut self, observed: CodexObservedState) {
        let Some(head) = self.head.as_ref() else {
            return;
        };
        self.pending_snapshot = None;
        self.snapshot_attempts = 0;
        self.verified_snapshot = Some((head.filename.clone(), observed));
    }

    fn accept_response(&mut self, message: &Value, observed: &CodexObservedState) -> Result<bool> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(false);
        };
        if message.get("method").is_some()
            || message.get("id") != Some(&Value::from(pending.request_id))
        {
            return Ok(false);
        }
        let pending = self
            .pending
            .take()
            .context("Codex delivery is not pending")?;
        if message.get("error").is_some() {
            // The request itself was refused: an authoritative negative acknowledgement about
            // this attempt, and the only thing that re-authorizes the same client ID here. A
            // delivery that already reached its ceiling cannot be un-settled by a late error.
            self.ledger.negative(
                &pending.filename,
                delivery_ledger::NegativeReceipt::Rejected,
            )?;
            self.rejected = Some(RejectedCodexDelivery {
                filename: pending.filename,
                observed: observed.clone(),
                rejected_at: Instant::now(),
            });
            return Ok(true);
        }
        let accepted_turn_id = match &pending.method {
            CodexDeliveryMethod::Start => {
                required_string(message, "/result/turn/id", "turn/start response")?
            }
            CodexDeliveryMethod::Steer { turn_id } => {
                let returned = required_string(message, "/result/turnId", "turn/steer response")?;
                anyhow::ensure!(
                    returned == turn_id,
                    "Codex turn/steer response returned a different turn"
                );
                returned
            }
        };
        // The result proves transport acceptance and names the exact turn that accepted the
        // delivery. Persist both facts atomically so a matching successful turn completion can
        // settle clients (including Codex 0.146) that omit the correlated user-message receipt.
        self.ledger
            .accept_codex_turn(&pending.filename, accepted_turn_id)?;
        // The live typed notification and bounded rollout tail settle consumption.
        self.rejected = None;
        Ok(true)
    }

    fn accept_typed_receipt(&mut self, message: &Value, state: &CodexControlState) -> Result<bool> {
        if message.get("method").and_then(Value::as_str) != Some("item/completed")
            || message.pointer("/params/item/type").and_then(Value::as_str) != Some("userMessage")
        {
            return Ok(false);
        }
        let Some(client_id) = message
            .pointer("/params/item/clientId")
            .and_then(Value::as_str)
        else {
            return Ok(false);
        };
        if message.pointer("/params/threadId").and_then(Value::as_str) != Some(state.thread_id())
            || state.runtime_incarnation != self.runtime.incarnation()
        {
            return Ok(false);
        }
        // One correlation may carry several inbox files, so one typed receipt settles every entry
        // it delivered — each on its own monotone entry.
        let settled: Vec<String> = self
            .ledger
            .correlated(client_id)
            .into_iter()
            .filter(|filename| {
                self.ledger.entry(filename).is_some_and(|entry| {
                    entry.binding == state.thread_id()
                        && entry.incarnation.as_deref() == Some(self.runtime.incarnation())
                })
            })
            .collect();
        if settled.is_empty() {
            return Ok(false);
        }
        for filename in &settled {
            self.ledger
                .record(filename, delivery_ledger::Evidence::Consumed)?;
        }
        Ok(true)
    }

    /// Accept the exact successful terminal turn as a fallback receipt.
    ///
    /// Some supported Codex app-server releases persist and process a `clientUserMessageId` but do
    /// not broadcast the corresponding `item/completed{userMessage}` on the control subscription.
    /// A successful `turn/completed` for the turn returned by `turn/start` or `turn/steer` is the
    /// next monotone observation: the accepted input's turn ran to its ordinary end. Binding,
    /// incarnation, and exact turn identity prevent another thread or attempt from settling it.
    fn accept_turn_completion_receipt(
        &mut self,
        message: &Value,
        state: &CodexControlState,
    ) -> Result<bool> {
        if message.get("method").and_then(Value::as_str) != Some("turn/completed")
            || message.pointer("/params/threadId").and_then(Value::as_str)
                != Some(state.thread_id())
            || state.runtime_incarnation != self.runtime.incarnation()
            || codex_turn_outcome(message.pointer("/params/turn")) != CodexTurnOutcome::Accepted
        {
            return Ok(false);
        }
        let turn_id = required_string(message, "/params/turn/id", "turn/completed")?;
        let settled = self
            .ledger
            .entries()
            .iter()
            .filter(|entry| {
                entry.binding == state.thread_id()
                    && entry.incarnation.as_deref() == Some(self.runtime.incarnation())
                    && entry.accepted_turn_id.as_deref() == Some(turn_id)
                    && entry.phase < delivery_ledger::Phase::Consumed
            })
            .map(|entry| entry.filename.clone())
            .collect::<Vec<_>>();
        for filename in &settled {
            self.ledger
                .record(filename, delivery_ledger::Evidence::Consumed)?;
        }
        Ok(!settled.is_empty())
    }

    /// Reconcile a pre-crash attempt against the typed history returned by `thread/resume` before
    /// the same client ID can be sent again.
    fn reconcile_resume(&mut self, message: &Value, state: &CodexControlState) -> Result<()> {
        if message.get("error").is_some() {
            return Ok(());
        }
        let unsettled: Vec<(String, String, Option<String>)> = self
            .ledger
            .entries()
            .iter()
            .filter(|entry| {
                entry.binding == state.thread_id() && entry.phase < delivery_ledger::Phase::Consumed
            })
            .map(|entry| {
                (
                    entry.filename.clone(),
                    entry.correlation.value.clone(),
                    entry.accepted_turn_id.clone(),
                )
            })
            .collect();
        if unsettled.is_empty() {
            return Ok(());
        }
        let turns = message
            .pointer("/result/thread/turns")
            .and_then(Value::as_array)
            .context(
                "Codex thread/resume response has no typed turn history for delivery recovery",
            )?;
        for (filename, client_id, accepted_turn_id) in unsettled {
            let accepted = turns.iter().any(|turn| {
                let correlated_item =
                    turn.get("items")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(Value::as_str) == Some("userMessage")
                                    && item.get("clientId").and_then(Value::as_str)
                                        == Some(client_id.as_str())
                            })
                        });
                let completed_accepted_turn = accepted_turn_id.as_deref().is_some_and(|expected| {
                    turn.get("id").and_then(Value::as_str) == Some(expected)
                        && turn.get("status").and_then(Value::as_str) == Some("completed")
                });
                correlated_item || completed_accepted_turn
            });
            if accepted {
                self.ledger
                    .record(&filename, delivery_ledger::Evidence::Consumed)?;
            } else {
                // An authoritative resumed history without the client ID proves the pre-crash
                // attempt never landed. That absence is the receipt — retained, not erased —
                // and only it may authorize sending the same stable ID again.
                self.ledger
                    .negative(&filename, delivery_ledger::NegativeReceipt::Absent)?;
            }
        }
        Ok(())
    }
}

fn should_track_timeline_usage(message: &Value, active_turn_id: Option<&str>) -> bool {
    if message.get("method").and_then(Value::as_str) != Some("thread/tokenUsage/updated") {
        return true;
    }
    message
        .pointer("/params/turnId")
        .and_then(Value::as_str)
        .is_some_and(|turn_id| active_turn_id == Some(turn_id))
}

fn stable_client_user_message_id(recipient: &str, thread_id: &str, filename: &str) -> String {
    let mut hash = Sha256::new();
    let graph_message = filename.starts_with("message/");
    hash.update(if graph_message {
        b"st.codex-client-user-message.v1".as_slice()
    } else {
        b"st2.codex-client-user-message.v1".as_slice()
    });
    for value in [
        recipient.as_bytes(),
        thread_id.as_bytes(),
        filename.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    format!(
        "{}:{:x}",
        if graph_message { "st" } else { "st2" },
        hash.finalize()
    )
}

/// Read the exact native inbox filenames whose Codex deliveries reached consumption.
///
/// This is the graph bridge's receipt boundary: copying a projected message into the native inbox
/// is not delivery to a turn. The same ledger loader and correlation derivation used by the pump
/// validate the record before a graph lifecycle may advance. A missing ledger means no receipts.
pub fn consumed_delivery_filenames(
    state_dir: &Path,
    identity: &str,
    runtime_id: &str,
) -> Result<BTreeSet<String>> {
    let path = state_dir.join(delivery_ledger::LEDGER_FILE);
    if !path.is_file() {
        return Ok(BTreeSet::new());
    }
    let ledger = delivery_ledger::Ledger::open(
        &path,
        delivery_ledger::Harness::Codex.profile(),
        identity,
        runtime_id,
        |thread, filename| stable_client_user_message_id(identity, thread, filename),
    );
    if let Some(reason) = ledger.quarantined() {
        anyhow::bail!("Codex delivery receipt ledger is quarantined: {reason}");
    }
    Ok(ledger
        .entries()
        .iter()
        .filter(|entry| entry.phase == delivery_ledger::Phase::Consumed)
        .map(|entry| entry.filename.clone())
        .collect())
}

fn codex_delivery_request(
    request_id: u64,
    thread_id: &str,
    client_id: &str,
    text: &str,
    method: &CodexDeliveryMethod,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "clientUserMessageId": client_id,
        "input": [{ "type": "text", "text": text, "text_elements": [] }]
    });
    let method_name = match method {
        CodexDeliveryMethod::Start => "turn/start",
        CodexDeliveryMethod::Steer { turn_id } => {
            params["expectedTurnId"] = Value::String(turn_id.clone());
            "turn/steer"
        }
    };
    json!({ "method": method_name, "id": request_id, "params": params })
}

enum SubscriptionAcceptance {
    Accepted { changed: bool },
    Deferred,
}

impl CodexControlState {
    fn new(runtime: &CodexRuntime, thread_id: String) -> Self {
        Self {
            schema: crate::contracts::schema_for_owner(&runtime.schema, CONTROL_STATE_SCHEMA),
            agent: runtime.agent.clone(),
            runtime_id: runtime.runtime_id.clone(),
            runtime_incarnation: runtime.incarnation.clone(),
            thread_id,
            subscribed: false,
            observed: CodexObservedState::AwaitingStatus,
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn observed(&self) -> &CodexObservedState {
        &self.observed
    }

    pub fn subscribed(&self) -> bool {
        self.subscribed
    }

    fn accept_subscription(&mut self, message: &Value) -> Result<SubscriptionAcceptance> {
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            let detail = error.get("message").and_then(Value::as_str);
            if code == Some(-32600)
                && detail
                    .is_some_and(|detail| detail.starts_with("no rollout found for thread id "))
            {
                return Ok(SubscriptionAcceptance::Deferred);
            }
            anyhow::bail!("Codex app-server rejected control thread/resume: {error}");
        }
        anyhow::ensure!(
            message.get("result").is_some(),
            "Codex control thread/resume response has no result"
        );
        let thread_id = required_string(message, "/result/thread/id", "thread/resume response")?;
        anyhow::ensure!(
            thread_id == self.thread_id,
            "Codex control thread/resume returned a different thread"
        );
        let status = required_string(
            message,
            "/result/thread/status/type",
            "thread/resume response",
        )?;
        let blocked = human_blocking_flag(message.pointer("/result/thread/status"));
        let before = (self.subscribed, self.observed.clone());
        self.subscribed = true;
        self.observe_thread_status(status, blocked);
        let active_turns = message
            .pointer("/result/thread/turns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
            .filter_map(|turn| turn.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        match active_turns.as_slice() {
            [turn_id] => self.observe_turn_evidence(turn_id),
            [_, _, ..] => {
                self.observed = CodexObservedState::Held {
                    reason: CodexHoldReason::ConflictingTurn,
                    turn_id: None,
                };
            }
            [] => {}
        }
        Ok(SubscriptionAcceptance::Accepted {
            changed: (self.subscribed, self.observed.clone()) != before,
        })
    }

    fn observe(&mut self, message: &Value) -> Result<bool> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let before = self.observed.clone();
        match method {
            "thread/started" => {
                let thread_id = required_string(message, "/params/thread/id", method)?;
                if thread_id != self.thread_id {
                    return Ok(false);
                }
                let status = required_string(message, "/params/thread/status/type", method)?;
                let blocked = human_blocking_flag(message.pointer("/params/thread/status"));
                self.observe_thread_status(status, blocked);
            }
            "thread/status/changed" => {
                let thread_id = required_string(message, "/params/threadId", method)?;
                if thread_id != self.thread_id {
                    return Ok(false);
                }
                let status = required_string(message, "/params/status/type", method)?;
                let blocked = human_blocking_flag(message.pointer("/params/status"));
                self.observe_thread_status(status, blocked);
            }
            "turn/started" => {
                let thread_id = required_string(message, "/params/threadId", method)?;
                if thread_id != self.thread_id {
                    return Ok(false);
                }
                let turn_id = required_string(message, "/params/turn/id", method)?.to_string();
                self.observe_turn_started(turn_id);
            }
            "turn/completed" => {
                let thread_id = required_string(message, "/params/threadId", method)?;
                if thread_id != self.thread_id {
                    return Ok(false);
                }
                let turn_id = required_string(message, "/params/turn/id", method)?;
                let outcome = codex_turn_outcome(message.pointer("/params/turn"));
                self.observe_turn_completed(turn_id, outcome);
            }
            "item/started" | "item/completed" => {
                let thread_id = required_string(message, "/params/threadId", method)?;
                if thread_id != self.thread_id {
                    return Ok(false);
                }
                let item_type = required_string(message, "/params/item/type", method)?;
                let reported_turn_id = message
                    .pointer("/params/turnId")
                    .and_then(Value::as_str)
                    .filter(|turn_id| !turn_id.is_empty());
                // A control subscriber can attach after the owning TUI has already started its
                // initial turn. Every typed item is exact evidence for that turn, so use it to
                // recover steerability instead of holding the durable inbox until the turn ends.
                // An exit marker is evidence only about a hold that already exists; using it as
                // standalone turn-start evidence would turn a stale review exit into a live turn.
                if item_type != "exitedReviewMode"
                    && let Some(turn_id) = reported_turn_id
                {
                    self.observe_turn_evidence(turn_id);
                }
                // Codex 0.151 omitted `turnId` from ordinary item notifications. A hold item is
                // still attributable when the turn lifecycle already established one, but an
                // item without either source must never invent an identity.
                let turn_id =
                    reported_turn_id
                        .map(str::to_owned)
                        .or_else(|| match &self.observed {
                            CodexObservedState::Active { turn_id }
                            | CodexObservedState::Held {
                                turn_id: Some(turn_id),
                                ..
                            } => Some(turn_id.clone()),
                            _ => None,
                        });
                // The admitted `ThreadItem` schema has only three variants that change
                // steerability. Every other classified item reports work inside a turn that the
                // turn and thread status already model, so it is ignored on purpose. A later
                // protocol item that gates or releases input must be added here explicitly.
                // Silently dropping one is how `exitedReviewMode` stayed unmatched. Review is
                // also the only hold that the protocol ends with a typed item of its own:
                // `contextCompaction` has no exit item, so both of its lifecycle edges keep
                // holding until the thread proves otherwise.
                let (reason, released) = match item_type {
                    "enteredReviewMode" => (CodexHoldReason::Review, false),
                    "exitedReviewMode" => (CodexHoldReason::Review, true),
                    "contextCompaction" => (CodexHoldReason::Compaction, false),
                    _ if CLASSIFIED_CODEX_THREAD_ITEMS.contains(&item_type) => {
                        return Ok(self.observed != before);
                    }
                    _ => (CodexHoldReason::UnknownProtocol, false),
                };
                if released {
                    if let Some(turn_id) = turn_id.as_deref() {
                        self.observe_hold_released(turn_id, reason);
                    }
                } else if let Some(turn_id) = turn_id.as_deref() {
                    self.observe_non_steerable(turn_id, reason);
                }
            }
            _ if message.get("id").is_some()
                && !CLASSIFIED_CODEX_SERVER_REQUESTS.contains(&method) =>
            {
                self.observe_unknown_protocol();
            }
            _ => return Ok(false),
        }
        Ok(self.observed != before)
    }

    fn observe_thread_status(&mut self, status: &str, blocked: Option<CodexHoldReason>) {
        self.observed = match status {
            "idle" => CodexObservedState::Idle,
            "active" => match (&self.observed, blocked) {
                // A human-blocking flag holds the exact turn already proven active. Clearing it
                // releases that same turn, because no second `turn/started` arrives mid-turn.
                (CodexObservedState::Active { turn_id }, Some(reason)) => {
                    CodexObservedState::Held {
                        reason,
                        turn_id: Some(turn_id.clone()),
                    }
                }
                (
                    CodexObservedState::Held {
                        reason:
                            CodexHoldReason::WaitingOnApproval | CodexHoldReason::WaitingOnUserInput,
                        turn_id,
                    },
                    Some(reason),
                ) => CodexObservedState::Held {
                    reason,
                    turn_id: turn_id.clone(),
                },
                (
                    CodexObservedState::Held {
                        reason:
                            CodexHoldReason::WaitingOnApproval | CodexHoldReason::WaitingOnUserInput,
                        turn_id: Some(turn_id),
                    },
                    None,
                ) => CodexObservedState::Active {
                    turn_id: turn_id.clone(),
                },
                // A more specific hold outranks the flag: its turn ID still tracks the lifecycle.
                (
                    CodexObservedState::Active { .. }
                    | CodexObservedState::Held {
                        reason:
                            CodexHoldReason::Review
                            | CodexHoldReason::Compaction
                            | CodexHoldReason::UnknownProtocol
                            | CodexHoldReason::ConflictingTurn,
                        ..
                    },
                    _,
                ) => self.observed.clone(),
                // Flagged without a known turn: still a hold, but it names what it waits on.
                (_, Some(reason)) => CodexObservedState::Held {
                    reason,
                    turn_id: None,
                },
                (_, None) => CodexObservedState::Held {
                    reason: CodexHoldReason::ActiveWithoutTurn,
                    turn_id: None,
                },
            },
            "notLoaded" => CodexObservedState::Held {
                reason: CodexHoldReason::NotLoaded,
                turn_id: None,
            },
            "systemError"
                if matches!(
                    self.observed,
                    CodexObservedState::TerminalError {
                        reason: CodexTerminalError::SystemError
                    }
                ) =>
            {
                self.observed.clone()
            }
            "systemError" => CodexObservedState::Held {
                reason: CodexHoldReason::SystemError,
                turn_id: None,
            },
            _ => CodexObservedState::Held {
                reason: CodexHoldReason::UnknownStatus,
                turn_id: None,
            },
        };
    }

    fn observe_turn_started(&mut self, turn_id: String) {
        self.observed = match &self.observed {
            CodexObservedState::Active { turn_id: current } if current == &turn_id => {
                self.observed.clone()
            }
            CodexObservedState::Held {
                reason:
                    reason @ (CodexHoldReason::Review
                    | CodexHoldReason::Compaction
                    | CodexHoldReason::UnknownProtocol),
                ..
            } => CodexObservedState::Held {
                reason: *reason,
                turn_id: Some(turn_id),
            },
            CodexObservedState::Active { .. }
            | CodexObservedState::Held {
                reason: CodexHoldReason::ConflictingTurn,
                ..
            } => CodexObservedState::Held {
                reason: CodexHoldReason::ConflictingTurn,
                turn_id: None,
            },
            _ => CodexObservedState::Active { turn_id },
        };
    }

    fn observe_turn_evidence(&mut self, turn_id: &str) {
        self.observed = match &self.observed {
            CodexObservedState::AwaitingStatus
            | CodexObservedState::Held {
                reason: CodexHoldReason::ActiveWithoutTurn,
                ..
            } => CodexObservedState::Active {
                turn_id: turn_id.to_string(),
            },
            CodexObservedState::Held {
                reason:
                    reason @ (CodexHoldReason::WaitingOnApproval | CodexHoldReason::WaitingOnUserInput),
                turn_id: None,
            } => CodexObservedState::Held {
                reason: *reason,
                turn_id: Some(turn_id.to_string()),
            },
            _ => self.observed.clone(),
        };
    }

    fn observe_turn_completed(&mut self, turn_id: &str, outcome: CodexTurnOutcome) {
        // A failed turn whose typed error names a rejected credential is a SEAT-level fact: the
        // account was refused, which does not depend on which turn st2 believed live. It is
        // therefore settled before the turn-identity match below, and it outranks a plain
        // `systemError` terminal because it names the same failure's cause. Delivery semantics are
        // untouched: every `TerminalError` already permits `turn/start`.
        if outcome == CodexTurnOutcome::ProviderAuthRejected {
            self.observed = CodexObservedState::TerminalError {
                reason: CodexTerminalError::ProviderAuthRejected,
            };
            return;
        }
        if outcome == CodexTurnOutcome::ProviderCapacity {
            self.observed = CodexObservedState::TerminalError {
                reason: CodexTerminalError::ProviderCapacity,
            };
            return;
        }
        self.observed = match &self.observed {
            CodexObservedState::Idle => CodexObservedState::Idle,
            CodexObservedState::TerminalError { .. } => self.observed.clone(),
            CodexObservedState::Active { turn_id: current } if current == turn_id => {
                CodexObservedState::Idle
            }
            CodexObservedState::AwaitingStatus
            | CodexObservedState::Held {
                reason: CodexHoldReason::ActiveWithoutTurn,
                ..
            } => CodexObservedState::Idle,
            // Every other hold is owned by a signal that is not the turn lifecycle. A completion
            // is not evidence that a review or a compaction ended, that the thread reloaded, that
            // a reported system error cleared, or that the human a turn was waiting on has
            // answered, so it does not speak for them. Only the signal that minted the hold
            // releases it: the waiting-on-human holds are minted from `activeFlags` on a thread
            // status and are cleared by the next thread status that omits the flag.
            CodexObservedState::Held {
                reason:
                    CodexHoldReason::Review
                    | CodexHoldReason::Compaction
                    | CodexHoldReason::UnknownProtocol
                    | CodexHoldReason::ConflictingTurn
                    | CodexHoldReason::WaitingOnApproval
                    | CodexHoldReason::WaitingOnUserInput
                    | CodexHoldReason::NotLoaded
                    | CodexHoldReason::UnknownStatus,
                ..
            } => self.observed.clone(),
            CodexObservedState::Held {
                reason: CodexHoldReason::SystemError,
                ..
            } => CodexObservedState::TerminalError {
                reason: CodexTerminalError::SystemError,
            },
            // A completion for a turn other than the one believed live is the only evidence here
            // that two turns exist. This match stays exhaustive so a new observed state cannot
            // silently arrive as a conflict it never was.
            CodexObservedState::Active { .. } => CodexObservedState::Held {
                reason: CodexHoldReason::ConflictingTurn,
                turn_id: None,
            },
        };
    }

    fn observe_non_steerable(&mut self, turn_id: &str, reason: CodexHoldReason) {
        self.observed = match &self.observed {
            CodexObservedState::Active { turn_id: current } if current == turn_id => {
                CodexObservedState::Held {
                    reason,
                    turn_id: Some(turn_id.to_string()),
                }
            }
            CodexObservedState::Held {
                reason: current_reason,
                ..
            } if current_reason == &reason
                && matches!(
                    reason,
                    CodexHoldReason::Review
                        | CodexHoldReason::Compaction
                        | CodexHoldReason::UnknownProtocol
                ) =>
            {
                self.observed.clone()
            }
            _ if matches!(
                reason,
                CodexHoldReason::Review
                    | CodexHoldReason::Compaction
                    | CodexHoldReason::UnknownProtocol
            ) =>
            {
                CodexObservedState::Held {
                    reason,
                    turn_id: Some(turn_id.to_string()),
                }
            }
            _ => CodexObservedState::Held {
                reason: CodexHoldReason::ConflictingTurn,
                turn_id: None,
            },
        };
    }

    /// A typed hold-exit item releases only the hold it ends, and only for the exact turn that
    /// hold carries. The exit arrives inside a running turn, so the honest result is that turn
    /// active again rather than idle. Anything else — a different hold reason, a hold bound to
    /// another turn, or no hold at all — is not evidence about this state, so it is left alone:
    /// an exit must never invent an active turn or release a hold it did not end.
    fn observe_hold_released(&mut self, turn_id: &str, reason: CodexHoldReason) {
        let CodexObservedState::Held {
            reason: current_reason,
            turn_id: held_turn_id,
        } = &self.observed
        else {
            return;
        };
        if *current_reason != reason || held_turn_id.as_deref() != Some(turn_id) {
            return;
        }
        self.observed = CodexObservedState::Active {
            turn_id: turn_id.to_string(),
        };
    }

    fn observe_unknown_protocol(&mut self) {
        if matches!(self.observed, CodexObservedState::TerminalError { .. }) {
            return;
        }
        let turn_id = match &self.observed {
            CodexObservedState::Active { turn_id }
            | CodexObservedState::Held {
                turn_id: Some(turn_id),
                ..
            } => Some(turn_id.clone()),
            _ => None,
        };
        self.observed = CodexObservedState::Held {
            reason: CodexHoldReason::UnknownProtocol,
            turn_id,
        };
    }
}

fn observed_from_thread_snapshot(
    message: &Value,
    expected_thread_id: &str,
    previous: &CodexObservedState,
) -> Result<CodexObservedState> {
    let thread_id = required_string(message, "/result/thread/id", "thread/read response")?;
    anyhow::ensure!(
        thread_id == expected_thread_id,
        "Codex thread/read returned a different thread"
    );
    let status_value = message.pointer("/result/thread/status");
    let status = required_string(
        message,
        "/result/thread/status/type",
        "thread/read response",
    )?;
    let blocked = human_blocking_flag(status_value);
    if status != "active" {
        return Ok(match status {
            "idle" => CodexObservedState::Idle,
            "notLoaded" => CodexObservedState::Held {
                reason: CodexHoldReason::NotLoaded,
                turn_id: None,
            },
            "systemError"
                if matches!(
                    previous,
                    CodexObservedState::TerminalError {
                        reason: CodexTerminalError::SystemError
                    }
                ) =>
            {
                previous.clone()
            }
            "systemError" => CodexObservedState::Held {
                reason: CodexHoldReason::SystemError,
                turn_id: None,
            },
            _ => CodexObservedState::Held {
                reason: CodexHoldReason::UnknownStatus,
                turn_id: None,
            },
        });
    }
    // includeTurns=false deliberately gives no turn inventory. The subscription and bounded
    // transcript tail establish the exact turn ID; the status read only confirms its liveness.
    Ok(match (previous, blocked) {
        (CodexObservedState::Active { turn_id }, Some(reason)) => CodexObservedState::Held {
            reason,
            turn_id: Some(turn_id.clone()),
        },
        (CodexObservedState::Active { turn_id }, None) => CodexObservedState::Active {
            turn_id: turn_id.clone(),
        },
        (
            CodexObservedState::Held {
                reason: CodexHoldReason::WaitingOnApproval | CodexHoldReason::WaitingOnUserInput,
                turn_id,
            },
            Some(reason),
        ) => CodexObservedState::Held {
            reason,
            turn_id: turn_id.clone(),
        },
        (
            CodexObservedState::Held {
                reason: CodexHoldReason::WaitingOnApproval | CodexHoldReason::WaitingOnUserInput,
                turn_id: Some(turn_id),
            },
            None,
        ) => CodexObservedState::Active {
            turn_id: turn_id.clone(),
        },
        (
            CodexObservedState::Held {
                reason:
                    CodexHoldReason::Review
                    | CodexHoldReason::Compaction
                    | CodexHoldReason::UnknownProtocol
                    | CodexHoldReason::ConflictingTurn,
                ..
            },
            _,
        ) => previous.clone(),
        (_, Some(reason)) => CodexObservedState::Held {
            reason,
            turn_id: None,
        },
        (_, None) => CodexObservedState::Held {
            reason: CodexHoldReason::ActiveWithoutTurn,
            turn_id: None,
        },
    })
}

/// Read the delivery-relevant part of `ThreadStatus.activeFlags`: the first flag that says this
/// thread is blocked on a human rather than on the model.
///
/// The startup gate requires `activeFlags` on the `active` arm of `ThreadStatus`. A missing or
/// malformed runtime array reads as no flag instead of killing the control watcher. The startup
/// gate rejects an unclassified flag before launch.
fn human_blocking_flag(status: Option<&Value>) -> Option<CodexHoldReason> {
    status?
        .get("activeFlags")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|flag| match flag {
            "waitingOnApproval" => Some(CodexHoldReason::WaitingOnApproval),
            "waitingOnUserInput" => Some(CodexHoldReason::WaitingOnUserInput),
            _ => None,
        })
}

fn required_string<'a>(message: &'a Value, pointer: &str, method: &str) -> Result<&'a str> {
    message
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{method} has no non-empty {pointer}"))
}

/// Run one authored Codex argv behind a dedicated app server and initialized control connection.
pub fn run_controlled(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
) -> Result<()> {
    run_controlled_with_required_resume(catalog_root, identity, runtime_id, codex_argv, None, None)
}

/// Run one host-owned cold-residency attempt under its exact incarnation.
pub fn run_controlled_residency_attempt(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    resume_generation: crate::residency::Generation,
    required_incarnation: String,
) -> Result<()> {
    anyhow::ensure!(
        !required_incarnation.is_empty(),
        "Codex required runtime incarnation is empty"
    );
    run_controlled_with_required_resume(
        catalog_root,
        identity,
        runtime_id,
        codex_argv,
        Some(resume_generation),
        Some(required_incarnation),
    )
}

fn run_controlled_with_required_resume(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    required_resume_generation: Option<crate::residency::Generation>,
    required_incarnation: Option<String>,
) -> Result<()> {
    anyhow::ensure!(
        !codex_argv.is_empty(),
        "Codex controlled launch argv is empty"
    );
    anyhow::ensure!(
        required_resume_generation.is_some() == required_incarnation.is_some(),
        "Codex residency launch has an incomplete attempt fence"
    );
    // Install before any protocol preflight child exists. A SIGTERM in the preflight window must
    // set the stop flag instead of leaving a detached app server and stale socket behind.
    crate::provider_session::install_signal_handler();
    let state_dir = state_dir(catalog_root, &identity);
    secure_dir(&state_dir)?;
    let _owner_lock = acquire_owner_lock(&state_dir)?;
    let binding_path = state_dir.join("binding.json");
    let resume_thread = match required_resume_generation {
        Some(generation) => Some(required_residency_resume(
            &state_dir,
            &identity,
            &runtime_id,
            generation,
            &codex_argv[1..],
        )?),
        None => load_resume_thread(&binding_path, &identity, &runtime_id)?,
    };

    let mut delivery = CodexDeliveryConfig::resolve(catalog_root, &identity)?;
    match ensure_supported_protocol(&codex_argv[0]) {
        // The admitted version is the one fact the gate learns that outlives it: the diagnostic
        // record names the producer it was measured against, exactly as the OpenCode driver does.
        Ok(version) => delivery.producer_version = Some(version),
        Err(error) => {
            delivery.report_protocol_rejection(&codex_argv[0], &error);
            return Err(error);
        }
    }

    let mut diagnostics = WrapperDiagnostics::open(&state_dir, &identity, &runtime_id)?;
    diagnostics.record("ownerAcquired", json!({}))?;

    let result = run_controlled_owned(
        catalog_root,
        &state_dir,
        identity,
        runtime_id,
        codex_argv,
        delivery,
        resume_thread,
        required_incarnation,
        true,
        &mut diagnostics,
    );
    match result {
        Ok(()) => {
            diagnostics.record("completed", json!({}))?;
            Ok(())
        }
        Err(error) => {
            let error_text = format!("{error:#}");
            if let Err(diagnostic_error) =
                diagnostics.record("failed", json!({ "error": error_text }))
            {
                return Err(error).context(format!(
                    "persisting Codex wrapper failure diagnostic: {diagnostic_error:#}"
                ));
            }
            Err(error)
        }
    }
}

/// Run the native Codex driver with explicit private state paths.
///
/// The claims-graph runtime uses this entry point without an st2 catalog. The driver keeps the
/// app-server protocol, delivery receipts, and harness records. Each st3 seat
/// launch starts a new Codex thread, except a resumed seat's: `resume_thread` names the thread it
/// suspended on, and the TUI and control connection resume exactly that thread or fail.
#[allow(clippy::too_many_arguments)]
pub fn run_controlled_paths(
    driver_root: &Path,
    state_dir: &Path,
    agent_dir: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    gate: crate::session_control::DeliveryGate,
    resume_thread: Option<String>,
) -> Result<()> {
    anyhow::ensure!(
        !codex_argv.is_empty(),
        "Codex controlled launch argv is empty"
    );
    // Install before any protocol preflight child exists, exactly as the catalog entry point does.
    crate::provider_session::install_signal_handler();
    let producer_version = ensure_supported_protocol(&codex_argv[0])?;
    secure_dir(driver_root)?;
    secure_dir(state_dir)?;
    secure_dir(agent_dir)?;
    let inbox = message::inbox_dir(agent_dir);
    if !crate::push_mailbox::managed(agent_dir) {
        secure_dir(&inbox)?;
        secure_dir(&message::archive_dir(agent_dir))?;
    }
    let delivery = CodexDeliveryConfig {
        control: crate::session_control::SessionControl::Graph(gate),
        catalog_root: driver_root.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        inbox,
        identity: identity.clone(),
        this_host: run::detect_host(),
        supervisor: None,
        producer_version: Some(producer_version),
        model: None,
    };
    let _owner_lock = acquire_owner_lock(state_dir)?;
    let mut diagnostics = WrapperDiagnostics::open(state_dir, &identity, &runtime_id)?;
    diagnostics.record("ownerAcquired", json!({ "mode": "explicit-paths" }))?;
    // The prior binding is always retired: a resumed thread binds again once its control
    // connection accepts `thread/resume`, and only that new binding proves the resume.
    let retired = select_resume_thread(
        &state_dir.join("binding.json"),
        &identity,
        &runtime_id,
        false,
    )?;
    if let Some(thread) = &resume_thread {
        anyhow::ensure!(
            resume_insertion_index(&codex_argv[1..])?.is_some(),
            "the Codex argv already selects a thread, so it cannot resume {thread}"
        );
    }
    let resume_thread = resume_thread.or(retired);
    let result = run_controlled_owned(
        driver_root,
        state_dir,
        identity,
        runtime_id,
        codex_argv,
        delivery,
        resume_thread,
        None,
        false,
        &mut diagnostics,
    );
    match result {
        Ok(()) => {
            diagnostics.record("completed", json!({}))?;
            Ok(())
        }
        Err(error)
            if error
                .downcast_ref::<crate::provider_session::Detached>()
                .is_some() =>
        {
            Err(error)
        }
        Err(error) => {
            let text = format!("{error:#}");
            let _ = diagnostics.record("failed", json!({ "error": text }));
            Err(error)
        }
    }
}

/// Resume a Codex session a predecessor driver image launched through [`run_controlled_paths`]
/// and released for adoption: take over its app-server group, reconnect a control observer to
/// the bound thread, and supervise the running TUI to its end.
#[allow(clippy::too_many_arguments)]
pub fn adopt_controlled_paths(
    driver_root: &Path,
    state_dir: &Path,
    agent_dir: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    tui_pid: u32,
    server_pid: u32,
    watchdog_pid: u32,
    owner_write_fd: i32,
    socket_path: PathBuf,
    safe_fallback: bool,
    gate: crate::session_control::DeliveryGate,
) -> Result<()> {
    anyhow::ensure!(
        !codex_argv.is_empty(),
        "Codex controlled launch argv is empty"
    );
    // SAFETY: the predecessor released this descriptor for exactly this adoption.
    let mut server = unsafe {
        OwnedProcessGroup::adopt(
            server_pid,
            watchdog_pid,
            owner_write_fd,
            socket_path.clone(),
        )
    };
    let producer_version = ensure_supported_protocol(&codex_argv[0])?;
    let inbox = message::inbox_dir(agent_dir);
    let delivery = CodexDeliveryConfig {
        control: crate::session_control::SessionControl::Graph(gate),
        catalog_root: driver_root.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        inbox,
        identity: identity.clone(),
        this_host: run::detect_host(),
        supervisor: None,
        producer_version: Some(producer_version),
        model: declared_codex_model(&codex_argv[1..]),
    };
    let _owner_lock = acquire_owner_lock(state_dir)?;
    let mut diagnostics = WrapperDiagnostics::open(state_dir, &identity, &runtime_id)?;
    diagnostics.record(
        "adoptedFromPredecessor",
        json!({ "tuiPid": tui_pid, "serverPid": server_pid }),
    )?;
    let runtime = load_runtime(&state_dir.join("runtime.json"), &identity, &runtime_id)?;
    let binding = load_current_binding(&state_dir.join("binding.json"), &runtime)?
        .context("the adopted Codex session has no thread binding")?;
    let safe_fallback_active = Arc::new(AtomicBool::new(safe_fallback));
    let result = run_adopted(
        server.child_mut(),
        &socket_path,
        state_dir,
        &runtime,
        binding.thread_id().to_owned(),
        ProviderProcess::adopted(tui_pid),
        delivery,
        safe_fallback_active.clone(),
        &mut diagnostics,
    );
    if let Err(error) = &result
        && let Some(detached) = error.downcast_ref::<TuiDetached>()
    {
        let tui_pid = detached.tui_pid;
        let released = server
            .release()
            .context("the Codex app-server group has no watchdog pipe to hand over")?;
        diagnostics.record(
            "detachedForAdoption",
            json!({ "tuiPid": tui_pid, "serverPid": released.server_pid }),
        )?;
        return Err(crate::provider_session::Detached {
            session: crate::provider_session::DetachedSession::Codex {
                tui_pid,
                server_pid: released.server_pid,
                watchdog_pid: released.watchdog_pid,
                owner_write_fd: released.owner_write_fd,
                socket_path,
                safe_fallback: safe_fallback_active.load(Ordering::SeqCst),
            },
        }
        .into());
    }
    server.terminate();
    match result {
        Ok(()) => {
            diagnostics.record("completed", json!({}))?;
            Ok(())
        }
        Err(error) => {
            let text = format!("{error:#}");
            let _ = diagnostics.record("failed", json!({ "error": text }));
            Err(error)
        }
    }
}

/// The adopted counterpart of [`run_connected`]: the TUI already owns the bound thread, so the new
/// control observer resumes that thread and the monitor starts from the binding wait.
#[allow(clippy::too_many_arguments)]
fn run_adopted(
    server: &mut ProviderProcess,
    socket_path: &Path,
    state_dir: &Path,
    runtime: &CodexRuntime,
    thread_id: String,
    mut tui: ProviderProcess,
    delivery: CodexDeliveryConfig,
    safe_fallback_active: Arc<AtomicBool>,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<()> {
    let harness_agent_dir = delivery.agent_dir.clone();
    let harness_identity = delivery.identity.clone();
    diagnostics.record("waitingForControlSocket", json!({ "pid": server.id() }))?;
    let connected = connect_control(server, socket_path, STARTUP_TIMEOUT)?;
    let websocket = match connected {
        Some(control) => {
            let shutdown = control.try_clone()?;
            initialize_control(control)?.map(|websocket| (websocket, shutdown))
        }
        None => None,
    };
    let Some((websocket, shutdown)) = websocket else {
        // A stop raised before the observer reconnected still ends this session.
        terminate_child(&mut tui);
        let status = tui.try_wait().ok().flatten();
        let mut writer = harness_state::Writer::new(
            &harness_agent_dir,
            harness_identity,
            "codex",
            Some(runtime.runtime_id().to_string()),
        )
        .with_session(runtime.incarnation());
        let _ = writer.ended(describe_tui_exit(status));
        return Ok(());
    };
    diagnostics.record("controlInitialized", json!({ "adopted": true }))?;
    let (events_tx, events_rx) = mpsc::channel();
    let binding_path = state_dir.join("binding.json");
    let control_state_path = state_dir.join("control-state.json");
    let runtime_for_reader = runtime.clone();
    // The TUI is already running and already loaded the thread, so the resume gate opens at once.
    let (ready_tx, ready_rx) = mpsc::channel();
    let _ = ready_tx.send(());
    let event_thread = thread::spawn(move || {
        let resume = ControlResume {
            thread_id: &thread_id,
            ready: ready_rx,
            tui_loaded_timeout: TUI_LOADED_TIMEOUT,
            permission_overrides: None,
            preload: false,
            preloaded: None,
        };
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime_for_reader,
            Some(resume),
            Some(delivery),
            safe_fallback_active,
            events_tx,
        )
    });
    let result = (|| -> Result<TuiEnd> {
        diagnostics.record("waitingForThreadBinding", json!({ "pid": tui.id() }))?;
        match wait_for_binding(&mut tui, &events_rx, STARTUP_TIMEOUT, diagnostics)? {
            BindingWait::Bound => {
                diagnostics.record("threadBound", json!({ "pid": tui.id(), "adopted": true }))?;
                monitor_bound_tui(&mut tui, &events_rx)
            }
            BindingWait::Stopped => {
                terminate_child(&mut tui);
                Ok(TuiEnd::Stopped(tui.try_wait().ok().flatten()))
            }
            BindingWait::TuiExited(status) => Ok(TuiEnd::Exited(status)),
        }
    })();
    if result.is_err() {
        terminate_child(&mut tui);
    }
    finish_tui_session(
        result,
        &mut tui,
        shutdown,
        event_thread,
        runtime,
        &harness_agent_dir,
        &harness_identity,
    )
}

fn run_controlled_owned(
    catalog_root: &Path,
    state_dir: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    mut delivery: CodexDeliveryConfig,
    resume_thread: Option<String>,
    required_incarnation: Option<String>,
    allow_safe_fallback: bool,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<()> {
    delivery.model = declared_codex_model(&codex_argv[1..]);
    let socket_path = socket_path(catalog_root, &identity)?;
    let socket_dir = socket_path
        .parent()
        .context("Codex app-server socket has no parent")?;
    secure_dir(socket_dir)?;
    prepare_socket_for_launch(&socket_path)?;

    let endpoint = format!("unix://{}", socket_path.display());
    let prepared =
        prepare_controlled_launch_args(&endpoint, &codex_argv[1..], resume_thread.as_deref());
    strict_launch_preflight(&prepared, allow_safe_fallback)?;
    let safe_fallback_active = Arc::new(AtomicBool::new(false));
    if prepared.safe_fallback {
        record_safe_fallback(
            diagnostics,
            &safe_fallback_active,
            "declaredArgumentsRejectedBeforeSpawn",
            &prepared.declared_options,
        )?;
    }

    // Publish the host-owned incarnation for a residency attempt only after this process holds
    // the owner lock. Ordinary launches continue to mint their incarnation at this boundary.
    let runtime = match required_incarnation {
        Some(incarnation) => CodexRuntime::with_incarnation(identity, runtime_id, incarnation)?,
        None => CodexRuntime::fresh(identity, runtime_id)?,
    };
    atomic_json(&state_dir.join("runtime.json"), &runtime)?;
    diagnostics.record(
        "runtimePublished",
        json!({
            "runtimeIncarnation": runtime.incarnation(),
            "resumeSelected": resume_thread.is_some(),
        }),
    )?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state_dir.join("app-server.log"))?;
    let mut server_args = prepared.server_args;
    if !prepared.safe_fallback
        && resume_thread.is_some()
        && authored_bypasses_hook_trust(&codex_argv[1..])?
    {
        let hook_cwd = controlled_hook_cwd(&codex_argv[1..])?;
        if let Some(projection) = preflight_hook_trust(
            &codex_argv[0],
            &server_args,
            &socket_path,
            &hook_cwd,
            &log,
            diagnostics,
        )? {
            insert_app_server_config_override(&mut server_args, projection.override_value)?;
        }
    }
    diagnostics.record(
        "appServerStarting",
        json!({ "safeFallback": prepared.safe_fallback }),
    )?;
    let mut server = spawn_controlled_app_server(&codex_argv[0], &server_args, &socket_path, &log)?;
    let mut result = diagnostics
        .record("appServerStarted", json!({ "pid": server.id() }))
        .and_then(|_| {
            run_connected(
                server.child_mut(),
                &socket_path,
                state_dir,
                &runtime,
                &codex_argv,
                prepared.tui_args.clone(),
                prepared.safe_tui_args.clone(),
                prepared.expected_resume.clone(),
                prepared.resume_permissions.clone(),
                safe_fallback_active.clone(),
                prepared.declared_options.clone(),
                delivery.clone(),
                allow_safe_fallback,
                diagnostics,
            )
        });
    if allow_safe_fallback
        && !prepared.safe_fallback
        && result.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<AppServerExitedBeforeControl>()
                .is_some()
        })
    {
        server.terminate();
        prepare_socket_for_launch(&socket_path)?;
        record_safe_fallback(
            diagnostics,
            &safe_fallback_active,
            "declaredAppServerExitedBeforeControl",
            &prepared.declared_options,
        )?;
        diagnostics.record("safeFallbackAppServerStarting", json!({}))?;
        server = spawn_controlled_app_server(
            &codex_argv[0],
            &safe_controlled_app_server_args(&endpoint),
            &socket_path,
            &log,
        )?;
        result = diagnostics
            .record(
                "safeFallbackAppServerStarted",
                json!({ "pid": server.id() }),
            )
            .and_then(|_| {
                run_connected(
                    server.child_mut(),
                    &socket_path,
                    state_dir,
                    &runtime,
                    &codex_argv,
                    prepared.safe_tui_args.clone(),
                    prepared.safe_tui_args.clone(),
                    resume_thread.clone(),
                    None,
                    safe_fallback_active.clone(),
                    prepared.declared_options.clone(),
                    delivery,
                    allow_safe_fallback,
                    diagnostics,
                )
            });
    }
    if let Err(error) = &result
        && let Some(detached) = error.downcast_ref::<TuiDetached>()
    {
        let tui_pid = detached.tui_pid;
        let released = server
            .release()
            .context("the Codex app-server group has no watchdog pipe to hand over")?;
        diagnostics.record(
            "detachedForAdoption",
            json!({ "tuiPid": tui_pid, "serverPid": released.server_pid }),
        )?;
        return Err(crate::provider_session::Detached {
            session: crate::provider_session::DetachedSession::Codex {
                tui_pid,
                server_pid: released.server_pid,
                watchdog_pid: released.watchdog_pid,
                owner_write_fd: released.owner_write_fd,
                socket_path,
                safe_fallback: safe_fallback_active.load(Ordering::SeqCst),
            },
        }
        .into());
    }
    server.terminate();
    result
}

fn declared_codex_model(args: &[String]) -> Option<String> {
    let mut selected = None;
    let mut index = 0;
    while index < args.len() && args[index] != "--" {
        if matches!(args[index].as_str(), "-m" | "--model") {
            selected = args.get(index + 1).cloned();
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--model=") {
            selected = Some(value.to_owned());
        }
        index += 1;
    }
    selected.filter(|model| !model.is_empty())
}

fn prepare_socket_for_launch(socket_path: &Path) -> Result<()> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_socket(),
                "Codex app-server path already exists and is not a socket: {}",
                socket_path.display()
            );
            match UnixStream::connect(socket_path) {
                Ok(_) => anyhow::bail!(
                    "Codex app-server socket {} is already live; refusing a second control owner",
                    socket_path.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(socket_path).with_context(|| {
                        format!("removing stale Codex socket {}", socket_path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "checking existing Codex socket {} before launch",
                            socket_path.display()
                        )
                    });
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("checking Codex socket path {}", socket_path.display()));
        }
    }
    Ok(())
}

fn spawn_controlled_app_server(
    codex: &str,
    args: &[String],
    socket_path: &Path,
    log: &File,
) -> Result<OwnedProcessGroup> {
    let mut command = Command::new(codex);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    spawn_process_group(&mut command, Some(socket_path))
        .with_context(|| format!("starting {codex} app-server"))
}

/// Settings every controlled TUI starts with. A seat starts with no prompt, so any interactive
/// startup screen would hold it before it creates a thread; the update notice is one.
const CONTROLLED_TUI_OVERRIDES: [&str; 2] = ["-c", "check_for_update_on_startup=false"];

fn controlled_tui_command(codex: &str, args: &[String]) -> Command {
    let mut command = Command::new(codex);
    command.args(CONTROLLED_TUI_OVERRIDES).args(args);
    command
}

fn spawn_controlled_tui(codex: &str, args: &[String]) -> std::io::Result<ProviderProcess> {
    controlled_tui_command(codex, args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map(ProviderProcess::Spawned)
}

fn claim_safe_fallback_attempt(attempted: &mut bool) -> bool {
    if *attempted {
        return false;
    }
    *attempted = true;
    true
}

fn run_connected(
    server: &mut ProviderProcess,
    socket_path: &Path,
    state_dir: &Path,
    runtime: &CodexRuntime,
    codex_argv: &[String],
    mut tui_args: Vec<String>,
    safe_tui_args: Vec<String>,
    expected_resume: Option<String>,
    resume_permissions: Option<ResumePermissionOverrides>,
    safe_fallback_active: Arc<AtomicBool>,
    declared_options: Vec<String>,
    delivery: CodexDeliveryConfig,
    allow_safe_fallback: bool,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<()> {
    // The stop handler is installed by the launch entry point before any spawn (the preflight's
    // detached app-server included); re-installing here would RESET a stop flag raised during
    // startup, so this function only relies on it.
    diagnostics.record("waitingForControlSocket", json!({ "pid": server.id() }))?;
    // A stop during startup ends the launch before anything was observed: no TUI exists, the
    // caller reaps the app-server, and this session leaves no record — its predecessor's ages
    // out on its own.
    let Some(control) = connect_control(server, socket_path, STARTUP_TIMEOUT)? else {
        diagnostics.record("stoppedDuringStartup", json!({ "phase": "connect" }))?;
        return Ok(());
    };
    diagnostics.record("controlSocketConnected", json!({}))?;
    let shutdown = control.try_clone()?;
    if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
        diagnostics.record("stoppedDuringStartup", json!({ "phase": "initialize" }))?;
        let _ = shutdown.shutdown(Shutdown::Both);
        return Ok(());
    }
    // The initialize wait itself polls the stop flag between short socket timeouts and returns
    // None on a stop; the recheck below covers a stop raised in the remaining gaps.
    let Some(websocket) = initialize_control(control)? else {
        diagnostics.record("stoppedDuringStartup", json!({ "phase": "initialize" }))?;
        let _ = shutdown.shutdown(Shutdown::Both);
        return Ok(());
    };
    if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
        diagnostics.record("stoppedDuringStartup", json!({ "phase": "initialized" }))?;
        let _ = shutdown.shutdown(Shutdown::Both);
        return Ok(());
    }
    diagnostics.record("controlInitialized", json!({}))?;
    let (events_tx, events_rx) = mpsc::channel();
    let binding_path = state_dir.join("binding.json");
    let control_state_path = state_dir.join("control-state.json");
    let runtime_for_reader = runtime.clone();
    let (mut resume_ready_tx, resume_ready_rx) = if expected_resume.is_some() {
        let (tx, rx) = mpsc::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let preload_resume = expected_resume.is_some() && resume_permissions.is_some();
    let (preloaded_tx, preloaded_rx) = if preload_resume {
        let (tx, rx) = mpsc::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let harness_agent_dir = delivery.agent_dir.clone();
    let harness_identity = delivery.identity.clone();
    let fallback_for_reader = safe_fallback_active.clone();
    let event_thread = thread::spawn(move || {
        let resume = expected_resume
            .as_deref()
            .zip(resume_ready_rx)
            .map(|(thread_id, ready)| ControlResume {
                thread_id,
                ready,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: resume_permissions,
                preload: preload_resume,
                preloaded: preloaded_tx,
            });
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime_for_reader,
            resume,
            Some(delivery),
            fallback_for_reader,
            events_tx,
        )
    });

    // Permission-declared resumes must load through control first. Once a remote TUI loads a saved
    // thread with the provider's default read-only profile, a later control resume cannot change
    // that live thread's sandbox. Resumes without declared permissions keep the TUI-first path.
    if let Some(preloaded) = preloaded_rx {
        let preload_result = (|| -> Result<()> {
            resume_ready_tx
                .take()
                .context("preloaded Codex resume has no start gate")?
                .send(())
                .context("starting preloaded Codex control resume")?;
            preloaded
                .recv_timeout(STARTUP_TIMEOUT)
                .context("Codex control did not preload the declared resume policy")?;
            Ok(())
        })();
        if let Err(error) = preload_result {
            drop(resume_ready_tx);
            let _ = shutdown.shutdown(Shutdown::Both);
            let _ = event_thread.join();
            return Err(error);
        }
    }
    // A fresh initialized observer reads before this child can issue thread/start.
    // Insert the remote endpoint as a global Codex option and preserve every authored argument
    // after the provider executable.
    let mut tui = match spawn_controlled_tui(&codex_argv[0], &tui_args) {
        Ok(tui) => tui,
        Err(error) => {
            drop(resume_ready_tx);
            let _ = shutdown.shutdown(Shutdown::Both);
            let _ = event_thread.join();
            // The claim already wrote its ended(superseded) placeholder; leaving that as the
            // last word would read as "another session took over". The launch failure is this
            // session's real terminal outcome — token-only adoption resolves to the claim's
            // sequence, since the claim put this token on disk.
            let mut writer = harness_state::Writer::new(
                &harness_agent_dir,
                harness_identity.clone(),
                "codex",
                Some(runtime.runtime_id().to_string()),
            )
            .with_session(runtime.incarnation());
            let _ = writer.observe(
                harness_state::Observation::new(
                    harness_state::Activity::Ended,
                    harness_state::BlockedOn::None,
                    harness_state::InputBuffer::Unknown,
                )
                .with_reason("launch-error")
                .with_exit("exit unknown"),
            );
            return Err(error)
                .with_context(|| format!("starting controlled {} TUI", codex_argv[0]));
        }
    };
    let result = (|| -> Result<TuiEnd> {
        let mut fallback_attempted = safe_fallback_active.load(Ordering::SeqCst);
        loop {
            diagnostics.record(
                "tuiStarted",
                json!({
                    "pid": tui.id(),
                    "safeFallback": safe_fallback_active.load(Ordering::SeqCst),
                }),
            )?;
            if let Some(ready) = resume_ready_tx.take() {
                ready
                    .send(())
                    .context("starting Codex control resume after the TUI launched")?;
            }
            diagnostics.record("waitingForThreadBinding", json!({ "pid": tui.id() }))?;
            match wait_for_binding(&mut tui, &events_rx, STARTUP_TIMEOUT, diagnostics)? {
                BindingWait::Bound => {
                    diagnostics.record("threadBound", json!({ "pid": tui.id() }))?;
                    return monitor_bound_tui(&mut tui, &events_rx);
                }
                BindingWait::Stopped => {
                    terminate_child(&mut tui);
                    return Ok(TuiEnd::Stopped(tui.try_wait().ok().flatten()));
                }
                BindingWait::TuiExited(status)
                    if allow_safe_fallback
                        && claim_safe_fallback_attempt(&mut fallback_attempted) =>
                {
                    record_safe_fallback(
                        diagnostics,
                        &safe_fallback_active,
                        "declaredTuiExitedBeforeThreadBinding",
                        &declared_options,
                    )?;
                    diagnostics.record(
                        "safeFallbackRetryStarting",
                        json!({ "previousExit": status.to_string() }),
                    )?;
                    tui_args.clone_from(&safe_tui_args);
                    tui = spawn_controlled_tui(&codex_argv[0], &tui_args).with_context(|| {
                        format!("starting known-safe fallback {} TUI", codex_argv[0])
                    })?;
                }
                BindingWait::TuiExited(status) => {
                    anyhow::bail!("controlled Codex TUI exited before thread binding: {status}");
                }
            }
        }
    })();
    if result.is_err() {
        terminate_child(&mut tui);
    }
    drop(resume_ready_tx);
    finish_tui_session(
        result,
        &mut tui,
        shutdown,
        event_thread,
        runtime,
        &harness_agent_dir,
        &harness_identity,
    )
}

/// End one controlled TUI session after its monitor returned: stop the control pump, then publish
/// the terminal observation. A detached session skips the terminal record, because its TUI and
/// app-server keep running under the driver's next image.
fn finish_tui_session(
    result: Result<TuiEnd>,
    tui: &mut ProviderProcess,
    shutdown: UnixStream,
    event_thread: thread::JoinHandle<()>,
    runtime: &CodexRuntime,
    harness_agent_dir: &Path,
    harness_identity: &str,
) -> Result<()> {
    let _ = shutdown.shutdown(Shutdown::Both);
    let _ = event_thread.join();
    if matches!(result, Ok(TuiEnd::Detached)) {
        return Err(TuiDetached { tui_pid: tui.id() }.into());
    }
    // The pump is gone, so nothing can observe this session again: publish the terminal
    // observation with the outcome the wrapper actually saw, before any staleness horizon.
    // Consumers must not branch on `reason`, so the observed exit always lands in `exit`.
    // Same incarnation as the pump's writer, adopting by token: the pump's written claim (or any
    // of its writes) put this token on disk, so token-only adoption resolves to this session's
    // claimed sequence — and the terminal record fences exactly the records this session wrote.
    let mut harness_writer = harness_state::Writer::new(
        harness_agent_dir,
        harness_identity.to_owned(),
        "codex",
        Some(runtime.runtime_id().to_string()),
    )
    .with_session(runtime.incarnation());
    let _ = match &result {
        Ok(TuiEnd::Exited(status)) => harness_writer.ended(describe_tui_exit(Some(*status))),
        Ok(TuiEnd::Stopped(status)) => harness_writer.ended(describe_tui_exit(*status)),
        Ok(TuiEnd::Detached) => Ok(()),
        Err(error) => {
            let observed_exit = tui.try_wait().ok().flatten();
            harness_writer.observe(
                harness_state::Observation::new(
                    harness_state::Activity::Ended,
                    harness_state::BlockedOn::None,
                    harness_state::InputBuffer::Unknown,
                )
                .with_exit(describe_tui_exit(observed_exit))
                .with_reason(format!("{error}")),
            )
        }
    };
    match result {
        Ok(TuiEnd::Exited(status)) => completed_tui(status),
        // The wrapper stopped its own session: not a failure, mirroring the shared wrapper body.
        Ok(TuiEnd::Stopped(_)) | Ok(TuiEnd::Detached) => Ok(()),
        Err(error) => Err(error),
    }
}

/// How the controlled TUI session came to an end, as the monitor saw it.
enum TuiEnd {
    /// The TUI exited on its own with this status.
    Exited(ExitStatus),
    /// The wrapper's stop flag ended the session; the reaped status when one was observable.
    Stopped(Option<ExitStatus>),
    /// [`crate::provider_session::DETACH`] released the live TUI to the next driver image.
    Detached,
}

/// The TUI was released for adoption; the launch path adds its app-server group.
#[derive(Debug)]
struct TuiDetached {
    tui_pid: u32,
}

impl fmt::Display for TuiDetached {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the Codex TUI {} was released for adoption",
            self.tui_pid
        )
    }
}

impl std::error::Error for TuiDetached {}

/// The label for a TUI end whose status may not have been observable at all. A status that WAS
/// reaped is spelled by the one shared exit-label map; no status is the same "unknown" the map's
/// own unanswerable arm reports.
fn describe_tui_exit(status: Option<ExitStatus>) -> String {
    status
        .map(crate::provider_session::describe_exit)
        .unwrap_or_else(|| "exit unknown".to_string())
}

/// Start app-server with the authored global configuration inputs that its CLI supports.
///
/// Project trust, strict parsing, and feature selection affect config and hook loading in the
/// server process. Passing them only to the remote TUI silently creates two different effective
/// configurations. TUI-only policy, model, workspace, authentication, and prompt arguments stay
/// on the TUI command.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedControlledLaunch {
    server_args: Vec<String>,
    tui_args: Vec<String>,
    safe_tui_args: Vec<String>,
    expected_resume: Option<String>,
    resume_permissions: Option<ResumePermissionOverrides>,
    declared_options: Vec<String>,
    safe_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumePermissionOverrides {
    approval_policy: Option<String>,
    approvals_reviewer: Option<String>,
    sandbox: Option<String>,
}

impl ResumePermissionOverrides {
    fn app_server_config_overrides(&self) -> Vec<String> {
        let mut overrides = Vec::new();
        for (key, value) in [
            ("approval_policy", self.approval_policy.as_deref()),
            ("approvals_reviewer", self.approvals_reviewer.as_deref()),
            ("sandbox_mode", self.sandbox.as_deref()),
        ] {
            if let Some(value) = value {
                overrides.push(format!("{key}={}", toml::Value::String(value.into())));
            }
        }
        overrides
    }

    fn apply_to(&self, params: &mut serde_json::Map<String, Value>) {
        if let Some(policy) = &self.approval_policy {
            params.insert("approvalPolicy".into(), Value::String(policy.clone()));
        }
        if let Some(reviewer) = &self.approvals_reviewer {
            params.insert("approvalsReviewer".into(), Value::String(reviewer.clone()));
        }
        if let Some(sandbox) = &self.sandbox {
            params.insert("sandbox".into(), Value::String(sandbox.clone()));
        }
    }

    fn diagnostic(&self, requested_policy_applied: bool) -> Value {
        json!({
            "approvalPolicy": self.approval_policy,
            "approvalsReviewer": self.approvals_reviewer,
            "sandbox": self.sandbox,
            "requestedPolicyApplied": requested_policy_applied,
        })
    }
}

fn prepare_controlled_launch_args(
    endpoint: &str,
    authored_args: &[String],
    resume_thread: Option<&str>,
) -> PreparedControlledLaunch {
    let declared_options = declared_option_names(authored_args);
    let exact = controlled_app_server_args(endpoint, authored_args).and_then(|mut server_args| {
        let resume_permissions =
            automatic_resume_permission_overrides(authored_args, resume_thread)?;
        if let Some(permissions) = &resume_permissions {
            // The remote TUI's resume argv cannot contain CLI permission flags. Project the
            // declaration into app-server defaults before either client loads the saved thread;
            // a later thread/resume cannot reliably change an already-loaded thread.
            for override_value in permissions.app_server_config_overrides() {
                insert_app_server_config_override(&mut server_args, override_value)?;
            }
        }
        Ok((
            server_args,
            controlled_tui_args(endpoint, authored_args, resume_thread)?,
            expected_resume_thread(authored_args, resume_thread)?.map(str::to_owned),
            resume_permissions,
        ))
    });
    match exact {
        Ok((server_args, tui_args, expected_resume, resume_permissions)) => {
            PreparedControlledLaunch {
                server_args,
                tui_args,
                safe_tui_args: safe_controlled_tui_args(endpoint, resume_thread),
                expected_resume,
                resume_permissions,
                declared_options,
                safe_fallback: false,
            }
        }
        Err(_) => PreparedControlledLaunch {
            server_args: safe_controlled_app_server_args(endpoint),
            tui_args: safe_controlled_tui_args(endpoint, resume_thread),
            safe_tui_args: safe_controlled_tui_args(endpoint, resume_thread),
            expected_resume: resume_thread.map(str::to_owned),
            resume_permissions: None,
            declared_options,
            safe_fallback: true,
        },
    }
}

fn strict_launch_preflight(
    prepared: &PreparedControlledLaunch,
    allow_safe_fallback: bool,
) -> Result<()> {
    anyhow::ensure!(
        allow_safe_fallback || !prepared.safe_fallback,
        "declared Codex options were rejected before launch: {}",
        prepared.declared_options.join(", ")
    );
    Ok(())
}

fn automatic_resume_permission_overrides(
    authored_args: &[String],
    resume_thread: Option<&str>,
) -> Result<Option<ResumePermissionOverrides>> {
    if resume_thread.is_none() {
        return Ok(None);
    }
    let Some(insertion) = resume_insertion_index(authored_args)? else {
        return Ok(None);
    };
    let mut overrides = ResumePermissionOverrides {
        approval_policy: None,
        approvals_reviewer: None,
        sandbox: None,
    };
    let mut index = 0;
    while index < insertion {
        let argument = authored_args[index].as_str();
        match argument {
            "--dangerously-bypass-approvals-and-sandbox" => {
                overrides.approval_policy = Some("never".into());
                overrides.sandbox = Some("danger-full-access".into());
                index += 1;
            }
            "--approve-for-me" => {
                overrides.approval_policy = Some("on-request".into());
                overrides.approvals_reviewer = Some("auto_review".into());
                overrides.sandbox = Some("workspace-write".into());
                index += 1;
            }
            "-s" | "--sandbox" => {
                let value = authored_args
                    .get(index + 1)
                    .context("Codex sandbox option has no value")?;
                validate_resume_sandbox(value)?;
                overrides.sandbox = Some(value.clone());
                index += 2;
            }
            "-a" | "--ask-for-approval" => {
                let value = authored_args
                    .get(index + 1)
                    .context("Codex approval option has no value")?;
                validate_resume_approval_policy(value)?;
                overrides.approval_policy = Some(value.clone());
                index += 2;
            }
            _ if argument.starts_with("--sandbox=") => {
                let value = argument.trim_start_matches("--sandbox=");
                validate_resume_sandbox(value)?;
                overrides.sandbox = Some(value.into());
                index += 1;
            }
            _ if argument.starts_with("--ask-for-approval=") => {
                let value = argument.trim_start_matches("--ask-for-approval=");
                validate_resume_approval_policy(value)?;
                overrides.approval_policy = Some(value.into());
                index += 1;
            }
            _ if argument.starts_with("-s") && argument.len() > 2 => {
                let value = &argument[2..];
                validate_resume_sandbox(value)?;
                overrides.sandbox = Some(value.into());
                index += 1;
            }
            _ if argument.starts_with("-a") && argument.len() > 2 => {
                let value = &argument[2..];
                validate_resume_approval_policy(value)?;
                overrides.approval_policy = Some(value.into());
                index += 1;
            }
            _ => {
                index += if matches!(
                    argument,
                    "-c" | "--config"
                        | "--enable"
                        | "--disable"
                        | "--remote-auth-token-env"
                        | "-m"
                        | "--model"
                        | "--local-provider"
                        | "-p"
                        | "--profile"
                        | "-C"
                        | "--cd"
                        | "--add-dir"
                ) {
                    2
                } else {
                    1
                };
            }
        }
    }
    Ok((overrides.approval_policy.is_some()
        || overrides.approvals_reviewer.is_some()
        || overrides.sandbox.is_some())
    .then_some(overrides))
}

fn validate_resume_sandbox(value: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(
            value,
            "read-only" | "workspace-write" | "danger-full-access"
        ),
        "unsupported Codex sandbox mode '{value}'"
    );
    Ok(())
}

fn validate_resume_approval_policy(value: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(value, "on-request" | "never"),
        "unsupported Codex approval policy '{value}'"
    );
    Ok(())
}

fn safe_controlled_app_server_args(endpoint: &str) -> Vec<String> {
    vec![
        "app-server".to_string(),
        "--listen".to_string(),
        endpoint.to_string(),
    ]
}

fn safe_controlled_tui_args(endpoint: &str, resume_thread: Option<&str>) -> Vec<String> {
    let mut args = vec!["--remote".to_string(), endpoint.to_string()];
    if let Some(thread_id) = resume_thread {
        args.extend(["resume".to_string(), thread_id.to_string()]);
    }
    args
}

fn declared_option_names(authored_args: &[String]) -> Vec<String> {
    let mut options = Vec::new();
    let mut index = 0;
    while index < authored_args.len() {
        let argument = authored_args[index].as_str();
        if argument == "--" || !argument.starts_with('-') || argument == "-" {
            break;
        }
        let option = diagnostic_option_name(argument);
        if !options.contains(&option) {
            options.push(option);
        }
        let exact_value_option = matches!(
            argument,
            "-c" | "--config"
                | "--enable"
                | "--disable"
                | "--remote-auth-token-env"
                | "-m"
                | "--model"
                | "--local-provider"
                | "-p"
                | "--profile"
                | "-s"
                | "--sandbox"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "-a"
                | "--ask-for-approval"
        );
        let known_flag = matches!(
            argument,
            "--strict-config"
                | "--oss"
                | "--approve-for-me"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--search"
                | "--no-alt-screen"
                | "-h"
                | "--help"
                | "-V"
                | "--version"
        );
        index += if exact_value_option { 2 } else { 1 };
        if !exact_value_option
            && !known_flag
            && !argument.contains('=')
            && !argument.starts_with("-c")
            && !argument.starts_with("-m")
            && !argument.starts_with("-p")
            && !argument.starts_with("-s")
            && !argument.starts_with("-C")
            && !argument.starts_with("-a")
        {
            // This is the rejected unknown option. Stop before a following token that might be
            // its secret value rather than another option.
            break;
        }
    }
    options
}

fn record_safe_fallback(
    diagnostics: &mut WrapperDiagnostics,
    active: &AtomicBool,
    cause: &str,
    declared_options: &[String],
) -> Result<()> {
    if active.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    diagnostics.record(
        "safeFallbackActivated",
        json!({
            "cause": cause,
            "declaredOptions": declared_options,
            "mode": "minimalRemoteTui",
            "requestedPolicyApplied": false,
        }),
    )?;
    eprintln!(
        "st codex: declared launch arguments were rejected; booting once with known-safe flags (declared options: {})",
        if declared_options.is_empty() {
            "none".to_string()
        } else {
            declared_options.join(", ")
        }
    );
    Ok(())
}

fn controlled_app_server_args(endpoint: &str, authored_args: &[String]) -> Result<Vec<String>> {
    let boundary = interactive_root_prefix_end(authored_args)?;
    let mut args = vec!["app-server".to_string()];
    let mut index = 0;
    while index < boundary {
        let argument = authored_args[index].as_str();
        if matches!(argument, "-c" | "--config" | "--enable" | "--disable") {
            args.push(argument.to_string());
            args.push(authored_args[index + 1].clone());
            index += 2;
            continue;
        }
        if argument == "--strict-config"
            || argument.starts_with("--config=")
            || argument.starts_with("--enable=")
            || argument.starts_with("--disable=")
            || (argument.starts_with("-c") && argument.len() > 2)
        {
            args.push(argument.to_string());
            index += 1;
            continue;
        }
        if matches!(
            argument,
            "--oss"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--search"
                | "--no-alt-screen"
        ) {
            index += 1;
            continue;
        }
        if matches!(argument, "-i" | "--image")
            || argument.starts_with("-i=")
            || argument.starts_with("--image=")
        {
            break;
        }
        let exact_value_option = matches!(
            argument,
            "--remote-auth-token-env"
                | "-m"
                | "--model"
                | "--local-provider"
                | "-p"
                | "--profile"
                | "-s"
                | "--sandbox"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "-a"
                | "--ask-for-approval"
        );
        index += if exact_value_option { 2 } else { 1 };
    }
    args.extend(["--listen".to_string(), endpoint.to_string()]);
    Ok(args)
}

fn authored_bypasses_hook_trust(authored_args: &[String]) -> Result<bool> {
    let boundary = interactive_root_prefix_end(authored_args)?;
    Ok(authored_args[..boundary]
        .iter()
        .any(|argument| argument == "--dangerously-bypass-hook-trust"))
}

/// Resolve the workspace whose non-managed hooks the remote TUI reviews before a resume.
///
/// st2 starts the wrapper in the declared workspace. An explicit Codex `--cd`/`-C` overrides it,
/// and the last occurrence wins just as the provider CLI does. The path must already exist because
/// both project-layer discovery and remote resume require a real directory.
fn controlled_hook_cwd(authored_args: &[String]) -> Result<PathBuf> {
    let boundary = interactive_root_prefix_end(authored_args)?;
    let mut selected = std::env::current_dir().context("reading controlled Codex workspace")?;
    let mut index = 0;
    while index < boundary {
        let argument = authored_args[index].as_str();
        if matches!(argument, "-C" | "--cd") {
            selected = PathBuf::from(&authored_args[index + 1]);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--cd=") {
            selected = PathBuf::from(value);
        } else if let Some(value) = argument.strip_prefix("-C")
            && !value.is_empty()
        {
            selected = PathBuf::from(value);
        }
        index += if matches!(
            argument,
            "-c" | "--config"
                | "--enable"
                | "--disable"
                | "--remote-auth-token-env"
                | "-m"
                | "--model"
                | "--local-provider"
                | "-p"
                | "--profile"
                | "-s"
                | "--sandbox"
                | "--add-dir"
                | "-a"
                | "--ask-for-approval"
        ) {
            2
        } else {
            1
        };
    }
    if selected.is_relative() {
        selected = std::env::current_dir()
            .context("reading controlled Codex workspace")?
            .join(selected);
    }
    fs::canonicalize(&selected).with_context(|| {
        format!(
            "resolving controlled Codex workspace {}",
            selected.display()
        )
    })
}

#[derive(Debug)]
struct HookTrustProjection {
    override_value: String,
    count: usize,
}

/// Codex 0.145/0.146 deliberately ignores the hook-trust bypass for startup review on every
/// persistent remote resume. Before the owning TUI starts, ask the same exact provider binary for
/// its typed hook keys and hashes, then project those hashes into the final app-server's session
/// flags. This implements the authored one-invocation bypass without writing persisted trust.
fn preflight_hook_trust(
    codex: &str,
    server_args: &[String],
    socket_path: &Path,
    cwd: &Path,
    log: &File,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<Option<HookTrustProjection>> {
    diagnostics.record("hookTrustPreflightStarting", json!({}))?;
    let mut server_command = Command::new(codex);
    server_command
        .args(server_args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let mut server = spawn_process_group(&mut server_command, Some(socket_path))
        .with_context(|| format!("starting {codex} hook-trust preflight app-server"))?;
    let result = diagnostics
        .record("hookTrustPreflightStarted", json!({ "pid": server.id() }))
        .and_then(|_| {
            let Some(control) = connect_control(server.child_mut(), socket_path, STARTUP_TIMEOUT)?
            else {
                // Stop requested mid-preflight: skip the projection — the launch proceeds to the
                // connect stage, whose own stop check exits gracefully before the TUI starts.
                return Ok(None);
            };
            let Some(mut websocket) = initialize_control(control)? else {
                return Ok(None);
            };
            query_hook_trust_projection(&mut websocket, cwd)
        });
    server.terminate();
    let projection = result?;
    diagnostics.record(
        "hookTrustPreflightComplete",
        json!({ "projectedHookCount": projection.as_ref().map_or(0, |value| value.count) }),
    )?;
    Ok(projection)
}

fn query_hook_trust_projection(
    websocket: &mut WebSocket<UnixStream>,
    cwd: &Path,
) -> Result<Option<HookTrustProjection>> {
    write_json_message(
        websocket,
        &json!({
            "method": "hooks/list",
            "id": HOOK_TRUST_PREFLIGHT_REQUEST_ID,
            "params": { "cwds": [cwd.to_string_lossy()] },
        }),
    )?;
    websocket.get_ref().set_read_timeout(Some(CONTROL_POLL))?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let response = loop {
        match read_startup_message(websocket, deadline)? {
            StartupRead::Message(message)
                if message.get("id") == Some(&Value::from(HOOK_TRUST_PREFLIGHT_REQUEST_ID)) =>
            {
                break message;
            }
            StartupRead::Message(_) => continue,
            // A stop mid-preflight skips the projection; the launch's own stop checks exit
            // gracefully before the real server spawns anything further.
            StartupRead::Stopped => return Ok(None),
            StartupRead::Closed => {
                anyhow::bail!("Codex app-server closed during hook-trust preflight")
            }
        }
    };
    if let Some(error) = response.get("error") {
        anyhow::bail!("Codex app-server rejected hooks/list preflight: {error}");
    }
    hook_trust_projection_from_response(&response, cwd)
}

fn hook_trust_projection_from_response(
    response: &Value,
    cwd: &Path,
) -> Result<Option<HookTrustProjection>> {
    let data = response
        .pointer("/result/data")
        .and_then(Value::as_array)
        .context("Codex hooks/list preflight response has no typed data")?;
    anyhow::ensure!(
        data.len() == 1,
        "Codex hooks/list preflight returned {} cwd entries instead of one",
        data.len()
    );
    let entry = &data[0];
    anyhow::ensure!(
        entry.get("cwd").and_then(Value::as_str) == Some(cwd.to_string_lossy().as_ref()),
        "Codex hooks/list preflight returned a different cwd"
    );
    let hooks = entry
        .get("hooks")
        .and_then(Value::as_array)
        .context("Codex hooks/list preflight cwd entry has no typed hooks")?;
    let mut projected = BTreeMap::new();
    for hook in hooks {
        let status = hook
            .get("trustStatus")
            .and_then(Value::as_str)
            .context("Codex hooks/list preflight hook has no trustStatus")?;
        match status {
            "trusted" | "managed" => continue,
            "untrusted" | "modified" => {}
            other => {
                anyhow::bail!("Codex hooks/list preflight returned unknown trustStatus '{other}'")
            }
        }
        anyhow::ensure!(
            hook.get("isManaged").and_then(Value::as_bool) == Some(false),
            "Codex hooks/list preflight returned a managed hook requiring trust"
        );
        let key = hook
            .get("key")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Codex hooks/list preflight hook has no non-empty key")?;
        let current_hash = hook
            .get("currentHash")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("sha256:") && value.len() > "sha256:".len())
            .context("Codex hooks/list preflight hook has no typed currentHash")?;
        if let Some(previous) = projected.insert(key.to_string(), current_hash.to_string()) {
            anyhow::ensure!(
                previous == current_hash,
                "Codex hooks/list preflight returned conflicting hashes for one hook key"
            );
        }
    }
    if projected.is_empty() {
        return Ok(None);
    }

    let mut state = toml::Table::new();
    for (key, current_hash) in projected {
        let mut trust = toml::Table::new();
        trust.insert(
            "trusted_hash".to_string(),
            toml::Value::String(current_hash),
        );
        state.insert(key, toml::Value::Table(trust));
    }
    Ok(Some(HookTrustProjection {
        count: state.len(),
        override_value: format!("hooks.state={}", toml::Value::Table(state)),
    }))
}

fn insert_app_server_config_override(
    server_args: &mut Vec<String>,
    override_value: String,
) -> Result<()> {
    let listen = server_args
        .iter()
        .position(|argument| argument == "--listen")
        .context("controlled Codex app-server argv has no --listen boundary")?;
    server_args.splice(listen..listen, ["-c".to_string(), override_value]);
    Ok(())
}

fn controlled_tui_args(
    endpoint: &str,
    authored_args: &[String],
    resume_thread: Option<&str>,
) -> Result<Vec<String>> {
    let mut args = vec!["--remote".to_string(), endpoint.to_string()];
    let Some(thread_id) = resume_thread else {
        args.extend_from_slice(authored_args);
        return Ok(args);
    };
    let Some(insertion) = resume_insertion_index(authored_args)? else {
        args.extend_from_slice(authored_args);
        return Ok(args);
    };
    args.push("resume".to_string());
    // A remote task owns its permission policy. Codex 0.156 rejects attempts to override that
    // policy while resuming, so automatic resume preserves every non-permission global option but
    // omits permission and hook-trust overrides here. Hook trust is projected through the typed
    // app-server preflight above, while the declared approval/sandbox policy is projected through
    // this driver's typed control `thread/resume` after the owning TUI loads the thread. Fresh
    // launches and explicit authored resume/fork commands remain byte-for-byte exact.
    args.extend(resume_compatible_root_args(&authored_args[..insertion]));
    args.push(thread_id.to_string());
    args.extend_from_slice(&authored_args[insertion..]);
    Ok(args)
}

fn resume_compatible_root_args(authored_prefix: &[String]) -> Vec<String> {
    let mut compatible = Vec::with_capacity(authored_prefix.len());
    let mut index = 0;
    while index < authored_prefix.len() {
        let argument = authored_prefix[index].as_str();
        if matches!(
            argument,
            "--approve-for-me"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
        ) {
            index += 1;
            continue;
        }
        if matches!(argument, "-s" | "--sandbox" | "-a" | "--ask-for-approval") {
            index += 2;
            continue;
        }
        if argument.starts_with("--sandbox=")
            || argument.starts_with("--ask-for-approval=")
            || (argument.starts_with("-s") && argument.len() > 2)
            || (argument.starts_with("-a") && argument.len() > 2)
        {
            index += 1;
            continue;
        }
        compatible.push(authored_prefix[index].clone());
        index += 1;
    }
    compatible
}

/// A saved binding constrains the watcher only when st2 inserted that resume selection.
///
/// An authored `resume` or `fork` command owns its own selection. The watcher binds the first typed
/// event from that command instead of rejecting it because it differs from an older saved binding.
fn expected_resume_thread<'a>(
    authored_args: &[String],
    resume_thread: Option<&'a str>,
) -> Result<Option<&'a str>> {
    let Some(thread_id) = resume_thread else {
        return Ok(None);
    };
    Ok(resume_insertion_index(authored_args)?
        .is_some()
        .then_some(thread_id))
}

/// Find where a supported Codex interactive argv begins its prompt or subcommand.
///
/// Automatic resume must insert `resume <thread>` after global options and before the authored
/// prompt. Unknown options fail closed because guessing can turn an option value into a prompt or a
/// prompt into a session selector. `--image` is variadic, so automatic resume requires an explicit
/// `--` boundary when that option is present.
fn resume_insertion_index(authored_args: &[String]) -> Result<Option<usize>> {
    let insertion = interactive_root_prefix_end(authored_args)?;
    if authored_args
        .get(insertion)
        .is_some_and(|argument| matches!(argument.as_str(), "resume" | "fork"))
    {
        Ok(None)
    } else {
        Ok(Some(insertion))
    }
}

fn interactive_root_prefix_end(authored_args: &[String]) -> Result<usize> {
    let delimiter = authored_args.iter().position(|arg| arg == "--");
    let mut index = 0;
    while index < authored_args.len() {
        let argument = authored_args[index].as_str();
        if argument == "--" {
            return Ok(index);
        }
        if !argument.starts_with('-') || argument == "-" {
            return Ok(index);
        }

        if matches!(
            argument,
            "--strict-config"
                | "--oss"
                | "--approve-for-me"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--search"
                | "--no-alt-screen"
        ) {
            index += 1;
            continue;
        }
        anyhow::ensure!(
            !matches!(argument, "-h" | "--help" | "-V" | "--version"),
            "cannot automatically resume a Codex help or version invocation"
        );

        let exact_value_option = matches!(
            argument,
            "-c" | "--config"
                | "--enable"
                | "--disable"
                | "--remote-auth-token-env"
                | "-m"
                | "--model"
                | "--local-provider"
                | "-p"
                | "--profile"
                | "-s"
                | "--sandbox"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "-a"
                | "--ask-for-approval"
        );
        if exact_value_option {
            anyhow::ensure!(
                index + 1 < authored_args.len(),
                "Codex option '{argument}' has no value"
            );
            index += 2;
            continue;
        }
        if matches!(argument, "-i" | "--image")
            || argument.starts_with("-i=")
            || argument.starts_with("--image=")
        {
            let boundary = delimiter.context(
                "automatic Codex resume with variadic --image requires an explicit `--` prompt boundary",
            )?;
            return Ok(boundary);
        }

        let long_value = [
            "--config=",
            "--enable=",
            "--disable=",
            "--remote-auth-token-env=",
            "--model=",
            "--local-provider=",
            "--profile=",
            "--sandbox=",
            "--cd=",
            "--add-dir=",
            "--ask-for-approval=",
        ]
        .iter()
        .any(|prefix| argument.starts_with(prefix));
        let short_value = ["-c", "-m", "-p", "-s", "-C", "-a"]
            .iter()
            .any(|prefix| argument.starts_with(prefix) && argument.len() > prefix.len());
        anyhow::ensure!(
            long_value || short_value,
            "cannot automatically resume through unknown Codex option '{}'",
            diagnostic_option_name(argument)
        );
        index += 1;
    }
    Ok(authored_args.len())
}

fn diagnostic_option_name(argument: &str) -> String {
    if let Some((name, _)) = argument.split_once('=') {
        return name.to_string();
    }
    if argument.starts_with("--") {
        return argument.to_string();
    }
    argument.chars().take(2).collect()
}

fn connect_control(
    server: &mut crate::provider_session::ProviderProcess,
    socket_path: &Path,
    timeout: Duration,
) -> Result<Option<UnixStream>> {
    let deadline = Instant::now() + timeout;
    loop {
        // st2's stop path may fire before the control socket ever connects; without this check
        // the wrapper would sit out the whole startup timeout with SIGTERM already delivered.
        if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(None);
        }
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(Some(stream)),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = server.try_wait()? {
                    return Err(AppServerExitedBeforeControl(status).into());
                }
                if error.kind() != std::io::ErrorKind::NotFound
                    && error.kind() != std::io::ErrorKind::ConnectionRefused
                {
                    return Err(error).with_context(|| {
                        format!("connecting Codex control socket {}", socket_path.display())
                    });
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Codex control socket {} was not ready within {}s",
                        socket_path.display(),
                        timeout.as_secs()
                    )
                });
            }
        }
    }
}

/// `Ok(None)` = a stop was raised mid-initialize; the caller exits gracefully.
fn initialize_control(stream: UnixStream) -> Result<Option<WebSocket<UnixStream>>> {
    // Nonblocking handshake reads produce resumable `Interrupted` states. This
    // avoids treating unrelated process signals as fatal socket I/O while
    // retaining a bounded stop-check cadence during a silent handshake.
    stream.set_nonblocking(true)?;
    let handshake_deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut pending = tungstenite::client("ws://localhost/", stream);
    let (mut websocket, response) = loop {
        match pending {
            Ok(done) => break done,
            Err(tungstenite::HandshakeError::Interrupted(resumable)) => {
                if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(None);
                }
                anyhow::ensure!(
                    Instant::now() < handshake_deadline,
                    "Codex WebSocket handshake timed out"
                );
                std::thread::sleep(CONTROL_POLL);
                pending = resumable.handshake();
            }
            Err(tungstenite::HandshakeError::Failure(error)) => {
                if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(None);
                }
                anyhow::bail!("Codex WebSocket handshake failed: {error}")
            }
        }
    };
    websocket.set_config(|config| {
        config.max_frame_size = Some(CODEX_CONTROL_MAX_MESSAGE_BYTES);
        config.max_message_size = Some(CODEX_CONTROL_MAX_MESSAGE_BYTES);
    });
    websocket.get_mut().set_nonblocking(false)?;
    websocket.get_mut().set_read_timeout(Some(CONTROL_POLL))?;
    anyhow::ensure!(
        response.status().as_u16() == 101,
        "Codex WebSocket handshake returned {}",
        response.status()
    );
    write_json_message(
        &mut websocket,
        &json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "st",
                    "title": "st",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }
        }),
    )?;

    // Short socket timeouts make the stop flag observable through the up-to-30s wait; the
    // startup timeout is restored below so later control reads keep their semantics.
    websocket.get_ref().set_read_timeout(Some(CONTROL_POLL))?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let message = match read_startup_message(&mut websocket, deadline)? {
            StartupRead::Message(message) => message,
            StartupRead::Stopped => return Ok(None),
            StartupRead::Closed => {
                anyhow::bail!("Codex app-server closed the control connection during initialize")
            }
        };
        if message.get("id") != Some(&Value::from(0)) {
            continue;
        }
        if let Some(error) = message.get("error") {
            anyhow::bail!("Codex app-server rejected initialize: {error}");
        }
        anyhow::ensure!(
            message.get("result").is_some(),
            "Codex app-server initialize response has no result"
        );
        break;
    }
    websocket
        .get_ref()
        .set_read_timeout(Some(STARTUP_TIMEOUT))?;
    write_json_message(
        &mut websocket,
        &json!({ "method": "initialized", "params": {} }),
    )?;
    websocket.get_ref().set_read_timeout(None)?;
    Ok(Some(websocket))
}

/// Wait until the owning TUI has loaded the preserved thread before this control connection
/// subscribes with its own `thread/resume` request.
///
/// Process creation is not ownership evidence. If control resumes immediately after spawn, it can
/// win the cold resume and create the session before the TUI has attached, so a successful control
/// response would not prove that the TUI consumed its authored prompt. `thread/loaded/list` is a
/// typed observation of the TUI's progress and is available in every admitted Codex version.
fn wait_for_tui_loaded_thread(
    websocket: &mut WebSocket<UnixStream>,
    expected_thread_id: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        eprintln!("codex control: requesting TUI-loaded thread list");
        write_json_message(
            websocket,
            &json!({
                "method": "thread/loaded/list",
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "params": {},
            }),
        )?;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                remaining >= Duration::from_millis(1),
                "controlled Codex TUI did not load preserved thread {expected_thread_id} before control resume"
            );
            websocket
                .get_ref()
                .set_read_timeout(Some(remaining.min(CONTROL_POLL)))?;
            let message = match poll_json_message(websocket)
                .context("polling Codex TUI-loaded response")?
            {
                ControlRead::Message(message) => message,
                ControlRead::Timeout => continue,
                ControlRead::Closed => anyhow::bail!(
                    "Codex app-server closed the control connection while waiting for the TUI to load preserved thread {expected_thread_id}"
                ),
            };
            if message.get("id") != Some(&Value::from(CONTROL_TUI_LOADED_REQUEST_ID)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                anyhow::bail!("Codex app-server rejected thread/loaded/list: {error}");
            }
            let loaded = message
                .pointer("/result/data")
                .and_then(Value::as_array)
                .context("Codex thread/loaded/list response has no typed data")?;
            let contains_expected = loaded.iter().try_fold(false, |found, thread_id| {
                let thread_id = thread_id
                    .as_str()
                    .context("Codex thread/loaded/list returned a non-string thread id")?;
                Ok::<_, anyhow::Error>(found || thread_id == expected_thread_id)
            })?;
            if contains_expected {
                return Ok(());
            }
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "controlled Codex TUI did not load preserved thread {expected_thread_id} before control resume"
        );
        thread::sleep(remaining.min(CONTROL_POLL));
    }
}

#[derive(Debug)]
enum ControlEvent {
    TuiThreadLoaded(Sender<()>),
    ResumePermissionPolicyApplied(ResumePermissionOverrides),
    SafeFallbackActivated {
        cause: &'static str,
        permissions: ResumePermissionOverrides,
    },
    Bound,
    Observed,
    Closed,
    Failed(String),
}

struct ControlResume<'a> {
    thread_id: &'a str,
    ready: Receiver<()>,
    tui_loaded_timeout: Duration,
    permission_overrides: Option<ResumePermissionOverrides>,
    preload: bool,
    preloaded: Option<Sender<()>>,
}

fn control_resume_request(
    thread_id: &str,
    permission_overrides: Option<&ResumePermissionOverrides>,
) -> Value {
    let mut params =
        serde_json::Map::from_iter([("threadId".into(), Value::String(thread_id.into()))]);
    if let Some(permission_overrides) = permission_overrides {
        permission_overrides.apply_to(&mut params);
    }
    json!({
        "method": "thread/resume",
        "id": CONTROL_SUBSCRIBE_REQUEST_ID,
        "params": params,
    })
}

fn resume_permission_overrides_applied(
    message: &Value,
    expected: &ResumePermissionOverrides,
) -> bool {
    let sandbox = expected.sandbox.as_deref().map(|sandbox| match sandbox {
        "read-only" => "readOnly",
        "workspace-write" => "workspaceWrite",
        "danger-full-access" => "dangerFullAccess",
        _ => "",
    });
    expected.approval_policy.as_deref().is_none_or(|policy| {
        message
            .pointer("/result/approvalPolicy")
            .and_then(Value::as_str)
            == Some(policy)
    }) && expected
        .approvals_reviewer
        .as_deref()
        .is_none_or(|reviewer| {
            message
                .pointer("/result/approvalsReviewer")
                .and_then(Value::as_str)
                == Some(reviewer)
        })
        && sandbox.is_none_or(|sandbox| {
            message
                .pointer("/result/sandbox/type")
                .and_then(Value::as_str)
                == Some(sandbox)
        })
}

fn pump_control(
    mut websocket: WebSocket<UnixStream>,
    binding_path: &Path,
    control_state_path: &Path,
    runtime: &CodexRuntime,
    resume: Option<ControlResume<'_>>,
    delivery: Option<CodexDeliveryConfig>,
    safe_fallback_active: Arc<AtomicBool>,
    events: Sender<ControlEvent>,
) {
    let result = (|| -> Result<()> {
        let (
            expected_resume,
            resume_ready,
            tui_loaded_timeout,
            mut resume_permissions,
            preload,
            mut preloaded,
        ) = match resume {
            Some(resume) => (
                Some(resume.thread_id),
                Some(resume.ready),
                resume.tui_loaded_timeout,
                resume.permission_overrides,
                resume.preload,
                resume.preloaded,
            ),
            None => (None, None, TUI_LOADED_TIMEOUT, None, false, None),
        };
        let mut control_state: Option<CodexControlState> = None;
        let mut subscription_pending = false;
        let mut last_transcript_turn_recovery = None;
        let mut peer_closed = false;
        let delivery_ledger_path = control_state_path.with_file_name(delivery_ledger::LEDGER_FILE);
        let mut delivery = delivery
            .map(|config| {
                CodexInboxDelivery::new(
                    config,
                    delivery_ledger_path.clone(),
                    runtime.clone(),
                    safe_fallback_active.clone(),
                )
            })
            .transpose()
            .context("initializing Codex inbox delivery")?;
        if let Some(thread_id) = expected_resume {
            resume_ready
                .context("saved Codex binding has no TUI-start gate")?
                .recv()
                .context("controlled Codex TUI ended before control resume")?;
            if !preload {
                wait_for_tui_loaded_thread(&mut websocket, thread_id, tui_loaded_timeout)
                    .context("waiting for Codex TUI thread load")?;
                let (diagnostic_tx, diagnostic_rx) = mpsc::channel();
                events
                    .send(ControlEvent::TuiThreadLoaded(diagnostic_tx))
                    .context("recording that the Codex TUI loaded the preserved thread")?;
                diagnostic_rx
                    .recv()
                    .context("waiting for the Codex TUI-loaded diagnostic before control resume")?;
            }
            if safe_fallback_active.load(Ordering::SeqCst) {
                resume_permissions = None;
            }
            write_json_message(
                &mut websocket,
                &control_resume_request(thread_id, resume_permissions.as_ref()),
            )
            .context("sending Codex thread resume request")?;
            subscription_pending = true;
        }
        loop {
            if let Some(delivery) = delivery.as_mut() {
                delivery.sync_safe_fallback_diagnostic();
            }
            if !peer_closed
                && let Err(error) = websocket.get_ref().set_read_timeout(Some(CONTROL_POLL))
            {
                if error.kind() == std::io::ErrorKind::InvalidInput {
                    // Darwin can reject setsockopt after the peer has closed
                    // the Unix socket. Keep reading: buffered WebSocket
                    // frames must be processed before EOF is reported.
                    peer_closed = true;
                    let _ = websocket.get_ref().set_read_timeout(None);
                } else {
                    return Err(error).context("setting Codex control poll timeout");
                }
            }
            let message =
                match poll_json_message(&mut websocket).context("polling Codex control socket")? {
                    ControlRead::Message(message) => Some(message),
                    ControlRead::Timeout => None,
                    ControlRead::Closed => {
                        let _ = events.send(ControlEvent::Closed);
                        return Ok(());
                    }
                };
            let Some(message) = message else {
                if let Some(state) = control_state.as_mut() {
                    if let Some(delivery) = delivery.as_mut() {
                        delivery.refresh_transcript_context_if_due(state.thread_id());
                    }
                    recover_transcript_turn_if_due(
                        state,
                        &mut delivery,
                        &mut last_transcript_turn_recovery,
                        control_state_path,
                        &events,
                    )?;
                    if let Some(delivery) = delivery.as_mut() {
                        if let Some(request) = delivery.maybe_account_request() {
                            write_json_message(&mut websocket, &request)
                                .context("sending Codex account/read")?;
                        }
                        if let Some(request) = delivery.maybe_snapshot_request(state)? {
                            write_json_message(&mut websocket, &request)
                                .context("sending Codex on-demand thread/read")?;
                        } else if let Some(request) = delivery.maybe_request(state)? {
                            write_json_message(&mut websocket, &request)
                                .context("sending Codex delivery request")?;
                        }
                    }
                }
                continue;
            };
            if let Some(delivery) = delivery.as_mut()
                && delivery.observe_account(&message)
            {
                continue;
            }
            if control_state.is_none() {
                if let Some(thread_id) = expected_resume {
                    if message.get("method").is_some()
                        || message.get("id") != Some(&Value::from(CONTROL_SUBSCRIBE_REQUEST_ID))
                    {
                        // The one notification worth reading before the resume response lands. The
                        // app-server replays `thread/tokenUsage/updated` to a newly attached
                        // connection, and the resumed thread still holds its context — so dropping
                        // it here would leave a seat that resumes and then waits for work reading
                        // `context: null` against a full window, with nothing to correct it until
                        // the next model response. The claim this construction already made removed
                        // the predecessor's record, so there is nothing else to fall back on.
                        //
                        // Ordering-agnostic on purpose: if the replay arrives after the response
                        // the loop below already sees it and this call reads nothing. A duplicate
                        // reading costs nothing — the bucket guard skips it.
                        //
                        // The fresh-binding path below needs no such call: there is no thread id
                        // until `binding_candidate` names one, and a thread starting now has no
                        // history to replay.
                        if let Some(delivery) = delivery.as_mut() {
                            delivery.observe_context(&message, thread_id, None);
                        }
                        continue;
                    }
                    anyhow::ensure!(
                        subscription_pending,
                        "Codex control received an unexpected initial thread/resume response"
                    );
                    if message.get("error").is_some()
                        && let Some(rejected) = resume_permissions.take()
                    {
                        safe_fallback_active.store(true, Ordering::SeqCst);
                        eprintln!(
                            "st codex: app-server rejected the declared resume permission policy; continuing once with the provider-safe policy"
                        );
                        let _ = events.send(ControlEvent::SafeFallbackActivated {
                            cause: "resumePermissionProjectionRejected",
                            permissions: rejected,
                        });
                        write_json_message(
                            &mut websocket,
                            &control_resume_request(thread_id, None),
                        )
                        .context(
                            "retrying Codex thread resume without rejected permission policy",
                        )?;
                        continue;
                    }
                    subscription_pending = false;
                    if let Some(expected_permissions) = resume_permissions.take() {
                        if resume_permission_overrides_applied(&message, &expected_permissions) {
                            let _ = events.send(ControlEvent::ResumePermissionPolicyApplied(
                                expected_permissions,
                            ));
                        } else {
                            safe_fallback_active.store(true, Ordering::SeqCst);
                            eprintln!(
                                "st codex: resumed thread did not report the declared permission policy; continuing in degraded provider-safe mode"
                            );
                            let _ = events.send(ControlEvent::SafeFallbackActivated {
                                cause: "resumePermissionProjectionMismatch",
                                permissions: expected_permissions,
                            });
                        }
                    }
                    let mut bound = CodexControlState::new(runtime, thread_id.to_string());
                    match bound
                        .accept_subscription(&message)
                        .context("accepting Codex resume subscription")?
                    {
                        SubscriptionAcceptance::Accepted { .. } => {
                            if let Some(delivery) = delivery.as_mut() {
                                delivery
                                    .reconcile_resume(&message, &bound)
                                    .context("reconciling Codex resume delivery")?;
                            }
                        }
                        SubscriptionAcceptance::Deferred => anyhow::bail!(
                            "saved Codex resume binding has no persisted rollout for thread {thread_id}"
                        ),
                    }
                    atomic_json(
                        binding_path,
                        &CodexThreadBinding::new(runtime, thread_id.to_string()),
                    )
                    .context("persisting Codex resume binding")?;
                    atomic_json(control_state_path, &bound)
                        .context("persisting Codex control state")?;
                    if let Some(delivery) = delivery.as_mut() {
                        delivery.observe_harness(&bound.observed);
                    }
                    control_state = Some(bound);
                    if let Some(preloaded) = preloaded.take() {
                        let _ = preloaded.send(());
                    }
                    let _ = events.send(ControlEvent::Bound);
                    continue;
                }

                let Some(thread_id) = binding_candidate(&message)
                    .context("reading Codex thread binding candidate")?
                else {
                    continue;
                };
                atomic_json(
                    binding_path,
                    &CodexThreadBinding::new(runtime, thread_id.to_string()),
                )
                .context("persisting Codex fresh binding")?;
                let mut bound = CodexControlState::new(runtime, thread_id.to_string());
                // A fresh control client that observes the owning TUI's `thread/started`
                // notification is already subscribed to that thread's broadcasts. Before its
                // first turn there is no persisted rollout for a redundant `thread/resume`.
                bound.subscribed = true;
                atomic_json(control_state_path, &bound)
                    .context("persisting Codex fresh control state")?;
                if let Some(delivery) = delivery.as_mut() {
                    delivery.observe_harness(&bound.observed);
                }
                control_state = Some(bound);
                let _ = events.send(ControlEvent::Bound);
            }

            let state = control_state
                .as_mut()
                .context("Codex control state is unbound")?;
            if let Some(delivery) = delivery.as_mut() {
                // Some Codex builds keep a secondary subscriber busy with status traffic while
                // omitting the compaction item itself. Rate-limit this independently of socket
                // timeouts so a chatty stream cannot starve transcript recovery.
                delivery.refresh_transcript_context_if_due(state.thread_id());
            }
            // The context record's whole input, taken before the delivery and state branches
            // because none of them reads a token count and every one of them may `continue`.
            //
            // Deliberately after binding: both unbound paths above skip any frame carrying a
            // `method`, so a `thread/tokenUsage/updated` replayed to a freshly attached connection
            // ahead of the resume response is dropped. The consequence is bounded — Codex emits
            // another reading on the next model response, roughly 10-15 per turn — and the record
            // is honest about the gap through `ageMs` meanwhile, which is cheaper than teaching the
            // binding handshake to hold observability frames it has no state to attribute yet.
            let mut turn_error_changed = false;
            if let Some(delivery) = delivery.as_mut() {
                let active_turn = match &state.observed {
                    CodexObservedState::Active { turn_id } => Some(turn_id.as_str()),
                    CodexObservedState::Held {
                        turn_id: Some(turn_id),
                        ..
                    } => Some(turn_id.as_str()),
                    _ => None,
                };
                delivery.observe_context(&message, state.thread_id(), active_turn);
                // The subagent axis, for the same reason. Fail-open like the context record.
                if let Err(error) = crate::subagents::observe_codex(
                    &delivery.config.agent_dir,
                    &message,
                    state.thread_id(),
                ) {
                    tracing::warn!("st codex: subagent ledger write failed: {error:#}");
                }
                // The credential axis, taken here for the same reason: it reads a typed turn
                // result no branch below looks at, and every one of them may `continue`.
                delivery.observe_provider_auth(&message, state.thread_id());
                turn_error_changed = delivery.observe_turn_error(&message, state.thread_id());
            }
            let before_delivery_state = state.observed.clone();
            let delivery_response = match delivery.as_mut() {
                Some(delivery) => {
                    delivery
                        .accept_snapshot_response(&message, state)
                        .context("accepting Codex on-demand thread/read response")?
                        || delivery
                            .accept_response(&message, &state.observed)
                            .context("accepting Codex delivery response")?
                        || delivery
                            .accept_typed_receipt(&message, state)
                            .context("accepting Codex typed receipt")?
                }
                None => false,
            };
            // Unlike an item receipt, a terminal turn notification still changes the harness
            // state below. Grade its delivery evidence without consuming the frame.
            if let Some(delivery) = delivery.as_mut() {
                delivery
                    .accept_turn_completion_receipt(&message, state)
                    .context("accepting Codex turn completion receipt")?;
            }
            let changed = if delivery_response {
                state.observed != before_delivery_state
            } else if message.get("method").is_none()
                && message.get("id") == Some(&Value::from(CONTROL_SUBSCRIBE_REQUEST_ID))
            {
                anyhow::ensure!(
                    subscription_pending,
                    "Codex control received an unexpected thread/resume response"
                );
                subscription_pending = false;
                match state
                    .accept_subscription(&message)
                    .context("accepting Codex subscription")?
                {
                    SubscriptionAcceptance::Accepted { changed } => {
                        if let Some(delivery) = delivery.as_mut() {
                            delivery
                                .reconcile_resume(&message, state)
                                .context("reconciling Codex subscription delivery")?;
                        }
                        changed
                    }
                    SubscriptionAcceptance::Deferred => false,
                }
            } else {
                state
                    .observe(&message)
                    .context("observing Codex control event")?
            };
            if changed {
                atomic_json(control_state_path, state)
                    .context("persisting Codex observed control state")?;
                if let Some(delivery) = delivery.as_mut() {
                    delivery.observe_harness(&state.observed);
                }
                let _ = events.send(ControlEvent::Observed);
            } else if turn_error_changed {
                // The delivery-relevant state is unchanged and the reason is not. That is the
                // exact shape of the overnight stall: Codex kept reporting a live turn while the
                // provider refused every attempt, so nothing here changed and nothing was
                // republished. The record has to learn the cause on this edge or never.
                if let Some(delivery) = delivery.as_mut() {
                    delivery.observe_harness(&state.observed);
                }
            }
            if !state.subscribed
                && !subscription_pending
                && subscription_candidate(&message, state.thread_id())
            {
                write_json_message(
                    &mut websocket,
                    &json!({
                        "method": "thread/resume",
                        "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                        "params": { "threadId": state.thread_id }
                    }),
                )
                .context("sending Codex subscription request")?;
                subscription_pending = true;
            }
            recover_transcript_turn_if_due(
                state,
                &mut delivery,
                &mut last_transcript_turn_recovery,
                control_state_path,
                &events,
            )?;
            if let Some(delivery) = delivery.as_mut() {
                if let Some(request) = delivery.maybe_account_request() {
                    write_json_message(&mut websocket, &request)
                        .context("sending Codex account/read")?;
                }
                if let Some(request) = delivery.maybe_snapshot_request(state)? {
                    write_json_message(&mut websocket, &request)
                        .context("sending Codex on-demand thread/read")?;
                } else if let Some(request) = delivery.maybe_request(state)? {
                    write_json_message(&mut websocket, &request)
                        .context("sending Codex delivery request")?;
                }
            }
        }
    })();
    if let Err(error) = result {
        let _ = events.send(ControlEvent::Failed(format!("{error:#}")));
    }
}

fn recover_active_codex_turn(thread_id: &str) -> Result<Option<String>> {
    latest_codex_transcript(thread_id)?
        .map(|path| active_turn_from_codex_transcript(&path))
        .transpose()
        .map(Option::flatten)
}

fn recover_transcript_turn_if_due(
    state: &mut CodexControlState,
    delivery: &mut Option<CodexInboxDelivery>,
    last_recovery: &mut Option<Instant>,
    control_state_path: &Path,
    events: &Sender<ControlEvent>,
) -> Result<()> {
    let system_error = matches!(
        state.observed,
        CodexObservedState::Held {
            reason: CodexHoldReason::SystemError,
            ..
        }
    );
    if last_recovery.is_some_and(|last| last.elapsed() < TRANSCRIPT_TURN_RECOVERY_INTERVAL)
        || (!system_error
            && (!delivery
                .as_ref()
                .is_some_and(CodexInboxDelivery::transcript_recovery_due)
                || !matches!(
                    state.observed,
                    CodexObservedState::AwaitingStatus
                        | CodexObservedState::Held {
                            reason: CodexHoldReason::ActiveWithoutTurn,
                            ..
                        }
                )))
    {
        return Ok(());
    }
    *last_recovery = Some(Instant::now());
    if system_error {
        let Some(path) = latest_codex_transcript(state.thread_id())? else {
            return Ok(());
        };
        if failed_completed_turn_from_codex_frames(&codex_transcript_tail(&path)?).is_none() {
            return Ok(());
        }
        // Some Codex app-server versions report the terminal thread status but
        // omit turn/completed. The saved task_complete with an error proves the
        // turn ended, so the next inbox delivery can safely start a new turn.
        state.observed = CodexObservedState::TerminalError {
            reason: CodexTerminalError::SystemError,
        };
        atomic_json(control_state_path, state)
            .context("persisting transcript-recovered Codex system error")?;
        if let Some(delivery) = delivery.as_mut() {
            delivery.observe_harness(&state.observed);
            delivery.accept_transcript_recovery(state.observed.clone());
        }
        let _ = events.send(ControlEvent::Observed);
        return Ok(());
    }
    let Some(turn_id) = recover_active_codex_turn(state.thread_id())? else {
        return Ok(());
    };
    let before = state.observed.clone();
    state.observe_turn_evidence(&turn_id);
    if state.observed != before {
        atomic_json(control_state_path, state)
            .context("persisting transcript-recovered Codex turn")?;
        if let Some(delivery) = delivery.as_mut() {
            delivery.observe_harness(&state.observed);
        }
        let _ = events.send(ControlEvent::Observed);
    }
    if let Some(delivery) = delivery.as_mut() {
        delivery.accept_transcript_recovery(state.observed.clone());
    }
    Ok(())
}

fn codex_turn_model(thread_id: &str, turn_id: &str) -> Result<Option<String>> {
    let Some(path) = latest_codex_transcript(thread_id)? else {
        return Ok(None);
    };
    let frames = codex_transcript_tail(&path)?;
    Ok(model_from_codex_frames(&frames, turn_id))
}

fn model_from_codex_frames(frames: &[Value], turn_id: &str) -> Option<String> {
    frames.iter().rev().find_map(|frame| {
        (frame.get("type").and_then(Value::as_str) == Some("turn_context")
            && frame.pointer("/payload/turn_id").and_then(Value::as_str) == Some(turn_id))
        .then(|| frame.pointer("/payload/model").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
    })
}

fn latest_codex_transcript(thread_id: &str) -> Result<Option<PathBuf>> {
    let Some(home) = codex_home() else {
        return Ok(None);
    };
    latest_codex_transcript_in(&home, thread_id)
}

/// `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// The newest rollout file of Codex thread `thread_id` beneath `home/sessions`.
pub fn latest_codex_transcript_in(home: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let sessions = home.join("sessions");
    let mut stack = vec![sessions];
    let mut inspected = 0_usize;
    let mut selected: Option<(SystemTime, PathBuf)> = None;
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            inspected = inspected.saturating_add(1);
            if inspected > TRANSCRIPT_DISCOVERY_FILE_LIMIT {
                return Ok(None);
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.contains(thread_id))
            {
                continue;
            }
            let modified = entry
                .metadata()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if selected
                .as_ref()
                .is_none_or(|(current, _)| modified > *current)
            {
                selected = Some((modified, path));
            }
        }
    }
    Ok(selected.map(|(_, path)| path))
}

fn active_turn_from_codex_transcript(path: &Path) -> Result<Option<String>> {
    let frames = codex_transcript_tail(path)?;
    Ok(active_turn_from_codex_frames(&frames))
}

fn codex_transcript_tail(path: &Path) -> Result<Vec<Value>> {
    Ok(codex_transcript_tail_with_bytes(path)?.0)
}

fn codex_transcript_tail_with_bytes(path: &Path) -> Result<(Vec<Value>, u64)> {
    let mut file =
        File::open(path).with_context(|| format!("read Codex transcript {}", path.display()))?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TRANSCRIPT_TURN_RECOVERY_BYTES);
    file.seek(SeekFrom::Start(start))?;
    // The rollout can grow while it is being read. Cap the stream itself so a busy writer
    // cannot make one recovery pass read past this fixed window.
    let mut reader = BufReader::new(file.take(TRANSCRIPT_TURN_RECOVERY_BYTES));
    if start != 0 {
        let mut partial = String::new();
        reader.read_line(&mut partial)?;
    }
    let mut frames = Vec::new();
    for line in (&mut reader).lines() {
        if let Ok(value) = serde_json::from_str::<Value>(&line?) {
            frames.push(value);
        }
    }
    Ok((
        frames,
        TRANSCRIPT_TURN_RECOVERY_BYTES - reader.get_ref().limit(),
    ))
}

fn active_turn_from_codex_frames(frames: &[Value]) -> Option<String> {
    let mut active = None;
    for value in frames {
        let event = value.pointer("/payload/type").and_then(Value::as_str);
        let turn_id = value
            .pointer("/payload/turn_id")
            .or_else(|| {
                value.pointer("/payload/internal_chat_message_metadata_passthrough/turn_id")
            })
            .and_then(Value::as_str);
        match (event, turn_id) {
            (Some("task_started"), Some(turn_id)) => active = Some(turn_id.to_string()),
            (Some("task_complete" | "turn_aborted"), Some(turn_id))
                if active.as_deref() == Some(turn_id) =>
            {
                active = None;
            }
            // A long-running turn can put task_started more than the bounded
            // transcript tail behind us. Its recent typed tool/response frames
            // still carry the same turn ID, which turn/steer fences at Codex.
            (Some("item_completed"), Some(turn_id)) if active.is_none() => {
                active = Some(turn_id.to_string());
            }
            (_, Some(turn_id))
                if active.is_none()
                    && value.get("type").and_then(Value::as_str) == Some("response_item") =>
            {
                active = Some(turn_id.to_string());
            }
            _ => {}
        }
    }
    active
}

fn failed_completed_turn_from_codex_frames(frames: &[Value]) -> Option<String> {
    let mut active = None;
    let mut failed = None;
    for value in frames {
        let event = value.pointer("/payload/type").and_then(Value::as_str);
        let turn_id = value.pointer("/payload/turn_id").and_then(Value::as_str);
        match (event, turn_id) {
            (Some("task_started"), Some(turn_id)) => {
                active = Some(turn_id);
                failed = None;
            }
            (Some("task_complete"), Some(turn_id)) if active == Some(turn_id) => {
                active = None;
                failed = value
                    .pointer("/payload/error")
                    .filter(|error| !error.is_null())
                    .map(|_| turn_id.to_string());
            }
            (Some("turn_aborted"), Some(turn_id)) if active == Some(turn_id) => {
                active = None;
                failed = None;
            }
            _ => {}
        }
    }
    failed
}

fn subscription_candidate(message: &Value, thread_id: &str) -> bool {
    match message.get("method").and_then(Value::as_str) {
        Some("thread/started") => {
            message.pointer("/params/thread/id").and_then(Value::as_str) == Some(thread_id)
                && matches!(
                    message
                        .pointer("/params/thread/status/type")
                        .and_then(Value::as_str),
                    Some("idle" | "active")
                )
        }
        Some("thread/status/changed") => {
            message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
                && matches!(
                    message
                        .pointer("/params/status/type")
                        .and_then(Value::as_str),
                    Some("idle" | "active")
                )
        }
        _ => false,
    }
}

fn binding_candidate(message: &Value) -> Result<Option<&str>> {
    match message.get("method").and_then(Value::as_str) {
        Some("thread/started") => {
            let thread_id = required_string(message, "/params/thread/id", "thread/started")?;
            Ok(Some(thread_id))
        }
        _ => Ok(None),
    }
}

/// How the binding wait ended: the thread bound, or st2's stop flag ended the session first.
enum BindingWait {
    Bound,
    Stopped,
    TuiExited(ExitStatus),
}

fn wait_for_binding(
    tui: &mut ProviderProcess,
    events: &Receiver<ControlEvent>,
    timeout: Duration,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<BindingWait> {
    let deadline = Instant::now() + timeout;
    loop {
        if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(BindingWait::Stopped);
        }
        if let Some(status) = tui.try_wait()? {
            return Ok(BindingWait::TuiExited(status));
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(CONTROL_POLL);
        if wait.is_zero() {
            anyhow::bail!(
                "controlled Codex TUI did not establish typed thread ownership within {}s",
                timeout.as_secs()
            );
        }
        match events.recv_timeout(wait) {
            Ok(ControlEvent::TuiThreadLoaded(acknowledge)) => {
                diagnostics.record("tuiThreadLoaded", json!({ "pid": tui.id() }))?;
                let _ = acknowledge.send(());
            }
            Ok(ControlEvent::ResumePermissionPolicyApplied(permissions)) => {
                diagnostics.record(
                    "resumePermissionPolicyApplied",
                    permissions.diagnostic(true),
                )?;
            }
            Ok(ControlEvent::SafeFallbackActivated { cause, permissions }) => {
                diagnostics.record(
                    "safeFallbackActivated",
                    json!({
                        "cause": cause,
                        "declaredPermissions": permissions.diagnostic(false),
                        "mode": "minimalRemoteTui",
                        "requestedPolicyApplied": false,
                    }),
                )?;
            }
            Ok(ControlEvent::Bound) => return Ok(BindingWait::Bound),
            Ok(ControlEvent::Observed) => {}
            Ok(ControlEvent::Closed) => {
                anyhow::bail!("Codex control connection closed before thread binding")
            }
            Ok(ControlEvent::Failed(error)) => {
                anyhow::bail!("Codex control failed before thread binding: {error}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Codex control observer ended before thread binding")
            }
        }
    }
}

fn stop_raised() -> bool {
    crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst)
}

fn monitor_bound_tui(tui: &mut ProviderProcess, events: &Receiver<ControlEvent>) -> Result<TuiEnd> {
    loop {
        if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
            // st2's stop path: end the session and return through the ordinary terminal-write
            // path so the record carries the observed outcome before the wrapper exits.
            terminate_child(tui);
            return Ok(TuiEnd::Stopped(tui.try_wait().ok().flatten()));
        }
        if let Some(status) = tui.try_wait()? {
            return Ok(TuiEnd::Exited(status));
        }
        // Only a bound session detaches: its binding and control state are on disk, so the next
        // image can resubscribe to exactly this thread.
        if crate::provider_session::DETACH.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(TuiEnd::Detached);
        }
        match events.recv_timeout(CONTROL_POLL) {
            Ok(ControlEvent::TuiThreadLoaded(acknowledge)) => {
                let _ = acknowledge.send(());
            }
            Ok(ControlEvent::ResumePermissionPolicyApplied(_))
            | Ok(ControlEvent::SafeFallbackActivated { .. }) => {}
            Ok(ControlEvent::Bound) => {}
            Ok(ControlEvent::Observed) => {}
            // A stop signals the whole process group: the app-server can end the control
            // connection before this loop reads the flag the same signal raised. That race is a
            // stop, not a control failure; a failure would leave the predecessor's "failed" record
            // for the replacement to inherit.
            Ok(ControlEvent::Closed) => {
                if stop_raised() {
                    continue;
                }
                anyhow::bail!("Codex control connection closed while the TUI was live")
            }
            Ok(ControlEvent::Failed(error)) => {
                if stop_raised() {
                    continue;
                }
                anyhow::bail!("Codex control failed while the TUI was live: {error}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if stop_raised() {
                    continue;
                }
                anyhow::bail!("Codex control observer ended while the TUI was live")
            }
        }
    }
}

fn completed_tui(status: ExitStatus) -> Result<()> {
    anyhow::ensure!(
        status.success(),
        "controlled Codex TUI exited with {status}"
    );
    Ok(())
}

mod protocol;
use self::protocol::*;

pub fn state_dir(catalog_root: &Path, identity: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir_in(&base, catalog_root, identity)
}

fn state_dir_in(base: &Path, catalog_root: &Path, identity: &str) -> PathBuf {
    base.join("st2")
        .join("codex")
        .join(runtime_key(catalog_root, identity))
}

fn socket_path(catalog_root: &Path, identity: &str) -> Result<PathBuf> {
    let key = runtime_key(catalog_root, identity);
    let preferred = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|base| base.join("st-codex").join(format!("{key}.sock")));
    if let Some(path) = preferred
        && path.as_os_str().as_bytes().len() <= SOCKET_PATH_BUDGET
    {
        return Ok(path);
    }
    let path = PathBuf::from("/tmp")
        .join(format!("st-{}", unsafe { libc::geteuid() }))
        .join("codex")
        .join(format!("{key}.sock"));
    anyhow::ensure!(
        path.as_os_str().as_bytes().len() <= SOCKET_PATH_BUDGET,
        "Codex app-server socket path is too long: {}",
        path.display()
    );
    Ok(path)
}

fn runtime_key(catalog_root: &Path, identity: &str) -> String {
    let mut hash = Sha256::new();
    for value in [catalog_root.as_os_str().as_bytes(), identity.as_bytes()] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    let digest = format!("{:x}", hash.finalize());
    digest[..24].to_string()
}

fn secure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn acquire_owner_lock(state_dir: &Path) -> Result<crate::flock::FileLock> {
    let path = state_dir.join("owner.lock");
    let file = crate::flock::open(&path, crate::flock::Open::Create)
        .with_context(|| format!("opening Codex runtime owner lock {}", path.display()))?;
    // Closing the descriptor releases the process-scoped lock, so a crashed owner leaves no stale
    // claim for the next runtime to trip over.
    match crate::flock::FileLock::hold(file, crate::flock::Mode::Exclusive, crate::flock::Wait::Now)
    {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(anyhow::anyhow!(
            "Codex runtime already has an owner at {}",
            path.display()
        )),
        Err(error) => Err(error)
            .with_context(|| format!("Codex runtime already has an owner at {}", path.display())),
    }
}

/// Stage-and-rename this runtime's own state files, deliberately NOT through the shared
/// `fsatomic` primitive.
///
/// Two reasons, and neither is the durability: [`secure_dir`] re-establishes `0700` on the state
/// directory on EVERY write, because this directory holds the Codex socket and its owner lock and
/// a mode drifting open there is a takeover surface rather than a readability question; and the
/// bytes are `to_writer_pretty`, because these files are read by humans debugging a live runtime.
/// The shared primitive owns neither, and giving it a "chmod the parent" mode would hand every
/// caller a directory-permissions policy it has no business having.
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    secure_dir(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        random_token()?
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The incarnation of the Codex runtime this state directory currently holds. Its records, such
/// as the harness timeline, are stamped with this ID rather than st's runtime incarnation.
pub fn current_runtime_incarnation(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
) -> Option<String> {
    load_runtime(&state_dir.join("runtime.json"), agent, runtime_id)
        .ok()
        .map(|runtime| runtime.incarnation)
}

fn load_runtime(path: &Path, agent: &str, runtime_id: &str) -> Result<CodexRuntime> {
    let bytes =
        fs::read(path).with_context(|| format!("reading Codex runtime {}", path.display()))?;
    let runtime: CodexRuntime = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        crate::contracts::schema_matches(&runtime.schema, RUNTIME_SCHEMA),
        "unsupported Codex runtime schema"
    );
    anyhow::ensure!(
        runtime.agent == agent && runtime.runtime_id == runtime_id,
        "Codex runtime belongs to a different agent runtime"
    );
    anyhow::ensure!(
        !runtime.incarnation.is_empty(),
        "Codex runtime incarnation is empty"
    );
    Ok(runtime)
}

fn load_current_binding(path: &Path, runtime: &CodexRuntime) -> Result<Option<CodexThreadBinding>> {
    let Some(binding) = load_thread_binding(path, &runtime.agent, &runtime.runtime_id)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        binding.runtime_incarnation == runtime.incarnation,
        "Codex thread binding belongs to a different runtime incarnation"
    );
    Ok(Some(binding))
}

#[cfg(test)]
fn load_current_control_state(
    path: &Path,
    runtime: &CodexRuntime,
    binding: &CodexThreadBinding,
) -> Result<Option<CodexControlState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state: CodexControlState = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        crate::contracts::schema_matches(&state.schema, CONTROL_STATE_SCHEMA),
        "unsupported Codex control-state schema"
    );
    anyhow::ensure!(
        state.agent == runtime.agent
            && state.runtime_id == runtime.runtime_id
            && state.runtime_incarnation == runtime.incarnation
            && state.thread_id == binding.thread_id,
        "Codex control state belongs to a different runtime binding"
    );
    Ok(Some(state))
}

fn select_resume_thread(
    path: &Path,
    agent: &str,
    runtime_id: &str,
    resume_existing: bool,
) -> Result<Option<String>> {
    if resume_existing {
        return load_resume_thread(path, agent, runtime_id);
    }
    // This runs under the owner lock. A stale binding must not make the next
    // incarnation look ready before its new control connection binds.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("retiring the prior Codex thread binding"),
    }
    Ok(None)
}

pub fn checkpoint_residency(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    source_generation: crate::residency::Generation,
    resume_generation: crate::residency::Generation,
) -> Result<CodexResidencyCheckpoint> {
    anyhow::ensure!(
        source_generation.0.checked_add(1) == Some(resume_generation.0),
        "Codex residency checkpoint generation is not monotonic"
    );
    let runtime = load_runtime(&state_dir.join("runtime.json"), agent, runtime_id)?;
    let binding = load_current_binding(&state_dir.join("binding.json"), &runtime)?
        .with_context(|| format!("Codex runtime {runtime_id:?} has no native thread binding"))?;
    let checkpoint = CodexResidencyCheckpoint {
        schema: crate::contracts::schema_for_owner(&runtime.schema, RESIDENCY_CHECKPOINT_SCHEMA),
        source_generation,
        resume_generation,
        binding,
    };
    atomic_json(&state_dir.join(RESIDENCY_CHECKPOINT_FILE), &checkpoint)?;
    Ok(checkpoint)
}

pub fn required_residency_resume(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
    authored_args: &[String],
) -> Result<String> {
    anyhow::ensure!(
        resume_insertion_index(authored_args)?.is_some(),
        "authored Codex session selection conflicts with mandatory residency resume"
    );
    let checkpoint = load_residency_checkpoint(state_dir, agent, runtime_id, resume_generation)?;
    let current = load_thread_binding(&state_dir.join("binding.json"), agent, runtime_id)?
        .with_context(|| format!("Codex runtime {runtime_id:?} has no native thread binding"))?;
    anyhow::ensure!(
        current == checkpoint.binding,
        "Codex native thread binding changed after residency checkpoint"
    );
    Ok(current.thread_id)
}

/// Whether the initialized binding proves an exact cold-residency resume.
///
/// A missing binding means the replacement provider has not initialized yet. Every binding that
/// does exist must belong to the requested runtime, retain the checkpointed native thread, and
/// come from the independently expected wrapper incarnation.
pub fn residency_ready(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
    expected_runtime_incarnation: &str,
) -> Result<bool> {
    let checkpoint = load_residency_checkpoint(state_dir, agent, runtime_id, resume_generation)?;
    let Some(current) = load_thread_binding(&state_dir.join("binding.json"), agent, runtime_id)?
    else {
        return Ok(false);
    };
    anyhow::ensure!(
        current.runtime_incarnation != checkpoint.binding.runtime_incarnation,
        "Codex residency binding did not advance to a new runtime incarnation"
    );
    anyhow::ensure!(
        current.thread_id == checkpoint.binding.thread_id,
        "Codex residency binding does not match the checkpointed native thread"
    );
    Ok(current.runtime_incarnation == expected_runtime_incarnation)
}

fn load_residency_checkpoint(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
) -> Result<CodexResidencyCheckpoint> {
    let path = state_dir.join(RESIDENCY_CHECKPOINT_FILE);
    let bytes = fs::read(&path)
        .with_context(|| format!("reading Codex residency checkpoint {}", path.display()))?;
    let checkpoint: CodexResidencyCheckpoint = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        crate::contracts::schema_matches(&checkpoint.schema, RESIDENCY_CHECKPOINT_SCHEMA),
        "unsupported Codex residency checkpoint schema"
    );
    anyhow::ensure!(
        checkpoint.resume_generation == resume_generation
            && checkpoint.source_generation.0.checked_add(1) == Some(resume_generation.0),
        "Codex residency checkpoint belongs to a different generation"
    );
    anyhow::ensure!(
        crate::contracts::schema_matches(&checkpoint.binding.schema, BINDING_SCHEMA)
            && checkpoint.binding.agent == agent
            && checkpoint.binding.runtime_id == runtime_id,
        "Codex residency checkpoint belongs to a different agent runtime"
    );
    anyhow::ensure!(
        !checkpoint.binding.runtime_incarnation.is_empty()
            && !checkpoint.binding.thread_id.is_empty(),
        "Codex residency checkpoint has an incomplete native thread binding"
    );
    Ok(checkpoint)
}

fn load_thread_binding(
    path: &Path,
    agent: &str,
    runtime_id: &str,
) -> Result<Option<CodexThreadBinding>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let binding: CodexThreadBinding = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        crate::contracts::schema_matches(&binding.schema, BINDING_SCHEMA),
        "unsupported Codex binding schema"
    );
    anyhow::ensure!(
        binding.agent == agent && binding.runtime_id == runtime_id,
        "Codex resume binding belongs to a different agent runtime"
    );
    anyhow::ensure!(
        !binding.runtime_incarnation.is_empty() && !binding.thread_id.is_empty(),
        "Codex resume binding is incomplete"
    );
    Ok(Some(binding))
}

fn load_resume_thread(path: &Path, agent: &str, runtime_id: &str) -> Result<Option<String>> {
    Ok(load_thread_binding(path, agent, runtime_id)?.map(|binding| binding.thread_id))
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_json_message(websocket: &mut WebSocket<UnixStream>, value: &Value) -> Result<()> {
    websocket.send(WebSocketMessage::Text(value.to_string().into()))?;
    Ok(())
}

/// One startup-phase read: polls the stop flag between short socket timeouts so a stop raised
/// during a slow handshake peer cannot sit out the full startup timeout (the caller sets a
/// short socket read timeout first).
enum StartupRead {
    Message(Value),
    Stopped,
    Closed,
}

fn read_startup_message(
    websocket: &mut WebSocket<UnixStream>,
    deadline: Instant,
) -> Result<StartupRead> {
    loop {
        if crate::provider_session::STOP.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(StartupRead::Stopped);
        }
        let message = match websocket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(StartupRead::Closed);
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Codex app-server startup read timed out"
                );
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        match message {
            WebSocketMessage::Text(text) => {
                let value = serde_json::from_str(&text)
                    .context("decoding Codex app-server WebSocket JSON")?;
                return Ok(StartupRead::Message(value));
            }
            WebSocketMessage::Close(_) => return Ok(StartupRead::Closed),
            WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => continue,
            WebSocketMessage::Binary(_) | WebSocketMessage::Frame(_) => {
                anyhow::bail!("Codex app-server sent a non-text WebSocket message")
            }
        }
    }
}

#[cfg(test)] // Production startup reads moved to the stop-aware read_startup_message.
fn read_json_message(websocket: &mut WebSocket<UnixStream>) -> Result<Option<Value>> {
    // Darwin reports a timed Unix-socket read as EAGAIN/EWOULDBLOCK.  During
    // handshake the peer may briefly be descheduled; treat that transient as
    // retryable instead of turning scheduler timing into a protocol failure.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let message = match websocket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(None);
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        match message {
            WebSocketMessage::Text(text) => {
                let value = serde_json::from_str(&text)
                    .context("decoding Codex app-server WebSocket JSON")?;
                return Ok(Some(value));
            }
            WebSocketMessage::Close(_) => return Ok(None),
            WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => continue,
            WebSocketMessage::Binary(_) | WebSocketMessage::Frame(_) => {
                anyhow::bail!("Codex app-server sent a non-text WebSocket message")
            }
        }
    }
}

enum ControlRead {
    Message(Value),
    Timeout,
    Closed,
}

fn poll_json_message(websocket: &mut WebSocket<UnixStream>) -> Result<ControlRead> {
    loop {
        let message = match websocket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(ControlRead::Closed);
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(ControlRead::Timeout);
            }
            Err(error) => return Err(error.into()),
        };
        match message {
            WebSocketMessage::Text(text) => {
                let value = serde_json::from_str(&text)
                    .context("decoding Codex app-server WebSocket JSON")?;
                return Ok(ControlRead::Message(value));
            }
            WebSocketMessage::Close(_) => return Ok(ControlRead::Closed),
            WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => continue,
            WebSocketMessage::Binary(_) | WebSocketMessage::Frame(_) => {
                anyhow::bail!("Codex app-server sent a non-text WebSocket message")
            }
        }
    }
}

mod process_group;
use self::process_group::*;

fn terminate_child(child: &mut ProviderProcess) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        _ => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests;
