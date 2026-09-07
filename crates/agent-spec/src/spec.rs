//! The agent job — st2's view of a rendered VRS `agent.kdl` (spec.md §2).
//!
//! A job reads like a Nomad job: the *agent* is the job, its **tasks** are `pty{}` (interactive —
//! allocates a terminal, an agent harness) and `exec{}` (a plain process — the ding, daemons, a
//! stage's script; must NOT allocate a terminal, R09). st2 reads only the runner-normative subset:
//! `identity`, presentation (`name`, `description`), `host`, `role` (metadata only), `type`,
//! `workspace`, whole-agent desired state (plus legacy `retired`), `keep`, `supervisor`,
//! `restart{}`, `deliver`, `session-driver`, typed harness drivers, task lifecycle, Resource
//! bindings (declaration metadata), and the tasks. Everything else that is render-only (`harness`,
//! `model`, `persona`, `permissions`, legacy `transport` metadata, `strategy`, `meta{}`) is baked
//! into the tasks/commands by the render layer and ignored here.
//!
//! Three on-disk formats lower to this model: KDL (canonical, parsed by hand in `kdl_format`), and
//! TOML/JSON (serde). Every spec is a `service` — `type = batch` is retired; evals run through the
//! native `st2 eval` path (`eval_spec`/`eval_run`), which has its own model.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

/// Maximum Unicode scalar count for an agent's human-facing label.
pub const AGENT_NAME_MAX_CHARS: usize = 160;
/// Maximum Unicode scalar count for an agent's enduring responsibility description.
pub const AGENT_DESCRIPTION_MAX_CHARS: usize = 1_000;
/// Maximum UTF-8 byte length for a non-running desired-state rationale.
pub const AGENT_DESIRED_STATE_REASON_MAX_BYTES: usize = 160;
/// Maximum ASCII length of an explicit agent address (R24).
pub const AGENT_ADDRESS_MAX_BYTES: usize = 255;
/// Maximum length of one dotted agent-address segment (R24).
pub const AGENT_ADDRESS_SEGMENT_MAX_BYTES: usize = 63;
/// Maximum ASCII length of an explicit immutable agent ID.
///
/// The two admitted producers are UUIDv7 for a new subject and the frozen `<host>.<identity>`
/// bus identity of a migrated legacy subject, so the ID namespace shares the address budget.
pub const AGENT_ID_MAX_BYTES: usize = 255;

/// Declarative whole-agent lifecycle intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDesiredState {
    Running,
    Suspended {
        reason: String,
    },
    /// `None` exists only for legacy `retired #true` declarations.
    Retired {
        reason: Option<String>,
    },
}

/// One provider-native message delivery transport declared by an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryTransport {
    Mcp,
    AppServer,
    PiChannel,
}

impl DeliveryTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::AppServer => "app-server",
            Self::PiChannel => "pi-channel",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "mcp" => Ok(Self::Mcp),
            "app-server" => Ok(Self::AppServer),
            "pi-channel" => Ok(Self::PiChannel),
            _ => anyhow::bail!(
                "unsupported `deliver` value '{value}' (expected `mcp`, `app-server`, or `pi-channel`)"
            ),
        }
    }

    pub fn session_driver(self) -> SessionDriver {
        match self {
            Self::Mcp => SessionDriver::Claude,
            Self::AppServer => SessionDriver::Codex,
            Self::PiChannel => SessionDriver::Pi,
        }
    }
}
/// The native session driver entered by an otherwise opaque launch.
///
/// This is an ownership assertion only. It does not render a provider launch or select a message
/// delivery transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionDriver {
    Claude,
    Codex,
    Pi,
    OpenCode,
    Omp,
}

impl SessionDriver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
            Self::Omp => "omp",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "pi" => Ok(Self::Pi),
            "opencode" => Ok(Self::OpenCode),
            "omp" => Ok(Self::Omp),
            _ => anyhow::bail!(
                "unsupported `session-driver` value '{value}' (expected `claude`, `codex`, `pi`, `opencode`, or `omp`)"
            ),
        }
    }

    /// Parse the canonical driver name carried by `session-driver`.
    pub fn from_name(value: &str) -> anyhow::Result<Self> {
        Self::parse(value)
    }
}

/// Non-secret facts that prove how a managed session can be admitted for delivery.
///
/// This is deliberately separate from activity and runtime health. A credential-backed seat names
/// only its opaque account identifier; an anonymous seat names the exact harness and model allowlist
/// it can launch without credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum DeliveryReadiness {
    Credential {
        account_id: Option<String>,
    },
    Anonymous {
        harness: SessionDriver,
        models: Vec<String>,
    },
}

impl DeliveryReadiness {
    pub fn validate(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Credential { account_id } => {
                if let Some(account_id) = account_id {
                    validate_delivery_readiness_value("account-id", account_id)?;
                }
            }
            Self::Anonymous { harness: _, models } => {
                anyhow::ensure!(
                    !models.is_empty(),
                    "anonymous delivery-readiness requires at least one model"
                );
                anyhow::ensure!(
                    models.len() <= 32,
                    "anonymous delivery-readiness accepts at most 32 models"
                );
                for model in models.iter() {
                    validate_delivery_readiness_value("model", model)?;
                }
                models.sort();
                models.dedup();
            }
        }
        Ok(())
    }
}

fn validate_delivery_readiness_value(field: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 200,
        "delivery-readiness {field} must be 1..=200 UTF-8 bytes"
    );
    anyhow::ensure!(
        value.trim() == value
            && !value
                .chars()
                .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')),
        "delivery-readiness {field} must have no surrounding whitespace, controls, or line separators"
    );
    Ok(())
}


/// One typed harness driver declaration.
///
/// st2 expands this field into inspectable Agent Spec KDL before task and render compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driver {
    Claude(ClaudeDriver),
    Codex(CodexDriver),
    Pi(PiDriver),
    OpenCode(OpenCodeDriver),
    Omp(OmpDriver),
}

impl Driver {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude(_) => "claude",
            Self::Codex(_) => "codex",
            Self::Pi(_) => "pi",
            Self::OpenCode(_) => "opencode",
            Self::Omp(_) => "omp",
        }
    }

    pub fn session_driver(&self) -> SessionDriver {
        match self {
            Self::Claude(_) => SessionDriver::Claude,
            Self::Codex(_) => SessionDriver::Codex,
            Self::Pi(_) => SessionDriver::Pi,
            Self::OpenCode(_) => SessionDriver::OpenCode,
            Self::Omp(_) => SessionDriver::Omp,
        }
    }
}

/// Typed fields accepted by a `claude {}` driver block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ClaudeDriver {
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub dev_channels: bool,
    pub prompt: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Typed fields accepted by a `pi {}` driver block.
///
/// `effort` carries pi's thinking level verbatim; st2 does not translate the maintained harnesses'
/// effort vocabularies into one another.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PiDriver {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Typed fields accepted by a `codex {}` driver block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CodexDriver {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Typed fields accepted by an `opencode {}` driver block.
///
/// OpenCode has no effort axis; its permission policy lives in its config file rather than a
/// launch flag, so neither appears here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OpenCodeDriver {
    pub model: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Typed fields accepted by an `omp {}` driver block.
