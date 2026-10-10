//! Conditions: a threshold on something the daemon measures, declared in KDL like an account,
//! that st evaluates itself and that wakes its owner only when its state changes.
//!
//! A condition names a metric, a scope (host, process, member or route), a threshold, how long it
//! must hold, and an owner. Each daemon evaluates the instances that live on it, at a fixed low
//! rate and never on the request path: a host's disks and processes, its own database and spend,
//! and its own request-latency windows. Entering breach wakes the owner once and recovery tells it
//! once. Between the two a hysteresis threshold and a hold time keep a value near the line from
//! flapping.
//!
//! Transitions of each instance are facts in the graph, a `condition.state` claim on its
//! hashed instance subject. Routine samples stay in a bounded local cache. `st doctor`,
//! `st conditions` and a person's alerts read that cache and the latest transition; later
//! graph watches or idle-wake waits can subscribe to the transition. See `docs/st3/conditions.md`.

pub mod evaluate;
pub mod probe;

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How often a daemon samples and evaluates its conditions.
pub const EVALUATE_EVERY_MS: u64 = 30_000;
/// Bounds on declaration and instance work per evaluation.
pub const MAX_CONDITIONS: usize = 32;
pub const MAX_INSTANCES: usize = 8;
pub const MAX_REMOTE_INSTANCES: usize = 256;
pub const MAX_WRITES_PER_TICK: usize = 16;
pub const COOLDOWN_MS: u128 = 5 * 60_000;
pub const STALE_AFTER_MS: u128 = 90_000;
/// How many recent samples a state claim carries.
pub const RING: usize = 8;

/// What part of the fleet a condition measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// A host's filesystems and memory.
    Host,
    /// The processes of one name on a host.
    Process,
    /// A fleet member's daemon: its database, its spend, its own CPU.
    Member,
    /// A daemon's request-latency target or route.
    Route,
}

impl Scope {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "host" => Self::Host,
            "process" => Self::Process,
            "member" => Self::Member,
            "route" => Self::Route,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Process => "process",
            Self::Member => "member",
            Self::Route => "route",
        }
    }
}

/// The metrics a condition can name. Each belongs to one scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Metric {
    /// Percent of a filesystem an unprivileged process can still use.
    DiskFreePercent,
    DiskFreeBytes,
    /// `MemAvailable` as a percent of `MemTotal`.
    MemoryAvailablePercent,
    /// Cores the named processes used between two samples, summed.
    ProcessCpuCores,
    ProcessRssBytes,
    /// The claim database file and its write-ahead log.
    DbSizeBytes,
    /// Bytes of claims this member wrote in the last 24 hours. Every member stores every claim, so
    /// this is what each member adds to every database in the fleet.
    DbAuthoredBytesPerDay,
    /// Model spend by seats on this member in the last 24 hours.
    CostUsdPerDay,
    /// The daemon's own CPU, from its request-latency windows.
    DaemonCpuCores,
    /// The share of a target's samples over target, as a multiple of the 1% a p99 target allows:
    /// 1 spends the error budget exactly as fast as it is earned.
    SloBurnRate,
    SloP99Ms,
}

const METRICS: &[(&str, Metric, Scope)] = &[
    ("disk.free-percent", Metric::DiskFreePercent, Scope::Host),
    ("disk.free-bytes", Metric::DiskFreeBytes, Scope::Host),
    (
        "memory.available-percent",
        Metric::MemoryAvailablePercent,
        Scope::Host,
    ),
    ("process.cpu-cores", Metric::ProcessCpuCores, Scope::Process),
    ("process.rss-bytes", Metric::ProcessRssBytes, Scope::Process),
    ("db.size-bytes", Metric::DbSizeBytes, Scope::Member),
    (
        "db.authored-bytes-per-day",
        Metric::DbAuthoredBytesPerDay,
        Scope::Member,
    ),
    ("cost.usd-per-day", Metric::CostUsdPerDay, Scope::Member),
    ("daemon.cpu-cores", Metric::DaemonCpuCores, Scope::Member),
    ("slo.burn-rate", Metric::SloBurnRate, Scope::Route),
    ("slo.p99-ms", Metric::SloP99Ms, Scope::Route),
];

impl Metric {
    fn parse(value: &str) -> Option<(Self, Scope)> {
        METRICS
            .iter()
            .find(|(name, ..)| *name == value)
            .map(|(_, metric, scope)| (*metric, *scope))
    }

    pub fn as_str(self) -> &'static str {
        METRICS
            .iter()
            .find(|(_, metric, _)| *metric == self)
            .map(|(name, ..)| *name)
            .expect("every metric is named")
    }

    pub fn scope(self) -> Scope {
        METRICS
            .iter()
            .find(|(_, metric, _)| *metric == self)
            .map(|(.., scope)| *scope)
            .expect("every metric has a scope")
    }

    /// Whether `window` applies: the request-latency windows are 1m, 5m and 1h.
    fn takes_window(self) -> bool {
        matches!(
            self,
            Self::SloBurnRate | Self::SloP99Ms | Self::DaemonCpuCores
        )
    }

    /// A value as a person reads it.
    pub fn describe(self, value: f64) -> String {
        match self {
            Self::DiskFreePercent | Self::MemoryAvailablePercent => format!("{}%", round(value)),
            Self::DiskFreeBytes
            | Self::ProcessRssBytes
            | Self::DbSizeBytes
            | Self::DbAuthoredBytesPerDay => bytes(value),
            Self::ProcessCpuCores | Self::DaemonCpuCores => format!("{} cores", round(value)),
            Self::CostUsdPerDay => format!("${}", round(value)),
            Self::SloBurnRate => format!("{}x budget", round(value)),
            Self::SloP99Ms => format!("{} ms", round(value)),
        }
    }
}

