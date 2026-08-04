//! The agent job — st2's view of a rendered VRS `agent.kdl` (spec.md §2).
//!
//! A job reads like a Nomad job: the *agent* is the job, its **tasks** are `pty{}` (interactive —
//! allocates a terminal, an agent harness) and `exec{}` (a plain process — the ding, daemons, a
//! stage's script; must NOT allocate a terminal, R09). st2 reads only the runner-normative subset:
//! `identity`, presentation (`name`, `description`), `host`, `role` (metadata only), `type`,
//! `workspace`, `retired`, `keep`, `supervisor`,
//! `restart{}`, task lifecycle, Resource bindings (declaration metadata), and the tasks. Everything render-only
//! (`harness`, `model`, `persona`, `permissions`, `transport`, `strategy`, `meta{}`) is baked into
//! the tasks/commands by the render layer and ignored here.
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

/// A rendered agent job, lowered to the shared declaration fields st2 and other readers inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Unique id; the bus id is `<host>.<identity>`.
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
    /// `true` decommissions the agent (an edit, never a file delete) → torn down by reconcile.
    pub retired: bool,
    /// Agent-level GC pin: `true` exempts all of its tasks from garbage collection.
    pub keep: bool,
    /// Crash/restart policy (§4). `None` → the runner's default policy.
    pub restart: Option<Restart>,
    /// Named typed references used by the agent. st2 preserves these for readers but does not
    /// resolve them or assign launch, readiness, access, or lifecycle semantics.
    pub resources: Vec<Resource>,
    /// The runnable tasks (`pty` + `exec`), sorted by name for determinism.
    pub tasks: Vec<Task>,
    /// Where this spec was loaded from — the anchor for its resources and for edits.
    pub path: PathBuf,
}

/// One agent-local semantic binding to an externally identified resource.
///
/// `name` is the role the resource plays for this agent, `tag` selects the downstream resource
/// contract, and `uri` is the exact absolute identity. The envelope deliberately carries no policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resource {
    name: String,
    #[serde(rename = "_tag")]
    tag: String,
    uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceDescriptor {
    name: String,
    #[serde(rename = "_tag")]
    tag: String,
    uri: String,
    relation: Option<String>,
    reason: Option<String>,
}

impl Resource {
    /// Construct a descriptor after enforcing the same invariants as catalog parsing.
    pub fn new(name: String, tag: String, uri: String) -> Result<Self, String> {
        if name.is_empty() {
            return Err("resource binding name cannot be empty".into());
        }
        if tag.is_empty() {
            return Err(format!("resource binding '{name}' has an empty `_tag`"));
        }
        validate_absolute_uri(&uri).map_err(|reason| {
            format!("resource binding '{name}' `uri` must be an exact absolute URI: {reason}")
        })?;
        Ok(Self {
            name,
            tag,
            uri,
            relation: None,
            reason: None,
        })
    }

    /// Construct a descriptor with an explicit semantic relation and human-facing rationale.
    pub fn new_with_relation_reason(
        name: String,
        tag: String,
        uri: String,
        relation: String,
        reason: String,
    ) -> Result<Self, String> {
        let mut resource = Self::new(name, tag, uri)?;
        validate_relation_reason(&resource.name, &relation, &reason)?;
        resource.relation = Some(relation);
        resource.reason = Some(reason);
        Ok(resource)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn relation(&self) -> Option<&str> {
        self.relation.as_deref()
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

impl<'de> Deserialize<'de> for Resource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let descriptor = ResourceDescriptor::deserialize(deserializer)?;
        let resource = match (descriptor.relation, descriptor.reason) {
            (None, None) => Self::new(descriptor.name, descriptor.tag, descriptor.uri),
            (Some(relation), Some(reason)) => Self::new_with_relation_reason(
                descriptor.name,
                descriptor.tag,
                descriptor.uri,
                relation,
                reason,
            ),
            (Some(_), None) => Err(format!(
                "resource binding '{}' with `relation` must also declare string `reason`",
                descriptor.name
            )),
            (None, Some(_)) => Err(format!(
                "resource binding '{}' with `reason` must also declare string `relation`",
                descriptor.name
            )),
        };
        resource.map_err(de::Error::custom)
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

    /// The host that should run this spec, defaulting to `this_host` when unset.
    pub fn resolved_host<'a>(&'a self, this_host: &'a str) -> &'a str {
        self.host.as_deref().unwrap_or(this_host)
    }

    /// True once at least one authored task carries an explicit shell command or direct argv (i.e.
    /// the job was rendered). A generated sidecar cannot make an otherwise-empty job runnable.
    pub fn is_runnable(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| !task.derived && (task.command.is_some() || task.argv.is_some()))
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
    pub identity: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub host: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "type")]
    pub job_type: Option<String>,
    pub workspace: Option<String>,
    pub supervisor: Option<String>,
    #[serde(default)]
    pub retired: bool,
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
    /// Compact catalog form: reconciliation policy for the generated agent PTY.
    pub lifecycle: Option<String>,
    /// `pty "<name>" {}` / `[pty.<name>]` — interactive tasks.
    #[serde(default)]
    pub pty: BTreeMap<String, RawTask>,
    /// `exec "<name>" {}` / `[exec.<name>]` — terminal-free tasks.
    #[serde(default)]
    pub exec: BTreeMap<String, RawTask>,
}