///
/// omp is pi-family and exposes the same two axes under the same flags: `effort` carries omp's
/// thinking level verbatim (`--thinking`), exactly as [`PiDriver`] does for pi.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OmpDriver {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl AgentDesiredState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Suspended { .. } => "suspended",
            Self::Retired { .. } => "retired",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Running => None,
            Self::Suspended { reason } => Some(reason),
            Self::Retired { reason } => reason.as_deref(),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended { .. })
    }

    pub fn is_retired(&self) -> bool {
        matches!(self, Self::Retired { .. })
    }
}

/// A rendered agent job, lowered to the shared declaration fields st2 and other readers inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Explicit immutable catalog-global agent ID (R24). `None` on a legacy declaration that
    /// catalog ID migration has not reached yet; required in the target grammar.
    pub id: Option<String>,
    /// Explicit mutable agent address (R24). `None` means the positional `identity` below is the
    /// effective legacy address.
    pub address: Option<String>,
    /// The positional declaration key and legacy address fallback. Not immutable subject identity.
    pub identity: String,
    /// Optional mutable human-facing label. Never used as an automation selector.
    pub name: Option<String>,
    /// Optional enduring responsibility boundary. Never used for lifecycle decisions.
    pub description: Option<String>,
    /// Which machine runs this agent. `None` → resolved to the path's host / this machine.
    pub host: Option<String>,
    /// Optional declared persona role. Preserved as metadata and ignored for execution.
    pub role: Option<String>,
    /// `service` (long-running, respawns) — the only job type. Defaults to service.
    pub job_type: JobType,
    /// The repo/worktree; **defaults each task's cwd** (spec.md §2).
    pub workspace: Option<String>,
    /// Bare identity or `<host>.<identity>` of this agent's supervisor — crash-dings route here.
    pub supervisor: Option<String>,
    /// Whole-agent lifecycle intent. Non-running states are reconciled absent.
    pub desired_state: AgentDesiredState,
    /// Agent-level GC pin: `true` exempts all of its tasks from garbage collection.
    pub keep: bool,
    /// Crash/restart policy (§4). `None` → the runner's default policy.
    pub restart: Option<Restart>,
    /// Provider-native delivery selected by `deliver`; `None` means legacy `ding` or no delivery.
    pub delivery: Option<DeliveryTransport>,
    /// Native session ownership asserted for an otherwise opaque launch.
    pub session_driver: Option<SessionDriver>,
    /// Typed harness declaration used by task and render compilation.
    pub driver: Option<Driver>,
    /// Non-secret admission facts for the managed delivery path.
    pub delivery_readiness: Option<DeliveryReadiness>,
    /// Named typed references used by the agent. st2 preserves these for readers but does not
    /// resolve them or assign launch, readiness, access, or lifecycle semantics.
    pub resources: Vec<Resource>,
    /// Named event subscriptions. Command-less streams are external ingress endpoints; launched
    /// streams additionally lower to one derived exec companion.
    pub streams: Vec<Stream>,
    /// The runnable tasks (`pty` + `exec`), sorted by name for determinism.
    pub tasks: Vec<Task>,
    /// Where this spec was loaded from — the anchor for its resources and for edits.
    pub path: PathBuf,
}

impl AgentSpec {
    /// The explicit native session owner after typed-driver normalization.
    pub fn effective_session_driver(&self) -> Option<SessionDriver> {
        self.session_driver
            .or_else(|| self.driver.as_ref().map(Driver::session_driver))
    }
}
fn deserialize_optional_selector<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// One agent-local semantic binding to an externally identified resource.
///
/// `name` is an agent-local label and `uri` is the exact absolute identity. `reason` explains why
/// the reference belongs in this Agent Spec. `inactive_reason` preserves a reference that is no
/// longer active for this agent without asserting anything about the resource itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resource {
    name: String,
    uri: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    inactive_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selector: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceDescriptor {
    name: String,
    uri: String,
    reason: String,
    inactive_reason: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_selector")]
    selector: Option<serde_json::Value>,
}

impl Resource {
    /// Construct a descriptor after enforcing the same invariants as catalog parsing.
    pub fn new(name: String, uri: String, reason: String) -> Result<Self, String> {
        if name.is_empty()
            || name.len() > 200
            || name.trim() != name
            || name.chars().any(char::is_control)
        {
            return Err(
                "resource binding name must be 1..=200 bytes without surrounding whitespace or controls"
                    .into(),
            );
        }
        if name == "declaration" {
            return Err("resource binding name 'declaration' is reserved by resync".into());
        }
        validate_resource_uri(&uri).map_err(|reason| {
            format!("resource binding '{name}' `uri` must be an exact absolute URI or a catalog-relative path: {reason}")
        })?;
        validate_resource_explanation(&name, "reason", &reason)?;
        Ok(Self {
            name,
            uri,
            reason,
            inactive_reason: None,
            selector: None,
        })
    }

    /// Construct a preserved reference that is inactive for this agent.
    pub fn new_inactive(
        name: String,
        uri: String,
        reason: String,
        inactive_reason: String,
    ) -> Result<Self, String> {
        let mut resource = Self::new(name, uri, reason)?;
        validate_resource_explanation(&resource.name, "inactive-reason", &inactive_reason)?;
        resource.inactive_reason = Some(inactive_reason);
        Ok(resource)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn inactive_reason(&self) -> Option<&str> {
        self.inactive_reason.as_deref()
    }

    pub fn selector(&self) -> Option<&serde_json::Value> {
        self.selector.as_ref()
    }

    pub fn with_selector(mut self, selector: serde_json::Value) -> Self {
        self.selector = Some(selector);
        self
    }
}

impl<'de> Deserialize<'de> for Resource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let descriptor = ResourceDescriptor::deserialize(deserializer)?;
        let selector = descriptor.selector;
        let resource = match descriptor.inactive_reason {
            None => Self::new(descriptor.name, descriptor.uri, descriptor.reason),
            Some(inactive_reason) => Self::new_inactive(
                descriptor.name,
                descriptor.uri,
                descriptor.reason,
                inactive_reason,
            ),
        };
        resource
            .map(|resource| match selector {
                Some(selector) => resource.with_selector(selector),
                None => resource,
            })
            .map_err(de::Error::custom)
    }
}

/// The kind of job. Only `service` (long-running) remains — `type = batch` is retired; the native
/// `st2 eval` path (eval_spec/eval_run) replaces the old staged batch executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobType {
    #[default]
    Service,
}