fn bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = value;
    let mut unit = 0;
    while value.abs() >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{} {}", round(value), UNITS[unit])
}

/// Three significant digits: what a state claim records, so noise below that does not count as a
/// change.
pub fn round(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let digits = 2 - value.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits);
    if !scale.is_finite() || scale == 0.0 || !(value * scale).is_finite() {
        return value;
    }
    (value * scale).round() / scale
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Comparison {
    Above,
    Below,
}

impl Comparison {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

/// The request-latency window a route or daemon CPU condition reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Window {
    OneMinute,
    FiveMinutes,
    OneHour,
}

impl Window {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "1m" => Self::OneMinute,
            "5m" => Self::FiveMinutes,
            "1h" => Self::OneHour,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneMinute => "1m",
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// A condition declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionDecl {
    /// The name after `condition/`.
    pub name: String,
    pub metric: Metric,
    pub scope: Scope,
    /// The hosts it applies to. Empty is every host.
    pub hosts: Vec<String>,
    /// A disk metric's filesystem, by a path on it. None is every local filesystem.
    pub path: Option<String>,
    /// A process metric's process name, as `/proc/PID/comm` shows it.
    pub process: Option<String>,
    /// A route metric's target name (`person-read`) or route (`GET /v1/client/now`).
    pub route: Option<String>,
    pub window: Window,
    pub comparison: Comparison,
    pub threshold: f64,
    /// The value it must pass back over to recover. Equal to the threshold without hysteresis.
    pub recover_at: f64,
    /// How long the threshold must be crossed before the condition enters breach.
    pub hold_ms: u128,
    /// How long it must be back past `recover_at` before it recovers.
    pub recover_hold_ms: u128,
    /// `agent/...` or `person/...`.
    pub owner: String,
    /// Where the series can be seen. `{host}` and `{instance}` are filled in.
    pub link: Option<String>,
}

fn children(node: &Value) -> impl Iterator<Item = &Value> {
    node.get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn named<'a>(node: &'a Value, name: &str) -> Vec<&'a Value> {
    children(node)
        .filter(|child| child.get("name").and_then(Value::as_str) == Some(name))
        .collect()
}