#[derive(Debug, Default)]
pub(crate) struct RawResources(BTreeMap<String, RawResource>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawResource {
    #[serde(rename = "_tag")]
    pub(crate) tag: String,
    pub(crate) uri: String,
    pub(crate) relation: Option<String>,
    pub(crate) reason: Option<String>,
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
                match (resource.relation, resource.reason) {
                    (None, None) => Resource::new(name, resource.tag, resource.uri),
                    (Some(relation), Some(reason)) => Resource::new_with_relation_reason(
                        name, resource.tag, resource.uri, relation, reason,
                    ),
                    (Some(_), None) => Err(format!(
                        "resource binding '{name}' with `relation` must also declare string `reason`"
                    )),
                    (None, Some(_)) => Err(format!(
                        "resource binding '{name}' with `reason` must also declare string `relation`"
                    )),
                }
                .map_err(anyhow::Error::msg)
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

fn validate_relation_reason(name: &str, relation: &str, reason: &str) -> Result<(), String> {
    let relation_bytes = relation.as_bytes();
    let valid_relation = (1..=64).contains(&relation_bytes.len())
        && relation_bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && relation_bytes
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && relation_bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && !relation_bytes.windows(2).any(|pair| pair == b"--");
    if !valid_relation {
        return Err(format!(
            "resource binding '{name}' `relation` must be ASCII kebab-case of 1..64 bytes"
        ));
    }

    if reason.is_empty() || reason.len() > 160 {
        return Err(format!(
            "resource binding '{name}' `reason` must be 1..160 UTF-8 bytes"
        ));
    }
    if reason.trim() != reason
        || reason
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(format!(
            "resource binding '{name}' `reason` must have no surrounding Unicode whitespace, controls, or line separators"
        ));
    }
    Ok(())
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
                b'-'
                    | b'.'
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
    /// A parsed file is a *spec candidate* when it carries an agent-shaped signal — an identity, a
    /// `type`, or task blocks. Random TOML/JSON in the tree has none of these and is skipped.
    pub(crate) fn looks_like_spec(&self) -> bool {
        self.identity.is_some()
            || self.job_type.is_some()
            || self.command.is_some()
            || self.argv.is_some()
            || self.ding
            || !self.resource.0.is_empty()
            || !self.pty.is_empty()
            || !self.exec.is_empty()
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
        validate_launch(
            &identity,
            self.command.as_ref(),
            self.argv.as_ref(),
            "compact task",
        )?;
        if (self.command.is_some() || self.argv.is_some()) && self.pty.contains_key("agent") {
            anyhow::bail!(
                "agent '{identity}' declares both a compact launch and `pty \"agent\"`; choose one form"
            );
        }
        let bus_id = format!("{}.{}", host.as_deref().unwrap_or_default(), identity)
            .trim_start_matches('.')
            .to_string();
        let mut tasks: Vec<Task> = Vec::new();
        for (name, t) in self.pty {
            tasks.push(t.lower(&identity, TaskKind::Pty, name, &self.env)?);
        }
        for (name, t) in self.exec {
            tasks.push(t.lower(&identity, TaskKind::Exec, name, &self.env)?);
        }
        if self.command.is_some() || self.argv.is_some() {
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
                env: self.env,
                keep: false,
                lifecycle: TaskLifecycle::Service,
            });
        }
        tasks.sort_by(|a, b| a.name.cmp(&b.name));

        // `service` is the only job type; a stray `type` string is caught by validate (unknown-type).
        let job_type = JobType::Service;
        let resources = self.resource.lower()?;

        Ok(AgentSpec {
            identity,
            name: self.name,
            description: self.description,
            host,
            role: self.role,
            job_type,
            workspace: self.workspace,
            supervisor: self.supervisor,
            retired: self.retired,
            keep: self.keep,
            restart: self.restart.map(RawRestart::lower),
            resources,
            tasks,
            path,
        })
    }
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
            "thing://bad|pipe",
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