/// A task: one process st2 keeps running (a `pty` task) or runs once (a terminal-free `exec` task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// `pty` (interactive, allocates a terminal) or `exec` (plain process, terminal-free).
    pub kind: TaskKind,
    /// `true` when st2 generated this task from shorthand rather than the author declaring runnable
    /// work. Derived sidecars run alongside an authored task, but cannot make a job runnable alone.
    pub derived: bool,
    /// The task name (`agent`, `ding`, …).
    pub name: String,
    /// Explicit on-disk id. `None` → `<host>.<identity>.<name>` at spawn.
    pub id: Option<String>,
    /// A shell program, run verbatim under `sh -c`.
    pub command: Option<String>,
    /// A direct program invocation. Element 0 is the program and the rest are its arguments.
    ///
    /// Mutually exclusive with [`Task::command`]. Neither field means this is not a launch target.
    pub argv: Option<Vec<String>>,
    /// Working dir; `None` → the agent's `workspace`, else the spec file's directory.
    pub cwd: Option<String>,
    /// Arbitrary metadata (values are `$`-expanded at spawn).
    pub tags: BTreeMap<String, String>,
    /// Environment (values are `$`-expanded at spawn).
    pub env: BTreeMap<String, String>,
    /// Per-task GC pin.
    pub keep: bool,
    /// Reconciliation policy. `adopt-only` is a migration fence: st2 may adopt a live generation,
    /// but must not reap a dead generation or create a missing replacement.
    pub lifecycle: TaskLifecycle,
}

/// Whether a task allocates a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Interactive — allocates a pseudo-terminal (an agent harness).
    Pty,
    /// Non-interactive — a plain process, no terminal (R09).
    Exec,
}

/// How st2 reconciles a declared task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskLifecycle {
    /// Ordinary service lifecycle: launch when absent and replace when dead.
    #[default]
    Service,
    /// Migration fence: adopt an already-live generation, otherwise hold without mutation.
    AdoptOnly,
}

/// Restart policy (§4). Applies to long-running `service` tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    /// Max restarts within `interval`.
    pub attempts: u32,
    /// The window `attempts` is counted over.
    pub interval: Duration,
    /// Wait between restarts.
    pub delay: Duration,
    /// `fail` = stop after `attempts` (surface it) · `delay` = keep restarting, resetting per interval.
    pub mode: RestartMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMode {
    /// Stop after `attempts` exhaust and surface it.
    Fail,
    /// Keep restarting, resetting the counter each interval.
    Delay,
}

impl Default for Restart {
    /// The runner default when a job omits `restart{}` — mirrors the pre-VRS flapping-cap (3 / 60s),
    /// keep-restarting.
    fn default() -> Self {
        Self {
            attempts: 3,
            interval: Duration::from_secs(60),
            delay: Duration::from_secs(0),
            mode: RestartMode::Delay,
        }
    }
}

impl AgentSpec {
    /// The bus id this spec compiles to — `<host>.<identity>` — using `this_host` when `host` is unset.
    pub fn bus_id(&self, this_host: &str) -> String {
        format!(
            "{}.{}",
            self.host.as_deref().unwrap_or(this_host),
            self.identity
        )
    }

    /// The effective immutable subject ID during the ID migration window (R24).
    ///
    /// The explicit `id` when the declaration carries one, otherwise the legacy
    /// `<host>.<identity>` bus identity — which is exactly the value catalog ID migration freezes
    /// as this subject's explicit ID, so a mixed catalog stays coherent while it is migrated.
    pub fn effective_id(&self, this_host: &str) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| self.bus_id(this_host))
    }

    /// The effective agent address (R24): the explicit `address` when present, otherwise the
    /// positional `identity` legacy fallback.
    pub fn effective_address(&self) -> &str {
        self.address.as_deref().unwrap_or(&self.identity)
    }

    /// The human-routable bus address `<host>.<effective address>` (R24), using `this_host` when
    /// `host` is unset. This is a mutable route, never the immutable subject ID.
    pub fn bus_address(&self, this_host: &str) -> String {
        format!(
            "{}.{}",
            self.host.as_deref().unwrap_or(this_host),
            self.effective_address()
        )
    }

    /// The host that should run this spec, defaulting to `this_host` when unset.
    pub fn resolved_host<'a>(&'a self, this_host: &'a str) -> &'a str {
        self.host.as_deref().unwrap_or(this_host)
    }

    /// True when a compiled or authored task contains a launch.
    /// Callers that accept driver blocks must compile generated tasks before this check.
    /// A generated sidecar cannot make an otherwise-empty job runnable.
    pub fn is_runnable(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| !task.derived && (task.command.is_some() || task.argv.is_some()))
    }

    /// True when the declaration selected legacy screen delivery or one native transport.
    pub fn has_delivery_transport(&self) -> bool {
        self.driver.is_some()
            || self.delivery.is_some()
            || self
                .tasks
                .iter()
                .any(|task| task.derived && task.kind == TaskKind::Exec && task.name == "ding")
    }

    /// The restart policy in effect (declared, else the runner default).
    pub fn restart_policy(&self) -> Restart {
        self.restart.clone().unwrap_or_default()
    }
}

// ---- Duration parsing ("60s", "5s", "20m", "2h", "3d") ---------------------------------------

/// Parse a duration like `60s` / `5m` / `2h` / `3d` (also a bare integer = seconds).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "s"), // bare number → seconds
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad duration number in '{s}'"))?;
    let secs = match unit.trim() {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        "ms" => return Ok(Duration::from_millis(n)),
        other => return Err(format!("unknown duration unit '{other}' in '{s}'")),
    };
    Ok(Duration::from_secs(secs))
}

// ---- Raw deserialization target (shared by TOML + JSON) --------------------------------------

/// The permissive on-disk shape. Unknown keys (`harness`, `model`, `persona`, `permissions`,
/// `transport`, `strategy`, `meta`, …) are intentionally dropped — that is how st2 stays
/// render-agnostic.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSpec {
    pub id: Option<String>,
    pub address: Option<String>,
    pub identity: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub host: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "type")]
    pub job_type: Option<String>,
    pub workspace: Option<String>,
    pub supervisor: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explicit_optional")]
    pub retired: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_explicit_optional")]
    pub desired_state: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_explicit_optional")]
    pub desired_state_reason: Option<Option<String>>,
    #[serde(default)]
    pub keep: bool,
    pub restart: Option<RawRestart>,
    /// Named resource bindings. Singular `resource` matches canonical KDL and keeps TOML/JSON maps
    /// aligned with `resource "<name>"`.
    #[serde(default)]
    pub resource: RawResources,
    /// Compact catalog form: the agent itself is one pty carrying this command.
    pub command: Option<String>,
    /// Compact catalog form: the agent itself is one pty launched directly with this argv.
    pub argv: Option<Vec<String>>,
    /// Compact catalog form: environment inherited by the agent pty and every sidecar.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Compact catalog form: include the built-in `st2 ding` sidecar.
    #[serde(default)]
    pub ding: bool,
    /// Compact catalog form: select one provider-native delivery transport.
    #[serde(default, deserialize_with = "deserialize_explicit_optional")]
    pub deliver: Option<Option<String>>,
    /// Native session ownership asserted for an otherwise opaque launch.
    #[serde(default, deserialize_with = "deserialize_explicit_optional")]
    pub session_driver: Option<Option<String>>,
    /// Non-secret facts used to admit the managed delivery path.
    pub delivery_readiness: Option<DeliveryReadiness>,
    /// Direct typed provider driver block.
    #[serde(flatten)]
    pub driver: RawDriver,
    /// Compact catalog form: reconciliation policy for the generated agent PTY.
    pub lifecycle: Option<String>,
    /// `pty "<name>" {}` / `[pty.<name>]` — interactive tasks.
    #[serde(default)]
    pub pty: BTreeMap<String, RawTask>,
    /// `exec "<name>" {}` / `[exec.<name>]` — terminal-free tasks.
    #[serde(default)]
    pub exec: BTreeMap<String, RawTask>,
    /// `stream "<name>" {}` / `[stream.<name>]` — an external event source whose stdout lines st2
    /// delivers into this agent's inbox. Lowers to one derived exec companion, exactly like `ding`.
    #[serde(default)]
    pub stream: BTreeMap<String, RawStream>,
}