fn arguments(node: &Value) -> &[Value] {
    node.get("arguments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

const CHILDREN: &[&str] = &[
    "metric",
    "scope",
    "host",
    "path",
    "process",
    "route",
    "window",
    "above",
    "below",
    "recover",
    "for",
    "recover-for",
    "owner",
    "link",
];

/// The one string a child carries, if the child is there.
fn one_string(node: &Value, name: &str) -> Result<Option<String>, String> {
    let found = named(node, name);
    let child = match found.as_slice() {
        [] => return Ok(None),
        [child] => *child,
        _ => return Err(format!("`{name}` repeats")),
    };
    if child.get("properties").is_some() || child.get("children").is_some() {
        return Err(format!("`{name}` takes one value and nothing else"));
    }
    match arguments(child) {
        [Value::String(value)] if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(format!("`{name}` needs one string")),
    }
}

fn one_number(node: &Value, name: &str) -> Result<Option<f64>, String> {
    let found = named(node, name);
    let child = match found.as_slice() {
        [] => return Ok(None),
        [child] => *child,
        _ => return Err(format!("`{name}` repeats")),
    };
    if child.get("properties").is_some() || child.get("children").is_some() {
        return Err(format!("`{name}` takes one value and nothing else"));
    }
    match arguments(child) {
        [Value::Number(value)] => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or_else(|| format!("`{name}` needs a finite number")),
        _ => Err(format!("`{name}` needs one number")),
    }
}

fn duration(node: &Value, name: &str) -> Result<Option<u128>, String> {
    let found = named(node, name);
    let child = match found.as_slice() {
        [] => return Ok(None),
        [child] => *child,
        _ => return Err(format!("`{name}` repeats")),
    };
    match arguments(child) {
        [Value::String(value)] if value == "0" || value == "0s" => Ok(Some(0)),
        [Value::String(value)] => crate::graph::parse_duration(value, true)
            .map(|ms| Some(u128::from(ms)))
            .map_err(|error| format!("`{name}`: {}", error.message)),
        [Value::Number(value)] if value.as_u64().is_some() => {
            Ok(Some(u128::from(value.as_u64().unwrap()) * 1_000))
        }
        _ => Err(format!("`{name}` needs one duration such as \"10m\"")),
    }
}

/// A condition declaration, from its canonical desired body. The publication check calls this
/// too, so a declaration that is published is one the daemon can evaluate.
pub fn parse_condition(subject: &str, desired: &Value) -> Result<ConditionDecl, String> {
    if subject.len() > 256
        || subject.chars().any(char::is_control)
        || children(desired).take(49).count() > 48
    {
        return Err("condition name or declaration exceeds its bounded size".into());
    }
    for child in children(desired) {
        if arguments(child).iter().any(|argument| {
            argument
                .as_str()
                .is_some_and(|text| text.len() > 1024 || text.chars().any(char::is_control))
        }) {
            return Err(
                "condition strings must be at most 1024 bytes and contain no control characters"
                    .into(),
            );
        }
    }
    let name = subject
        .strip_prefix("condition/")
        .ok_or_else(|| format!("`{subject}` is not a condition"))?;
    if desired.get("properties").is_some() {
        return Err("a condition takes no properties".into());
    }
    for child in children(desired) {
        let child = child.get("name").and_then(Value::as_str).unwrap_or("");
        if !CHILDREN.contains(&child) {
            return Err(format!("a condition does not accept `{child}`"));
        }
    }
    let metric_name = one_string(desired, "metric")?.ok_or("a condition needs a `metric`")?;
    let (metric, metric_scope) = Metric::parse(&metric_name).ok_or_else(|| {
        format!(
            "unknown metric `{metric_name}`; the metrics are {}",
            METRICS
                .iter()
                .map(|(name, ..)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let scope_name = one_string(desired, "scope")?.ok_or("a condition needs a `scope`")?;
    let scope = Scope::parse(&scope_name).ok_or_else(|| {
        format!("unknown scope `{scope_name}`; use host, process, member or route")
    })?;
    if scope != metric_scope {
        return Err(format!(
            "metric `{metric_name}` is measured per {}, not per {scope_name}",
            metric_scope.as_str()
        ));
    }
    let mut hosts = Vec::new();
    for host in named(desired, "host") {
        match arguments(host) {
            [Value::String(value)] if !value.is_empty() && host.get("children").is_none() => {
                if hosts.contains(value) {
                    return Err(format!("host `{value}` is named twice"));
                }
                hosts.push(value.clone());
            }
            _ => return Err("`host` needs one host name".into()),
        }
    }
    let path = one_string(desired, "path")?;
    if let Some(path) = &path {
        if !matches!(metric, Metric::DiskFreePercent | Metric::DiskFreeBytes) {
            return Err("only a disk metric takes a `path`".into());
        }
        if !path.starts_with('/') {
            return Err(format!("path `{path}` is not absolute"));
        }
    }
    let process = one_string(desired, "process")?;
    match (scope, &process) {
        (Scope::Process, None) => return Err("a process condition needs a `process`".into()),
        (Scope::Process, Some(name)) if name.len() > 15 => {
            return Err(format!(
                "process `{name}` is longer than the 15 characters /proc/PID/comm keeps"
            ));
        }
        (Scope::Process, Some(_)) | (_, None) => {}
        (_, Some(_)) => return Err("only a process condition takes a `process`".into()),
    }
    let route = one_string(desired, "route")?;
    match (scope, &route) {
        (Scope::Route, None) => return Err("a route condition needs a `route`".into()),
        (Scope::Route, Some(_)) | (_, None) => {}
        (_, Some(_)) => return Err("only a route condition takes a `route`".into()),
    }
    let window = match one_string(desired, "window")? {
        None if metric == Metric::DaemonCpuCores => Window::FiveMinutes,
        None => Window::OneHour,
        Some(_) if !metric.takes_window() => {
            return Err(format!("metric `{metric_name}` takes no `window`"));
        }
        Some(value) => Window::parse(&value)
            .ok_or_else(|| format!("window `{value}` is not one of 1m, 5m or 1h"))?,
    };
    let (comparison, threshold) =
        match (one_number(desired, "above")?, one_number(desired, "below")?) {
            (Some(value), None) => (Comparison::Above, value),
            (None, Some(value)) => (Comparison::Below, value),
            (None, None) => {
                return Err("a condition needs a threshold: `above N` or `below N`".into());
            }
            (Some(_), Some(_)) => {
                return Err("a condition takes `above` or `below`, not both".into());
            }
        };
    let recover_at = one_number(desired, "recover")?.unwrap_or(threshold);
    match comparison {
        Comparison::Above if recover_at > threshold => {
            return Err(format!(
                "`recover {recover_at}` must be at or below the threshold {threshold}"
            ));
        }
        Comparison::Below if recover_at < threshold => {
            return Err(format!(
                "`recover {recover_at}` must be at or above the threshold {threshold}"
            ));
        }
        _ => {}
    }
    let hold_ms = duration(desired, "for")?.ok_or(
        "a condition needs `for`: how long the threshold must be crossed, such as \"10m\"",
    )?;
    let recover_hold_ms = duration(desired, "recover-for")?.unwrap_or(hold_ms);
    if hold_ms < 60_000 || recover_hold_ms < 60_000 {
        return Err("condition entry and recovery holds must be at least 60 seconds".into());
    }
    if hosts.len() > 32 {
        return Err("a condition accepts at most 32 host selectors".into());
    }
    let owner = one_string(desired, "owner")?.ok_or("a condition needs an `owner`")?;
    let owner_name = owner
        .strip_prefix("agent/")
        .or_else(|| owner.strip_prefix("person/"))
        .ok_or_else(|| format!("owner `{owner}` is not an agent/... or a person/..."))?;
    if owner_name.is_empty()
        || owner.len() > 256
        || owner_name.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        })
    {
        return Err(format!("owner `{owner}` names no one"));
    }
    let link = one_string(desired, "link")?;
    if let Some(link) = &link
        && !(link.starts_with("https://") || link.starts_with("http://"))
    {
        return Err(format!("link `{link}` is not an http(s) URL"));
    }
    Ok(ConditionDecl {
        name: name.to_owned(),
        metric,
        scope,
        hosts,
        path,
        process,
        route,
        window,
        comparison,
        threshold,
        recover_at,
        hold_ms,
        recover_hold_ms,
        owner,
        link,
    })
}

impl ConditionDecl {
    pub fn subject(&self) -> String {
        format!("condition/{}", self.name)
    }

    /// Whether this host evaluates the condition.
    pub fn applies_to(&self, host: &str) -> bool {
        self.hosts.is_empty() || self.hosts.iter().any(|name| name == host)
    }

    /// Whether `value` is past the threshold.
    pub fn breaches(&self, value: f64) -> bool {
        match self.comparison {
            Comparison::Above => value > self.threshold,
            Comparison::Below => value < self.threshold,
        }
    }

    /// Whether `value` is back past the recovery threshold.
    pub fn recovered(&self, value: f64) -> bool {
        match self.comparison {
            Comparison::Above => value <= self.recover_at,
            Comparison::Below => value >= self.recover_at,
        }
    }

    /// Where the owner can see the series of one instance.
    pub fn series_link(&self, host: &str, instance: &str) -> String {
        match &self.link {
            Some(link) => link
                .replace("{host}", &urlencoding::encode(host))
                .replace("{instance}", &urlencoding::encode(instance)),
            None => format!("st conditions show {}", self.name),
        }
    }

    /// `disk.free-percent below 15 (recovers at 18) for 10m`.
    pub fn describe_rule(&self) -> String {
        let mut rule = format!(
            "{} {} {}",
            self.metric.as_str(),
            self.comparison.as_str(),
            self.metric.describe(self.threshold)
        );
        if self.recover_at != self.threshold {
            rule.push_str(&format!(
                " (recovers at {})",
                self.metric.describe(self.recover_at)
            ));
        }
        rule.push_str(&format!(" for {}", duration_text(self.hold_ms)));
        rule
    }
}

pub fn duration_text(ms: u128) -> String {
    let seconds = ms / 1_000;
    match seconds {
        0 => "0s".into(),
        s if s.is_multiple_of(86_400) => format!("{}d", s / 86_400),
        s if s.is_multiple_of(3_600) => format!("{}h", s / 3_600),
        s if s >= 3_600 && s.is_multiple_of(60) => format!("{}h{}m", s / 3_600, s % 3_600 / 60),
        s if s >= 3_600 => format!("{}h{}m{}s", s / 3_600, s % 3_600 / 60, s % 60),
        s if s.is_multiple_of(60) => format!("{}m", s / 60),
        s if s >= 60 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// Where an instance stands. Only `Clear` to `Breach` (enter) and `Breach` to `Clear` (recover) are
/// transitions; `Pending` and `Recovering` are the hold before each.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Clear,
    /// Past the threshold, not yet for the hold time.
    Pending,
    Breach,
    /// Back past the recovery threshold, not yet for the recovery hold time.
    Recovering,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Pending => "pending",
            Self::Breach => "breach",
            Self::Recovering => "recovering",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "clear" => Self::Clear,
            "pending" => Self::Pending,
            "breach" => Self::Breach,
            "recovering" => Self::Recovering,
            _ => return None,
        })
    }

    /// Whether the owner has been told of a breach that has not recovered.
    pub fn in_breach(self) -> bool {
        matches!(self, Self::Breach | Self::Recovering)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transition {
    Enter,
    Recover,
}

impl Transition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enter => "enter",
            Self::Recover => "recover",
        }
    }
}

/// What was last written to the graph for an instance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Recorded {
    pub at: u128,
    pub phase: Phase,
    pub value: f64,
}

