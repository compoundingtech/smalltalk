//! Versioned, fail-closed capture boundary. Slice 1 admits typed controls only.
//! Credential values and complete logical fields remain in memory; rejected bytes are never hashed.
use serde_json::{Map, Value, json};
use std::borrow::Cow;

pub const POLICY_VERSION: u64 = 1;
pub const MAX_FIELD_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_WORK: usize = 128 * 1024;
const MAX_NODES: usize = 4_096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Producer {
    OmpChannel,
    NativeReplay,
    Driver,
    Daemon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithholdReason {
    MissingCoverage,
    Secret,
    UnsupportedEncoding,
    ScannerFailure,
    Bounds,
    UnknownField,
    PolicyVersion,
}
impl WithholdReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCoverage => "missing-coverage",
            Self::Secret => "credential",
            Self::UnsupportedEncoding => "unsupported-encoding",
            Self::ScannerFailure => "scanner-failure",
            Self::Bounds => "scan-bound",
            Self::UnknownField => "unregistered-field",
            Self::PolicyVersion => "policy-version",
        }
    }
}
pub fn withheld(reason: WithholdReason, bytes: usize) -> Value {
    // Size is deliberately not exported: even rejected length is unnecessary provenance.
    let _ = bytes;
    json!({"withheld":true,"reason":reason.as_str(),"policy_version":POLICY_VERSION})
}
fn withheld_body(event: &str, reason: WithholdReason) -> Value {
    let mut body = withheld(reason, 0);
    if event == "content" {
        body["media_type"] = json!("text/plain");
        body["text"] = json!("[withheld]");
    }
    body
}

/// Never implements Debug/Serialize; never resolves secret references or exports values.
#[derive(Default)]
pub struct SecretRegistry {
    values: Vec<String>,
    bytes: usize,
    exhausted: bool,
}
impl SecretRegistry {
    pub fn register(&mut self, value: impl Into<String>) {
        let value = value.into();
        if value.is_empty() || is_locator(&value) || self.values.contains(&value) {
            return;
        }
        if self.values.len() >= 256 || self.bytes.saturating_add(value.len()) > MAX_FIELD_BYTES {
            self.exhausted = true;
            return;
        }
        self.bytes += value.len();
        self.values.push(value);
    }
    pub fn from_existing_environment() -> Self {
        let mut registry = Self::default();
        // Value variables only. No *_FILE, profile, account, broker, or locator discovery.
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "CODEX_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GROQ_API_KEY",
            "MISTRAL_API_KEY",
            "OPENROUTER_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
        ] {
            if let Ok(value) = std::env::var(key) {
                registry.register(value);
            }
        }
        registry
    }
}
fn is_locator(value: &str) -> bool {
    ["op://", "sekrets://", "secret://", "1password://"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

enum FieldSafety {
    UncoveredText,
    Control(&'static [&'static str]),
}
/// Buffers the entire field, including chunk boundaries; releases no prefix or preview.
/// Only reviewed finite controls have coverage in policy v1; clean arbitrary prose is still withheld.
pub struct FieldScanner<'a> {
    registry: &'a SecretRegistry,
    safety: FieldSafety,
    buffer: String,
    failure: Option<WithholdReason>,
}
impl<'a> FieldScanner<'a> {
    pub fn new(registry: &'a SecretRegistry) -> Self {
        Self::with_safety(registry, FieldSafety::UncoveredText)
    }
    fn with_safety(registry: &'a SecretRegistry, safety: FieldSafety) -> Self {
        Self {
            registry,
            safety,
            buffer: String::new(),
            failure: None,
        }
    }
    /// Coverage comes only from a reviewed producer/event/field entry, never caller assertions.
    pub fn for_field(
        registry: &'a SecretRegistry,
        producer: Producer,
        event: &str,
        field: &str,
    ) -> Result<Self, WithholdReason> {
        let rule = ADMISSION_REGISTRY
            .iter()
            .find(|rule| rule.event == event && rule.producers.contains(&producer))
            .and_then(|rule| rule.fields.iter().find(|rule| rule.name == field))
            .ok_or(WithholdReason::UnknownField)?;
        match rule.field_type {
            FieldType::Enum(allowed) => {
                Ok(Self::with_safety(registry, FieldSafety::Control(allowed)))
            }
            _ => Err(WithholdReason::MissingCoverage),
        }
    }
    pub fn push(&mut self, chunk: &str) -> Result<(), WithholdReason> {
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        if self.buffer.len().saturating_add(chunk.len()) > MAX_FIELD_BYTES {
            self.buffer.clear();
            self.failure = Some(WithholdReason::Bounds);
            return Err(WithholdReason::Bounds);
        }
        if self.buffer.try_reserve(chunk.len()).is_err() {
            self.failure = Some(WithholdReason::ScannerFailure);
            return Err(WithholdReason::ScannerFailure);
        }
        self.buffer.push_str(chunk);
        Ok(())
    }
    pub fn finish(self) -> Result<(), WithholdReason> {
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        if self.registry.exhausted {
            return Err(WithholdReason::ScannerFailure);
        }
        let mut work = 0;
        scan(&self.buffer, self.registry, 0, &mut work)?;
        match self.safety {
            FieldSafety::Control(allowed) if allowed.contains(&self.buffer.as_str()) => Ok(()),
            _ => Err(WithholdReason::MissingCoverage),
        }
    }
    /// An upstream decoding/IO failure invalidates all buffered bytes, never an admitted prefix.
    pub fn fail(&mut self) {
        self.buffer.clear();
        self.failure = Some(WithholdReason::ScannerFailure);
    }
}