/// A declared event source. Exactly one of `command` / `argv` launches the source process.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawStream {
    pub command: Option<String>,
    pub argv: Option<Vec<String>>,
}

/// One agent-owned event subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stream {
    pub name: String,
    pub launch: Option<StreamLaunch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "type", content = "value")]
pub enum StreamLaunch {
    Command(String),
    Argv(Vec<String>),
}

/// The longest stream name that still leaves a legible `<host>.<identity>.stream-<name>` runtime id.
const STREAM_NAME_MAX_CHARS: usize = 40;

/// Derived stream task names are exactly `stream-<declared name>`.
pub const STREAM_TASK_PREFIX: &str = "stream-";

/// The declared stream name behind a derived task name, if this is a stream companion.
pub fn stream_name_of_task(task_name: &str) -> Option<&str> {
    task_name.strip_prefix(STREAM_TASK_PREFIX)
}

/// A stream name becomes part of a runner-owned task name and runtime id, so it is restricted to a
/// lowercase slug. This keeps `stream-<name>` unambiguous against the `<bus-id>.<task>` id grammar.
fn validate_stream_name(identity: &str, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.chars().count() <= STREAM_NAME_MAX_CHARS,
        "agent '{identity}' stream name '{name}' must be 1..={STREAM_NAME_MAX_CHARS} characters"
    );
    anyhow::ensure!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-'),
        "agent '{identity}' stream name '{name}' must match [a-z0-9]([a-z0-9-]*[a-z0-9])?"
    );
    anyhow::ensure!(
        name != "resync",
        "agent '{identity}' stream name '{name}' is reserved for built-in resync events"
    );
    Ok(())
}

/// The permissive raw envelope keeps the provider name at the same level in KDL, TOML, and JSON.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawDriver {
    pub(crate) claude: Option<ClaudeDriver>,
    pub(crate) codex: Option<CodexDriver>,
    pub(crate) pi: Option<PiDriver>,
    pub(crate) opencode: Option<OpenCodeDriver>,
    pub(crate) omp: Option<OmpDriver>,
}

impl RawDriver {
    fn lower(self, identity: &str) -> anyhow::Result<Option<Driver>> {
        // Named rather than positional so a fourth provider cannot silently widen a tuple match.
        let mut declared: Vec<(&str, Driver)> = Vec::new();
        if let Some(driver) = self.claude {
            declared.push(("claude", Driver::Claude(driver)));
        }
        if let Some(driver) = self.codex {
            declared.push(("codex", Driver::Codex(driver)));
        }
        if let Some(driver) = self.pi {
            declared.push(("pi", Driver::Pi(driver)));
        }
        if let Some(driver) = self.opencode {
            declared.push(("opencode", Driver::OpenCode(driver)));
        }
        if let Some(driver) = self.omp {
            declared.push(("omp", Driver::Omp(driver)));
        }
        match declared.len() {
            0 => Ok(None),
            1 => Ok(Some(declared.pop().expect("length was just checked").1)),
            _ => {
                let names = declared
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join("` and `");
                anyhow::bail!("agent '{identity}' declares both `{names}`; choose one driver")
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RawResources(BTreeMap<String, RawResource>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawResource {
    pub(crate) uri: String,
    pub(crate) reason: String,
    pub(crate) inactive_reason: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_selector")]
    pub(crate) selector: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawTask {
    pub id: Option<String>,
    pub command: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub keep: bool,
    pub lifecycle: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawRestart {
    pub attempts: Option<u32>,
    pub interval: Option<String>,
    pub delay: Option<String>,
    pub mode: Option<String>,
}

impl RawRestart {
    pub(crate) fn lower(self) -> Restart {
        let d = Restart::default();
        Restart {
            attempts: self.attempts.unwrap_or(d.attempts),
            interval: self
                .interval
                .and_then(|s| parse_duration(&s).ok())
                .unwrap_or(d.interval),
            delay: self
                .delay
                .and_then(|s| parse_duration(&s).ok())
                .unwrap_or(d.delay),
            mode: match self.mode.as_deref() {
                Some("fail") => RestartMode::Fail,
                Some("delay") => RestartMode::Delay,
                _ => d.mode,
            },
        }
    }
}

impl RawResources {
    pub(crate) fn insert(&mut self, name: String, resource: RawResource) -> anyhow::Result<()> {
        if self.0.insert(name.clone(), resource).is_some() {
            anyhow::bail!("duplicate resource binding '{name}'");
        }
        Ok(())
    }

    fn lower(self) -> anyhow::Result<Vec<Resource>> {
        self.0
            .into_iter()
            .map(|(name, resource)| {
                let selector = resource.selector;
                let resource = match resource.inactive_reason {
                    None => Resource::new(name, resource.uri, resource.reason),
                    Some(inactive_reason) => {
                        Resource::new_inactive(name, resource.uri, resource.reason, inactive_reason)
                    }
                }
                .map_err(anyhow::Error::msg)?;
                Ok(match selector {
                    Some(selector) => resource.with_selector(selector),
                    None => resource,
                })
            })
            .collect()
    }
}

impl<'de> Deserialize<'de> for RawResources {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ResourceMapVisitor;

        impl<'de> Visitor<'de> for ResourceMapVisitor {
            type Value = RawResources;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of uniquely named resource bindings")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut resources = BTreeMap::new();
                while let Some((name, resource)) = map.next_entry::<String, RawResource>()? {
                    if resources.insert(name.clone(), resource).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate resource binding '{name}'"
                        )));
                    }
                }
                Ok(RawResources(resources))
            }
        }

        deserializer.deserialize_map(ResourceMapVisitor)
    }
}

fn validate_resource_explanation(name: &str, field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 160 {
        return Err(format!(
            "resource binding '{name}' `{field}` must be 1..160 UTF-8 bytes"
        ));
    }
    if value.trim() != value
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(format!(
            "resource binding '{name}' `{field}` must have no surrounding Unicode whitespace, controls, or line separators"
        ));
    }
    Ok(())
}

/// A resource URI is either an exact absolute URI (any scheme) or a catalog-relative path with no
/// scheme at all, resolved by the consumer against the declaration directory. Relative carriers
/// are an st2 extension pending canonical Agent Spec adoption (see 06-resync).
fn validate_resource_uri(uri: &str) -> Result<(), &'static str> {
    if let Some(colon) = uri.find(':') {
        let first_separator = uri.find(|character| matches!(character, '/' | '\\'));
        if first_separator.is_none_or(|separator| colon < separator) {
            return validate_absolute_uri(uri);
        }
    }
    if uri.is_empty() {
        return Err("catalog-relative uri must be a non-empty relative path");
    }
    let decoded = decode_percent_path(uri)?;
    if decoded.starts_with('/') || decoded.split('/').any(|part| part == "..") {
        return Err(
            "catalog-relative uri must be a relative path without parent (`..`) components",
        );
    }
    Ok(())
}