/// One instance's evaluation: a condition on one host, or on one filesystem of a host.
#[derive(Clone, Debug, PartialEq)]
pub struct Tracker {
    pub phase: Phase,
    /// When the current phase began.
    pub phase_since: u128,
    /// When the value first crossed the threshold for the current breach, or the pending one.
    pub breach_since: Option<u128>,
    /// The most recent samples, oldest first.
    pub values: VecDeque<(u128, f64)>,
    pub recorded: Option<Recorded>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            phase: Phase::Clear,
            phase_since: 0,
            breach_since: None,
            values: VecDeque::new(),
            recorded: None,
        }
    }
}

impl Tracker {
    /// Continue from what the graph last recorded for this instance, so a restarted daemon does
    /// not announce a breach its owner already knows of. A hold that was under way starts over.
    pub fn restore(phase: Phase, breach_since: Option<u128>, recorded: Recorded) -> Self {
        let (phase, breach_since) = match phase {
            Phase::Breach | Phase::Recovering => (Phase::Breach, breach_since),
            Phase::Clear | Phase::Pending => (Phase::Clear, None),
        };
        Self {
            phase,
            phase_since: recorded.at,
            breach_since,
            values: VecDeque::new(),
            recorded: Some(recorded),
        }
    }

    /// A missing sample cannot prove that a threshold held continuously.
    pub fn interrupt_hold(&mut self, now: u128) {
        match self.phase {
            Phase::Pending => {
                self.breach_since = None;
                self.set(Phase::Clear, now);
            }
            Phase::Recovering => self.set(Phase::Breach, now),
            _ => {}
        }
    }

    /// Fold one sample in. Returns the transition it caused, if any.
    pub fn observe(&mut self, decl: &ConditionDecl, value: f64, now: u128) -> Option<Transition> {
        if self.values.len() == RING {
            self.values.pop_front();
        }
        self.values.push_back((now, value));
        let breaching = decl.breaches(value);
        let recovered = decl.recovered(value);
        match self.phase {
            Phase::Clear
                if breaching
                    && !self.recorded.is_some_and(|recorded| {
                        recorded.phase == Phase::Clear
                            && now >= recorded.at
                            && now.saturating_sub(recorded.at) < COOLDOWN_MS
                    }) =>
            {
                self.breach_since = Some(now);
                self.set(Phase::Pending, now);
                if decl.hold_ms == 0 {
                    self.set(Phase::Breach, now);
                    return Some(Transition::Enter);
                }
            }
            Phase::Clear => {}
            Phase::Pending if !breaching => {
                self.breach_since = None;
                self.set(Phase::Clear, now);
            }
            Phase::Pending => {
                if now.saturating_sub(self.phase_since) >= decl.hold_ms {
                    self.set(Phase::Breach, now);
                    return Some(Transition::Enter);
                }
            }
            Phase::Breach if recovered => {
                self.set(Phase::Recovering, now);
                if decl.recover_hold_ms == 0 {
                    self.set(Phase::Clear, now);
                    return Some(Transition::Recover);
                }
            }
            Phase::Breach => {}
            Phase::Recovering if !recovered => self.set(Phase::Breach, now),
            Phase::Recovering => {
                if now.saturating_sub(self.phase_since) >= decl.recover_hold_ms {
                    self.set(Phase::Clear, now);
                    return Some(Transition::Recover);
                }
            }
        }
        None
    }