fn scan(
    text: &str,
    registry: &SecretRegistry,
    depth: usize,
    work: &mut usize,
) -> Result<(), WithholdReason> {
    *work = work.saturating_add(text.len());
    if depth > MAX_DEPTH || *work > MAX_WORK {
        return Err(WithholdReason::Bounds);
    }
    if registry.values.iter().any(|value| text.contains(value)) {
        return Err(WithholdReason::Secret);
    }
    let lower = if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(text.to_ascii_lowercase())
    } else {
        Cow::Borrowed(text)
    };
    if [
        "authorization",
        "bearer ",
        "password=",
        "password:",
        "token=",
        "api_key=",
        "api-key:",
        "secret=",
        "private key",
        "sk-",
        "ghp_",
        "github_pat_",
        "akia",
        "aws_secret_access_key",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return Err(WithholdReason::Secret);
    }
    if lower.lines().any(|line| {
        line.split_once('=').is_some_and(|(key, _)| {
            ["token", "password", "secret", "credential", "api_key"]
                .iter()
                .any(|suffix| key.trim_end().ends_with(suffix))
        })
    }) || lower.split_whitespace().any(|word| word == "bearer")
    {
        return Err(WithholdReason::Secret);
    }
    if let Some((_, rest)) = lower.split_once("://")
        && rest
            .split('/')
            .next()
            .is_some_and(|authority| authority.contains('@'))
    {
        return Err(WithholdReason::Secret);
    }
    // JSON escaping is decoded as a whole JSON string, not a permissive replacement.
    if text.contains('\\') {
        let wrapped = format!("\"{}\"", text.replace('"', "\\\""));
        let decoded: String =
            serde_json::from_str(&wrapped).map_err(|_| WithholdReason::UnsupportedEncoding)?;
        scan(&decoded, registry, depth + 1, work)?;
    }
    if text.contains('%') {
        let mut decoded = Vec::with_capacity(text.len());
        let bytes = text.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'%' {
                let pair = bytes
                    .get(at + 1..at + 3)
                    .ok_or(WithholdReason::UnsupportedEncoding)?;
                let hi = hex_digit(pair[0]).ok_or(WithholdReason::UnsupportedEncoding)?;
                let lo = hex_digit(pair[1]).ok_or(WithholdReason::UnsupportedEncoding)?;
                decoded.push(hi * 16 + lo);
                at += 3;
            } else {
                decoded.push(bytes[at]);
                at += 1;
            }
        }
        let decoded =
            String::from_utf8(decoded).map_err(|_| WithholdReason::UnsupportedEncoding)?;
        scan(&decoded, registry, depth + 1, work)?;
    }
    // Explicit encoding envelopes; opaque/binary decoding is unsupported and withheld.
    for (prefix, is_hex) in [("base64:", false), ("hex:", true)] {
        if let Some(encoded) = text.strip_prefix(prefix) {
            let decoded = if is_hex {
                decode_hex(encoded)?
            } else {
                decode_base64(encoded)?
            };
            let decoded =
                String::from_utf8(decoded).map_err(|_| WithholdReason::UnsupportedEncoding)?;
            scan(&decoded, registry, depth + 1, work)?;
        }
    }
    // Bare opaque encoded text cannot be admitted as a control, but scan recognizable encodings.
    if text.len() >= 8
        && text.len().is_multiple_of(2)
        && text.bytes().all(|b| hex_digit(b).is_some())
    {
        if let Ok(decoded) = String::from_utf8(decode_hex(text)?) {
            scan(&decoded, registry, depth + 1, work)?;
        }
    } else if text.len() >= 8
        && text.len().is_multiple_of(4)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        if let Ok(decoded) = decode_base64(text).and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| WithholdReason::UnsupportedEncoding)
        }) {
            scan(&decoded, registry, depth + 1, work)?;
        }
    }
    if lower.starts_with("encoding:")
        || lower.starts_with("gzip:")
        || lower.starts_with("encrypted:")
    {
        return Err(WithholdReason::UnsupportedEncoding);
    }
    Ok(())
}
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
fn decode_hex(text: &str) -> Result<Vec<u8>, WithholdReason> {
    if !text.len().is_multiple_of(2) {
        return Err(WithholdReason::UnsupportedEncoding);
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            Ok(
                hex_digit(pair[0]).ok_or(WithholdReason::UnsupportedEncoding)? * 16
                    + hex_digit(pair[1]).ok_or(WithholdReason::UnsupportedEncoding)?,
            )
        })
        .collect()
}
fn decode_base64(text: &str) -> Result<Vec<u8>, WithholdReason> {
    if !text.len().is_multiple_of(4) {
        return Err(WithholdReason::UnsupportedEncoding);
    }
    let digit = |byte| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut result = Vec::with_capacity(text.len() / 4 * 3);
    for (index, part) in text.as_bytes().chunks_exact(4).enumerate() {
        let a = digit(part[0]).ok_or(WithholdReason::UnsupportedEncoding)?;
        let b = digit(part[1]).ok_or(WithholdReason::UnsupportedEncoding)?;
        let last = index + 1 == text.len() / 4;
        if part[2] == b'=' {
            if !last || part[3] != b'=' || b & 15 != 0 {
                return Err(WithholdReason::UnsupportedEncoding);
            }
            result.push(a << 2 | b >> 4);
            continue;
        }
        let c = digit(part[2]).ok_or(WithholdReason::UnsupportedEncoding)?;
        result.push(a << 2 | b >> 4);
        result.push(b << 4 | c >> 2);
        if part[3] == b'=' {
            if !last || c & 3 != 0 {
                return Err(WithholdReason::UnsupportedEncoding);
            }
        } else {
            result.push(c << 6 | digit(part[3]).ok_or(WithholdReason::UnsupportedEncoding)?);
        }
    }
    Ok(result)
}