/// Decode filesystem-path percent escapes without allowing an escape to change path segmentation.
///
/// Consumers may apply additional policy to the decoded path (for example, catalog-relative
/// resources reject every parent component). This shared boundary rejects bytes that cannot name
/// the same UTF-8 filesystem path the resource URI visibly denotes.
pub fn decode_percent_path(encoded_path: &str) -> Result<String, &'static str> {
    let bytes = encoded_path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut component_start = 0;
    let mut component_had_escape = false;
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'/' {
            if component_had_escape && decoded[component_start..] == *b".." {
                return Err("path contains an encoded parent (`..`) component");
            }
            decoded.push(b'/');
            component_start = decoded.len();
            component_had_escape = false;
            offset += 1;
            continue;
        }
        if bytes[offset] != b'%' {
            decoded.push(bytes[offset]);
            offset += 1;
            continue;
        }
        let Some(high) = bytes.get(offset + 1).and_then(|byte| hex_digit(*byte)) else {
            return Err("path contains an invalid percent escape");
        };
        let Some(low) = bytes.get(offset + 2).and_then(|byte| hex_digit(*byte)) else {
            return Err("path contains an invalid percent escape");
        };
        let byte = (high << 4) | low;
        if matches!(byte, b'/' | b'\\') {
            return Err("path contains an encoded separator");
        }
        if byte == b'\0' {
            return Err("path decodes to NUL");
        }
        decoded.push(byte);
        component_had_escape = true;
        offset += 3;
    }
    if component_had_escape && decoded[component_start..] == *b".." {
        return Err("path contains an encoded parent (`..`) component");
    }
    String::from_utf8(decoded).map_err(|_| "percent-decoded path is not valid UTF-8")
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_absolute_uri(uri: &str) -> Result<(), &'static str> {
    let Some(colon) = uri.find(':') else {
        return Err("missing scheme");
    };
    let scheme = &uri[..colon];
    let mut chars = scheme.chars();
    if !chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return Err("invalid scheme");
    }
    if !uri.is_ascii() {
        return Err("contains non-ASCII characters");
    }

    let remainder = &uri[colon + 1..];
    let (without_fragment, fragment) = match remainder.split_once('#') {
        Some((head, fragment)) if !fragment.contains('#') => (head, Some(fragment)),
        Some(_) => return Err("contains more than one fragment delimiter"),
        None => (remainder, None),
    };
    let (hierarchy, query) = match without_fragment.split_once('?') {
        Some((hierarchy, query)) => (hierarchy, Some(query)),
        None => (without_fragment, None),
    };

    if let Some(authority_and_path) = hierarchy.strip_prefix("//") {
        let (authority, path) = match authority_and_path.find('/') {
            Some(slash) => (&authority_and_path[..slash], &authority_and_path[slash..]),
            None => (authority_and_path, ""),
        };
        validate_uri_authority(authority)?;
        validate_uri_component(path, b":@/")?;
    } else {
        validate_uri_component(hierarchy, b":@/")?;
    }
    if let Some(query) = query {
        validate_uri_component(query, b":@/?")?;
    }
    if let Some(fragment) = fragment {
        validate_uri_component(fragment, b":@/?")?;
    }
    Ok(())
}

fn validate_uri_authority(authority: &str) -> Result<(), &'static str> {
    let mut at_parts = authority.split('@');
    let first = at_parts.next().unwrap_or_default();
    let second = at_parts.next();
    if at_parts.next().is_some() {
        return Err("authority contains more than one userinfo delimiter");
    }
    let host_port = match second {
        Some(host_port) => {
            validate_uri_component(first, b":")?;
            host_port
        }
        None => first,
    };

    if let Some(literal) = host_port.strip_prefix('[') {
        let Some(close) = literal.find(']') else {
            return Err("authority contains an unclosed IP literal");
        };
        let address = &literal[..close];
        let after = &literal[close + 1..];
        if address.is_empty() || after.contains('[') || after.contains(']') {
            return Err("authority contains an invalid IP literal");
        }
        if address.starts_with('v') || address.starts_with('V') {
            validate_ipv_future(address)?;
        } else if address.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("authority contains an invalid IPv6 literal");
        }
        validate_uri_port(after)?;
        return Ok(());
    }
    if host_port.contains('[') || host_port.contains(']') {
        return Err("authority contains misplaced IP-literal brackets");
    }

    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err("IPv6 authority literals must be bracketed");
            }
            (host, Some(port))
        }
        None => (host_port, None),
    };
    validate_uri_component(host, b"")?;
    if let Some(port) = port
        && !port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("authority port contains a non-digit");
    }
    Ok(())
}

fn validate_uri_port(after_literal: &str) -> Result<(), &'static str> {
    if after_literal.is_empty() {
        return Ok(());
    }
    let Some(port) = after_literal.strip_prefix(':') else {
        return Err("IP literal is followed by invalid authority syntax");
    };
    if !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("authority port contains a non-digit");
    }
    Ok(())
}

fn validate_ipv_future(address: &str) -> Result<(), &'static str> {
    let Some((version, body)) = address[1..].split_once('.') else {
        return Err("authority contains an invalid IPvFuture literal");
    };
    if version.is_empty()
        || !version.bytes().all(|byte| byte.is_ascii_hexdigit())
        || body.is_empty()
        || body.contains('%')
    {
        return Err("authority contains an invalid IPvFuture literal");
    }
    validate_uri_component(body, b":")
}

fn validate_uri_component(value: &str, extra: &[u8]) -> Result<(), &'static str> {
    let bytes = value.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let byte = bytes[offset];
        if byte == b'%' {
            if offset + 2 >= bytes.len()
                || !bytes[offset + 1].is_ascii_hexdigit()
                || !bytes[offset + 2].is_ascii_hexdigit()
            {
                return Err("contains an invalid percent escape");
            }
            offset += 3;
        } else if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
            )
            || extra.contains(&byte)
        {
            offset += 1;
        } else {
            return Err("contains a raw character forbidden by RFC 3986");
        }
    }
    Ok(())
}

impl RawSpec {
    /// A parsed file is a *spec candidate* when it carries an agent-shaped signal — an identity,
    /// lifecycle intent, the supported `service` type, or task blocks. Random TOML/JSON in the tree
    /// has none of these and is skipped.
    ///
    /// `id` and `address` are deliberately not signals. They are two of the most common keys in
    /// arbitrary JSON/TOML — a dropped GitHub payload, a task cache, a session record — and a file
    /// whose only agent-shaped key is one of them cannot be a valid declaration anyway: it carries
    /// no launch, so it would only ever join the roster as a phantom agent and stop the catalog
    /// from admitting. Migration adds `id` to declarations that are already candidates.
    pub(crate) fn looks_like_spec(&self) -> bool {
        self.identity.is_some()
            || self.job_type.as_deref() == Some("service")
            || self.retired.is_some()
            || self.desired_state.is_some()
            || self.desired_state_reason.is_some()
            || self.command.is_some()
            || self.argv.is_some()
            || self.ding
            || self.deliver.is_some()
            || self.session_driver.is_some()
            || self.delivery_readiness.is_some()
            || self.driver.claude.is_some()
            || self.driver.codex.is_some()
            // pi predates this predicate gaining driver awareness and was silently skipped too:
            // an identity-omitting file whose only agent-shaped signal is its driver block must
            // still be a candidate, whichever provider the block names.
            || self.driver.pi.is_some()
            || self.driver.opencode.is_some()
            || self.driver.omp.is_some()
            || !self.resource.0.is_empty()
            || !self.pty.is_empty()
            || !self.exec.is_empty()
            || !self.stream.is_empty()
    }