    fn set(&mut self, phase: Phase, now: u128) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since = now;
        }
    }

    /// Routine samples and hold phases stay local; only completed transitions replicate.
    pub fn should_record(&self, transition: Option<Transition>, _now: u128) -> bool {
        transition.is_some()
    }

    pub fn mark_recorded(&mut self, now: u128) {
        if let Some((_, value)) = self.values.back() {
            self.recorded = Some(Recorded {
                at: now,
                phase: self.phase,
                value: *value,
            });
        }
    }
}

/// The text that wakes an owner: a title and a body with the value, the threshold, since when and
/// where to see the series.
pub fn transition_text(
    decl: &ConditionDecl,
    transition: Transition,
    host: &str,
    instance: &str,
    tracker: &Tracker,
    now: u128,
) -> (String, String) {
    let value = tracker
        .values
        .back()
        .map(|(_, value)| *value)
        .unwrap_or(f64::NAN);
    let since = tracker.breach_since.unwrap_or(now);
    let place = if instance == host {
        host.to_owned()
    } else {
        instance.to_owned()
    };
    let recent = tracker
        .values
        .iter()
        .map(|(_, value)| decl.metric.describe(*value))
        .collect::<Vec<_>>()
        .join(", ");
    let (title, opening) = match transition {
        Transition::Enter => (
            format!("Condition breached: {} on {place}", decl.name),
            format!(
                "{} is {}, {} its threshold of {}, since {} ({} ago).",
                decl.metric.as_str(),
                decl.metric.describe(value),
                decl.comparison.as_str(),
                decl.metric.describe(decl.threshold),
                utc(since),
                duration_text(now.saturating_sub(since))
            ),
        ),
        Transition::Recover => (
            format!("Condition recovered: {} on {place}", decl.name),
            format!(
                "{} is back to {} after {} in breach (since {}).",
                decl.metric.as_str(),
                decl.metric.describe(value),
                duration_text(now.saturating_sub(since)),
                utc(since)
            ),
        ),
    };
    let body = format!(
        "{opening}\n\nRule: {}\nRecent values: {recent}\nSeries: {}\nSource: {} ({})\n\nst tells you once when this condition enters breach and once when it recovers.",
        decl.describe_rule(),
        decl.series_link(host, instance),
        decl.subject(),
        instance,
    );
    (bounded_text(title, 512), bounded_text(body, 3500))
}

fn bounded_text(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut boundary = limit - 3;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str("...");
    }
    text
}