#[derive(Clone, Copy)]
pub enum FieldType {
    Counter,
    Number,
    Boolean,
    Enum(&'static [&'static str]),
    DeliveryFilenames,
    DeclaredAccountReference,
}
pub struct FieldRule {
    pub name: &'static str,
    pub field_type: FieldType,
    pub max_bytes: usize,
}
/// Declared account names use the graph's bounded ASCII subject-name grammar, never login paths.
pub const ACCOUNT_REF_RULE: FieldRule = FieldRule {
    name: "account_ref",
    field_type: FieldType::DeclaredAccountReference,
    max_bytes: 512,
};
pub struct EventRule {
    pub event: &'static str,
    pub event_version: u64,
    pub adapter: &'static str,
    pub adapter_version: u64,
    pub producers: &'static [Producer],
    pub fields: &'static [FieldRule],
}
pub struct AdapterRule {
    pub producer: Producer,
    pub provider: &'static str,
    pub adapter: &'static str,
    pub version: u64,
}
/// Versions name the reviewed adapter protocol, not blanket trust in any provider release.
pub const ADAPTER_REGISTRY: &[AdapterRule] = &[
    AdapterRule {
        producer: Producer::OmpChannel,
        provider: "omp",
        adapter: "omp-channel",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Driver,
        provider: "codex",
        adapter: "codex-app-server",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Driver,
        provider: "claude",
        adapter: "claude-hooks",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Driver,
        provider: "pi",
        adapter: "native-channel",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Driver,
        provider: "omp",
        adapter: "native-channel",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Driver,
        provider: "opencode",
        adapter: "opencode-events",
        version: 1,
    },
    AdapterRule {
        producer: Producer::NativeReplay,
        provider: "codex",
        adapter: "native-session",
        version: 1,
    },
    AdapterRule {
        producer: Producer::NativeReplay,
        provider: "claude",
        adapter: "native-session",
        version: 1,
    },
    AdapterRule {
        producer: Producer::NativeReplay,
        provider: "pi",
        adapter: "native-session",
        version: 1,
    },
    AdapterRule {
        producer: Producer::NativeReplay,
        provider: "omp",
        adapter: "native-session",
        version: 1,
    },
    AdapterRule {
        producer: Producer::NativeReplay,
        provider: "opencode",
        adapter: "native-session",
        version: 1,
    },
    AdapterRule {
        producer: Producer::Daemon,
        provider: "st",
        adapter: "timeline-recheck",
        version: 1,
    },
];
const PRODUCERS: &[Producer] = &[
    Producer::OmpChannel,
    Producer::NativeReplay,
    Producer::Driver,
    Producer::Daemon,
];
const STATUSES: &[&str] = &[
    "queued",
    "running",
    "waiting",
    "completed",
    "failed",
    "cancelled",
    "success",
    "error",
    "aborted",
    "unknown",
    "pending",
    "timeout",
];
const ROLES: &[&str] = &["system", "user", "assistant", "tool"];
const REASONS: &[&str] = &[
    "missing-coverage",
    "credential",
    "unsupported-encoding",
    "scanner-failure",
    "scan-bound",
    "unregistered-field",
    "policy-version",
    "sensitive-content",
    "producer-bound",
    "producer-retention",
    "retention-bound",
    "unavailable",
    "unsupported",
];
const COMMON: &[FieldRule] = &[
    FieldRule {
        name: "role",
        field_type: FieldType::Enum(ROLES),
        max_bytes: 16,
    },
    FieldRule {
        name: "status",
        field_type: FieldType::Enum(STATUSES),
        max_bytes: 16,
    },
    FieldRule {
        name: "retryable",
        field_type: FieldType::Boolean,
        max_bytes: 5,
    },
    FieldRule {
        name: "is_error",
        field_type: FieldType::Boolean,
        max_bytes: 5,
    },
    FieldRule {
        name: "duration_ms",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "timeout_seconds",
        field_type: FieldType::Number,
        max_bytes: 24,
    },
    FieldRule {
        name: "width",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "height",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "skipped_rows",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
];
const CONTENT: &[FieldRule] = &[FieldRule {
    name: "delivery_filenames",
    field_type: FieldType::DeliveryFilenames,
    max_bytes: 24,
}];
const USAGE: &[FieldRule] = &[
    FieldRule {
        name: "semantics",
        field_type: FieldType::Enum(&["response", "session_total", "context_occupancy"]),
        max_bytes: 32,
    },
    FieldRule {
        name: "driver",
        field_type: FieldType::Enum(&["codex", "claude", "omp", "pi", "opencode"]),
        max_bytes: 16,
    },
    FieldRule {
        name: "provider",
        field_type: FieldType::Enum(&["anthropic", "openai", "google", "amazon-bedrock", "azure"]),
        max_bytes: 32,
    },
    FieldRule {
        name: "currency",
        field_type: FieldType::Enum(&["USD"]),
        max_bytes: 3,
    },
    FieldRule {
        name: "input_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "output_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "cached_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "cache_write_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "cache_write_1h_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "reasoning_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "total_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "context_used_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "context_window_tokens",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "context_used_percent",
        field_type: FieldType::Number,
        max_bytes: 24,
    },
    FieldRule {
        name: "cost",
        field_type: FieldType::Number,
        max_bytes: 24,
    },
];
const NOTICES: &[FieldRule] = &[
    FieldRule {
        name: "reason",
        field_type: FieldType::Enum(REASONS),
        max_bytes: 32,
    },
    FieldRule {
        name: "withheld_items",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "omitted_from_sequence",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "omitted_to_sequence",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
    FieldRule {
        name: "skipped_rows",
        field_type: FieldType::Counter,
        max_bytes: 20,
    },
];
macro_rules! event_rule {
    ($event:literal, $fields:ident) => {
        EventRule {
            event: $event,
            event_version: 1,
            adapter: "st.normalized-timeline",
            adapter_version: 1,
            producers: PRODUCERS,
            fields: $fields,
        }
    };
}
/// No tools/jobs/reasoning/IRC/compaction/Latest bodies are registered by this slice.
pub const ADMISSION_REGISTRY: &[EventRule] = &[
    event_rule!("message", COMMON),
    event_rule!("content", CONTENT),
    event_rule!("tool_call", COMMON),
    event_rule!("tool_result", COMMON),
    event_rule!("status", COMMON),
    event_rule!("error", COMMON),
    event_rule!("usage", USAGE),
    event_rule!("redaction", NOTICES),
    event_rule!("redacted", NOTICES),
    event_rule!("truncation", NOTICES),
    event_rule!("truncated", NOTICES),
    event_rule!("unknown", NOTICES),
    event_rule!("image", COMMON),
];

fn within_bounds(value: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> bool {
    *nodes = nodes.saturating_add(1);
    if depth > MAX_DEPTH || *nodes > MAX_NODES {
        return false;
    }
    match value {
        Value::String(text) => *bytes = bytes.saturating_add(text.len()),
        Value::Object(fields) => {
            for (key, value) in fields {
                *bytes = bytes.saturating_add(key.len());
                if !within_bounds(value, depth + 1, nodes, bytes) {
                    return false;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                if !within_bounds(value, depth + 1, nodes, bytes) {
                    return false;
                }
            }
        }
        _ => *bytes = bytes.saturating_add(24),
    }
    *bytes <= MAX_FIELD_BYTES
}
fn admit_field(rule: &FieldRule, value: &Value, registry: &SecretRegistry) -> Option<Value> {
    match rule.field_type {
        FieldType::Counter => value.as_u64().map(|_| value.clone()),
        FieldType::Number => value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|_| value.clone()),
        FieldType::Boolean => value.as_bool().map(|_| value.clone()),
        FieldType::Enum(allowed) => {
            let text = value.as_str()?;
            if text.len() > rule.max_bytes {
                return None;
            }
            let mut scanner = FieldScanner::with_safety(registry, FieldSafety::Control(allowed));
            scanner.push(text).ok()?;
            scanner.finish().ok()?;
            Some(value.clone())
        }
        FieldType::DeclaredAccountReference => {
            let name = value.as_str()?;
            if name.is_empty() || name.len() > rule.max_bytes || !name.is_ascii()
                || !name.as_bytes()[0].is_ascii_alphanumeric()
                || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-@/".contains(&byte))
                || name.ends_with('/')
                || name.split('/').any(|part| part.is_empty() || part == "..")
                || registry.exhausted
            {
                return None;
            }
            scan(name, registry, 0, &mut 0).ok()?;
            Some(value.clone())
        }
        FieldType::DeliveryFilenames => {
            let names = value.as_array()?;
            if names.len() > 64 {
                return None;
            }
            let mut safe = Vec::with_capacity(names.len());
            for name in names {
                let name = name.as_str()?;
                if name.len() > rule.max_bytes || !crate::message::is_message_filename(name) {
                    return None;
                }
                scan(name, registry, 0, &mut 0).ok()?;
                safe.push(json!(name));
            }
            Some(Value::Array(safe))
        }
    }
}
pub fn sanitize_body(producer: Producer, event: &str, body: &Value) -> Value {
    sanitize_body_with_registry(
        producer,
        event,
        body,
        &SecretRegistry::from_existing_environment(),
    )
}
pub fn sanitize_body_with_registry(
    producer: Producer,
    event: &str,
    body: &Value,
    registry: &SecretRegistry,
) -> Value {
    if producer == Producer::Daemon
        && body.get("policy_version").and_then(Value::as_u64) != Some(POLICY_VERSION)
    {
        return withheld_body(event, WithholdReason::PolicyVersion);
    }
    let Some(rule) = ADMISSION_REGISTRY
        .iter()
        .find(|rule| rule.event == event && rule.producers.contains(&producer))
    else {
        return withheld_body(event, WithholdReason::UnknownField);
    };
    let (mut nodes, mut bytes) = (0, 0);
    let bounded = within_bounds(body, 0, &mut nodes, &mut bytes);
    if !body.is_object() || registry.exhausted {
        return withheld_body(event, WithholdReason::ScannerFailure);
    }
    if body.get("withheld") == Some(&Value::Bool(true))
        && body.as_object().is_some_and(|fields| fields.len() == 3)
        && let Some(reason) = body
            .get("reason")
            .and_then(Value::as_str)
            .and_then(reason_from_str)
    {
        return withheld_body(event, reason);
    }
    let mut output = Map::new();
    output.insert("policy_version".into(), json!(POLICY_VERSION));
    if body.get("withheld") == Some(&Value::Bool(true))
        && let Some(reason) = body
            .get("reason")
            .and_then(Value::as_str)
            .and_then(reason_from_str)
    {
        output.insert("withheld".into(), json!(true));
        output.insert("reason".into(), json!(reason.as_str()));
    }
    for field in rule.fields {
        if let Some(value) = body
            .get(field.name)
            .and_then(|v| admit_field(field, v, registry))
        {
            output.insert(field.name.into(), value);
        }
    }
    if event == "message"
        && let Some(id) = body.get("message_id").and_then(Value::as_str)
        && let Some(id) = generated_identifier(id, registry)
    {
        output.insert("message_id".into(), json!(id));
    }
    if !bounded {
        output.insert("withheld".into(), json!(true));
        output.insert("reason".into(), json!("scan-bound"));
    }
    if bounded && event == "content" && producer == Producer::Driver {
        let names = delivery_filenames_with_registry(body, registry);
        if !names.is_empty() {
            output.insert("delivery_filenames".into(), json!(names));
        }
    }
    match event {
        "content" => {
            output.insert("media_type".into(), json!("text/plain"));
            output.insert("text".into(), json!("[withheld]"));
            output.insert("withheld".into(), json!(true));
            output
                .entry("reason")
                .or_insert_with(|| json!("missing-coverage"));
        }
        "tool_call" => {
            output.insert(
                "arguments".into(),
                withheld(WithholdReason::MissingCoverage, 0),
            );
        }
        "tool_result" => {
            output.insert(
                "content".into(),
                withheld(WithholdReason::MissingCoverage, 0),
            );
        }
        "error" => {
            output.insert(
                "details".into(),
                withheld(WithholdReason::MissingCoverage, 0),
            );
        }
        "message" | "image" | "unknown" => {
            output.insert("withheld".into(), json!(true));
        }
        _ => {}
    }
    Value::Object(output)
}

/// OMP adapter v1: exact existing event envelopes only; arbitrary native provenance never survives.
pub fn sanitize_channel_payload(event: &str, input: &Value) -> Value {
    let registry = SecretRegistry::from_existing_environment();
    let (mut nodes, mut bytes) = (0, 0);
    if !within_bounds(input, 0, &mut nodes, &mut bytes) {
        return withheld(WithholdReason::Bounds, 0);
    }
    // This identity is minted by the extension, not copied from native provider IDs.
    let capture_id = input
        .get("capture_id")
        .and_then(Value::as_str)
        .and_then(|id| generated_identifier(id, &registry))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("source/{}", uuid::Uuid::new_v4()));
    match event {
        "message_end" => {
            let message = input.get("message").unwrap_or(input);
            let mut safe =
                sanitize_body_with_registry(Producer::OmpChannel, "message", message, &registry);
            safe["id"] = json!(capture_id);
            // Preserve existing control counters in the native shape consumed by the driver.
            if let Some(usage) = message.get("usage").filter(|u| u.is_object()) {
                let mut fields = Map::new();
                for key in [
                    "input",
                    "inputTokens",
                    "output",
                    "outputTokens",
                    "cacheRead",
                    "cacheWrite",
                    "totalTokens",
                ] {
                    if let Some(v) = usage.get(key).filter(|v| v.as_u64().is_some()) {
                        fields.insert(key.into(), v.clone());
                    }
                }
                if let Some(v) = usage
                    .get("cost")
                    .and_then(|cost| cost.get("total").or(Some(cost)))
                    .filter(|v| v.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0))
                {
                    fields.insert("cost".into(), v.clone());
                }
                safe["usage"] = Value::Object(fields);
            }
            safe["content"] = json!([{ "type":"text", "text":"[withheld]" }]);
            json!({"message":safe,"policy_version":POLICY_VERSION})
        }
        "tool_call" => {
            json!({"toolCallId":capture_id,"input":withheld(WithholdReason::MissingCoverage,0),"policy_version":POLICY_VERSION})
        }
        "tool_result" => {
            json!({"toolCallId":capture_id,"isError":input.get("isError").and_then(Value::as_bool).unwrap_or(false),"content":withheld(WithholdReason::MissingCoverage,0),"policy_version":POLICY_VERSION})
        }
        _ => withheld(WithholdReason::UnknownField, 0),
    }
}

fn reason_from_str(value: &str) -> Option<WithholdReason> {
    [
        WithholdReason::MissingCoverage,
        WithholdReason::Secret,
        WithholdReason::UnsupportedEncoding,
        WithholdReason::ScannerFailure,
        WithholdReason::Bounds,
        WithholdReason::UnknownField,
        WithholdReason::PolicyVersion,
    ]
    .into_iter()
    .find(|reason| reason.as_str() == value)
}
pub(crate) fn generated_identifier<'a>(
    value: &'a str,
    registry: &SecretRegistry,
) -> Option<&'a str> {
    let valid = ["native/", "timeline-entry/native-"].iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|index| {
            !index.is_empty()
                && index.len() <= 20
                && index.bytes().all(|b| b.is_ascii_digit())
                && index.parse::<u64>().is_ok()
        })
    }) || ["source/", "timeline-entry/", "call/"]
        .iter()
        .any(|prefix| value.strip_prefix(prefix).is_some_and(canonical_uuid));
    if !valid || registry.exhausted {
        return None;
    }
    scan(value, registry, 0, &mut 0).ok()?;
    Some(value)
}
fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}
/// For authoritative routing fences, not an admission claim about provider-supplied identities.
pub fn sanitize_identifier(value: &str) -> Option<String> {
    let registry = SecretRegistry::from_existing_environment();
    if let Some(id) = generated_identifier(value, &registry) {
        return Some(id.into());
    }
    if canonical_uuid(value) {
        scan(value, &registry, 0, &mut 0).ok()?;
        if !registry.exhausted {
            return Some(value.into());
        }
    }
    None
}
pub(crate) fn safe_routing_fence(value: &str, registry: &SecretRegistry) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !registry.exhausted
        && scan(value, registry, 0, &mut 0).is_ok()
}
/// Daemon re-enforcement of the complete timeline envelope before validation, storage, or export.
/// Native adapters must replace native identifiers first; grammar alone is not producer coverage.
pub fn sanitize_timeline_fields(fields: &Value) -> Value {
    let registry = SecretRegistry::from_existing_environment();
    let sequence = fields.get("sequence").and_then(Value::as_u64).unwrap_or(0);
    let entry_type = fields
        .get("entry_type")
        .and_then(Value::as_str)
        .filter(|event| ADMISSION_REGISTRY.iter().any(|rule| rule.event == *event))
        .unwrap_or("redaction");
    let control = |key: &str, allowed: &'static [&'static str], default: &str| -> Value {
        let rule = FieldRule {
            name: "",
            field_type: FieldType::Enum(allowed),
            max_bytes: 32,
        };
        fields
            .get(key)
            .and_then(|value| admit_field(&rule, value, &registry))
            .unwrap_or_else(|| json!(default))
    };
    let version_valid =
        fields.get("policy_version").and_then(Value::as_u64) == Some(POLICY_VERSION);
    let body = if version_valid {
        sanitize_body_with_registry(
            Producer::Daemon,
            entry_type,
            fields.get("body").unwrap_or(&Value::Null),
            &registry,
        )
    } else {
        withheld_body(entry_type, WithholdReason::PolicyVersion)
    };
    let entry_id = fields
        .get("entry_id")
        .and_then(Value::as_str)
        .and_then(|id| generated_identifier(id, &registry))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("native/{sequence}"));
    // Caller first binds this routing fence to authoritative seat/runtime state.
    let incarnation = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() <= 256 && !registry.exhausted && scan(value, &registry, 0, &mut 0).is_ok()
        })
        .map(str::to_owned)
        .unwrap_or_else(|| "withheld".into());
    let mut output = json!({
        "policy_version":POLICY_VERSION, "sequence":sequence, "entry_id":entry_id,
        "operation":control("operation", &["append","replace","finalize"], "append"),
        "revision":fields.get("revision").and_then(Value::as_u64).unwrap_or(1),
        "role":control("role",ROLES,"system"), "entry_type":entry_type,
        "final":fields.get("final").and_then(Value::as_bool).unwrap_or(true),
        "driver":control("driver",&["codex","claude","omp","pi","opencode"],"omp"),
        "incarnation_id":incarnation, "body":body,
    });
    if version_valid
        && let Some(account_ref) = fields.get(ACCOUNT_REF_RULE.name)
            .and_then(|value| admit_field(&ACCOUNT_REF_RULE, value, &registry))
    {
        if entry_type == "usage" {
            let driver = output["driver"].as_str().unwrap_or("omp");
            let reference = account_ref.as_str().unwrap();
            let label = crate::account::account_label(driver, &format!("declared:{reference}"));
            output["body"]["account"] = json!(label);
        }
        output[ACCOUNT_REF_RULE.name] = account_ref;
    }
    if version_valid
        && let Some(source) = fields
            .get("source_id")
            .and_then(Value::as_str)
            .and_then(|value| generated_identifier(value, &registry))
    {
        output["source_id"] = json!(source);
    }
    // Timestamp is an already typed clock, never raw provider text. No diagnostic/provenance refs.
    for key in ["observed_at_unix_ms", "timestamp_unix_ms"] {
        if let Some(value) = fields.get(key).filter(|v| v.as_u64().is_some()) {
            output[key] = value.clone();
        }
    }
    if version_valid && let Some(spend) = fields.get("spend") {
        let mut safe = Map::new();
        if let Some(value) = spend
            .get("cost_microusd")
            .filter(|value| value.as_u64().is_some())
        {
            safe.insert("cost_microusd".into(), value.clone());
        }
        let basis = FieldRule {
            name: "basis",
            field_type: FieldType::Enum(&["reported", "estimated", "unpriced"]),
            max_bytes: 16,
        };
        if let Some(value) = spend
            .get("basis")
            .and_then(|value| admit_field(&basis, value, &registry))
        {
            safe.insert("basis".into(), value);
        }
        if !safe.is_empty() {
            output["spend"] = Value::Object(safe);
        }
    }
    output
}