    /// Lower into an [`AgentSpec`], with `identity`/`host` resolved from the path when content omits them.
    pub(crate) fn into_agent_spec(
        self,
        identity: String,
        host: Option<String>,
        path: PathBuf,
    ) -> anyhow::Result<AgentSpec> {
        validate_presentation("name", self.name.as_deref(), AGENT_NAME_MAX_CHARS)?;
        validate_presentation(
            "description",
            self.description.as_deref(),
            AGENT_DESCRIPTION_MAX_CHARS,
        )?;
        if let Some(id) = self.id.as_deref() {
            validate_agent_id(id)?;
        }
        if let Some(address) = self.address.as_deref() {
            validate_agent_address(address)?;
        }
        let retired = reject_explicit_null("retired", self.retired)?;
        let desired_state_value = reject_explicit_null("desired_state", self.desired_state)?;
        let desired_state_reason =
            reject_explicit_null("desired_state_reason", self.desired_state_reason)?;
        let desired_state = lower_desired_state(
            retired,
            desired_state_value.as_deref(),
            desired_state_reason,
        )?;
        let deliver = reject_explicit_null("deliver", self.deliver)?;
        let delivery = deliver
            .as_deref()
            .map(DeliveryTransport::parse)
            .transpose()?;
        let session_driver = reject_explicit_null("session_driver", self.session_driver)?
            .as_deref()
            .map(SessionDriver::parse)
            .transpose()?;
        let driver = self.driver.lower(&identity)?;
        let has_driver = driver.is_some();
        let mut delivery_readiness = self.delivery_readiness;
        if let Some(readiness) = delivery_readiness.as_mut() {
            readiness.validate()?;
        }
        anyhow::ensure!(
            !(self.ding && delivery.is_some()),
            "agent '{identity}' declares both `ding` and `deliver`; choose one transport"
        );
        anyhow::ensure!(
            !(self.ding && (has_driver || session_driver.is_some() || delivery_readiness.is_some())),
            "agent '{identity}' declares managed native delivery together with `ding`; generic Ding is only for opaque non-harness PTYs"
        );
        anyhow::ensure!(
            !(session_driver.is_some() && has_driver),
            "agent '{identity}' declares both `session-driver` and a typed driver; choose one session owner"
        );
        let effective_session_driver =
            session_driver.or_else(|| driver.as_ref().map(Driver::session_driver));
        if let (Some(delivery), Some(effective)) = (delivery, effective_session_driver) {
            anyhow::ensure!(
                delivery.session_driver() == effective,
                "agent '{identity}' delivery transport '{}' requires session-driver '{}', not '{}'",
                delivery.as_str(),
                delivery.session_driver().as_str(),
                effective.as_str()
            );
        }
        if let (
            Some(DeliveryReadiness::Anonymous { harness, .. }),
            Some(effective),
        ) = (delivery_readiness.as_ref(), effective_session_driver)
        {
            anyhow::ensure!(
                *harness == effective,
                "agent '{identity}' anonymous delivery-readiness harness '{}' does not match effective session-driver '{}'",
                harness.as_str(),
                effective.as_str()
            );
        }
        validate_launch(
            &identity,
            self.command.as_ref(),
            self.argv.as_ref(),
            "compact task",
        )?;
        if (self.command.is_some() || self.argv.is_some() || has_driver)
            && self.pty.contains_key("agent")
        {
            anyhow::bail!(
                "agent '{identity}' declares both a compact launch and `pty \"agent\"`; choose one form"
            );
        }
        let bus_id = format!("{}.{}", host.as_deref().unwrap_or_default(), identity)
            .trim_start_matches('.')
            .to_string();
        let mut tasks: Vec<Task> = Vec::new();
        // Authored task names, captured before the maps are consumed: a derived stream companion must
        // not silently shadow an explicit sibling that already owns `stream-<name>`.
        let authored_task_names = self
            .pty
            .keys()
            .chain(self.exec.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for (name, t) in self.pty {
            tasks.push(t.lower(&identity, TaskKind::Pty, name, &self.env)?);
        }
        for (name, t) in self.exec {
            tasks.push(t.lower(&identity, TaskKind::Exec, name, &self.env)?);
        }
        if self.command.is_some() || self.argv.is_some() || has_driver {
            let lifecycle =
                parse_task_lifecycle(&identity, "compact task", self.lifecycle.as_deref())?;
            tasks.push(Task {
                kind: TaskKind::Pty,
                derived: false,
                name: "agent".to_string(),
                // An agent IS its pty: ding defaults its poke target to this same bus id.
                id: Some(bus_id.clone()),
                command: self.command,
                argv: self.argv,
                cwd: None,
                tags: BTreeMap::new(),
                env: self.env.clone(),
                keep: false,
                lifecycle,
            });
        }
        if self.ding {
            tasks.push(Task {
                kind: TaskKind::Exec,
                derived: true,
                name: "ding".to_string(),
                id: Some(format!("{bus_id}.ding")),
                command: Some(format!("st2 ding --identity {bus_id} --root $ST_ROOT")),
                argv: None,
                cwd: None,
                tags: BTreeMap::new(),
                env: self.env.clone(),
                keep: false,
                lifecycle: TaskLifecycle::Service,
            });
        }
        let has_canonical_agent_task = tasks
            .iter()
            .any(|task| !task.derived && task.name == "agent");
        // One derived exec companion per stream, through the exact seam the DING sidecar uses. The
        // marker argv carries the declared source launch; `reconcile` late-binds argv[0] to the
        // running st2 binary and substitutes the effective `ST_ROOT`, exactly as it does for DING
        // and for a driver expansion.
        let mut streams = Vec::new();
        for (name, stream) in self.stream {
            validate_stream_name(&identity, &name)?;
            let task_name = format!("stream-{name}");
            anyhow::ensure!(
                !authored_task_names.contains(&task_name),
                "agent '{identity}' declares both `stream \"{name}\"` and a task named \
                 `{task_name}`; choose one form"
            );
            let (command, argv) = match (stream.command, stream.argv) {
                (Some(command), None) => {
                    anyhow::ensure!(
                        !command.trim().is_empty(),
                        "agent '{identity}' stream '{name}' has an empty `command`"
                    );
                    (Some(command), None)
                }
                (None, Some(argv)) => {
                    anyhow::ensure!(
                        !argv.is_empty() && !argv[0].trim().is_empty(),
                        "agent '{identity}' stream '{name}' has an empty `argv`"
                    );
                    (None, Some(argv))
                }
                (None, None) => (None, None),
                (Some(_), Some(_)) => anyhow::bail!(
                    "agent '{identity}' stream '{name}' declares both `command` and `argv`; choose one"
                ),
            };
            let launch = match (&command, &argv) {
                (Some(command), None) => Some(StreamLaunch::Command(command.clone())),
                (None, Some(argv)) => Some(StreamLaunch::Argv(argv.clone())),
                (None, None) => None,
                (Some(_), Some(_)) => unreachable!("validated above"),
            };
            anyhow::ensure!(
                launch.is_none() || has_canonical_agent_task,
                "agent '{identity}' launched stream '{name}' requires a canonical `agent` task"
            );
            streams.push(Stream {
                name: name.clone(),
                launch,
            });
            if command.is_none() && argv.is_none() {
                continue;
            }
            tasks.push(Task {
                kind: TaskKind::Exec,
                derived: true,
                name: task_name.clone(),
                id: Some(format!("{bus_id}.{task_name}")),
                command,
                argv,
                cwd: None,
                tags: BTreeMap::new(),
                env: self.env.clone(),
                keep: false,
                lifecycle: TaskLifecycle::Service,
            });
        }
        tasks.sort_by(|a, b| a.name.cmp(&b.name));

        // `service` is the only job type; a stray `type` string is caught by validate (unknown-type).
        let job_type = JobType::Service;
        let resources = self.resource.lower()?;

        Ok(AgentSpec {
            id: self.id,
            address: self.address,
            identity,
            name: self.name,
            description: self.description,
            host,
            role: self.role,
            job_type,
            workspace: self.workspace,
            supervisor: self.supervisor,
            desired_state,
            keep: self.keep,
            restart: self.restart.map(RawRestart::lower),
            delivery,
            session_driver,
            driver,
            delivery_readiness,
            resources,
            streams,
            tasks,
            path,
        })
    }
}

/// Preserve the distinction between an omitted TOML/JSON field and an explicit `null`.
/// Serde's ordinary `Option<T>` representation intentionally collapses those cases.
fn deserialize_explicit_optional<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

fn reject_explicit_null<T>(field: &str, value: Option<Option<T>>) -> anyhow::Result<Option<T>> {
    match value {
        None => Ok(None),
        Some(Some(value)) => Ok(Some(value)),
        Some(None) => anyhow::bail!("agent lifecycle field `{field}` must not be null"),
    }
}

fn lower_desired_state(
    retired: Option<bool>,
    state: Option<&str>,
    reason: Option<String>,
) -> anyhow::Result<AgentDesiredState> {
    anyhow::ensure!(
        retired.is_none() || state.is_none(),
        "agent declares both legacy `retired` and `desired-state`; choose one lifecycle form"
    );
    match (state, reason) {
        (None, None) => Ok(if retired == Some(true) {
            AgentDesiredState::Retired { reason: None }
        } else {
            AgentDesiredState::Running
        }),
        (None, Some(_)) => anyhow::bail!("agent lifecycle `reason` requires `desired-state`"),
        (Some("running"), None) => Ok(AgentDesiredState::Running),
        (Some("running"), Some(_)) => {
            anyhow::bail!("agent `desired-state \"running\"` forbids `reason`")
        }
        (Some("suspended"), Some(reason)) => {
            validate_desired_state_reason(&reason)?;
            Ok(AgentDesiredState::Suspended { reason })
        }
        (Some("retired"), Some(reason)) => {
            validate_desired_state_reason(&reason)?;
            Ok(AgentDesiredState::Retired {
                reason: Some(reason),
            })
        }
        (Some("suspended" | "retired"), None) => {
            anyhow::bail!("agent `desired-state {state:?}` requires `reason`")
        }
        (Some(other), _) => anyhow::bail!(
            "unknown agent desired state '{other}'; expected running, suspended, or retired"
        ),
    }
}

/// Validate the rationale carried by a non-running desired state.
pub fn validate_desired_state_reason(reason: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !reason.is_empty() && reason.len() <= AGENT_DESIRED_STATE_REASON_MAX_BYTES,
        "agent desired-state `reason` must be 1..{AGENT_DESIRED_STATE_REASON_MAX_BYTES} UTF-8 bytes"
    );
    anyhow::ensure!(
        reason.trim() == reason
            && !reason
                .chars()
                .any(|character| character.is_control()
                    || matches!(character, '\u{2028}' | '\u{2029}')),
        "agent desired-state `reason` must have no surrounding Unicode whitespace, controls, or line separators"
    );
    Ok(())
}