pub fn utc(ms: u128) -> String {
    let seconds = i64::try_from(ms / 1_000).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Stable per-instance subjects let latest-state reads skip history using the existing
/// subject/kind index. The condition root is carried in the state fields for graph watches.
pub fn instance_subject_prefix(condition: &str) -> String {
    use sha2::Digest as _;
    format!(
        "condition-instance/{}/",
        hex::encode(sha2::Sha256::digest(breach_subject(condition).as_bytes()))
    )
}

pub fn instance_origin_prefix(condition: &str, origin: &str) -> String {
    use sha2::Digest as _;
    format!(
        "{}{}/",
        instance_subject_prefix(condition),
        hex::encode(sha2::Sha256::digest(origin.as_bytes()))
    )
}

pub fn instance_subject(condition: &str, instance: &str) -> String {
    let origin = instance
        .split_once(':')
        .map_or(instance, |(origin, _)| origin);
    instance_subject_with_origin(condition, origin, instance)
}

pub(crate) fn instance_subject_with_origin(
    condition: &str,
    origin: &str,
    instance: &str,
) -> String {
    use sha2::Digest as _;
    format!(
        "{}{}",
        instance_origin_prefix(condition, origin),
        hex::encode(sha2::Sha256::digest(instance.as_bytes()))
    )
}

/// Safe text for terminals and one-line notification titles.
pub fn display_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Verify that a state is bounded and belongs to its authenticated author. The instance
/// contains that host so another member cannot choose an instance hash belonging to it.
pub(crate) fn valid_state_identity(subject: &str, origin: &str, fields: &Value) -> bool {
    let text = |name: &str| fields.get(name).and_then(Value::as_str).unwrap_or("");
    let condition = text("condition");
    let instance = text("instance");
    text("host") == origin
        && !origin.is_empty()
        && origin.len() <= 256
        && condition.starts_with("condition/")
        && condition.len() <= 256
        && instance.len() <= 2048
        && !instance.chars().any(char::is_control)
        && !origin.chars().any(char::is_control)
        && (instance == origin
            || instance
                .strip_prefix(origin)
                .is_some_and(|suffix| suffix.starts_with(':')))
        && subject == instance_subject_with_origin(condition, origin, instance)
        && text("owner").len() <= 256
        && text("notification_title").len() <= 512
        && text("notification_body").len() <= 3500
        && fields
            .get("values")
            .and_then(Value::as_array)
            .is_none_or(|values| {
                values.len() <= RING
                    && values.iter().all(|sample| {
                        sample.as_array().is_some_and(|pair| {
                            pair.len() == 2
                                && pair[0].as_u64().is_some()
                                && pair[1].as_f64().is_some_and(f64::is_finite)
                        })
                    })
            })
}

pub fn breach_subject(condition: &str) -> String {
    if condition.starts_with("condition/") {
        condition.to_owned()
    } else {
        format!("condition/{condition}")
    }
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Summarize each declared condition using only recorded state.
pub fn doctor_lines(
    conditions: &[crate::store::ConditionView],
) -> Vec<(String, &'static str, String)> {
    doctor_lines_at(conditions, now_ms())
}

pub fn doctor_lines_at(
    conditions: &[crate::store::ConditionView],
    now: u128,
) -> Vec<(String, &'static str, String)> {
    conditions
        .iter()
        .map(|condition| {
            let stale = |instance: &crate::store::ConditionInstanceView| {
                instance.local_observation
                    && Phase::parse(&instance.phase).is_some_and(Phase::in_breach)
                    && instance.measured_at.is_some_and(|at| {
                        now < u128::from(at) || now.saturating_sub(u128::from(at)) > STALE_AFTER_MS
                    })
            };
            let status = if condition.invalid.is_some()
                || condition.instances.iter().any(|instance| {
                    Phase::parse(&instance.phase).is_some_and(Phase::in_breach) || stale(instance)
                }) {
                "warn"
            } else if condition.instances.is_empty()
                || condition
                    .instances
                    .iter()
                    .any(|instance| instance.phase == "pending")
            {
                "info"
            } else {
                "pass"
            };
            let states = condition
                .instances
                .iter()
                .map(|instance| {
                    format!(
                        "{} {}{} (value {})",
                        display_text(&instance.instance),
                        instance.phase,
                        if stale(instance) { "; stale" } else { "" },
                        instance
                            .value
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let detail = condition.invalid.clone().unwrap_or_else(|| {
                format!(
                    "{}; owner {}; {}",
                    condition.rule.as_deref().unwrap_or("unknown rule"),
                    condition.owner.as_deref().unwrap_or("unknown"),
                    if states.is_empty() {
                        "awaiting a sample"
                    } else {
                        &states
                    }
                )
            });
            (condition.subject.clone(), status, detail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(children: Value) -> Value {
        json!({"name": "condition", "arguments": ["fleet/disk"], "children": children})
    }

    fn disk() -> ConditionDecl {
        parse_condition(
            "condition/fleet/disk",
            &body(json!([
                {"name": "metric", "arguments": ["disk.free-percent"]},
                {"name": "scope", "arguments": ["host"]},
                {"name": "below", "arguments": [15]},
                {"name": "recover", "arguments": [18]},
                {"name": "for", "arguments": ["10m"]},
                {"name": "recover-for", "arguments": ["5m"]},
                {"name": "owner", "arguments": ["agent/ops"]},
            ])),
        )
        .unwrap()
    }

    const MIN: u128 = 60_000;

    #[test]
    fn a_condition_declares_a_metric_scope_threshold_hold_and_owner() {
        let decl = disk();
        assert_eq!(decl.subject(), "condition/fleet/disk");
        assert_eq!(decl.metric, Metric::DiskFreePercent);
        assert_eq!(decl.scope, Scope::Host);
        assert_eq!(decl.comparison, Comparison::Below);
        assert_eq!((decl.threshold, decl.recover_at), (15.0, 18.0));
        assert_eq!((decl.hold_ms, decl.recover_hold_ms), (10 * MIN, 5 * MIN));
        assert!(decl.applies_to("any-host"));
        assert_eq!(
            decl.describe_rule(),
            "disk.free-percent below 15% (recovers at 18%) for 10m"
        );
        assert_eq!(
            decl.series_link("alder", "alder:/"),
            "st conditions show fleet/disk"
        );
    }

    #[test]
    fn a_declaration_that_cannot_be_evaluated_is_refused() {
        let base = || {
            vec![
                json!({"name": "metric", "arguments": ["process.cpu-cores"]}),
                json!({"name": "scope", "arguments": ["process"]}),
                json!({"name": "process", "arguments": ["collector"]}),
                json!({"name": "above", "arguments": [1.5]}),
                json!({"name": "for", "arguments": ["10m"]}),
                json!({"name": "owner", "arguments": ["person/ada"]}),
            ]
        };
        let parse = |children: Vec<Value>| {
            parse_condition("condition/fleet/disk", &body(Value::Array(children)))
        };
        assert!(parse(base()).is_ok());
        let replace = |index: usize, child: Value| {
            let mut children = base();
            children[index] = child;
            parse(children).unwrap_err()
        };
        assert!(
            replace(0, json!({"name": "metric", "arguments": ["cpu"]})).contains("unknown metric")
        );
        assert!(
            replace(1, json!({"name": "scope", "arguments": ["host"]}))
                .contains("measured per process")
        );
        assert!(
            replace(2, json!({"name": "route", "arguments": ["person-read"]}))
                .contains("needs a `process`")
        );
        assert!(
            replace(
                2,
                json!({"name": "process", "arguments": ["a-process-name-too-long"]})
            )
            .contains("15 characters")
        );
        assert!(replace(4, json!({"name": "recover", "arguments": [1]})).contains("needs `for`"));
        assert!(
            replace(5, json!({"name": "owner", "arguments": ["ada"]})).contains("not an agent")
        );
        let mut both = base();
        both.push(json!({"name": "below", "arguments": [1]}));
        assert!(parse(both).unwrap_err().contains("not both"));
        let mut loose = base();
        loose.push(json!({"name": "recover", "arguments": [2]}));
        assert!(parse(loose).unwrap_err().contains("at or below"));
        let mut unknown = base();
        unknown.push(json!({"name": "severity", "arguments": ["high"]}));
        assert!(parse(unknown).unwrap_err().contains("does not accept"));
        let mut window = base();
        window.push(json!({"name": "window", "arguments": ["5m"]}));
        assert!(parse(window).unwrap_err().contains("takes no `window`"));
    }

    #[test]
    fn entering_breach_waits_for_the_hold_and_happens_once() {
        let decl = disk();
        let mut tracker = Tracker::default();
        assert_eq!(tracker.observe(&decl, 40.0, 0), None);
        assert_eq!(tracker.observe(&decl, 14.0, MIN), None);
        assert_eq!(tracker.phase, Phase::Pending);
        assert_eq!(tracker.observe(&decl, 13.0, 6 * MIN), None);
        // Ten minutes after the first crossing, not after the latest sample.
        assert_eq!(
            tracker.observe(&decl, 12.0, 11 * MIN),
            Some(Transition::Enter)
        );
        assert_eq!(tracker.phase, Phase::Breach);
        assert_eq!(tracker.breach_since, Some(MIN));
        for minute in 12..40 {
            assert_eq!(tracker.observe(&decl, 10.0, minute * MIN), None);
        }
        assert_eq!(tracker.phase, Phase::Breach);
    }

    #[test]
    fn a_crossing_shorter_than_the_hold_never_enters() {
        let decl = disk();
        let mut tracker = Tracker::default();
        tracker.observe(&decl, 14.0, 0);
        tracker.observe(&decl, 14.0, 9 * MIN);
        assert_eq!(tracker.observe(&decl, 16.0, 9 * MIN + 30_000), None);
        assert_eq!(tracker.phase, Phase::Clear);
        // The next crossing starts its hold over.
        tracker.observe(&decl, 14.0, 10 * MIN);
        assert_eq!(tracker.observe(&decl, 14.0, 19 * MIN), None);
        assert_eq!(
            tracker.observe(&decl, 14.0, 20 * MIN),
            Some(Transition::Enter)
        );
    }

    #[test]
    fn recovery_needs_the_recovery_threshold_held_and_happens_once() {
        let decl = disk();
        let mut tracker = Tracker::default();
        tracker.observe(&decl, 10.0, 0);
        assert_eq!(
            tracker.observe(&decl, 10.0, 10 * MIN),
            Some(Transition::Enter)
        );
        // Above the threshold but under the recovery line is still breach.
        assert_eq!(tracker.observe(&decl, 16.0, 11 * MIN), None);
        assert_eq!(tracker.phase, Phase::Breach);
        assert_eq!(tracker.observe(&decl, 19.0, 12 * MIN), None);
        assert_eq!(tracker.phase, Phase::Recovering);
        assert_eq!(tracker.observe(&decl, 19.0, 16 * MIN), None);
        assert_eq!(
            tracker.observe(&decl, 20.0, 17 * MIN),
            Some(Transition::Recover)
        );
        assert_eq!(tracker.phase, Phase::Clear);
        assert_eq!(tracker.observe(&decl, 30.0, 30 * MIN), None);
    }

    #[test]
    fn a_value_flapping_around_the_threshold_wakes_the_owner_once_each_way() {
        let decl = disk();
        let mut tracker = Tracker::default();
        let mut transitions = Vec::new();
        // Thirty minutes of samples alternating either side of 15%: never above 18%, so once in
        // breach it never recovers, and the alternation before that keeps resetting the hold.
        for tick in 0..60u128 {
            let value = if tick % 2 == 0 { 14.0 } else { 16.0 };
            transitions.extend(tracker.observe(&decl, value, tick * 30_000));
        }
        assert!(
            transitions.is_empty(),
            "no hold of ten minutes: {transitions:?}"
        );
        // Held under the threshold, it enters once; flapping between 14 and 17 afterwards stays in
        // breach; only a held climb past 18 recovers, once.
        for tick in 60..90u128 {
            transitions.extend(tracker.observe(&decl, 14.0, tick * 30_000));
        }
        for tick in 90..150u128 {
            let value = if tick % 2 == 0 { 14.0 } else { 17.0 };
            transitions.extend(tracker.observe(&decl, value, tick * 30_000));
        }
        for tick in 150..170u128 {
            let value = if tick % 2 == 0 { 19.0 } else { 17.0 };
            transitions.extend(tracker.observe(&decl, value, tick * 30_000));
        }
        for tick in 170..190u128 {
            transitions.extend(tracker.observe(&decl, 19.0, tick * 30_000));
        }
        assert_eq!(transitions, [Transition::Enter, Transition::Recover]);
    }

    #[test]
    fn a_zero_hold_enters_and_recovers_on_the_first_sample() {
        let mut decl = disk();
        decl.hold_ms = 0;
        decl.recover_hold_ms = 0;
        let mut tracker = Tracker::default();
        assert_eq!(tracker.observe(&decl, 1.0, 0), Some(Transition::Enter));
        assert_eq!(tracker.observe(&decl, 50.0, 1), Some(Transition::Recover));
    }

    #[test]
    fn a_restored_breach_is_not_announced_again() {
        let decl = disk();
        let recorded = Recorded {
            at: 5 * MIN,
            phase: Phase::Recovering,
            value: 18.5,
        };
        let mut tracker = Tracker::restore(Phase::Recovering, Some(MIN), recorded);
        assert_eq!(tracker.phase, Phase::Breach);
        assert_eq!(tracker.observe(&decl, 10.0, 6 * MIN), None);
        let mut pending = Tracker::restore(
            Phase::Pending,
            Some(MIN),
            Recorded {
                phase: Phase::Pending,
                ..recorded
            },
        );
        assert_eq!(pending.phase, Phase::Clear);
        assert_eq!(pending.observe(&decl, 10.0, 6 * MIN), None);
        assert_eq!(pending.phase, Phase::Pending);
    }

    #[test]
    fn routine_values_stay_local_and_only_completed_transitions_replicate() {
        let decl = disk();
        let mut tracker = Tracker::default();
        tracker.observe(&decl, 40.0, 0);
        assert!(
            !tracker.should_record(None, 0),
            "the first routine sample is local"
        );
        tracker.observe(&decl, 39.0, MIN);
        assert!(!tracker.should_record(None, MIN), "changed, but too soon");
        tracker.observe(&decl, 40.0, 6 * MIN);
        assert!(!tracker.should_record(None, 6 * MIN), "the same as written");
        tracker.observe(&decl, 41.0, 7 * MIN);
        assert!(!tracker.should_record(None, 7 * MIN));
        let transition = tracker.observe(&decl, 1.0, 8 * MIN);
        assert_eq!(transition, None);
        tracker.observe(&decl, 1.0, 18 * MIN);
        assert!(tracker.should_record(Some(Transition::Enter), 18 * MIN));
        assert_eq!(tracker.values.len(), 6);
        for minute in 19..40 {
            tracker.observe(&decl, 1.0, minute * MIN);
        }
        assert_eq!(tracker.values.len(), RING);
    }

    #[test]
    fn recovery_cooldown_prevents_repeated_entry_and_preserves_exact_values() {
        let mut decl = disk();
        decl.hold_ms = MIN;
        decl.recover_hold_ms = MIN;
        let mut tracker = Tracker::default();
        tracker.observe(&decl, 12.3456, 0);
        assert_eq!(
            tracker.observe(&decl, 12.3456, MIN),
            Some(Transition::Enter)
        );
        assert_eq!(
            tracker.values.back().map(|(_, value)| *value),
            Some(12.3456)
        );
        tracker.mark_recorded(MIN);
        tracker.observe(&decl, 30.0, 2 * MIN);
        assert_eq!(
            tracker.observe(&decl, 30.0, 3 * MIN),
            Some(Transition::Recover)
        );
        tracker.mark_recorded(3 * MIN);
        for minute in 4..8 {
            assert_eq!(tracker.observe(&decl, 12.0, minute * MIN), None);
            assert_eq!(tracker.phase, Phase::Clear);
        }
        assert_eq!(tracker.observe(&decl, 12.0, 8 * MIN), None);
        assert_eq!(tracker.phase, Phase::Pending);
        assert_eq!(
            tracker.observe(&decl, 12.0, 9 * MIN),
            Some(Transition::Enter)
        );
    }

    #[test]
    fn state_identity_is_bound_to_the_authenticated_host_and_is_bounded() {
        let mut fields = json!({"condition":"condition/fleet/disk", "host":"alder", "instance":"alder:/srv", "owner":"agent/ops", "values":[]});
        let subject = instance_subject("condition/fleet/disk", "alder:/srv");
        assert!(valid_state_identity(&subject, "alder", &fields));
        assert!(!valid_state_identity(&subject, "birch", &fields));
        fields["host"] = json!("birch");
        assert!(!valid_state_identity(&subject, "birch", &fields));
        fields["host"] = json!("alder");
        fields["values"] = json!(vec![0; 9]);
        assert!(!valid_state_identity(&subject, "alder", &fields));
    }

    #[test]
    fn declarations_reject_holds_below_one_minute() {
        for duration in ["0s", "30s"] {
            let mut desired = body(json!([
                {"name":"metric", "arguments":["disk.free-percent"]},
                {"name":"scope", "arguments":["host"]},
                {"name":"below", "arguments":[15]},
                {"name":"for", "arguments":[duration]},
                {"name":"owner", "arguments":["agent/ops"]}
            ]));
            assert!(parse_condition("condition/disk", &desired).is_err());
            desired["children"][3]["arguments"] = json!(["1m"]);
            assert!(parse_condition("condition/disk", &desired).is_ok());
        }
    }

    #[test]
    fn the_wake_names_the_value_threshold_since_and_series() {
        let mut decl = disk();
        decl.link = Some("https://observe.example/d?host={host}".into());
        let mut tracker = Tracker::default();
        tracker.observe(&decl, 14.2, 1_760_000_000_000);
        tracker.observe(&decl, 12.3456, 1_760_000_000_000 + 10 * MIN);
        let (title, body) = transition_text(
            &decl,
            Transition::Enter,
            "alder",
            "alder:/srv",
            &tracker,
            1_760_000_000_000 + 10 * MIN,
        );
        assert_eq!(title, "Condition breached: fleet/disk on alder:/srv");
        assert!(body.starts_with("disk.free-percent is 12.3%, below its threshold of 15%, since 2025-10-09 08:53 UTC (10m ago)."), "{body}");
        assert!(body.contains("Recent values: 14.2%, 12.3%"), "{body}");
        assert!(
            body.contains("Series: https://observe.example/d?host=alder"),
            "{body}"
        );
        assert!(
            body.contains("Source: condition/fleet/disk (alder:/srv)"),
            "{body}"
        );
    }

    #[test]
    fn a_backward_clock_does_not_suppress_entry_until_the_old_time() {
        let mut declaration = disk();
        declaration.hold_ms = 60_000;
        let mut tracker = Tracker::restore(
            Phase::Clear,
            None,
            Recorded {
                at: 1_000_000,
                phase: Phase::Clear,
                value: 30.0,
            },
        );
        assert_eq!(tracker.observe(&declaration, 1.0, 100), None);
        assert_eq!(tracker.phase, Phase::Pending);
        assert_eq!(
            tracker.observe(&declaration, 1.0, 60_100),
            Some(Transition::Enter)
        );
        assert_eq!(round(f64::from_bits(1)), f64::from_bits(1));
    }

    #[test]
    fn values_are_rounded_to_three_significant_digits() {
        assert_eq!(round(12.3456), 12.3);
        assert_eq!(round(0.012345), 0.0123);
        assert_eq!(round(123_456.0), 123_000.0);
        assert_eq!(round(0.0), 0.0);
        assert_eq!(
            Metric::DbSizeBytes.describe(3.5 * 1024.0 * 1024.0),
            "3.5 MiB"
        );
        assert_eq!(duration_text(90 * 60_000), "1h30m");
    }
}