/// The context argument MUST come from daemon graph authority, never producer fields.
/// It is not a free-text coverage assertion: these are existing authenticated routing subjects.
pub fn sanitize_timeline_fields_with_context(fields: &Value, trusted_context: &Value) -> Value {
    let mut output = sanitize_timeline_fields(fields);
    let registry = SecretRegistry::from_existing_environment();
    let graph_subject = |value: &Value| -> Option<Value> {
        if value.is_null() {
            return Some(Value::Null);
        }
        let text = value.as_str()?;
        if text.is_empty()
            || text.len() > 256
            || registry.exhausted
            || !text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b':'))
        {
            return None;
        }
        scan(text, &registry, 0, &mut 0).ok()?;
        Some(value.clone())
    };
    if let Some(attribution) = trusted_context
        .get("attribution")
        .filter(|value| value.is_object())
    {
        let mut safe = Map::new();
        for key in ["agent_id", "mission_run_id", "generation_id", "step_id"] {
            if let Some(value) = attribution.get(key).and_then(graph_subject) {
                safe.insert(key.into(), value);
            }
        }
        if !safe.is_empty() {
            output["attribution"] = Value::Object(safe);
        }
    }
    if let Some(host) = trusted_context.get("host").and_then(graph_subject) {
        output["host"] = host;
    }
    output
}