/// Validate one optional presentation field at the shared parse/authoring boundary.
pub fn validate_presentation(
    field: &str,
    value: Option<&str>,
    max_chars: usize,
) -> anyhow::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    anyhow::ensure!(
        !value.is_empty(),
        "agent presentation `{field}` cannot be empty; omit it to clear it"
    );
    anyhow::ensure!(
        value.trim() == value,
        "agent presentation `{field}` cannot begin or end with whitespace"
    );
    anyhow::ensure!(
        !value.chars().any(|character| {
            character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
        }),
        "agent presentation `{field}` must be one printable line without control characters or Unicode line separators"
    );
    anyhow::ensure!(
        value.chars().count() <= max_chars,
        "agent presentation `{field}` exceeds the {max_chars}-character limit"
    );
    Ok(())
}

/// Validate an explicit immutable agent ID at the shared parse/authoring boundary (R24).
///
/// The ID is opaque, but not arbitrary: R26 reuses it verbatim as the canonical task ID and
/// therefore as a session socket path component, and IDs travel through shell-adjacent text (bus
/// messages, notices, journal lines), where a backtick has already executed a command on a live
/// host (schickling/dotfiles#1614). So the grammar is the closed safe set `[A-Za-z0-9._-]`, which
/// admits both admitted producers — a new subject's UUIDv7 and a migrated subject's frozen
/// `<host>.<identity>` bus identity — and refuses every shell metacharacter, path and host
/// separator, whitespace byte, and non-ASCII byte outright.
///
/// It stays wider than the address grammar on purpose: a frozen legacy ID keeps host-looking bytes
/// and whatever case and underscores its identity carried, so freezing an admissible identity can
/// never be refused here. Equal bytes in the two namespaces do not collide.
pub fn validate_agent_id(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "agent `id` cannot be empty");
    anyhow::ensure!(
        value.len() <= AGENT_ID_MAX_BYTES,
        "agent `id` '{value}' exceeds the {AGENT_ID_MAX_BYTES}-byte limit"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "agent `id` '{value}' must match [A-Za-z0-9._-]+"
    );
    anyhow::ensure!(
        !value.starts_with('.') && !value.ends_with('.'),
        "agent `id` '{value}' cannot begin or end with `.`"
    );
    Ok(())
}

