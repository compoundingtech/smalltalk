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

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tungstenite::{Message as WebSocketMessage, WebSocket};

use crate::{
    delivery_ledger, ding, driver_diagnostic, harness_context, harness_state, message, run, status,
};

const REQUIRED_CODEX_CLIENT_REQUESTS: &[&str] = &[
    "hooks/list",
    "initialize",
    "thread/loaded/list",
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
const RUNTIME_SCHEMA: &str = "st2.codex-runtime.v1";
const BINDING_SCHEMA: &str = "st2.codex-thread-binding.v1";
const CONTROL_STATE_SCHEMA: &str = "st2.codex-control-state.v1";
const WRAPPER_DIAGNOSTIC_SCHEMA: &str = "st2.codex-wrapper-diagnostic.v1";
const CONTROL_TUI_LOADED_REQUEST_ID: u64 = 0;
const CONTROL_SUBSCRIBE_REQUEST_ID: u64 = 1;
const FIRST_DELIVERY_REQUEST_ID: u64 = 2;
const HOOK_TRUST_PREFLIGHT_REQUEST_ID: u64 = 1;
// The inner provider result must reach the wrapper before the outer ownership wait expires.
const TUI_LOADED_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_POLL: Duration = Duration::from_millis(100);
const INBOX_REFRESH_FALLBACK: Duration = Duration::from_secs(15);
const SOCKET_PATH_BUDGET: usize = 96;

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
        Ok(Self {
            schema: RUNTIME_SCHEMA.to_string(),
            agent,
            runtime_id,
            incarnation: random_token()?,
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
            schema: BINDING_SCHEMA.to_string(),
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
pub(crate) enum CodexHoldReason {
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
pub(crate) enum CodexTerminalError {
    SystemError,
    ProviderAuthRejected,
}

/// The `CodexErrorInfo` word that names a rejected provider credential.
///
/// It is the 401/invalid-credential arm of Codex's own closed error vocabulary and is distinct
/// from both quota words (`usageLimitExceeded`, `rateLimitExceeded`) — the protocol gate pins all
/// three present so a release that merged them refuses the launch instead of silently making st2
/// call an exhausted allowance a rejected credential.
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
        _ => CodexTurnOutcome::Indeterminate,
    }
}

impl CodexObservedState {
    /// Driver-side projection into the generic observed-harness-state vocabulary (#162). `Held` is
    /// a delivery predicate — the complement of steerable — and never leaks into the published
    /// record: holds Codex positively reported as work project to `active` (with the human-blocking
    /// ones setting the blocked axis), while holds that only mean "st2 cannot currently prove
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
            // Both terminals project to `ended`, exactly as before; the reason is what names the
            // cause. `providerAuth` is the same word OpenCode's `ProviderAuthError` already
            // publishes, so one roster consumer classifies the credential class across harnesses.
            CodexObservedState::TerminalError { reason } => Some(
                observation(Activity::Ended, BlockedOn::None).with_reason(match reason {
                    CodexTerminalError::SystemError => "systemError",
                    CodexTerminalError::ProviderAuthRejected => "providerAuth",
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
    catalog_root: PathBuf,
    agent_dir: PathBuf,
    inbox: PathBuf,
    identity: String,
    this_host: String,
    supervisor: Option<String>,
    /// The codex-cli version the protocol gate admitted, carried for the native-driver
    /// diagnostic's `producerVersion`. `None` only in tests that build a config without a gate.
    producer_version: Option<String>,
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
            catalog_root: catalog_root.to_path_buf(),
            inbox: message::inbox_dir(&agent_dir),
            agent_dir,
            identity: identity.to_string(),
            this_host,
            supervisor,
            producer_version: None,
        })
    }

    fn report_protocol_rejection(&self, codex: &str, error: &anyhow::Error) {
        let Some(supervisor) = self.supervisor.as_deref() else {
            eprintln!(
                "st2 codex: agent '{}' has no supervisor for a protocol rejection report",
                self.identity
            );
            return;
        };
        let subject = format!("Codex protocol rejected: {}", self.identity);
        let body = format!(
            "st2 rejected the installed Codex app-server protocol for agent '{}'. Native delivery did not start. Codex executable: '{}'. Error: {error:#}",
            self.identity, codex
        );
        let mut key_hash = Sha256::new();
        key_hash.update(b"st2.codex-protocol-rejection.v1");
        key_hash.update(body.as_bytes());
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
                    "st2 codex: failed to resolve the endpoints of agent '{}' protocol rejection report: {resolve_error:#}",
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
                "st2 codex: failed to report agent '{}' protocol rejection to supervisor '{}': {report_error:#}",
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
}

struct CodexInboxDelivery {
    config: CodexDeliveryConfig,
    runtime: CodexRuntime,
    wake: Receiver<()>,
    _watcher: Option<notify::RecommendedWatcher>,
    next_inbox_refresh: Instant,
    next_presence_refresh: Instant,
    head: Option<message::Message>,
    suppressed: bool,
    ledger: delivery_ledger::Ledger,
    pending: Option<PendingCodexDelivery>,
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
    /// The native-driver boundary record. Codex publishes exactly one stage on it — the provider
    /// credential — because every earlier boundary is already fail-closed at admission: an
    /// incompatible protocol refuses the launch instead of degrading into an observation.
    diagnostics: driver_diagnostic::Publisher,
}

impl CodexInboxDelivery {
    fn new(
        config: CodexDeliveryConfig,
        ledger_path: PathBuf,
        runtime: CodexRuntime,
    ) -> Result<Self> {
        fs::create_dir_all(&config.inbox).with_context(|| {
            format!(
                "creating Codex native delivery inbox {}",
                config.inbox.display()
            )
        })?;
        let (wake_tx, wake) = mpsc::channel();
        // Scoped to inbox + status: this pump's own process group writes runtime records (presence
        // refreshes, harness-state transitions) into the same agent dir, and those must not wake it.
        let watcher = crate::watch::watch_delivery_inputs(&config.agent_dir, wake_tx);
        let identity = config.identity.clone();
        let ledger = delivery_ledger::Ledger::open(
            &ledger_path,
            delivery_ledger::Harness::Codex.profile(),
            &config.identity,
            runtime.runtime_id(),
            |thread, filename| stable_client_user_message_id(&identity, thread, filename),
        );
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
                        "st2 codex: observed-state claim failed; degrading to token-only: {error:#}"
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
        let context = match harness_context::Writer::new(
            &config.agent_dir,
            config.identity.clone(),
            harness_context::Harness::Codex,
        ) {
            Ok(writer) => Some(CodexContextProducer::new(
                writer.with_session(runtime.incarnation()),
            )),
            Err(error) => {
                tracing::warn!(
                    "st2 codex: harness-context writer unavailable; context stays unpublished: {error:#}"
                );
                None
            }
        };
        // The record belongs to this incarnation: the protocol gate already admitted the version
        // it names, so `support` is a measured fact rather than a probe result.
        let diagnostics = driver_diagnostic::Publisher::new(
            &config.agent_dir,
            driver_diagnostic::Driver::Codex,
            config.producer_version.clone(),
            driver_diagnostic::Support::Supported,
        );
        Ok(Self {
            config,
            runtime,
            wake,
            _watcher: watcher,
            next_inbox_refresh: Instant::now(),
            next_presence_refresh: Instant::now(),
            head: None,
            suppressed: false,
            ledger,
            pending: None,
            rejected: None,
            next_request_id: FIRST_DELIVERY_REQUEST_ID,
            harness_writer,
            harness_evidence: false,
            pending_observation: None,
            context,
            diagnostics,
        })
    }

    /// Publish the generic observed-harness-state projection of a control-state change. Best-effort
    /// like the presence refresh: a failed record write must not disturb delivery — but it must
    /// not count as evidence either. A transition whose write failed is retained as pending and
    /// retried before any heartbeat, so a stale on-disk state is never kept fresh in
    /// contradiction of the latest observation.
    fn observe_harness(&mut self, observed: &CodexObservedState) {
        match observed.harness_observation() {
            Some(observation) => self.publish_observation(observation),
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
    fn observe_context(&mut self, message: &Value, thread_id: &str) {
        if let Some(context) = self.context.as_mut()
            && let Err(error) = context.observe(message, thread_id)
        {
            tracing::warn!("st2 codex: harness-context write failed: {error:#}");
        }
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
            CodexTurnOutcome::Indeterminate => {}
        }
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
            .prune(|filename| unread.iter().any(|message| message.filename == filename))
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
            // This wrapper owns the live provider session. It therefore owns the presence lease.
            // Preserve busy or available, and let dnd age out.
            let _ = status::refresh(&status::status_path(&self.config.agent_dir));
            if self.harness_evidence {
                let _ = self.harness_writer.heartbeat();
            }
            self.next_presence_refresh = now + status::STATUS_REFRESH;
        }
        let mut due = now >= self.next_inbox_refresh;
        while self.wake.try_recv().is_ok() {
            due = true;
        }
        if !due {
            return Ok(());
        }
        let unread = message::list_inbox(&self.config.inbox)?;
        self.reconcile_inbox(&unread)?;
        if self.rejected.as_ref().is_some_and(|rejected| {
            unread
                .iter()
                .all(|message| message.filename != rejected.filename)
        }) {
            self.rejected = None;
        }
        self.head = unread.into_iter().next();
        self.suppressed =
            status::read_state(&status::status_path(&self.config.agent_dir)) == status::State::Dnd;
        self.next_inbox_refresh = Instant::now() + INBOX_REFRESH_FALLBACK;
        Ok(())
    }

    fn maybe_request(&mut self, state: &CodexControlState) -> Result<Option<Value>> {
        self.refresh_if_due()?;
        if self.pending.is_some() || !state.subscribed || self.suppressed {
            return Ok(None);
        }
        // Fail closed: an unreadable ledger holds and surfaces rather than guessing. It never
        // refuses to start — a control connection that will not start delivers nothing at all.
        // The operator-visible surface is the existing typed boundary — the transport is
        // unavailable — and the raw reason stays in tracing, so no unbounded prose reaches the
        // record. Restating it is coalesced by the publisher, so a held pass costs no write.
        if let Some(reason) = self.ledger.quarantined().map(str::to_string) {
            tracing::warn!("st2 codex: delivery ledger is quarantined: {reason}");
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
        if self.rejected.as_ref().is_some_and(|rejected| {
            rejected.filename == head.filename && rejected.observed == state.observed
        }) {
            return Ok(None);
        }
        // Exactly one delivery is outstanding at a time on this transport: an entry bound to some
        // other file holds the pump until archive precedence resolves it, so a message arriving
        // out of filename order can never open a second concurrent delivery.
        if !self.ledger.entries().is_empty() && self.ledger.entry(&head.filename).is_none() {
            return Ok(None);
        }
        // An attempt this pump already owns is held until evidence settles or refuses it. Only an
        // authoritative "no" — a rejected request, or resumed history proving the client ID never
        // landed — authorizes sending the same identity again.
        if self.ledger.retry(&head.filename) != delivery_ledger::RetryDecision::Retry {
            return Ok(None);
        }
        let method = match &state.observed {
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
        });
        Ok(Some(request))
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
            });
            return Ok(true);
        }
        match &pending.method {
            CodexDeliveryMethod::Start => {
                required_string(message, "/result/turn/id", "turn/start response")?;
            }
            CodexDeliveryMethod::Steer { turn_id } => {
                let returned = required_string(message, "/result/turnId", "turn/steer response")?;
                anyhow::ensure!(
                    returned == turn_id,
                    "Codex turn/steer response returned a different turn"
                );
            }
        }
        // The request returned a well-formed result. That is a fact about the call, never about
        // the model, so it grades no higher than `transportAccepted`.
        self.ledger.record(
            &pending.filename,
            delivery_ledger::Evidence::TransportAccepted,
        )?;
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

    /// Reconcile a pre-crash attempt against the typed history returned by `thread/resume` before
    /// the same client ID can be sent again.
    fn reconcile_resume(&mut self, message: &Value, state: &CodexControlState) -> Result<()> {
        if message.get("error").is_some() {
            return Ok(());
        }
        let unsettled: Vec<(String, String)> = self
            .ledger
            .entries()
            .iter()
            .filter(|entry| {
                entry.binding == state.thread_id()
                    && entry.phase < delivery_ledger::Phase::Consumed
            })
            .map(|entry| (entry.filename.clone(), entry.correlation.value.clone()))
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
        for (filename, client_id) in unsettled {
            let accepted = turns.iter().any(|turn| {
                turn.get("items")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("type").and_then(Value::as_str) == Some("userMessage")
                                && item.get("clientId").and_then(Value::as_str)
                                    == Some(client_id.as_str())
                        })
                    })
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

fn stable_client_user_message_id(recipient: &str, thread_id: &str, filename: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"st2.codex-client-user-message.v1");
    for value in [
        recipient.as_bytes(),
        thread_id.as_bytes(),
        filename.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    format!("st2:{:x}", hash.finalize())
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
            schema: CONTROL_STATE_SCHEMA.to_string(),
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
                    _ if CLASSIFIED_CODEX_THREAD_ITEMS.contains(&item_type) => return Ok(false),
                    _ => (CodexHoldReason::UnknownProtocol, false),
                };
                let turn_id = required_string(message, "/params/turnId", method)?;
                if released {
                    self.observe_hold_released(turn_id, reason);
                } else {
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
    anyhow::ensure!(
        !codex_argv.is_empty(),
        "Codex controlled launch argv is empty"
    );
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

    let state_dir = state_dir(catalog_root, &identity);
    secure_dir(&state_dir)?;
    let _owner_lock = acquire_owner_lock(&state_dir)?;
    let mut diagnostics = WrapperDiagnostics::open(&state_dir, &identity, &runtime_id)?;
    diagnostics.record("ownerAcquired", json!({}))?;

    let result = run_controlled_owned(
        catalog_root,
        &state_dir,
        identity,
        runtime_id,
        codex_argv,
        delivery,
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

fn run_controlled_owned(
    catalog_root: &Path,
    state_dir: &Path,
    identity: String,
    runtime_id: String,
    codex_argv: Vec<String>,
    delivery: CodexDeliveryConfig,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<()> {
    // Installed before ANY child exists — the hook-trust preflight spawns a detached app-server
    // first, and a SIGTERM landing in that window must set the stop flag its connect loop polls
    // rather than killing this wrapper around a leaked server and a stale socket. (Installing
    // resets the flag, so this must also run exactly once per launch.)
    crate::provider_session::install_signal_handler();
    let binding_path = state_dir.join("binding.json");
    let resume_thread = load_resume_thread(&binding_path, &identity, &runtime_id)?;

    let socket_path = socket_path(catalog_root, &identity)?;
    let socket_dir = socket_path
        .parent()
        .context("Codex app-server socket has no parent")?;
    secure_dir(socket_dir)?;
    prepare_socket_for_launch(&socket_path)?;

    // Publish a new incarnation only after this process holds the owner lock and has proved that no
    // older daemon is live. A rejected second owner must not invalidate the first owner's binding.
    let runtime = CodexRuntime::fresh(identity, runtime_id)?;
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
    let endpoint = format!("unix://{}", socket_path.display());
    let mut server_args = controlled_app_server_args(&endpoint, &codex_argv[1..])?;
    if resume_thread.is_some() && authored_bypasses_hook_trust(&codex_argv[1..])? {
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
    diagnostics.record("appServerStarting", json!({}))?;
    let mut server_command = Command::new(&codex_argv[0]);
    server_command
        .args(server_args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut server = spawn_process_group(&mut server_command, Some(&socket_path))
        .with_context(|| format!("starting {} app-server", codex_argv[0]))?;
    let result = diagnostics
        .record("appServerStarted", json!({ "pid": server.id() }))
        .and_then(|_| {
            run_connected(
                server.child_mut(),
                &socket_path,
                &runtime,
                &codex_argv,
                resume_thread.as_deref(),
                delivery,
                diagnostics,
            )
        });
    server.terminate();
    result
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

fn run_connected(
    server: &mut Child,
    socket_path: &Path,
    runtime: &CodexRuntime,
    codex_argv: &[String],
    resume_thread: Option<&str>,
    delivery: CodexDeliveryConfig,
    diagnostics: &mut WrapperDiagnostics,
) -> Result<()> {
    // The stop handler is installed by run_controlled_owned before any spawn (the preflight's
    // detached app-server included); re-installing here would RESET a stop flag raised during
    // startup, so this function only relies on it.
    let state_dir = state_dir(&delivery.catalog_root, &delivery.identity);
    let endpoint = format!("unix://{}", socket_path.display());
    let tui_args = controlled_tui_args(&endpoint, &codex_argv[1..], resume_thread)?;
    let expected_resume =
        expected_resume_thread(&codex_argv[1..], resume_thread)?.map(str::to_owned);
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
    let harness_agent_dir = delivery.agent_dir.clone();
    let harness_identity = delivery.identity.clone();
    let event_thread = thread::spawn(move || {
        let resume = expected_resume
            .as_deref()
            .zip(resume_ready_rx)
            .map(|(thread_id, ready)| ControlResume {
                thread_id,
                ready,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
            });
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime_for_reader,
            resume,
            Some(delivery),
            events_tx,
        )
    });

    // A fresh initialized observer reads before this child can issue thread/start. A resumed
    // observer waits on the gate below, then proves through thread/loaded/list that the TUI issued
    // its own resume. Only after that typed observation may control send its redundant resume.
    // Insert the remote endpoint as a global Codex option and preserve every authored argument
    // after the provider executable.
    let mut tui_command = Command::new(&codex_argv[0]);
    tui_command.args(tui_args);
    let mut tui = match tui_command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
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
        diagnostics.record("tuiStarted", json!({ "pid": tui.id() }))?;
        if let Some(ready) = resume_ready_tx.take() {
            ready
                .send(())
                .context("starting Codex control resume after the TUI launched")?;
        }
        diagnostics.record("waitingForThreadBinding", json!({ "pid": tui.id() }))?;
        match wait_for_binding(&mut tui, &events_rx, STARTUP_TIMEOUT, diagnostics)? {
            BindingWait::Bound => {
                diagnostics.record("threadBound", json!({ "pid": tui.id() }))?;
                monitor_bound_tui(&mut tui, &events_rx)
            }
            BindingWait::Stopped => {
                terminate_child(&mut tui);
                Ok(TuiEnd::Stopped(tui.try_wait().ok().flatten()))
            }
        }
    })();
    if result.is_err() {
        terminate_child(&mut tui);
    }
    drop(resume_ready_tx);
    let _ = shutdown.shutdown(Shutdown::Both);
    let _ = event_thread.join();
    // The pump is gone, so nothing can observe this session again: publish the terminal
    // observation with the outcome the wrapper actually saw, before any staleness horizon.
    // Consumers must not branch on `reason`, so the observed exit always lands in `exit`.
    // Same incarnation as the pump's writer, adopting by token: the pump's written claim (or any
    // of its writes) put this token on disk, so token-only adoption resolves to this session's
    // claimed sequence — and the terminal record fences exactly the records this session wrote.
    let mut harness_writer = harness_state::Writer::new(
        &harness_agent_dir,
        harness_identity.clone(),
        "codex",
        Some(runtime.runtime_id().to_string()),
    )
    .with_session(runtime.incarnation());
    let _ = match &result {
        Ok(TuiEnd::Exited(status)) => harness_writer.ended(describe_tui_exit(Some(*status))),
        Ok(TuiEnd::Stopped(status)) => harness_writer.ended(describe_tui_exit(*status)),
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
        Ok(TuiEnd::Stopped(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

/// How the controlled TUI session came to an end, as the monitor saw it.
enum TuiEnd {
    /// The TUI exited on its own with this status.
    Exited(ExitStatus),
    /// The wrapper's stop flag ended the session; the reaped status when one was observable.
    Stopped(Option<ExitStatus>),
}

fn describe_tui_exit(status: Option<ExitStatus>) -> String {
    match status.map(|status| (status.code(), status.signal())) {
        Some((Some(code), _)) => format!("exit {code}"),
        Some((None, Some(signal))) => format!("signal {signal}"),
        _ => "exit unknown".to_string(),
    }
}

/// Start app-server with the authored global configuration inputs that its CLI supports.
///
/// Project trust, strict parsing, and feature selection affect config and hook loading in the
/// server process. Passing them only to the remote TUI silently creates two different effective
/// configurations. TUI-only policy, model, workspace, authentication, and prompt arguments stay
/// on the TUI command.
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
    // Codex models these flags on the `resume` command as well as the root command. Keep them
    // before SESSION_ID so clap does not treat a following flag as the optional prompt.
    args.extend_from_slice(&authored_args[..insertion]);
    args.push(thread_id.to_string());
    args.extend_from_slice(&authored_args[insertion..]);
    Ok(args)
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
    server: &mut Child,
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
                    anyhow::bail!("Codex app-server exited before control connected: {status}");
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
                    "name": "st2",
                    "title": "st2",
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
    Bound,
    Observed,
    Closed,
    Failed(String),
}

struct ControlResume<'a> {
    thread_id: &'a str,
    ready: Receiver<()>,
    tui_loaded_timeout: Duration,
}

fn pump_control(
    mut websocket: WebSocket<UnixStream>,
    binding_path: &Path,
    control_state_path: &Path,
    runtime: &CodexRuntime,
    resume: Option<ControlResume<'_>>,
    delivery: Option<CodexDeliveryConfig>,
    events: Sender<ControlEvent>,
) {
    let result = (|| -> Result<()> {
        let (expected_resume, resume_ready, tui_loaded_timeout) = match resume {
            Some(resume) => (
                Some(resume.thread_id),
                Some(resume.ready),
                resume.tui_loaded_timeout,
            ),
            None => (None, None, TUI_LOADED_TIMEOUT),
        };
        let mut control_state: Option<CodexControlState> = None;
        let mut subscription_pending = false;
        let mut peer_closed = false;
        let delivery_ledger_path =
            control_state_path.with_file_name(delivery_ledger::LEDGER_FILE);
        let mut delivery = delivery
            .map(|config| {
                CodexInboxDelivery::new(config, delivery_ledger_path.clone(), runtime.clone())
            })
            .transpose()
            .context("initializing Codex inbox delivery")?;
        if let Some(thread_id) = expected_resume {
            resume_ready
                .context("saved Codex binding has no TUI-start gate")?
                .recv()
                .context("controlled Codex TUI ended before control resume")?;
            wait_for_tui_loaded_thread(&mut websocket, thread_id, tui_loaded_timeout)
                .context("waiting for Codex TUI thread load")?;
            let (diagnostic_tx, diagnostic_rx) = mpsc::channel();
            eprintln!("codex control: emitting TuiThreadLoaded");
            events
                .send(ControlEvent::TuiThreadLoaded(diagnostic_tx))
                .context("recording that the Codex TUI loaded the preserved thread")?;
            diagnostic_rx
                .recv()
                .context("waiting for the Codex TUI-loaded diagnostic before control resume")?;
            write_json_message(
                &mut websocket,
                &json!({
                    "method": "thread/resume",
                    "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                    "params": { "threadId": thread_id }
                }),
            )
            .context("sending Codex thread resume request")?;
            subscription_pending = true;
        }
        loop {
            if !peer_closed {
                if let Err(error) = websocket.get_ref().set_read_timeout(Some(CONTROL_POLL)) {
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
                if let (Some(state), Some(delivery)) = (control_state.as_ref(), delivery.as_mut())
                    && let Some(request) = delivery.maybe_request(state)?
                {
                    write_json_message(&mut websocket, &request)
                        .context("sending Codex delivery request")?;
                }
                continue;
            };
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
                            delivery.observe_context(&message, thread_id);
                        }
                        continue;
                    }
                    anyhow::ensure!(
                        subscription_pending,
                        "Codex control received an unexpected initial thread/resume response"
                    );
                    subscription_pending = false;
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
            // The context record's whole input, taken before the delivery and state branches
            // because none of them reads a token count and every one of them may `continue`.
            //
            // Deliberately after binding: both unbound paths above skip any frame carrying a
            // `method`, so a `thread/tokenUsage/updated` replayed to a freshly attached connection
            // ahead of the resume response is dropped. The consequence is bounded — Codex emits
            // another reading on the next model response, roughly 10-15 per turn — and the record
            // is honest about the gap through `ageMs` meanwhile, which is cheaper than teaching the
            // binding handshake to hold observability frames it has no state to attribute yet.
            if let Some(delivery) = delivery.as_mut() {
                delivery.observe_context(&message, state.thread_id());
                // The credential axis, taken here for the same reason: it reads a typed turn
                // result no branch below looks at, and every one of them may `continue`.
                delivery.observe_provider_auth(&message, state.thread_id());
            }
            let delivery_response = match delivery.as_mut() {
                Some(delivery) => {
                    delivery
                        .accept_response(&message, &state.observed)
                        .context("accepting Codex delivery response")?
                        || delivery
                            .accept_typed_receipt(&message, state)
                            .context("accepting Codex typed receipt")?
                }
                None => false,
            };
            let changed = if delivery_response {
                false
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
            if let Some(delivery) = delivery.as_mut()
                && let Some(request) = delivery.maybe_request(state)?
            {
                write_json_message(&mut websocket, &request)
                    .context("sending Codex delivery request")?;
            }
        }
    })();
    if let Err(error) = result {
        let _ = events.send(ControlEvent::Failed(format!("{error:#}")));
    }
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
}

fn wait_for_binding(
    tui: &mut Child,
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
            anyhow::bail!("controlled Codex TUI exited before thread binding: {status}");
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

fn monitor_bound_tui(tui: &mut Child, events: &Receiver<ControlEvent>) -> Result<TuiEnd> {
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
        match events.recv_timeout(CONTROL_POLL) {
            Ok(ControlEvent::TuiThreadLoaded(acknowledge)) => {
                let _ = acknowledge.send(());
            }
            Ok(ControlEvent::Bound) => {}
            Ok(ControlEvent::Observed) => {}
            Ok(ControlEvent::Closed) => {
                anyhow::bail!("Codex control connection closed while the TUI was live")
            }
            Ok(ControlEvent::Failed(error)) => {
                anyhow::bail!("Codex control failed while the TUI was live: {error}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
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
        .map(|base| base.join("st2-codex").join(format!("{key}.sock")));
    if let Some(path) = preferred
        && path.as_os_str().as_bytes().len() <= SOCKET_PATH_BUDGET
    {
        return Ok(path);
    }
    let path = PathBuf::from("/tmp")
        .join(format!("st2-{}", unsafe { libc::geteuid() }))
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

fn acquire_owner_lock(state_dir: &Path) -> Result<File> {
    let path = state_dir.join("owner.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening Codex runtime owner lock {}", path.display()))?;
    // SAFETY: `file` owns this descriptor until the returned guard is dropped. `flock` does not
    // access Rust memory, and closing the descriptor releases the process-scoped lock after crash.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Codex runtime already has an owner at {}", path.display()));
    }
    Ok(file)
}

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

#[cfg(test)]
fn load_current_binding(
    path: &Path,
    runtime: &CodexRuntime,
) -> Result<Option<CodexThreadBinding>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let binding: CodexThreadBinding = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        binding.schema == BINDING_SCHEMA,
        "unsupported Codex binding schema"
    );
    anyhow::ensure!(
        binding.agent == runtime.agent
            && binding.runtime_id == runtime.runtime_id
            && binding.runtime_incarnation == runtime.incarnation,
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
        state.schema == CONTROL_STATE_SCHEMA,
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

fn load_resume_thread(path: &Path, agent: &str, runtime_id: &str) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let binding: CodexThreadBinding = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        binding.schema == BINDING_SCHEMA,
        "unsupported Codex binding schema"
    );
    anyhow::ensure!(
        binding.agent == agent && binding.runtime_id == runtime_id,
        "Codex resume binding belongs to a different agent runtime"
    );
    anyhow::ensure!(
        !binding.thread_id.is_empty(),
        "Codex resume binding has an empty thread id"
    );
    Ok(Some(binding.thread_id))
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
    let deadline = Instant::now() + Duration::from_secs(5);
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

fn terminate_child(child: &mut Child) {
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