/// Receipt-only reading for old/mixed local records; never exports their legacy prose.
pub fn delivery_filenames(body: &Value) -> Vec<String> {
    delivery_filenames_with_registry(body, &SecretRegistry::from_existing_environment())
}
pub fn safe_delivery_filename(value: &str) -> bool {
    let registry = SecretRegistry::from_existing_environment();
    !registry.exhausted
        && crate::message::is_message_filename(value)
        && scan(value, &registry, 0, &mut 0).is_ok()
}
fn delivery_filenames_with_registry(body: &Value, registry: &SecretRegistry) -> Vec<String> {
    let (mut nodes, mut bytes) = (0, 0);
    if registry.exhausted || !within_bounds(body, 0, &mut nodes, &mut bytes) {
        return Vec::new();
    }
    let mut names = Vec::new();
    if let Some(values) = body.get("delivery_filenames") {
        let rule = &CONTENT[0];
        if let Some(values) =
            admit_field(rule, values, registry).and_then(|value| value.as_array().cloned())
        {
            names.extend(
                values
                    .into_iter()
                    .filter_map(|value| value.as_str().map(str::to_owned)),
            );
        }
    }
    // Existing Claude prompt marker, parsed before withholding, never broadened to another format.
    if let Some(text) = body.get("text").and_then(Value::as_str) {
        for suffix in text.split("[st3-delivery:").skip(1) {
            let Some((name, _)) = suffix.split_once(']') else {
                continue;
            };
            if !crate::message::is_message_filename(name) {
                continue;
            }
            if scan(name, registry, 0, &mut 0).is_err() {
                return Vec::new();
            }
            if !names.iter().any(|value| value == name) {
                if names.len() == 64 {
                    return Vec::new();
                }
                names.push(name.into());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declared_account_references_are_typed_bounded_and_independently_rechecked() {
        let mut registry = SecretRegistry::default();
        registry.register("ada/registered-credential");
        for valid in ["ada/claude-1", "ada", "Team_1/key.@2"] {
            assert_eq!(admit_field(&ACCOUNT_REF_RULE, &json!(valid), &registry), Some(json!(valid)));
        }
        for invalid in ["", "free text", "/ada/key", "ada//key", "ada/../key", "ada/key/",
            "~/.claude", "é/key", "ada/registered-credential"] {
            assert!(admit_field(&ACCOUNT_REF_RULE, &json!(invalid), &registry).is_none());
        }
        assert!(admit_field(&ACCOUNT_REF_RULE, &json!("a".repeat(513)), &registry).is_none());
        assert!(admit_field(&ACCOUNT_REF_RULE, &json!(19), &registry).is_none());
        let envelope = |reference: &str| json!({
            "policy_version": POLICY_VERSION, "entry_type": "usage",
            "body": {"policy_version": POLICY_VERSION, "input_tokens": 17},
            "account_ref": reference,
        });
        let admitted = sanitize_timeline_fields(&envelope("ada/claude-1"));
        assert_eq!(admitted["account_ref"], "ada/claude-1");
        assert_eq!(sanitize_timeline_fields(&admitted), admitted);
        assert!(sanitize_timeline_fields(&envelope("fake credential prose")).get("account_ref").is_none());
        assert!(sanitize_timeline_fields(&json!({"account_ref":"ada/claude-1"})).get("account_ref").is_none());
    }
    fn encode_base64_fixture(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut result = String::new();
        for part in bytes.chunks(3) {
            let a = part[0];
            let b = part.get(1).copied().unwrap_or(0);
            let c = part.get(2).copied().unwrap_or(0);
            result.push(ALPHABET[(a >> 2) as usize] as char);
            result.push(ALPHABET[((a & 3) << 4 | b >> 4) as usize] as char);
            result.push(if part.len() > 1 {
                ALPHABET[((b & 15) << 2 | c >> 6) as usize] as char
            } else {
                '='
            });
            result.push(if part.len() > 2 {
                ALPHABET[(c & 63) as usize] as char
            } else {
                '='
            });
        }
        result
    }
    #[test]
    fn complete_controls_survive_but_all_free_text_and_unknown_fields_are_withheld() {
        let mut registry = SecretRegistry::default();
        registry.register("invented-credential-7");
        for secret in [
            "invented-credential-7",
            "unregistered-invented-value",
            "sk-unregistered",
            "https://u:p@host",
        ] {
            for producer in [
                Producer::Driver,
                Producer::NativeReplay,
                Producer::OmpChannel,
            ] {
                for event in [
                    "content",
                    "tool_call",
                    "tool_result",
                    "error",
                    "message",
                    "unknown",
                ] {
                    let raw = json!({"text":secret,"content":secret,"arguments":{"nested":[secret]},"diagnostic":secret,"message_id":secret,"status":"success","duration_ms":17});
                    let safe = sanitize_body_with_registry(producer, event, &raw, &registry);
                    assert!(!safe.to_string().contains(secret));
                    assert!(safe.get("sha256").is_none());
                }
            }
        }
        let controls = json!({"status":"running","retryable":false,"duration_ms":42,"diagnostic":"invented-credential-7"});
        let safe = sanitize_body_with_registry(Producer::Driver, "status", &controls, &registry);
        assert_eq!(safe["status"], "running");
        assert_eq!(safe["duration_ms"], 42);
        assert_eq!(safe["retryable"], false);
        assert!(safe.get("diagnostic").is_none());
        assert_eq!(
            sanitize_body_with_registry(Producer::Daemon, "status", &safe, &registry),
            safe
        );
        registry.register("running");
        assert!(
            sanitize_body_with_registry(Producer::Driver, "status", &controls, &registry)
                .get("status")
                .is_none()
        );
    }
    #[test]
    fn full_field_scanning_decodes_credentials_across_chunks_before_any_admission() {
        let mut registry = SecretRegistry::default();
        registry.register("fake-value");
        for encoded in [
            "fake-value",
            "fake\\u002dvalue",
            "%66ake%2dvalue",
            "base64:ZmFrZS12YWx1ZQ==",
            "hex:66616b652d76616c7565",
            "base64:aGV4OjY2NjE2YjY1MmQ3NjYxNmM3NTY1",
        ] {
            let mut scanner = FieldScanner::new(&registry);
            let split = encoded.len() / 2;
            scanner.push(&encoded[..split]).unwrap();
            scanner.push(&encoded[split..]).unwrap();
            assert_eq!(scanner.finish(), Err(WithholdReason::Secret), "{encoded}");
        }
        for unknown in [
            "authorization: hidden",
            "TOKEN=unknown",
            "ENV_SECRET = unknown",
            "Bearer\tunknown",
            "https://a:unknown@host/path",
            "sk-unregistered",
            "-----BEGIN PRIVATE KEY-----",
        ] {
            let mut scanner = FieldScanner::new(&registry);
            scanner.push(unknown).unwrap();
            assert_eq!(scanner.finish(), Err(WithholdReason::Secret));
        }
        let mut control =
            FieldScanner::for_field(&registry, Producer::Driver, "status", "status").unwrap();
        control.push("run").unwrap();
        control.push("ning").unwrap();
        assert_eq!(control.finish(), Ok(()));
        let mut text = FieldScanner::new(&registry);
        text.push("safe-looking prose").unwrap();
        assert_eq!(text.finish(), Err(WithholdReason::MissingCoverage));
        assert!(matches!(
            FieldScanner::for_field(&registry, Producer::Driver, "reasoning", "text"),
            Err(WithholdReason::UnknownField)
        ));
        assert!(matches!(
            FieldScanner::for_field(&registry, Producer::Driver, "status", "diagnostic"),
            Err(WithholdReason::UnknownField)
        ));
    }
    #[test]
    fn unsupported_encodings_failure_and_exhausted_bounds_withhold_entire_fields() {
        let registry = SecretRegistry::default();
        for text in ["gzip:opaque", "%zz", "base64:%%%", "\\q"] {
            let mut scanner = FieldScanner::new(&registry);
            scanner.push(text).unwrap();
            assert_eq!(scanner.finish(), Err(WithholdReason::UnsupportedEncoding));
        }
        let mut scanner =
            FieldScanner::for_field(&registry, Producer::Driver, "status", "status").unwrap();
        scanner.push("running").unwrap();
        scanner.fail();
        assert_eq!(scanner.finish(), Err(WithholdReason::ScannerFailure));
        let mut scanner = FieldScanner::new(&registry);
        assert_eq!(
            scanner.push(&"x".repeat(MAX_FIELD_BYTES + 1)),
            Err(WithholdReason::Bounds)
        );
        assert_eq!(scanner.finish(), Err(WithholdReason::Bounds));
        let mut nested = "running".to_owned();
        for _ in 0..MAX_DEPTH + 1 {
            nested = format!(
                "hex:{}",
                nested
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
        }
        let mut scanner = FieldScanner::new(&registry);
        scanner.push(&nested).unwrap();
        assert_eq!(scanner.finish(), Err(WithholdReason::Bounds));
        let mut nested = "x".repeat(21_000);
        for _ in 0..3 {
            nested = format!("base64:{}", encode_base64_fixture(nested.as_bytes()));
        }
        let mut scanner = FieldScanner::new(&registry);
        scanner.push(&nested).unwrap();
        assert_eq!(scanner.finish(), Err(WithholdReason::Bounds));
        let mut value = json!("fake-value");
        for _ in 0..MAX_DEPTH + 1 {
            value = json!({"nested":value});
        }
        assert_eq!(
            sanitize_body(Producer::Driver, "content", &value)["reason"],
            "scan-bound"
        );
        assert_eq!(
            sanitize_body(
                Producer::Driver,
                "content",
                &json!({"text":"x".repeat(MAX_FIELD_BYTES+1)})
            )["reason"],
            "scan-bound"
        );
        let many = json!({"arguments":vec![0;MAX_NODES+1]});
        assert_eq!(
            sanitize_body(Producer::Driver, "tool_call", &many)["reason"],
            "scan-bound"
        );
    }
    #[test]
    fn registry_exhaustion_locators_and_policy_versions_fail_closed() {
        let mut registry = SecretRegistry::default();
        registry.register("op://vault/item/credential");
        assert!(registry.values.is_empty());
        registry.register("x".repeat(MAX_FIELD_BYTES + 1));
        let mut scanner =
            FieldScanner::for_field(&registry, Producer::Driver, "status", "status").unwrap();
        scanner.push("running").unwrap();
        assert_eq!(scanner.finish(), Err(WithholdReason::ScannerFailure));
        for body in [
            json!({"status":"running"}),
            json!({"policy_version":2,"status":"running"}),
        ] {
            assert_eq!(
                sanitize_body(Producer::Daemon, "status", &body)["reason"],
                "policy-version"
            );
        }
        assert_eq!(
            sanitize_body(Producer::Driver, "reasoning", &json!({"text":"fake"}))["reason"],
            "unregistered-field"
        );
    }
    #[test]
    fn numeric_usage_and_channel_controls_retain_exact_values_not_invalid_types() {
        let safe = sanitize_body(
            Producer::NativeReplay,
            "usage",
            &json!({"input_tokens":12,"output_tokens":4,"cost":0.125,"currency":"USD","model":"unregistered-secret","total_tokens":"12","cached_tokens":-1}),
        );
        assert_eq!(safe["input_tokens"], 12);
        assert_eq!(safe["output_tokens"], 4);
        assert_eq!(safe["cost"], 0.125);
        for key in ["model", "total_tokens", "cached_tokens"] {
            assert!(safe.get(key).is_none());
        }
        let safe = sanitize_channel_payload(
            "message_end",
            &json!({"message":{"role":"assistant","id":"unregistered-secret","content":"unregistered-secret","usage":{"input":2,"output":3,"cost":0.001,"diagnostic":"unregistered-secret"}}}),
        );
        assert_eq!(safe["message"]["usage"]["input"], 2);
        assert_eq!(safe["message"]["usage"]["cost"], 0.001);
        assert!(!safe.to_string().contains("unregistered-secret"));
    }

    #[test]
    fn receipt_only_reading_merges_old_and_typed_markers_without_releasing_prompt_text() {
        let legacy = "1784649988123-abc23z.md";
        let typed = "1784649988124-def456.md";
        let body = json!({"delivery_filenames":[typed],"text":format!("[st3-delivery:{legacy}] invented-credential [st3-delivery:../../escape] [st3-delivery:{typed}]")});
        assert_eq!(
            delivery_filenames(&body),
            vec![typed.to_owned(), legacy.to_owned()]
        );
        let registry = SecretRegistry::default();
        let safe = sanitize_body_with_registry(Producer::Driver, "content", &body, &registry);
        assert_eq!(safe["delivery_filenames"], json!([typed, legacy]));
        assert!(!safe.to_string().contains("invented-credential"));
        assert_eq!(
            sanitize_body_with_registry(Producer::Daemon, "content", &safe, &registry),
            safe
        );
        let mut registry = SecretRegistry::default();
        registry.register(legacy);
        assert!(delivery_filenames_with_registry(&body, &registry).is_empty());
        let oversize =
            json!({"text":format!("[st3-delivery:{legacy}]{}","x".repeat(MAX_FIELD_BYTES))});
        assert!(delivery_filenames(&oversize).is_empty());
    }

    #[test]
    fn daemon_envelopes_are_idempotent_and_do_not_trust_provider_attribution() {
        let fields = json!({
            "policy_version":POLICY_VERSION,"entry_id":"timeline-entry/native-7","sequence":7,
            "operation":"append","revision":1,"role":"system","entry_type":"usage","final":true,
            "driver":"omp","incarnation_id":"native-pty:2026-10-06",
            "body": {"policy_version":POLICY_VERSION,"semantics":"response","input_tokens":3,"cost":0.25},
            "spend":{"cost_microusd":250_000,"basis":"reported","pricing":"invented-secret"},
            "attribution":{"agent_id":"invented-secret"},"host":"invented-secret","source_id":"invented-secret",
        });
        let safe = sanitize_timeline_fields(&fields);
        assert_eq!(safe["entry_id"], "timeline-entry/native-7");
        assert_eq!(safe["incarnation_id"], "native-pty:2026-10-06");
        assert_eq!(
            safe["spend"],
            json!({"cost_microusd":250_000,"basis":"reported"})
        );
        assert!(!safe.to_string().contains("invented-secret"));
        assert_eq!(sanitize_timeline_fields(&safe), safe);
        let trusted = json!({"attribution":{"agent_id":"agent/seat","mission_run_id":"mission-run/run","generation_id":null,"step_id":"step-run/run/build"},"host":"node.example"});
        let authorized = sanitize_timeline_fields_with_context(&fields, &trusted);
        assert_eq!(authorized["attribution"], trusted["attribution"]);
        assert_eq!(authorized["host"], trusted["host"]);
        assert_eq!(
            sanitize_timeline_fields_with_context(&authorized, &trusted),
            authorized
        );
        let old = json!({"entry_id":"invented-secret","sequence":9,"entry_type":"content","body":{"text":"invented-secret"}});
        let safe = sanitize_timeline_fields(&old);
        assert_eq!(safe["entry_id"], "native/9");
        assert_eq!(safe["body"]["reason"], "policy-version");
        assert!(!safe.to_string().contains("invented-secret"));
    }
}