/// Validate an explicit mutable agent address at the shared parse/authoring boundary (R24).
///
/// An explicit address is at most [`AGENT_ADDRESS_MAX_BYTES`] ASCII characters and is a dotted
/// sequence of 1-to-[`AGENT_ADDRESS_SEGMENT_MAX_BYTES`]-character segments. Each segment contains
/// only lowercase letters, digits, and hyphens and begins and ends with a letter or digit.
pub fn validate_agent_address(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty(),
        "agent `address` cannot be empty; omit it to fall back to the positional identity"
    );
    anyhow::ensure!(
        value.len() <= AGENT_ADDRESS_MAX_BYTES,
        "agent `address` '{value}' exceeds the {AGENT_ADDRESS_MAX_BYTES}-character limit"
    );
    anyhow::ensure!(
        value.is_ascii(),
        "agent `address` '{value}' must be ASCII"
    );
    for segment in value.split('.') {
        anyhow::ensure!(
            !segment.is_empty(),
            "agent `address` '{value}' has an empty dotted segment"
        );
        anyhow::ensure!(
            segment.len() <= AGENT_ADDRESS_SEGMENT_MAX_BYTES,
            "agent `address` '{value}' segment '{segment}' exceeds {AGENT_ADDRESS_SEGMENT_MAX_BYTES} characters"
        );
        anyhow::ensure!(
            segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "agent `address` '{value}' segment '{segment}' must match [a-z0-9-]+"
        );
        let first = segment.as_bytes()[0];
        let last = segment.as_bytes()[segment.len() - 1];
        anyhow::ensure!(
            (first.is_ascii_lowercase() || first.is_ascii_digit())
                && (last.is_ascii_lowercase() || last.is_ascii_digit()),
            "agent `address` '{value}' segment '{segment}' must begin and end with a letter or digit"
        );
    }
    Ok(())
}

impl RawTask {
    pub(crate) fn lower(
        self,
        identity: &str,
        kind: TaskKind,
        name: String,
        inherited_env: &BTreeMap<String, String>,
    ) -> anyhow::Result<Task> {
        validate_launch(
            identity,
            self.command.as_ref(),
            self.argv.as_ref(),
            &format!("{kind:?} task '{name}'"),
        )?;
        let mut env = inherited_env.clone();
        env.extend(self.env);
        let lifecycle = parse_task_lifecycle(
            identity,
            &format!("{kind:?} task '{name}'"),
            self.lifecycle.as_deref(),
        )?;
        Ok(Task {
            kind,
            derived: false,
            name,
            id: self.id,
            command: self.command,
            argv: self.argv,
            cwd: self.cwd,
            tags: self.tags,
            env,
            keep: self.keep,
            lifecycle,
        })
    }
}

fn parse_task_lifecycle(
    identity: &str,
    location: &str,
    lifecycle: Option<&str>,
) -> anyhow::Result<TaskLifecycle> {
    match lifecycle {
        None | Some("service") => Ok(TaskLifecycle::Service),
        Some("adopt-only") => Ok(TaskLifecycle::AdoptOnly),
        Some(other) => {
            anyhow::bail!("agent '{identity}' {location} has unknown lifecycle '{other}'")
        }
    }
}

fn validate_launch(
    identity: &str,
    command: Option<&String>,
    argv: Option<&Vec<String>>,
    location: &str,
) -> anyhow::Result<()> {
    if command.is_some() && argv.is_some() {
        anyhow::bail!(
            "agent '{identity}' {location} declares both `command` and `argv`; choose one launch form"
        );
    }
    if argv.is_some_and(Vec::is_empty) {
        anyhow::bail!("agent '{identity}' {location} declares an empty `argv`");
    }
    if argv.is_some_and(|argv| argv.first().is_some_and(String::is_empty)) {
        anyhow::bail!("agent '{identity}' {location} declares an empty `argv` program");
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    /// A driver block alone is an agent-shaped signal for every provider: an identity-omitting
    /// `agent.toml` whose only content is `[opencode]` (or `[pi]`) must stay a spec candidate,
    /// or path-derived discovery silently skips the seat.
    #[test]
    fn a_lone_driver_block_of_any_provider_is_a_spec_candidate() {
        for provider in ["claude", "codex", "pi", "opencode", "omp"] {
            let block = format!("[{provider}]\nprompt = \"Start the assigned work.\"");
            let raw: super::RawSpec = toml::from_str(&block).unwrap();
            assert!(raw.looks_like_spec(), "[{provider}] must look like a spec");
        }
        let raw: super::RawSpec = toml::from_str("unrelated = true").unwrap();
        assert!(!raw.looks_like_spec());
    }

    #[test]
    fn only_service_type_is_a_spec_candidate_by_itself() {
        let esm_manifest: super::RawSpec = serde_json::from_str(r#"{"type":"module"}"#).unwrap();
        assert!(!esm_manifest.looks_like_spec());

        let service: super::RawSpec = serde_json::from_str(r#"{"type":"service"}"#).unwrap();
        assert!(service.looks_like_spec());

        let unknown_type_declaration: super::RawSpec =
            serde_json::from_str(r#"{"identity":"worker","type":"module"}"#).unwrap();
        assert!(unknown_type_declaration.looks_like_spec());
    }
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("3d").unwrap(), Duration::from_secs(259200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30)); // bare = seconds
        assert!(parse_duration("nope").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn catalog_relative_uris_share_filesystem_safe_percent_decoding() {
        for uri in [
            "report..md",
            "reports/.../goal.md",
            "reports/%2Ereport.md",
            "reports/child..name/goal.md",
            "reports/with%20space/%E2%82%AC.md",
        ] {
            assert_eq!(validate_resource_uri(uri), Ok(()), "{uri}");
        }

        for uri in [
            "..",
            "../goal.md",
            "reports/../goal.md",
            "%2e%2e/goal.md",
            "reports/%2E%2e/goal.md",
            "reports%2f..%2fgoal.md",
            "reports/encoded%2Fseparator",
            "reports/encoded%5cseparator",
            "reports/a%00b",
            "reports/%FF.md",
            "reports/bad%escape",
        ] {
            assert!(validate_resource_uri(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn absolute_uri_syntax_accepts_rfc3986_characters_without_normalizing() {
        for uri in [
            "urn:isbn:978-0-395-36341-6",
            "mailto:agent@example.com",
            "https://example.com/a%20b?x=1&y=two#fragment",
            "https://[2001:db8::1]:443/a",
            "https://[v1.fe80]:443/a",
            "file:///tmp/worktree",
            "vendor+thing://authority/path;v=1",
            "data:text/plain,hello",
        ] {
            assert_eq!(validate_absolute_uri(uri), Ok(()), "{uri}");
        }
    }

    #[test]
    fn absolute_uri_syntax_rejects_forbidden_raw_characters_and_bad_escapes() {
        for uri in [
            "thing://bad\"quote",
            "thing://bad\\slash",
            "thing://bad<left",
            "thing://bad>right",
            "thing://bad^caret",
            "thing://bad`tick",
            "thing://bad{brace",
            "thing://bad|stream",
            "thing://bad}brace",
            "thing://bad space",
            "thing://bad%2",
            "thing://bad%zz",
            "thing://host:not-a-port/path",
            "thing://[2001:db8::1/path",
            "thing://[v1.bad%20body]/path",
            "thing://host/path#one#two",
            "1thing://bad-scheme",
            "./relative",
        ] {
            assert!(validate_absolute_uri(uri).is_err(), "{uri}");
        }
    }
}
