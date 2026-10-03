//! The authoritative st3 subject, resource, and claim registry.

pub mod glasses;
pub mod owned_terminals;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const SCHEMA_NAME: &str = "st3.v1";

pub const HARNESS_TODO_MAX_PHASES: usize = 16;
pub const HARNESS_TODO_MAX_TASKS: usize = 100;
pub const HARNESS_TODO_MAX_PHASE_BYTES: usize = 128;
pub const HARNESS_TODO_MAX_TEXT_BYTES: usize = 512;
pub const HARNESS_TODO_MAX_FIELDS_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTask {
    pub content: String,
    pub status: HarnessTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessPhase {
    pub name: String,
    pub tasks: Vec<HarnessTask>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTodoTotals {
    pub pending: u64,
    pub in_progress: u64,
    pub completed: u64,
    pub blocked: u64,
    #[serde(default)]
    pub abandoned: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTodoSnapshot {
    pub harness: String,
    pub session_id: String,
    pub incarnation_id: String,
    pub observed_at: String,
    pub source_op: String,
    pub phases: Vec<HarnessPhase>,
    pub totals: HarnessTodoTotals,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValueType {
    Any,
    Boolean,
    Integer,
    Number,
    String,
    Array,
    Object,
    SubjectReference,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WritePolicy {
    SystemOnly,
    SameSubjectActor,
    AuthorizedParticipant,
    AuthorizedRequester,
    CapabilityHolder,
    OrdinaryClient,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cardinality {
    Append,
    Once,
    OncePerActor,
    OncePerAttempt,
    StateTransition,
}

/// How a subagent ended: as its harness reported (`completed`, `failed`, `interrupted`), or as st
/// closed it when its lease ran out (`expired`), its parent session ended (`session-ended`), its
/// harness exited or restarted (`harness-exited`), or its seat was stopped or removed
/// (`seat-stopped`).
pub const SUBAGENT_OUTCOMES: &[&str] = &[
    "completed",
    "failed",
    "interrupted",
    "expired",
    "session-ended",
    "harness-exited",
    "seat-stopped",
];

/// Where a claim kind lives once written.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Retention {
    /// A fact in the replicated claim log, kept on every node.
    #[default]
    Durable,
    /// An observation kept only in the local observation log of the node that
    /// made it and trimmed after that node's retention window.
    Local,
    /// An observation kept in the local observation log. The replicated claim log
    /// gets a claim only when its state changes; each claim replaces the previous
    /// one for the same subject.
    Latest,
    /// `Local` when the system records it without an actor; a request or result
    /// that a person or agent writes as its actor replicates as a claim.
    SystemLocal,
}

impl Retention {
    pub fn is_durable(&self) -> bool {
        *self == Self::Durable
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FieldSpec {
    pub value_type: ValueType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default)]
    pub immutable: bool,
    #[serde(default)]
    pub reference: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_families: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubjectSpec {
    pub family: String,
    pub pattern: String,
    pub description: String,
    pub client_writable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResourceSpec {
    pub kind: String,
    pub description: String,
    pub open_facts: bool,
    pub fields: BTreeMap<String, FieldSpec>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimSpec {
    pub kind: String,
    pub subjects: Vec<String>,
    pub fields: BTreeMap<String, FieldSpec>,
    pub additional_fields: bool,
    pub write_policy: WritePolicy,
    pub cardinality: Cardinality,
    pub projection: Option<String>,
    pub wakes_reconciler: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_kdl: Vec<String>,
    #[serde(default, skip_serializing_if = "Retention::is_durable")]
    pub retention: Retention,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Registry {
    pub name: String,
    pub subjects: BTreeMap<String, SubjectSpec>,
    pub resources: BTreeMap<String, ResourceSpec>,
    pub claims: BTreeMap<String, ClaimSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

impl Registry {
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("the schema registry is serializable");
        hex::encode(Sha256::digest(bytes))
    }

    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# st3 schema registry\n\nThis file is generated from `st3-schema`.\n\nSchema: `{}`\nDigest: `{}`\n\n",
            self.name,
            self.digest()
        );
        output.push_str("## Subject families\n\n| Family | Pattern | Client writable | Description |\n|---|---|---:|---|\n");
        for spec in self.subjects.values() {
            output.push_str(&format!(
                "| `{}` | `{}` | {} | {} |\n",
                spec.family,
                spec.pattern,
                if spec.client_writable { "yes" } else { "no" },
                markdown_cell(&spec.description)
            ));
        }
        output.push_str("\nCustom subjects use `custom/NAMESPACE/NAME`. Custom claims use `custom.NAMESPACE.NAME`.\n\n");
        output.push_str("## Resource kinds\n\n| Kind | Facts | Description |\n|---|---|---|\n");
        for spec in self.resources.values() {
            output.push_str(&format!(
                "| `{}` | {} | {} |\n",
                spec.kind,
                field_summary(&spec.fields),
                markdown_cell(&spec.description)
            ));
        }
        output.push_str(
            "| `custom.NAMESPACE.NAME` | open fact bag | A namespaced custom resource. |\n\n",
        );
        output.push_str("## Claim kinds\n\n| Kind | Subjects | Write policy | Cardinality | Retention | Fields | KDL source |\n|---|---|---|---|---|---|---|\n");
        for spec in self.claims.values() {
            output.push_str(&format!(
                "| `{}` | {} | `{}` | `{}` | `{}` | {} | {} |\n",
                spec.kind,
                spec.subjects
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                enum_label(&spec.write_policy),
                enum_label(&spec.cardinality),
                enum_label(&spec.retention),
                field_summary(&spec.fields),
                spec.source_kdl
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        output.push_str("\n`resource.observed` validates facts against the resource kind. Custom resource facts remain open.\n");
        output.push_str("\nA `durable` claim is a fact in the replicated claim log. A `local` claim is an observation kept only in the local observation log of the node that made it, trimmed after that node's retention window. A `latest` claim is an observation kept in that log whose replicated claims are written only when its state changes; each one replaces the previous one for its subject. A `system-local` claim is `local` when the system records it without an actor and replicates when a person or agent writes it as its actor.\n");
        output.push_str("\n## Harness todo snapshots\n\n`harness.todo.observed` replaces the entire seat todo list. Session and incarnation identify its source; `observed_at` is source timestamp provenance, not an ordering clock. Keep the last snapshot until replaced, and expose stale provenance rather than presenting an old binding as current. Missing means unobserved; `phases: []`, zero totals and `truncated: false` means known empty.\n\nEach phase has `name` and `tasks`; each task has `content`, `status` (`pending`, `in_progress`, `completed`, `blocked`) and optional string `blocker`. The shared phase/task shape can also represent a future plan with one unnamed phase. Bounds are 16 phases, 100 tasks total, 128 UTF-8 bytes per phase name and 512 per content/blocker. Producers shorten at UTF-8 boundaries and omit trailing tasks/phases in source order to keep serialized claim fields within 64 KiB (including JSON escaping). Bound-driven shortening or omission sets `truncated`. `totals` contains nonnegative integer counts for all four statuses from the full source: counts equal the visible list when not truncated and cannot be less than visible counts when truncated. Unknown nested fields, invalid statuses, null blockers and oversized fields are rejected.\n");
        output.push_str("\nOMP's native `abandoned` tasks are omitted from phase tasks rather than relabeled as completed. Their enclosing phase is preserved when it fits. `totals.abandoned` counts these dropped tasks separately; it is optional on the wire and defaults to zero when absent. Totals for the four task statuses count the full source snapshot and exclude abandoned tasks from active progress. Dropping an abandoned task does not set `truncated`; that flag describes text/list/serialized-size bounds only. The OMP producer always emits the abandoned count and reserves 4 KiB of the serialized-fields budget for authenticated provenance.\n");
        output
    }

    pub fn subject(&self, value: &str) -> Option<&SubjectSpec> {
        let family = value.split_once('/').map(|(family, _)| family)?;
        self.subjects.get(family)
    }

    pub fn claim(&self, kind: &str) -> Option<&ClaimSpec> {
        self.claims.get(kind)
    }

    pub fn resource(&self, kind: &str) -> Option<&ResourceSpec> {
        self.resources.get(kind)
    }

    pub fn validate_subject(&self, subject: &str) -> Result<&SubjectSpec, ValidationError> {
        if subject.is_empty()
            || subject.len() > 512
            || !subject.contains('/')
            || subject.chars().any(char::is_whitespace)
            || subject.chars().any(char::is_control)
        {
            return Err(error(
                "invalid-claim-subject",
                "a subject must be a bounded full subject without whitespace",
            ));
        }
        let spec = self.subject(subject).ok_or_else(|| {
            error(
                "unknown-subject-family",
                format!("subject `{subject}` does not use a registered family"),
            )
        })?;
        let suffix = subject
            .strip_prefix(&format!("{}/", spec.family))
            .unwrap_or_default();
        if suffix.is_empty() || suffix.split('/').any(str::is_empty) {
            return Err(error(
                "invalid-claim-subject",
                "a subject needs a non-empty path after its registered family",
            ));
        }
        if spec.family == "custom" {
            let parts = subject.split('/').collect::<Vec<_>>();
            if parts.len() < 3 || !parts[1..].iter().all(|part| valid_identifier_part(part)) {
                return Err(error(
                    "invalid-custom-subject",
                    "a custom subject must use custom/NAMESPACE/NAME",
                ));
            }
        }
        if spec.family == "file" {
            let Some((host, path)) = suffix.split_once(':') else {
                return Err(error(
                    "invalid-file-subject",
                    "a file subject must use file/HOST:/ABSOLUTE_PATH",
                ));
            };
            let valid_host = host
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && host.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@')
                });
            if !valid_host
                || !path.starts_with('/')
                || path.contains("/../")
                || path.ends_with("/..")
            {
                return Err(error(
                    "invalid-file-subject",
                    "a file subject must use file/HOST:/ABSOLUTE_PATH",
                ));
            }
        }
        Ok(spec)
    }

    pub fn validate_claim(
        &self,
        subject: &str,
        kind: &str,
        fields: &BTreeMap<String, Value>,
    ) -> Result<&ClaimSpec, ValidationError> {
        let subject_spec = self.validate_subject(subject)?;
        if is_custom_claim_kind(kind) {
            if subject_spec.family != "custom" {
                return Err(error(
                    "invalid-custom-claim-subject",
                    "a custom claim requires a custom/ subject",
                ));
            }
            return Ok(custom_claim_spec());
        }
        if kind.starts_with("custom.") {
            return Err(error(
                "invalid-custom-claim-kind",
                "a custom claim must use custom.NAMESPACE.NAME",
            ));
        }
        if subject_spec.family == "custom" {
            return Err(error(
                "invalid-custom-subject-claim",
                "a custom/ subject requires a custom. claim",
            ));
        }
        let spec = self.claim(kind).ok_or_else(|| {
            error(
                "unknown-claim-kind",
                format!("claim kind `{kind}` is not registered in {SCHEMA_NAME}"),
            )
        })?;
        if !spec
            .subjects
            .iter()
            .any(|family| family == "*" || family == &subject_spec.family)
        {
            return Err(error(
                "invalid-claim-subject",
                format!("claim kind `{kind}` cannot target `{subject}`"),
            ));
        }
        for (name, field) in &spec.fields {
            if field.required && !fields.contains_key(name) {
                return Err(error(
                    "missing-claim-field",
                    format!("claim kind `{kind}` needs field `{name}`"),
                ));
            }
        }
        if !spec.additional_fields
            && let Some(name) = fields.keys().find(|name| !spec.fields.contains_key(*name))
        {
            return Err(error(
                "unknown-claim-field",
                format!("claim kind `{kind}` does not define field `{name}`"),
            ));
        }
        for (name, value) in fields {
            if let Some(field) = spec.fields.get(name) {
                validate_value(kind, name, value, field)?;
                self.validate_reference(kind, name, value, field)?;
            }
        }
        if kind == "harness.todo.observed" {
            validate_harness_todo(fields)?;
        }
        if subject_spec.family == "glass" {
            glasses::owner(subject)?;
            if !matches!(kind, "glass.upserted" | "glass.deleted") {
                return Err(error(
                    "claim-write-forbidden",
                    "a glass requires a dedicated glass claim",
                ));
            }
            if kind == "glass.upserted" {
                glasses::body_for_read(fields.get("body").unwrap_or(&Value::Null))?;
            }
        }
        Ok(spec)
    }

    pub fn validate_resource_kind(&self, kind: &str) -> Result<&ResourceSpec, ValidationError> {
        if is_custom_resource_kind(kind) {
            return Ok(custom_resource_spec());
        }
        self.resource(kind).ok_or_else(|| {
            error(
                "unknown-resource-kind",
                format!("resource kind `{kind}` is not registered"),
            )
        })
    }

    pub fn validate_resource_facts(
        &self,
        kind: &str,
        facts: &BTreeMap<String, Value>,
    ) -> Result<&ResourceSpec, ValidationError> {
        let spec = self.validate_resource_kind(kind)?;
        if !spec.open_facts
            && let Some(name) = facts.keys().find(|name| !spec.fields.contains_key(*name))
        {
            return Err(error(
                "unknown-resource-field",
                format!("resource kind `{kind}` does not define field `{name}`"),
            ));
        }
        for (name, value) in facts {
            if let Some(field) = spec.fields.get(name) {
                validate_value(kind, name, value, field)?;
                self.validate_reference(kind, name, value, field)?;
            }
        }
        Ok(spec)
    }

    fn validate_reference(
        &self,
        kind: &str,
        name: &str,
        value: &Value,
        field: &FieldSpec,
    ) -> Result<(), ValidationError> {
        if !field.reference || value.is_null() {
            return Ok(());
        }
        let reference = value.as_str().ok_or_else(|| {
            error(
                "invalid-claim-field",
                format!("claim field `{name}` on `{kind}` must be a subject reference"),
            )
        })?;
        let subject = self.validate_subject(reference).map_err(|_| {
            error(
                "invalid-subject-reference",
                format!("claim field `{name}` on `{kind}` is not a valid subject reference"),
            )
        })?;
        if !field.reference_families.is_empty()
            && !field
                .reference_families
                .iter()
                .any(|family| family == &subject.family)
        {
            return Err(error(
                "invalid-subject-reference",
                format!(
                    "claim field `{name}` on `{kind}` requires a {} subject",
                    field.reference_families.join(" or ")
                ),
            ));
        }
        Ok(())
    }

    pub fn validate_public_claim(
        &self,
        subject: &str,
        kind: &str,
        fields: &BTreeMap<String, Value>,
        actor: Option<&str>,
    ) -> Result<&ClaimSpec, ValidationError> {
        let spec = self.validate_claim(subject, kind, fields)?;
        let allowed = spec.write_policy == WritePolicy::OrdinaryClient
            || (spec.write_policy == WritePolicy::SameSubjectActor && actor == Some(subject));
        if !allowed {
            return Err(error(
                "claim-write-forbidden",
                format!("claim kind `{kind}` requires a dedicated authorized operation"),
            ));
        }
        Ok(spec)
    }
}

fn enum_label(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn field_summary(fields: &BTreeMap<String, FieldSpec>) -> String {
    if fields.is_empty() {
        return "none".into();
    }
    fields
        .iter()
        .map(|(name, spec)| {
            let required = if spec.required { "!" } else { "" };
            let immutable = if spec.immutable { " immutable" } else { "" };
            let value_type = enum_label(&spec.value_type);
            let value_type = if spec.reference_families.is_empty() {
                value_type
            } else {
                format!("{value_type}({})", spec.reference_families.join("|"))
            };
            format!("`{name}{required}:{value_type}{immutable}`",)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
}

pub fn is_known_claim(kind: &str) -> bool {
    registry().claims.contains_key(kind) || is_custom_claim_kind(kind)
}

pub fn is_custom_claim_kind(kind: &str) -> bool {
    let mut parts = kind.split('.');
    parts.next() == Some("custom") && parts.clone().count() >= 2 && parts.all(valid_identifier_part)
}

pub fn is_custom_resource_kind(kind: &str) -> bool {
    let mut parts = kind.split('.');
    parts.next() == Some("custom") && parts.clone().count() >= 2 && parts.all(valid_identifier_part)
}

fn valid_identifier_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_value(
    kind: &str,
    name: &str,
    value: &Value,
    field: &FieldSpec,
) -> Result<(), ValidationError> {
    if value.is_null() && !field.required {
        return Ok(());
    }
    let valid = match field.value_type {
        ValueType::Any => true,
        ValueType::Boolean => value.is_boolean(),
        ValueType::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
        ValueType::Number => value.is_number(),
        ValueType::String | ValueType::SubjectReference => value.is_string(),
        ValueType::Array => value.is_array(),
        ValueType::Object => value.is_object(),
    };
    if !valid {
        return Err(error(
            "invalid-claim-field",
            format!("claim field `{name}` on `{kind}` has the wrong type"),
        ));
    }
    if !field.values.is_empty()
        && !value
            .as_str()
            .is_some_and(|value| field.values.iter().any(|allowed| allowed == value))
    {
        return Err(error(
            "invalid-claim-field",
            format!("claim field `{name}` on `{kind}` has an invalid value"),
        ));
    }
    Ok(())
}

fn build_registry() -> Registry {
    let subjects = [
        (
            "account",
            "account/NAME",
            "A model account: its provider, owner, plan and where its login lives.",
            false,
        ),
        (
            "agent",
            "agent/RUN/LOCAL_ID",
            "A mission-run agent runtime.",
            false,
        ),
        (
            "attention",
            "attention/ID",
            "An explicit request for human attention.",
            true,
        ),
        (
            "checkpoint",
            "checkpoint/DAY",
            "A checkpoint that trims replicated history dated before a UTC day.",
            false,
        ),
        (
            "checkpoint-excusal",
            "checkpoint-excusal/ID",
            "A person's excusal of an unreachable writer from checkpoints.",
            false,
        ),
        (
            "custom",
            "custom/NAMESPACE/NAME",
            "An extension subject.",
            true,
        ),
        ("daemon", "daemon/NODE", "An st3 daemon.", false),
        (
            "doc",
            "doc/NAME",
            "A named immutable document lineage.",
            false,
        ),
        (
            "exec",
            "exec/RUN/LOCAL_ID",
            "A mission-run exec runtime.",
            false,
        ),
        (
            "file",
            "file/HOST:/ABSOLUTE_PATH",
            "A read-only file gate target.",
            false,
        ),
        (
            "gate-operation",
            "gate-operation/IDENTITY",
            "One gate evaluation attempt.",
            false,
        ),
        (
            "fleet-invite",
            "fleet-invite/ID",
            "A single-use fleet join invite.",
            false,
        ),
        (
            "github-post",
            "github-post/OWNER/REPO/KIND/ID",
            "A GitHub comment or review an agent seat posted, by its GitHub ID.",
            false,
        ),
        ("host", "host/NAME", "A graph host.", false),
        (
            "lane",
            "lane/RUN/LOCAL_ID",
            "A mission-run lane: an ordered line of entries its run works through front first.",
            false,
        ),
        ("message", "message/ID", "A Small Talk message.", true),
        (
            "observer",
            "observer/RUN/LOCAL_ID",
            "A mission-run resource observer.",
            false,
        ),
        ("person", "person/IDENTITY", "A human actor.", false),
        (
            "glass",
            "glass/person/NAME/UUID",
            "A private person workspace.",
            false,
        ),
        (
            "mission",
            "mission/ID",
            "An immutable mission revision lineage.",
            false,
        ),
        (
            "mission-run",
            "mission-run/ID",
            "A mission execution.",
            false,
        ),
        (
            "loop-run",
            "loop-run/GENERATION/PATH",
            "One bounded loop execution.",
            false,
        ),
        (
            "owned-set",
            "owned-set/NAME",
            "A graph-owned set of declarations with source ordering and omission retirement.",
            false,
        ),
        (
            "planning-session",
            "planning-session/ID",
            "A durable planning session.",
            false,
        ),
        (
            "pty",
            "pty/RUN/LOCAL_ID",
            "A mission-run terminal runtime.",
            false,
        ),
        (
            "resource",
            "resource/NAME",
            "An observed external or durable fact bag.",
            true,
        ),
        (
            "repair",
            "repair/ID",
            "An immutable receipt for a bounded graph or replication repair.",
            true,
        ),
        (
            "rule",
            "rule/NAME",
            "A permission rule, its mode, and the writes it audited.",
            false,
        ),
        (
            "revision-proposal",
            "revision-proposal/ID",
            "A mission revision proposal.",
            false,
        ),
        (
            "run-generation",
            "run-generation/ID",
            "An immutable mission-run generation.",
            false,
        ),
        (
            "schedule",
            "schedule/RUN/LOCAL_ID",
            "A mission-run schedule.",
            false,
        ),
        (
            "step-run",
            "step-run/GENERATION/PATH",
            "One step attempt lineage.",
            false,
        ),
        (
            "subscription",
            "subscription/RUN/LOCAL_ID",
            "A mission-run observer subscription.",
            false,
        ),
    ]
    .into_iter()
    .map(|(family, pattern, description, client_writable)| {
        (
            family.into(),
            SubjectSpec {
                family: family.into(),
                pattern: pattern.into(),
                description: description.into(),
                client_writable,
            },
        )
    })
    .collect();

    let resources = resource_specs();
    let claims = claim_specs();
    Registry {
        name: SCHEMA_NAME.into(),
        subjects,
        resources,
        claims,
    }
}

fn resource_specs() -> BTreeMap<String, ResourceSpec> {
    let mut resources = BTreeMap::new();
    resources.insert(
        "vcs.repository".into(),
        resource(
            "vcs.repository",
            "A version control repository.",
            &[
                ("url", string()),
                ("vcs", enumeration(&["git"])),
                ("default_ref", reference()),
                ("head", reference()),
                ("state", string()),
                ("pull_requests", array()),
                ("issues", array()),
                ("repository_id", integer()),
                ("github_http_requests_since_start", integer()),
            ],
        ),
    );
    resources.insert(
        "vcs.commit".into(),
        resource(
            "vcs.commit",
            "An immutable version control commit.",
            &[
                ("repository", immutable_reference()),
                ("sha", immutable_string()),
                ("tree", immutable_string()),
                ("parents", array()),
                ("author", string()),
                ("committer", string()),
                ("message", string()),
                ("committed_at", string()),
                ("url", string()),
                ("state", string()),
            ],
        ),
    );
    resources.insert(
        "vcs.ref".into(),
        resource(
            "vcs.ref",
            "A named version control reference.",
            &[
                ("repository", immutable_reference()),
                ("name", immutable_string()),
                ("target", reference()),
                ("head", string()),
                ("ancestors", array()),
                ("ref_type", enumeration(&["branch", "tag", "other"])),
                ("url", string()),
            ],
        ),
    );
    resources.insert(
        "vcs.pull-request".into(),
        resource(
            "vcs.pull-request",
            "A version control pull request.",
            &[
                ("repository", reference()),
                ("number", integer()),
                ("url", string()),
                ("title", string()),
                ("author", string()),
                ("head", reference()),
                ("head_sha", string()),
                ("branch", string()),
                ("opened_by", reference()),
                ("opened_by_run", reference()),
                ("base", reference()),
                ("base_branch", string()),
                ("state", string()),
                ("draft", boolean()),
                ("merged", boolean()),
                ("created_at", string()),
                ("updated_at", string()),
                ("checks_state", string()),
                ("checks", array()),
                ("review_decision", string()),
                ("reviews", array()),
                ("merge_queue", object()),
                ("required_checks", object()),
                ("comments", integer()),
                ("last_comment", object()),
                ("recent_comments", array()),
                ("reactions", object()),
                ("mentions", array()),
            ],
        ),
    );
    resources.insert(
        "vcs.issue".into(),
        resource(
            "vcs.issue",
            "A version control issue.",
            &[
                ("repository", reference()),
                ("number", integer()),
                ("url", string()),
                ("title", string()),
                ("author", string()),
                ("opened_by", reference()),
                ("opened_by_run", reference()),
                ("state", string()),
                ("state_reason", string()),
                ("created_at", string()),
                ("updated_at", string()),
                ("labels", array()),
                ("comments", integer()),
                ("last_comment", object()),
                ("recent_comments", array()),
                ("reactions", object()),
                ("mentions", array()),
            ],
        ),
    );
    resources.insert(
        "ci.run".into(),
        resource(
            "ci.run",
            "A continuous integration run.",
            &[
                ("repository", reference()),
                ("commit", reference()),
                ("pull_request", reference()),
                ("provider", string()),
                ("external_id", string()),
                ("url", string()),
                ("name", string()),
                (
                    "status",
                    enumeration(&["queued", "in-progress", "completed"]),
                ),
                ("conclusion", string()),
                ("started_at", string()),
                ("completed_at", string()),
            ],
        ),
    );
    resources.insert(
        "human.review".into(),
        resource(
            "human.review",
            "A human review of another graph subject.",
            &[
                ("target", reference()),
                ("document", string()),
                ("reviewer", reference()),
                (
                    "decision",
                    enumeration(&["approve", "request-changes", "comment"]),
                ),
                ("reason", string()),
                ("submitted_at", string()),
            ],
        ),
    );
    resources.insert(
        "filesystem.file".into(),
        resource(
            "filesystem.file",
            "A file observed through an explicit local path.",
            &[
                ("status", enumeration(&["ready", "missing", "unreadable"])),
                ("path", immutable_string()),
                ("content_hash", string()),
                ("size", integer()),
                ("mode", integer()),
                ("reason", string()),
            ],
        ),
    );
    resources.insert(
        "harness.session-file".into(),
        resource(
            "harness.session-file",
            "A harness session file that can outlive one runtime incarnation.",
            &[
                ("harness", immutable_string()),
                ("path", string()),
                ("session_id", string()),
                ("agent", reference()),
                ("incarnation_id", string()),
                (
                    "status",
                    enumeration(&["active", "inactive", "missing", "unknown"]),
                ),
                ("modified_at", string()),
            ],
        ),
    );
    resources
}

fn claim_specs() -> BTreeMap<String, ClaimSpec> {
    type ClaimDefinition<'a> = (
        &'a str,
        &'a [&'a str],
        WritePolicy,
        Cardinality,
        Option<&'a str>,
        bool,
        &'a [&'a str],
    );

    let mut claims = BTreeMap::new();
    let definitions: &[ClaimDefinition<'_>] = &[
        (
            "harness.todo.observed",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            None,
            false,
            &[],
        ),
        (
            "agent.account",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::StateTransition,
            Some("agents"),
            false,
            &[],
        ),
        (
            "attention.requested",
            &["attention"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Once,
            Some("attention"),
            false,
            &[],
        ),
        (
            "attention.resolved",
            &["attention"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Once,
            Some("attention"),
            false,
            &[],
        ),
        (
            "intent.desired",
            &["*"],
            WritePolicy::AuthorizedRequester,
            Cardinality::StateTransition,
            Some("desired"),
            true,
            &[
                "account",
                "agent",
                "doc",
                "exec",
                "host",
                "lane",
                "message",
                "observer",
                "mission",
                "mission-run",
                "planning-session",
                "pty",
                "resource",
                "schedule",
                "step",
                "stop",
                "subscription",
            ],
        ),
        (
            "glass.upserted",
            &["glass"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("glasses"),
            false,
            &[],
        ),
        (
            "glass.deleted",
            &["glass"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("glasses"),
            false,
            &[],
        ),
        (
            "doc.bound",
            &["doc"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("documents"),
            true,
            &["doc"],
        ),
        (
            "owned-set.revised",
            &["owned-set"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("owned-sets"),
            true,
            &[],
        ),
        (
            "mission.published",
            &["mission"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("missions"),
            true,
            &["mission"],
        ),
        (
            "mission.produced",
            &["mission", "step-run"],
            WritePolicy::CapabilityHolder,
            Cardinality::Append,
            Some("mission-products"),
            true,
            &["produces"],
        ),
        (
            "mission-run.created",
            &["mission-run"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("mission-runs"),
            true,
            &["mission-run"],
        ),
        (
            "mission-run.state",
            &["mission-run"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("mission-runs"),
            true,
            &["mission-run", "completion", "finally", "cancellation"],
        ),
        (
            "loop.round-result",
            &["loop-run"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("loops"),
            true,
            &["loop", "round"],
        ),
        (
            "loop.round-dispatch",
            &["loop-run"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("loops"),
            true,
            &["loop", "round"],
        ),
        (
            "loop.state",
            &["loop-run"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("loops"),
            true,
            &["loop"],
        ),
        (
            "publication.operation",
            &["*"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            None,
            true,
            &["revision", "reset", "cancellation", "refresh", "feedback"],
        ),
        (
            "run-generation.created",
            &["run-generation"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("run-generations"),
            true,
            &["mission-run", "revision"],
        ),
        (
            "run-generation.state",
            &["run-generation"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("run-generations"),
            true,
            &[
                "mission-run",
                "step",
                "completion",
                "finally",
                "revision",
                "cancellation",
            ],
        ),
        (
            "run-generation.superseded",
            &["run-generation"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("run-generations"),
            true,
            &["revision"],
        ),
        (
            "step-run.state",
            &["step-run"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("step-runs"),
            true,
            &["step"],
        ),
        (
            "step-run.retried",
            &["step-run"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("step-runs"),
            true,
            &["step"],
        ),
        (
            "step-run.carried",
            &["step-run"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("step-runs"),
            true,
            &["step"],
        ),
        (
            "work.person-asked",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.person-done",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.person-cancelled",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.claimed",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::StateTransition,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.renewed",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.progress",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.submitted",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::OncePerAttempt,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.failed",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::OncePerAttempt,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.released",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "work.extended",
            &["step-run"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("work"),
            true,
            &[],
        ),
        (
            "gate.requested",
            &["gate-operation"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("gates"),
            true,
            &["gate"],
        ),
        (
            "gate.result",
            &["gate-operation"],
            WritePolicy::CapabilityHolder,
            Cardinality::Append,
            Some("gates"),
            true,
            &["gate"],
        ),
        (
            "eval.verdict",
            &["mission-run"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("evals"),
            true,
            &[],
        ),
        (
            "file.observed",
            &["file"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("files"),
            true,
            &["gate"],
        ),
        (
            "resource.observed",
            &["resource"],
            WritePolicy::OrdinaryClient,
            Cardinality::Append,
            Some("resources"),
            true,
            &["resource"],
        ),
        (
            "observer.observed",
            &["observer"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("observers"),
            true,
            &["observer"],
        ),
        (
            "observer.refresh-requested",
            &["observer"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("observers"),
            true,
            &["refresh"],
        ),
        (
            "observer.state",
            &["observer"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("observers"),
            true,
            &["observer"],
        ),
        (
            "subscription.state",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::StateTransition,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "daemon.started",
            &["daemon"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("daemons"),
            true,
            &["reset"],
        ),
        (
            "daemon.diagnostic",
            &["daemon"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("daemons"),
            true,
            &[],
        ),
        (
            "runtime.action.requested",
            &["agent", "exec", "pty", "gate-operation"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("runtime-actions"),
            true,
            &["stop", "gate"],
        ),
        (
            "runtime.action.succeeded",
            &["agent", "exec", "pty", "gate-operation"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtime-actions"),
            true,
            &["stop", "gate"],
        ),
        (
            "runtime.action.failed",
            &["agent", "exec", "pty", "gate-operation"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtime-actions"),
            true,
            &["stop", "gate"],
        ),
        (
            "runtime.action.deadline-reached",
            &["agent", "exec", "pty", "gate-operation"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtime-actions"),
            true,
            &["stop", "gate"],
        ),
        (
            "runtime.observed",
            &["agent", "exec", "pty", "gate-operation"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("runtimes"),
            true,
            &[],
        ),
        (
            "workspace.observed",
            &["agent"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            None,
            true,
            &[],
        ),
        (
            "render.applied",
            &["agent", "exec", "pty"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtimes"),
            false,
            &[],
        ),
        (
            "runtime.restart-window-reset",
            &["agent", "exec", "pty"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtimes"),
            true,
            &["reset"],
        ),
        (
            "runtime.reconcile-decision",
            &["agent", "exec", "pty", "schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtimes"),
            true,
            &[],
        ),
        (
            "runtime.readiness-deadline-reached",
            &["agent"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("runtimes"),
            true,
            &[],
        ),
        (
            "delivery.hold",
            &["agent"],
            WritePolicy::AuthorizedRequester,
            Cardinality::StateTransition,
            Some("delivery-holds"),
            false,
            &[],
        ),
        (
            "agent.presence",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("agents"),
            true,
            &[],
        ),
        (
            "agent.queue.moved",
            &["agent"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("agents"),
            true,
            &[],
        ),
        (
            "lane.approved",
            &["lane"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("lanes"),
            false,
            &[],
        ),
        (
            "lane.joined",
            &["lane"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("lanes"),
            false,
            &[],
        ),
        (
            "lane.left",
            &["lane"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("lanes"),
            false,
            &[],
        ),
        (
            "lane.marked",
            &["lane"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("lanes"),
            false,
            &[],
        ),
        (
            "lane.moved",
            &["lane"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("lanes"),
            false,
            &[],
        ),
        (
            "harness.observed",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("harnesses"),
            true,
            &[],
        ),
        (
            "harness.session-file",
            &["agent"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("harnesses"),
            false,
            &[],
        ),
        (
            "harness.diagnostic",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("harnesses"),
            false,
            &[],
        ),
        (
            "harness.usage",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("harnesses"),
            false,
            &[],
        ),
        (
            "harness.limits",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("harnesses"),
            false,
            &[],
        ),
        // A subagent a seat's harness started, its lease renewals while it runs, and its end.
        // The parent seat records them as itself; the reconciler on the node that recorded an
        // appearance ends a subagent whose lease ran out or whose seat went away.
        (
            "subagent.appeared",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("subagents"),
            false,
            &[],
        ),
        (
            "subagent.renewed",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("subagents"),
            false,
            &[],
        ),
        (
            "subagent.ended",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("subagents"),
            false,
            &[],
        ),
        (
            "harness.timeline",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            Some("harnesses"),
            false,
            &[],
        ),
        (
            "harness.telemetry",
            &["agent"],
            WritePolicy::SameSubjectActor,
            Cardinality::Append,
            None,
            false,
            &[],
        ),
        (
            "harness.context-clear.requested",
            &["agent"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("harness-control"),
            true,
            &[],
        ),
        (
            "harness.context-clear.result",
            &["agent"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("harness-control"),
            true,
            &[],
        ),
        (
            "terminal.input.requested",
            &["agent", "pty"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("terminal-control"),
            true,
            &[],
        ),
        (
            "terminal.input.result",
            &["agent", "pty"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("terminal-control"),
            true,
            &[],
        ),
        (
            "message.sent",
            &["message"],
            WritePolicy::OrdinaryClient,
            Cardinality::Once,
            Some("messages"),
            true,
            &["message"],
        ),
        (
            "message.staged",
            &["message"],
            WritePolicy::SystemOnly,
            Cardinality::OncePerActor,
            Some("messages"),
            true,
            &["message"],
        ),
        (
            "message.delivered",
            &["message"],
            WritePolicy::SystemOnly,
            Cardinality::OncePerActor,
            Some("messages"),
            true,
            &["message"],
        ),
        (
            "message.read",
            &["message"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::OncePerActor,
            Some("messages"),
            true,
            &[],
        ),
        (
            "message.closed",
            &["message"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::OncePerActor,
            Some("messages"),
            true,
            &[],
        ),
        (
            "planning-session.started",
            &["planning-session"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Once,
            Some("planning"),
            true,
            &["planning-session"],
        ),
        (
            "planning-session.candidate-submitted",
            &["planning-session"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("planning"),
            true,
            &[],
        ),
        (
            "planning-session.previewed",
            &["planning-session"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("planning"),
            true,
            &[],
        ),
        (
            "planning-session.revision-requested",
            &["planning-session"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("planning"),
            true,
            &["feedback"],
        ),
        (
            "planning-session.approved",
            &["planning-session"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Once,
            Some("planning"),
            true,
            &[],
        ),
        (
            "planning-session.cancelled",
            &["planning-session"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Once,
            Some("planning"),
            true,
            &["cancellation"],
        ),
        (
            "planning-session.question-requested",
            &["planning-session"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("planning"),
            true,
            &[],
        ),
        (
            "planning-session.question-answered",
            &["planning-session"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Append,
            Some("planning"),
            true,
            &[],
        ),
        (
            "revision-proposal.created",
            &["revision-proposal"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Once,
            Some("revision-proposals"),
            true,
            &[],
        ),
        (
            "revision-proposal.approved",
            &["revision-proposal"],
            WritePolicy::AuthorizedRequester,
            Cardinality::OncePerActor,
            Some("revision-proposals"),
            true,
            &[],
        ),
        (
            "revision-proposal.cancelled",
            &["revision-proposal"],
            WritePolicy::AuthorizedRequester,
            Cardinality::Once,
            Some("revision-proposals"),
            true,
            &[],
        ),
        (
            "revision-proposal.applied",
            &["revision-proposal"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("revision-proposals"),
            true,
            &[],
        ),
        (
            "schedule.occurrence-scheduled",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "schedule.occurrence-reached",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "schedule.occurrence-cancelled",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "schedule.work-requested",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "schedule.work-started",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "schedule.work-failed",
            &["schedule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("schedules"),
            true,
            &["schedule"],
        ),
        (
            "subscription.mission-requested",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.mission-request-cancelled",
            &["subscription"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.mission-request-released",
            &["subscription"],
            WritePolicy::AuthorizedParticipant,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.mission-started",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.batched",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "github.posted",
            &["github-post"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("subscriptions"),
            true,
            &[],
        ),
        (
            "subscription.watch-ended",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.batch-sent",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.mission-deferred",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "subscription.mission-failed",
            &["subscription"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("subscriptions"),
            true,
            &["subscription"],
        ),
        (
            "fleet.invite-created",
            &["fleet-invite"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.invite-redeemed",
            &["fleet-invite"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.invite-revoked",
            &["fleet-invite"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.member-admitted",
            &["host"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.member-endpoints",
            &["host"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.member-left",
            &["host"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "fleet.member-removed",
            &["host"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("fleet"),
            false,
            &[],
        ),
        (
            "rule.set",
            &["rule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("rules"),
            false,
            &[],
        ),
        (
            "rule.audited",
            &["rule"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("rules"),
            false,
            &[],
        ),
        (
            "principal.key-granted",
            &["person", "agent"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("principals"),
            false,
            &[],
        ),
        (
            "principal.key-revoked",
            &["person", "agent"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("principals"),
            false,
            &[],
        ),
        (
            "transport.observed",
            &["host"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("transport"),
            true,
            &[],
        ),
        (
            "operational.failure",
            &[
                "agent",
                "exec",
                "pty",
                "observer",
                "subscription",
                "schedule",
                "daemon",
                "machine",
                "step-run",
                "mission-run",
                "loop-run",
                "resource",
                "checkpoint",
            ],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("operational"),
            true,
            &[],
        ),
        (
            "operational.recovered",
            &[
                "agent",
                "exec",
                "pty",
                "observer",
                "subscription",
                "schedule",
                "daemon",
                "machine",
                "step-run",
                "mission-run",
                "loop-run",
                "resource",
                "checkpoint",
            ],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("operational"),
            true,
            &[],
        ),
        (
            "reconcile.fault",
            &[
                "daemon",
                "mission-run",
                "observer",
                "schedule",
                "step-run",
                "subscription",
            ],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("daemons"),
            false,
            &[],
        ),
        (
            "checkpoint.sealed",
            &["checkpoint"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("checkpoints"),
            false,
            &[],
        ),
        (
            "checkpoint.verified",
            &["checkpoint"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("checkpoints"),
            false,
            &[],
        ),
        (
            "checkpoint.excused",
            &["checkpoint-excusal"],
            WritePolicy::SystemOnly,
            Cardinality::Append,
            Some("checkpoints"),
            false,
            &[],
        ),
        (
            "record.repaired",
            &["repair"],
            WritePolicy::OrdinaryClient,
            Cardinality::Once,
            Some("replication"),
            false,
            &["repair"],
        ),
        (
            "repair.applied",
            &["repair"],
            WritePolicy::SystemOnly,
            Cardinality::Once,
            Some("repairs"),
            true,
            &[],
        ),
    ];
    for (kind, subjects, policy, cardinality, projection, wake, source_kdl) in definitions {
        claims.insert(
            (*kind).into(),
            ClaimSpec {
                kind: (*kind).into(),
                subjects: subjects.iter().map(|value| (*value).into()).collect(),
                fields: claim_fields(kind),
                additional_fields: *kind == "resource.observed",
                write_policy: policy.clone(),
                cardinality: cardinality.clone(),
                projection: projection.map(str::to_owned),
                wakes_reconciler: *wake,
                source_kdl: source_kdl.iter().map(|value| (*value).into()).collect(),
                retention: claim_retention(kind),
            },
        );
    }
    claims
}

/// Observations that only the node that made them reads. See
/// `docs/st3/data-authority.md` for the local observation log.
fn claim_retention(kind: &str) -> Retention {
    match kind {
        // The owner reads a transcript from the harness's own session file, or from this log
        // when there is none. Other nodes relay timeline reads to the owner.
        "harness.timeline" | "harness.telemetry" => Retention::Local,
        // Only the node that made them reads these: render receipts and the readiness
        // deadline, whose attention request replicates.
        "render.applied" | "runtime.readiness-deadline-reached" => Retention::Local,
        // The owner's reconciler records its own starts, stops and kills: the stop deadline
        // fence, restart windows and adoption read them on that node only. Another node
        // stops a runtime through a replicated `stop` intent. A person's signal names its
        // requester and replicates.
        "runtime.action.requested"
        | "runtime.action.succeeded"
        | "runtime.action.failed"
        | "runtime.action.deadline-reached" => Retention::SystemLocal,
        // Other nodes read the current harness state and usage: step readiness is judged on
        // the mission's node and fleet views run anywhere. Nothing reads a heartbeat.
        "harness.observed" | "harness.usage" | "harness.todo.observed" | "workspace.observed" => {
            Retention::Latest
        }
        _ => Retention::Durable,
    }
}

fn claim_fields(kind: &str) -> BTreeMap<String, FieldSpec> {
    let names: &[(&str, FieldSpec)] = match kind {
        "workspace.observed" => &[
            ("host", required_string()),
            ("workspace", required_string()),
            ("repository", string()),
        ],
        "harness.todo.observed" => &[
            ("harness", required_string()),
            ("session_id", required_string()),
            ("incarnation_id", required_string()),
            ("observed_at", required_string()),
            ("source_op", required_string()),
            ("phases", required_array()),
            ("totals", required_object()),
            ("truncated", required_boolean()),
        ],
        "agent.account" => &[("account", required_reference_to(&["account"]))],
        "attention.requested" => &[
            ("reviewer", required_reference_to(&["person"])),
            ("title", required_string()),
            ("reason", required_string()),
            ("severity", required_enum(&["warning", "error"])),
            ("targets", array()),
            ("until", string()),
            ("step", reference_to(&["step-run"])),
            ("step_attempt", integer()),
            ("closed_by", enumeration(&["person", "st"])),
        ],
        "attention.resolved" => &[
            ("request", required_string()),
            (
                "outcome",
                required_enum(&["resolved", "dismissed", "withdrawn"]),
            ),
            ("reason", string()),
        ],
        "intent.desired" => &[
            ("kind", string()),
            ("revision", string()),
            ("desired", object()),
        ],
        "glass.upserted" => &[
            ("body", object()),
            ("base_revision", string()),
            ("replaced_revision", string()),
        ],
        "glass.deleted" => &[("base_revision", string()), ("replaced_revision", string())],
        "doc.bound" => &[
            ("name", string()),
            ("hash", string()),
            ("size", integer()),
            ("executable", boolean()),
        ],
        "owned-set.revised" => &[
            ("revision", required_string()),
            ("body", required_object()),
        ],
        "mission.published" => &[
            ("revision", string()),
            ("state", string()),
            ("body", object()),
        ],
        "mission.produced" => &[
            ("name", string()),
            ("mission", reference()),
            ("revision", string()),
            ("step_definition", string()),
            ("attempt", integer()),
        ],
        "mission-run.created" => &[
            ("status", string()),
            ("mission", reference()),
            ("revision", string()),
            ("generation", reference()),
            ("initial_revision", string()),
            ("current_generation", reference()),
            ("root_revision", string()),
            ("root_mission_run", reference()),
            ("workspace", string()),
            ("requester", reference()),
            ("mode", string()),
            ("inputs", object()),
            ("timeout_ms", integer()),
            ("deadline_at_unix_ms", integer()),
            ("parent_step_run", reference()),
            ("default_selector", object()),
            ("after", reference()),
        ],
        "mission-run.state" => &[
            ("status", string()),
            ("phase", string()),
            ("previous_phase", string()),
            ("reason", string()),
            ("completion", string()),
            ("finally", string()),
        ],
        "loop.round-result" => &[
            ("round", required_integer()),
            (
                "status",
                required_enum(&["completed", "failed", "discarded"]),
            ),
            ("mission_run", required_reference_to(&["mission-run"])),
            ("metrics", object()),
            ("feedback", reference_to(&["doc"])),
            ("item", any()),
            ("candidate", integer()),
            ("reason", string()),
            ("token_usage", integer()),
        ],
        "loop.round-dispatch" => &[
            ("round", required_integer()),
            ("dispatch", required_integer()),
            ("status", required_enum(&["rescheduled"])),
            ("mission_run", required_reference_to(&["mission-run"])),
            ("candidate", integer()),
            ("item_id", string()),
            ("reason", required_string()),
        ],
        "loop.state" => &[
            (
                "status",
                required_enum(&["running", "completed", "failed", "exhausted"]),
            ),
            ("round", integer()),
            ("reason", string()),
            ("best_round", integer()),
            ("best_metrics", object()),
            ("feedback", reference_to(&["doc"])),
            ("items", array()),
            ("winner", integer()),
        ],
        "publication.operation" => &[
            ("operation", string()),
            ("action", string()),
            ("status", required_enum(&["accepted"])),
        ],
        "run-generation.created" => &[
            ("run", reference()),
            ("revision", string()),
            ("status", string()),
            ("predecessor", reference()),
            ("reason", string()),
            ("compatible_steps", array()),
        ],
        "run-generation.state" | "run-generation.superseded" => &[
            ("status", string()),
            ("phase", string()),
            ("previous_phase", string()),
            ("reason", string()),
            ("successor", reference()),
        ],
        "step-run.state" => &[
            ("status", string()),
            ("reason", string()),
            ("readiness_epoch", integer()),
            ("attempt", integer()),
        ],
        "step-run.retried" => &[
            ("status", string()),
            ("attempt", integer()),
            ("reason", string()),
            ("goals", array()),
            ("not_before_unix_ms", integer()),
        ],
        "step-run.carried" => &[
            ("source", reference()),
            ("source_step_run", reference()),
            ("source_generation", reference()),
            ("definition_hash", string()),
            ("status", string()),
            ("attempt", integer()),
            ("worker_reported", boolean()),
            ("claimant", reference()),
            ("claim_incarnation", string()),
            ("claim_expires_at_unix_ms", integer()),
        ],
        "work.person-asked" => &[
            ("run", reference()),
            ("generation", reference()),
            ("origin_step", reference()),
            ("origin_attempt", integer()),
            ("person", reference()),
            ("title", string()),
            ("reason", string()),
            ("key", string()),
            ("attempt", integer()),
            ("status", string()),
            ("mission_spec", object()),
            ("owner_run", reference()),
            ("owner_generation", reference()),
            ("waiting_since", string()),
            ("legacy_request", string()),
            ("requester_declaration", string()),
            ("request", object()),
        ],
        "work.person-done" | "work.person-cancelled" => &[
            ("attempt", integer()),
            ("status", string()),
            ("summary", string()),
            ("key", string()),
            ("episode", string()),
            ("answer", object()),
        ],
        "work.claimed" | "work.renewed" | "work.progress" | "work.submitted" | "work.failed"
        | "work.released" | "work.extended" => &[
            ("attempt", integer()),
            ("status", string()),
            ("summary", string()),
            ("reason", string()),
            ("worker_reported", boolean()),
            ("claimant", reference()),
            ("claim_incarnation", string()),
            ("claim_expires_at_unix_ms", integer()),
            ("readiness_epoch", integer()),
            ("extend_ms", integer()),
        ],
        "gate.requested" => &[
            ("status", string()),
            ("owner", reference()),
            ("reviewer", reference()),
            ("mode", enumeration(&["approve", "feedback"])),
            ("question", string()),
            ("review_targets", array()),
            ("decisions", array()),
            ("operation", reference()),
            ("mission_revision", string()),
            ("step_definition", string()),
            ("attempt", integer()),
            ("runner", string()),
            ("model", string()),
            ("token_budget", integer()),
            ("tools", array()),
            ("capability_hash", string()),
            ("capability_expires_at", string()),
            ("gate", string()),
            ("baseline", boolean()),
        ],
        "gate.result" => &[
            (
                "verdict",
                required_enum(&["pass", "fail", "error", "feedback"]),
            ),
            ("decision", string()),
            ("reason", string()),
            ("operation", reference()),
            ("request", string()),
            ("gate", string()),
            ("baseline", boolean()),
            ("field", string()),
            ("value", any()),
            ("token_usage", integer()),
            ("stage", string()),
        ],
        "eval.verdict" => &[
            ("verdict", required_enum(&["pass", "fail", "void"])),
            ("reason", string()),
            ("residue", array()),
        ],
        "file.observed" => &[
            ("status", required_enum(&["observed", "unreadable"])),
            ("path", required_string()),
            ("content_hash", string()),
            ("blob_hash", string()),
            ("content", string()),
            ("mode", integer()),
            ("reason", string()),
        ],
        "lane.joined" => &[("entry", required_reference()), ("reason", string())],
        "lane.left" => &[
            ("entry", required_reference()),
            ("outcome", required_enum(&["completed", "removed"])),
            ("reason", string()),
        ],
        "lane.moved" => &[
            ("entry", required_reference()),
            (
                "placement",
                required_enum(&["top", "bottom", "before", "after"]),
            ),
            ("anchor", reference()),
            ("reason", string()),
        ],
        "lane.marked" => &[
            ("entry", required_reference()),
            (
                "state",
                required_enum(&["waiting", "held", "ready", "running"]),
            ),
            ("detail", string()),
            ("head", string()),
        ],
        "lane.approved" => &[("entry", required_reference()), ("reason", string())],
        "agent.queue.moved" => &[
            ("run", required_reference_to(&["mission-run"])),
            (
                "placement",
                required_enum(&["top", "bottom", "before", "after"]),
            ),
            ("anchor", reference_to(&["mission-run"])),
            ("reason", string()),
        ],
        "agent.presence" => &[
            (
                "presence",
                required_enum(&["available", "busy", "dnd", "offline"]),
            ),
            (
                "reachability",
                enumeration(&["reachable", "unreachable", "indeterminate"]),
            ),
            ("reason", string()),
        ],
        "observer.observed" => &[
            ("status", string()),
            ("revision", string()),
            ("attempt", string()),
            ("changed", boolean()),
            ("cursor", string()),
            ("next_check_unix_ms", string()),
            ("resource", reference()),
            ("provider", string()),
            ("locator", string()),
            ("changed_fields", array()),
            ("observation", reference()),
        ],
        "observer.refresh-requested" => &[
            ("revision", required_string()),
            ("attempt", required_string()),
        ],
        "observer.state" => &[
            (
                "state",
                required_enum(&["healthy", "degraded", "unreachable", "stopped"]),
            ),
            ("reason", string()),
            ("error_code", string()),
            ("revision", string()),
            ("attempt", string()),
            ("next_check_unix_ms", string()),
        ],
        "subscription.state" => &[
            ("state", required_enum(&["pending", "active", "stopped"])),
            ("reason", string()),
            ("observer", reference()),
            ("to", reference()),
            ("fields", array()),
        ],
        "daemon.started" => &[
            ("features", object()),
            ("status", required_enum(&["running"])),
            ("pid", integer()),
            ("version", string()),
            ("schema", string()),
            ("schema_digest", string()),
        ],
        "daemon.diagnostic" => &[
            ("severity", required_enum(&["warning", "error"])),
            ("code", required_string()),
            ("status", string()),
            ("reason", required_string()),
        ],
        "fleet.invite-created" => &[
            ("sponsor", required_reference_to(&["host"])),
            ("name", string()),
            ("expires_at_unix_ms", required_integer()),
            ("transports", array()),
            ("created_by", reference_to(&["person"])),
        ],
        "fleet.invite-redeemed" => &[
            ("name", required_string()),
            ("member_key", required_string()),
        ],
        "fleet.invite-revoked" => &[
            ("reason", required_string()),
            ("revoked_by", reference_to(&["person"])),
        ],
        "fleet.member-admitted" => &[
            ("fleet_id", required_string()),
            ("member_key", required_string()),
            ("via", required_enum(&["anchor", "invite", "migration"])),
            ("sponsor", reference_to(&["host"])),
            ("invite", reference_to(&["fleet-invite"])),
            ("mode", required_enum(&["listening", "dial-out"])),
            ("writer_floor", integer()),
            ("admitted_by", reference_to(&["person"])),
        ],
        "fleet.member-endpoints" => &[
            ("member_key", required_string()),
            ("mode", required_enum(&["listening", "dial-out"])),
            ("endpoints", array()),
            ("build", string()),
        ],
        "fleet.member-left" => &[
            ("member_key", required_string()),
            ("high_water", required_integer()),
        ],
        "fleet.member-removed" => &[
            ("member_key", string()),
            ("high_water", required_integer()),
            ("reason", required_string()),
            ("removed_by", reference_to(&["person"])),
        ],
        "rule.set" => &[
            ("mode", required_enum(&["off", "audit", "enforce"])),
            ("description", string()),
            ("actors", array()),
            ("except", array()),
            ("kinds", array()),
            ("subjects", array()),
            ("unless_subjects", array()),
        ],
        "rule.audited" => &[
            ("rule", required_string()),
            ("actor", required_string()),
            ("action", required_string()),
            ("target", required_string()),
        ],
        "principal.key-granted" => &[
            ("key", required_string()),
            ("role", required_enum(&["root", "device", "agent", "plugin"])),
            ("issuer", required_string()),
            ("issuer_key", required_string()),
            ("label", string()),
        ],
        "principal.key-revoked" => &[
            ("key", required_string()),
            ("reason", string()),
        ],
        "transport.observed" => &[
            ("status", required_enum(&["up", "down", "unknown"])),
            ("reason", string()),
            ("protocol", string()),
            ("last_success_at", integer()),
            ("remote_heads", object()),
        ],
        "operational.failure" => &[
            ("episode", string()),
            ("condition", string()),
            ("reviewer", reference()),
            ("title", string()),
            ("reason", string()),
            ("severity", string()),
            ("targets", array()),
            ("source_revision", string()),
            ("incarnation", string()),
        ],
        "operational.recovered" => &[
            ("episode", string()),
            ("failure", string()),
            ("reason", string()),
        ],
        "reconcile.fault" => &[
            ("scope", required_string()),
            ("status", required_enum(&["faulted", "recovered"])),
            ("reason", string()),
        ],
        "record.repaired" => &[
            ("record", required_string()),
            ("replacement", required_string()),
            ("reason", required_string()),
        ],
        "repair.applied" => &[
            ("token", required_string()),
            ("item_count", integer()),
            ("affected_subjects", array()),
            ("reason", required_string()),
        ],
        "runtime.action.requested" => &[
            ("action", string()),
            ("rollout", object()),
            ("operation", string()),
            ("runtime_id", string()),
            ("terminal", boolean()),
            ("incarnation_id", string()),
            ("deadline_unix_ms", string()),
            ("signal", string()),
            ("reason", string()),
        ],
        "runtime.action.succeeded"
        | "runtime.action.failed"
        | "runtime.action.deadline-reached" => &[
            ("action", string()),
            ("rollout", object()),
            ("operation", string()),
            ("runtime_id", string()),
            ("terminal", boolean()),
            ("incarnation_id", string()),
            ("deadline_key", string()),
            ("desired_token", string()),
            ("rollout_operation", string()),
            ("reason", string()),
            ("signal", string()),
            ("operation_status", string()),
            ("code", string()),
            ("blocking", array()),
            ("harness", string()),
            ("native_session_id", string()),
        ],
        "runtime.observed" => &[
            ("status", string()),
            ("runtime_id", string()),
            ("terminal", boolean()),
            ("reachability", string()),
            ("reason", string()),
            ("exit_code", integer()),
            ("exit_signal", integer()),
            ("incarnation_id", string()),
            ("adopted", boolean()),
            ("driver", string()),
            ("host", string()),
            ("shutdown_timeout_ms", integer()),
        ],
        "render.applied" => &[("writes", array()), ("warnings", array())],
        "runtime.restart-window-reset" => &[
            ("desired_token", string()),
            ("incarnation_id", required_string()),
            ("reason", required_string()),
        ],
        "runtime.reconcile-decision" => &[
            ("decision", string()),
            ("reachability", string()),
            ("reason", string()),
            ("restart_at_unix_ms", string()),
            ("gate", string()),
            ("key", string()),
            ("input_number", integer()),
        ],
        "runtime.readiness-deadline-reached" => &[
            ("runtime_id", required_string()),
            ("driver", required_string()),
            ("incarnation_id", required_string()),
            ("deadline_unix_ms", required_string()),
            ("reason", required_string()),
        ],
        "delivery.hold" => &[
            ("held", required_boolean()),
            ("until_unix_ms", required_integer()),
            ("reason", required_string()),
            ("legacy_adoption", boolean()),
        ],
        "harness.observed" => &[
            (
                "state",
                required_enum(&[
                    "starting",
                    "ready",
                    "idle",
                    "working",
                    "blocked",
                    "ended",
                    "indeterminate",
                ]),
            ),
            ("background_jobs", integer()),
            ("driver", string()),
            ("reason", string()),
            ("incarnation_id", string()),
            ("transport", string()),
            ("blocked_on", string()),
            ("ask", string()),
            ("input_buffer", string()),
            ("exit", string()),
            ("observed_since_ms", integer()),
            ("observed_at_ms", integer()),
            ("ownership_sequence", integer()),
            ("transition_sequence", integer()),
            ("evidence_incarnation", string()),
            ("rollout_operation", string()),
            ("quiescent", boolean()),
            ("blocking", array()),
        ],
        "harness.session-file" => &[
            ("harness", required_string()),
            ("account_ref", string()),
            ("path", string()),
            ("session_id", required_string()),
            ("source_session", string()),
            ("discovery_revision", string()),
            ("agent", reference_to(&["agent"])),
            ("incarnation_id", string()),
            (
                "status",
                enumeration(&["active", "inactive", "missing", "unknown"]),
            ),
            ("modified_at", string()),
        ],
        "harness.diagnostic" => &[
            ("severity", enumeration(&["warning", "error"])),
            ("status", string()),
            ("code", string()),
            ("reason", string()),
            ("incarnation_id", string()),
            ("matched_line", string()),
            ("step_run", reference_to(&["step-run"])),
            ("wake_attempts", integer()),
            ("attempt", integer()),
            ("readiness_epoch", integer()),
            ("observed_since_ms", integer()),
            ("retry_attempt", integer()),
            ("retry_after_unix_ms", integer()),
        ],
        "harness.usage" => &[
            ("input_tokens", integer()),
            ("output_tokens", integer()),
            ("total_tokens", integer()),
            ("cached_tokens", integer()),
            ("cache_write_tokens", integer()),
            ("owner_run", string()),
            ("owner_step", string()),
            ("host", string()),
            ("observed_at_unix_ms", integer()),
            ("context_used_tokens", integer()),
            ("context_window_tokens", integer()),
            ("context_used_percent", number()),
            ("compactions", integer()),
            ("last_compaction_ms", integer()),
            ("last_compaction_trigger", string()),
            ("cost", number()),
            ("currency", string()),
            ("account", string()),
            ("cache_write_1h_tokens", integer()),
            ("cost_microusd", integer()),
            ("reported_cost_microusd", integer()),
            ("unpriced_tokens", integer()),
            ("pricing", string()),
            (
                "semantics",
                required_enum(&[
                    "context_occupancy",
                    "session_cumulative",
                    "response",
                    "response_rollup",
                ]),
            ),
            ("driver", required_string()),
            ("model", string()),
            ("incarnation_id", required_string()),
        ],
        // A harness's reading of its paying account's limits. Percentages are the harness's own;
        // absent windows are unknown, never zero.
        "harness.limits" => &[
            ("driver", required_string()),
            ("incarnation_id", string()),
            ("account", string()),
            // The declared account the seat was launched on (`ada/claude`), when it was bound to one.
            ("account_ref", string()),
            ("plan", string()),
            ("five_hour_percent", number()),
            ("five_hour_resets_at_unix_ms", integer()),
            ("weekly_percent", number()),
            ("weekly_resets_at_unix_ms", integer()),
            ("measured_at_unix_ms", required_integer()),
        ],
        // The harness's own subagent ID names the subagent within its parent seat. The prompt and
        // transcript stay on the host.
        "subagent.appeared" => &[
            ("subagent_id", required_string()),
            ("subagent_type", string()),
            ("description", string()),
            ("driver", required_string()),
            ("session_id", string()),
            ("incarnation_id", required_string()),
            ("step_run", reference_to(&["step-run"])),
            ("started_at_unix_ms", integer()),
            ("lease_expires_at_unix_ms", required_integer()),
        ],
        "subagent.renewed" => &[
            ("subagent_id", required_string()),
            ("incarnation_id", string()),
            ("lease_expires_at_unix_ms", required_integer()),
        ],
        // Token buckets are the subagent's own responses, which also count in the parent's usage.
        "subagent.ended" => &[
            ("subagent_id", required_string()),
            ("outcome", required_enum(SUBAGENT_OUTCOMES)),
            ("reason", string()),
            ("ended_at_unix_ms", integer()),
            ("duration_ms", integer()),
            ("input_tokens", integer()),
            ("output_tokens", integer()),
            ("cache_write_tokens", integer()),
            ("cached_tokens", integer()),
            ("total_tokens", integer()),
        ],
        "harness.telemetry" => &[
            ("driver", required_enum(&["claude"])),
            ("unit", required_enum(&["hook"])),
            ("incarnation_id", required_string()),
            ("signals", required_object()),
        ],
        "harness.timeline" => &[
            (
                "operation",
                required_enum(&["append", "replace", "finalize"]),
            ),
            ("entry_id", required_string()),
            ("source_id", string()),
            ("sequence", integer()),
            ("revision", required_integer()),
            (
                "role",
                required_enum(&["system", "user", "assistant", "tool"]),
            ),
            (
                "entry_type",
                required_enum(&[
                    "message",
                    "content",
                    "tool_call",
                    "tool_result",
                    "status",
                    "error",
                    "usage",
                    "redaction",
                    "truncation",
                ]),
            ),
            ("final", required_boolean()),
            ("body", required_object()),
            ("driver", required_string()),
            ("incarnation_id", required_string()),
            ("observed_at_unix_ms", integer()),
        ],
        "harness.context-clear.requested" => &[
            ("runtime_id", string()),
            ("incarnation_id", string()),
            ("context_epoch", string()),
            ("operation_status", string()),
        ],
        "harness.context-clear.result" => &[
            (
                "result",
                required_enum(&["confirmed", "failed", "indeterminate"]),
            ),
            ("context_epoch", string()),
            ("reason", string()),
            ("incarnation_id", string()),
            ("runtime_id", string()),
        ],
        "terminal.input.requested" => &[
            (
                "mode",
                enumeration(&["line", "raw", "key", "text", "binary"]),
            ),
            ("sha256", string()),
            ("byte_count", integer()),
            ("runtime_id", string()),
            ("incarnation_id", string()),
            ("intent", enumeration(&["context-compaction"])),
            ("sequence", integer()),
        ],
        "terminal.input.result" => &[
            ("result", required_enum(&["written", "failed"])),
            ("reason", string()),
            ("incarnation_id", string()),
            ("runtime_id", string()),
            ("sequence", integer()),
        ],
        "message.sent" => &[
            ("from", reference()),
            ("to", reference()),
            ("content", string()),
            ("status", required_enum(&["sent"])),
            ("session_id", string()),
            ("title", string()),
            ("in_reply_to", reference()),
            ("tags", array()),
            ("attachments", array()),
        ],
        "message.staged" => &[
            ("status", required_enum(&["staged"])),
            ("recipient", reference()),
            ("transport", string()),
            ("runtime_id", string()),
        ],
        "message.delivered" => &[
            ("status", required_enum(&["delivered"])),
            ("recipient", reference()),
            ("transport", string()),
            ("runtime_id", string()),
        ],
        "message.read" => &[("status", required_enum(&["read"]))],
        "message.closed" => &[("status", required_enum(&["closed"]))],
        "planning-session.started" => &[
            ("mission", reference()),
            ("request", reference()),
            ("workspace", string()),
            ("requester", reference()),
            ("planner", reference()),
            ("planner_config", object()),
            ("target_run", reference()),
            ("target_generation", reference()),
        ],
        "planning-session.candidate-submitted" => &[
            ("variant", string()),
            ("revision", integer()),
            ("candidate_revision", integer()),
            ("markdown", reference()),
            ("kdl", reference()),
            ("mission_revision", string()),
        ],
        "planning-session.previewed" => &[
            ("variant", string()),
            ("candidate_revision", integer()),
            ("preview_hash", string()),
            ("store_index", integer()),
            ("graph", string()),
            ("diff", string()),
            ("mission", object()),
        ],
        "planning-session.revision-requested" => &[
            ("variant", string()),
            ("candidate_revision", integer()),
            ("feedback", reference()),
            ("requester", reference()),
        ],
        "planning-session.approved" => &[
            ("variant", string()),
            ("candidate_revision", integer()),
            ("preview_hash", string()),
            ("preview_token", string()),
            ("mission_revision", string()),
            ("markdown", reference()),
            ("kdl", reference()),
            ("requester", reference()),
        ],
        "planning-session.cancelled" => &[("reason", string()), ("requester", reference())],
        "planning-session.question-requested" => &[
            ("decision_id", string()),
            ("revision", integer()),
            ("question", string()),
            (
                "decision_type",
                required_enum(&["boolean", "single-choice", "multiple-choice", "rank"]),
            ),
            ("options", array()),
            ("requester", reference()),
            ("planner", reference()),
        ],
        "planning-session.question-answered" => &[
            ("decision_id", string()),
            ("expected_revision", integer()),
            ("response", object()),
            ("explanation", string()),
            ("requester", reference()),
        ],
        "revision-proposal.created" => &[
            ("run", reference()),
            ("source_generation", reference()),
            ("candidate_revision", string()),
            ("reason", string()),
            ("status", string()),
            ("cutover", string()),
            ("compatible_steps", array()),
            ("reviewers", array()),
            ("preview_hash", string()),
        ],
        "revision-proposal.approved" => &[
            ("reviewer", reference()),
            ("all_approved", boolean()),
            ("preview_hash", string()),
        ],
        "revision-proposal.cancelled" => &[("status", string()), ("reason", string())],
        "revision-proposal.applied" => &[
            ("status", string()),
            ("successor_generation", reference()),
            ("reason", string()),
        ],
        "schedule.occurrence-scheduled" => &[
            ("occurrence", integer()),
            ("revision", string()),
            ("at_unix_ms", integer()),
            ("scheduled_at_unix_ms", string()),
        ],
        "schedule.occurrence-reached" => &[
            ("occurrence", integer()),
            ("revision", string()),
            ("at_unix_ms", integer()),
            ("scheduled_at_unix_ms", string()),
            ("scheduled", reference()),
        ],
        "schedule.occurrence-cancelled" => &[
            ("occurrence", integer()),
            ("revision", string()),
            ("reason", string()),
        ],
        "schedule.work-requested" => &[
            ("revision", required_string()),
            ("occurrence", required_integer()),
            ("mission", required_reference_to(&["mission"])),
            ("mission_revision", required_string()),
            ("workspace", required_string()),
            ("inputs", required_object()),
        ],
        "schedule.work-failed" => &[
            ("request", required_string()),
            ("code", required_string()),
            ("reason", required_string()),
        ],
        "schedule.work-started" => &[
            ("request", required_string()),
            ("mission_run", required_reference_to(&["mission-run"])),
        ],
        "subscription.mission-requested" => &[
            ("mission", required_reference_to(&["mission"])),
            ("mission_revision", string()),
            ("resource", required_reference_to(&["resource"])),
            ("resource_input", required_string()),
            ("workspace", required_string()),
            ("discovery", required_string()),
            ("delivery_key", string()),
            ("requester", reference_to(&["agent", "person"])),
            ("held", boolean()),
            ("text_input", string()),
            ("text", string()),
        ],
        "subscription.mission-request-cancelled" | "subscription.mission-request-released" => {
            &[("request", required_string()), ("reason", string())]
        }
        "subscription.mission-started" => &[
            ("request", required_string()),
            ("mission_run", required_reference_to(&["mission-run"])),
        ],
        "subscription.batched" => &[
            ("entries", required_array()),
            ("delivery_key", required_string()),
        ],
        "github.posted" => &[
            ("agent", required_reference_to(&["agent"])),
            ("repository", required_string()),
            ("item", required_integer()),
            ("kind", required_enum(&["comment", "review"])),
            ("id", required_integer()),
            ("url", string()),
            ("login", string()),
        ],
        "subscription.watch-ended" => &[
            (
                "reason",
                required_enum(&[
                    "closed",
                    "merged",
                    "deadline",
                    "unwatched",
                    "seat-ended",
                ]),
            ),
            ("since_unix_ms", required_string()),
            ("message", reference_to(&["message"])),
        ],
        "subscription.batch-sent" => &[
            ("through", required_string()),
            ("message", required_reference_to(&["message"])),
            ("entries", required_integer()),
        ],
        "checkpoint.sealed" => &[
            ("cut_unix_ms", required_integer()),
            ("participants", array()),
            ("sealed_digest", required_string()),
            ("sealed_count", required_integer()),
            ("rules_digest", required_string()),
            ("checkpoint_protocol", required_integer()),
            ("build", string()),
        ],
        "checkpoint.verified" => &[
            ("cut_unix_ms", required_integer()),
            ("participants", array()),
            ("sealed_digest", required_string()),
            ("rules_digest", required_string()),
            ("drop_digest", required_string()),
            ("dropped_envelopes", required_integer()),
            ("dropped_claims", required_integer()),
            ("retained_digest", required_string()),
            ("graph_digest", required_string()),
            ("reader_digest", required_string()),
            ("checkpoint_protocol", required_integer()),
            ("build", string()),
        ],
        "checkpoint.excused" => &[("writer", required_string()), ("reason", required_string())],
        "subscription.mission-deferred" => &[
            ("request", required_string()),
            ("not_before_unix_ms", required_integer()),
        ],
        "subscription.mission-failed" => &[
            ("request", required_string()),
            ("code", required_string()),
            ("reason", required_string()),
        ],
        "resource.observed" => &[
            ("kind", string()),
            ("state", any()),
            ("observed_at", integer()),
        ],
        _ => &[],
    };
    names
        .iter()
        .cloned()
        .map(|(name, spec)| (name.into(), spec))
        .collect()
}

fn resource(kind: &str, description: &str, fields: &[(&str, FieldSpec)]) -> ResourceSpec {
    ResourceSpec {
        kind: kind.into(),
        description: description.into(),
        open_facts: false,
        fields: fields
            .iter()
            .cloned()
            .map(|(name, spec)| (name.into(), spec))
            .collect(),
    }
}

fn any() -> FieldSpec {
    field(ValueType::Any)
}
fn array() -> FieldSpec {
    field(ValueType::Array)
}
fn boolean() -> FieldSpec {
    field(ValueType::Boolean)
}
fn integer() -> FieldSpec {
    field(ValueType::Integer)
}
fn number() -> FieldSpec {
    field(ValueType::Number)
}
fn object() -> FieldSpec {
    field(ValueType::Object)
}
fn string() -> FieldSpec {
    field(ValueType::String)
}
fn required_string() -> FieldSpec {
    FieldSpec {
        required: true,
        ..string()
    }
}

fn required_integer() -> FieldSpec {
    FieldSpec {
        required: true,
        ..integer()
    }
}

fn required_array() -> FieldSpec {
    FieldSpec {
        required: true,
        ..array()
    }
}

fn required_boolean() -> FieldSpec {
    FieldSpec {
        required: true,
        ..boolean()
    }
}

fn required_object() -> FieldSpec {
    FieldSpec {
        required: true,
        ..object()
    }
}
fn reference() -> FieldSpec {
    FieldSpec {
        reference: true,
        value_type: ValueType::SubjectReference,
        ..field(ValueType::SubjectReference)
    }
}
fn required_reference() -> FieldSpec {
    FieldSpec {
        required: true,
        ..reference()
    }
}
fn required_reference_to(families: &[&str]) -> FieldSpec {
    FieldSpec {
        required: true,
        reference_families: families.iter().map(|family| (*family).into()).collect(),
        ..reference()
    }
}
fn reference_to(families: &[&str]) -> FieldSpec {
    FieldSpec {
        reference_families: families.iter().map(|family| (*family).into()).collect(),
        ..reference()
    }
}
fn immutable_string() -> FieldSpec {
    FieldSpec {
        immutable: true,
        ..string()
    }
}
fn immutable_reference() -> FieldSpec {
    FieldSpec {
        immutable: true,
        ..reference()
    }
}
fn enumeration(values: &[&str]) -> FieldSpec {
    FieldSpec {
        values: values.iter().map(|value| (*value).into()).collect(),
        ..string()
    }
}
fn required_enum(values: &[&str]) -> FieldSpec {
    FieldSpec {
        required: true,
        ..enumeration(values)
    }
}
fn field(value_type: ValueType) -> FieldSpec {
    FieldSpec {
        value_type,
        required: false,
        values: Vec::new(),
        immutable: false,
        reference: false,
        reference_families: Vec::new(),
    }
}

fn custom_claim_spec() -> &'static ClaimSpec {
    static SPEC: OnceLock<ClaimSpec> = OnceLock::new();
    SPEC.get_or_init(|| ClaimSpec {
        kind: "custom.*".into(),
        subjects: vec!["custom".into()],
        fields: BTreeMap::new(),
        additional_fields: true,
        write_policy: WritePolicy::OrdinaryClient,
        cardinality: Cardinality::Append,
        projection: None,
        wakes_reconciler: false,
        source_kdl: Vec::new(),
        retention: Retention::Durable,
    })
}

fn custom_resource_spec() -> &'static ResourceSpec {
    static SPEC: OnceLock<ResourceSpec> = OnceLock::new();
    SPEC.get_or_init(|| ResourceSpec {
        kind: "custom.*.*".into(),
        description: "A namespaced custom resource with an open fact bag.".into(),
        open_facts: true,
        fields: BTreeMap::new(),
    })
}

fn error(code: &'static str, message: impl Into<String>) -> ValidationError {
    ValidationError {
        code,
        message: message.into(),
    }
}

fn validate_harness_todo(fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    // Count the actual JSON representation without allocating a serialized copy.
    struct BoundedWriter(usize);
    impl std::io::Write for BoundedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > HARNESS_TODO_MAX_FIELDS_BYTES - self.0 {
                return Err(std::io::Error::other("todo fields exceed 64 KiB"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(BoundedWriter(0), fields)
        .map_err(|_| error("invalid-harness-todo", "todo fields exceed 64 KiB"))?;
    let invalid = || error("invalid-harness-todo", "invalid todo shape, bounds or totals");
    for name in ["harness", "session_id", "incarnation_id", "observed_at", "source_op"] {
        if fields.get(name).and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err(invalid());
        }
    }
    let phases = fields.get("phases").and_then(Value::as_array).ok_or_else(invalid)?;
    if phases.len() > HARNESS_TODO_MAX_PHASES {
        return Err(invalid());
    }
    let statuses = ["pending", "in_progress", "completed", "blocked"];
    let mut visible = [0_u64; 4];
    let mut task_count = 0;
    for phase in phases {
        let phase = phase.as_object().ok_or_else(invalid)?;
        if phase.len() != 2
            || phase.get("name").and_then(Value::as_str)
                .is_none_or(|name| name.len() > HARNESS_TODO_MAX_PHASE_BYTES)
        {
            return Err(invalid());
        }
        let tasks = phase.get("tasks").and_then(Value::as_array).ok_or_else(invalid)?;
        task_count += tasks.len();
        if task_count > HARNESS_TODO_MAX_TASKS {
            return Err(invalid());
        }
        for task in tasks {
            let task = task.as_object().ok_or_else(invalid)?;
            if task.keys().any(|key| !matches!(key.as_str(), "content" | "status" | "blocker"))
                || task.get("content").and_then(Value::as_str)
                    .is_none_or(|content| content.len() > HARNESS_TODO_MAX_TEXT_BYTES)
            {
                return Err(invalid());
            }
            if let Some(blocker) = task.get("blocker")
                && blocker.as_str().is_none_or(|text| text.len() > HARNESS_TODO_MAX_TEXT_BYTES)
            {
                return Err(invalid());
            }
            let status = task.get("status").and_then(Value::as_str).ok_or_else(invalid)?;
            let index = statuses.iter().position(|candidate| *candidate == status).ok_or_else(invalid)?;
            visible[index] += 1;
        }
    }
    let totals = fields.get("totals").and_then(Value::as_object).ok_or_else(invalid)?;
    let truncated = fields.get("truncated").and_then(Value::as_bool).ok_or_else(invalid)?;
    if totals.keys().any(|key| !statuses.contains(&key.as_str()) && key != "abandoned")
        || totals.get("abandoned").is_some_and(|value| value.as_u64().is_none())
    {
        return Err(invalid());
    }
    for (index, status) in statuses.iter().enumerate() {
        let total = totals.get(*status).and_then(Value::as_u64).ok_or_else(invalid)?;
        if total < visible[index] || (!truncated && total != visible[index]) {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo_fields() -> BTreeMap<String, Value> {
        serde_json::from_value(serde_json::json!({
            "harness": "omp", "session_id": "session", "incarnation_id": "incarnation",
            "observed_at": "2026-10-03T14:00:00Z", "source_op": "hydrate",
            "phases": [], "totals": {"pending": 0, "in_progress": 0, "completed": 0, "blocked": 0},
            "truncated": false
        })).unwrap()
    }

    fn validate_todo(fields: &BTreeMap<String, Value>) -> Result<&ClaimSpec, ValidationError> {
        registry().validate_claim("agent/run/worker", "harness.todo.observed", fields)
    }

    #[test]
    fn harness_todo_accepts_known_empty_and_fences_the_writer() {
        let fields = todo_fields();
        let spec = registry().validate_public_claim(
            "agent/run/worker", "harness.todo.observed", &fields, Some("agent/run/worker"),
        ).unwrap();
        assert_eq!(spec.retention, Retention::Latest);
        assert_eq!(spec.cardinality, Cardinality::Append);
        assert_eq!(registry().validate_public_claim(
            "agent/run/worker", "harness.todo.observed", &fields, Some("agent/run/other"),
        ).unwrap_err().code, "claim-write-forbidden");
        assert!(registry().validate_claim("resource/example", "harness.todo.observed", &fields).is_err());
    }

    #[test]
    fn harness_todo_validates_nested_shape_and_full_source_totals() {
        let mut fields = todo_fields();
        fields.insert("phases".into(), serde_json::json!([{"name": "", "tasks": [
            {"content": "work", "status": "blocked", "blocker": "approval"}
        ]}]));
        fields.get_mut("totals").unwrap()["blocked"] = Value::from(1);
        validate_todo(&fields).unwrap();
        for task in [
            serde_json::json!({"content": "work", "status": "unknown"}),
            serde_json::json!({"content": "work", "status": "blocked", "blocker": null}),
            serde_json::json!({"content": "work", "status": "blocked", "extra": true}),
            serde_json::json!({"status": "blocked"}),
        ] {
            let mut invalid = fields.clone();
            invalid.get_mut("phases").unwrap()[0]["tasks"][0] = task;
            assert!(validate_todo(&invalid).is_err());
        }
        fields.get_mut("totals").unwrap()["blocked"] = Value::from(2);
        assert!(validate_todo(&fields).is_err());
        fields.insert("truncated".into(), Value::Bool(true));
        validate_todo(&fields).unwrap();
        fields.get_mut("totals").unwrap()["blocked"] = Value::from(0);
        assert!(validate_todo(&fields).is_err());
        for total in [Value::from(-1), Value::from(1.5), Value::Null] {
            fields.get_mut("totals").unwrap()["blocked"] = total;
            assert!(validate_todo(&fields).is_err());
        }
    }

    #[test]
    fn harness_todo_counts_abandoned_without_truncating_or_adding_a_task_status() {
        let mut fields = todo_fields();
        let old_snapshot: HarnessTodoSnapshot =
            serde_json::from_value(serde_json::to_value(&fields).unwrap()).unwrap();
        assert_eq!(old_snapshot.totals.abandoned, 0);
        fields.get_mut("totals").unwrap()["abandoned"] = Value::from(3);
        validate_todo(&fields).unwrap();
        let snapshot: HarnessTodoSnapshot =
            serde_json::from_value(serde_json::to_value(&fields).unwrap()).unwrap();
        assert_eq!(snapshot.totals.abandoned, 3);
        assert!(!snapshot.truncated);
        for count in [Value::from(-1), Value::from(1.5), Value::Null, Value::from("3")] {
            let mut invalid = fields.clone();
            invalid.get_mut("totals").unwrap()["abandoned"] = count;
            assert!(validate_todo(&invalid).is_err());
        }
        let mut unknown = fields.clone();
        unknown.get_mut("totals").unwrap()["dropped"] = Value::from(3);
        assert!(validate_todo(&unknown).is_err());
        fields.get_mut("totals").unwrap().as_object_mut().unwrap().remove("pending");
        assert!(validate_todo(&fields).is_err());
        let mut task_status = todo_fields();
        task_status.insert("phases".into(), serde_json::json!([{"name":"Dropped","tasks":[
            {"content":"Dropped task","status":"abandoned"}
        ]}]));
        assert!(validate_todo(&task_status).is_err());
    }

    #[test]
    fn harness_todo_enforces_utf8_collection_and_escaped_size_bounds() {
        let mut fields = todo_fields();
        fields.insert("phases".into(), serde_json::json!([{"name": "é".repeat(64), "tasks": [
            {"content": "é".repeat(256), "status": "pending", "blocker": "é".repeat(256)}
        ]}]));
        fields.get_mut("totals").unwrap()["pending"] = Value::from(1);
        validate_todo(&fields).unwrap();
        for (key, oversized) in [("content", "é".repeat(257)), ("blocker", "é".repeat(257))] {
            let mut invalid = fields.clone();
            invalid.get_mut("phases").unwrap()[0]["tasks"][0][key] = Value::String(oversized);
            assert!(validate_todo(&invalid).is_err());
        }
        let mut invalid = fields.clone();
        invalid.get_mut("phases").unwrap()[0]["name"] = Value::String("é".repeat(65));
        assert!(validate_todo(&invalid).is_err());
        fields.insert("phases".into(), serde_json::json!(
            vec![serde_json::json!({"name": "", "tasks": []}); 16]
        ));
        fields.get_mut("totals").unwrap()["pending"] = Value::from(0);
        validate_todo(&fields).unwrap();
        fields.get_mut("phases").unwrap().as_array_mut().unwrap()
            .push(serde_json::json!({"name": "", "tasks": []}));
        assert!(validate_todo(&fields).is_err());
        let task = serde_json::json!({"content": "x", "status": "pending"});
        fields.insert("phases".into(), serde_json::json!([{"name": "", "tasks": vec![task.clone(); 100]}]));
        fields.get_mut("totals").unwrap()["pending"] = Value::from(100);
        validate_todo(&fields).unwrap();
        fields.get_mut("phases").unwrap()[0]["tasks"].as_array_mut().unwrap().push(task);
        assert!(validate_todo(&fields).is_err());
        let escaped = serde_json::json!({"content": "\u{0001}".repeat(512), "status": "pending"});
        fields.insert("phases".into(), serde_json::json!([{"name": "", "tasks": vec![escaped; 100]}]));
        // Each string fits its byte bound, but JSON escaping exceeds the fields cap.
        assert!(validate_todo(&fields).is_err());
        fields = todo_fields();
        fields.insert("session_id".into(), Value::String("x".repeat(HARNESS_TODO_MAX_FIELDS_BYTES)));
        assert!(validate_todo(&fields).is_err());
    }

    #[test]
    fn harness_todo_accepts_the_exact_fields_cap_but_not_one_byte_more() {
        let mut fields = todo_fields();
        let size = serde_json::to_vec(&fields).unwrap().len();
        let session_size = fields["session_id"].as_str().unwrap().len();
        fields.insert("session_id".into(), Value::String(
            "x".repeat(HARNESS_TODO_MAX_FIELDS_BYTES - size + session_size),
        ));
        validate_todo(&fields).unwrap();
        let mut session = fields["session_id"].as_str().unwrap().to_owned();
        session.push('x');
        fields.insert("session_id".into(), Value::String(session));
        assert!(validate_todo(&fields).is_err());
    }

    #[test]
    fn registry_matches_the_exact_manifests() {
        let registry = registry();
        assert_eq!(
            registry
                .subjects
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "account",
                "agent",
                "attention",
                "checkpoint",
                "checkpoint-excusal",
                "custom",
                "daemon",
                "doc",
                "exec",
                "file",
                "fleet-invite",
                "gate-operation",
                "github-post",
                "glass",
                "host",
                "lane",
                "loop-run",
                "message",
                "mission",
                "mission-run",
                "observer",
                "owned-set",
                "person",
                "planning-session",
                "pty",
                "repair",
                "resource",
                "revision-proposal",
                "rule",
                "run-generation",
                "schedule",
                "step-run",
                "subscription",
            ]
        );
        assert_eq!(
            registry
                .resources
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "ci.run",
                "filesystem.file",
                "harness.session-file",
                "human.review",
                "vcs.commit",
                "vcs.issue",
                "vcs.pull-request",
                "vcs.ref",
                "vcs.repository",
            ]
        );
        assert_eq!(
            registry.resources["human.review"]
                .fields
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "decision",
                "document",
                "reason",
                "reviewer",
                "submitted_at",
                "target"
            ]
        );
        assert_eq!(
            registry
                .claims
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "agent.account",
                "agent.presence",
                "agent.queue.moved",
                "attention.requested",
                "attention.resolved",
                "checkpoint.excused",
                "checkpoint.sealed",
                "checkpoint.verified",
                "daemon.diagnostic",
                "daemon.started",
                "delivery.hold",
                "doc.bound",
                "eval.verdict",
                "file.observed",
                "fleet.invite-created",
                "fleet.invite-redeemed",
                "fleet.invite-revoked",
                "fleet.member-admitted",
                "fleet.member-endpoints",
                "fleet.member-left",
                "fleet.member-removed",
                "gate.requested",
                "gate.result",
                "github.posted",
                "glass.deleted",
                "glass.upserted",
                "harness.context-clear.requested",
                "harness.context-clear.result",
                "harness.diagnostic",
                "harness.limits",
                "harness.observed",
                "harness.session-file",
                "harness.telemetry",
                "harness.timeline",
                "harness.todo.observed",
                "harness.usage",
                "intent.desired",
                "lane.approved",
                "lane.joined",
                "lane.left",
                "lane.marked",
                "lane.moved",
                "loop.round-dispatch",
                "loop.round-result",
                "loop.state",
                "message.closed",
                "message.delivered",
                "message.read",
                "message.sent",
                "message.staged",
                "mission-run.created",
                "mission-run.state",
                "mission.produced",
                "mission.published",
                "observer.observed",
                "observer.refresh-requested",
                "observer.state",
                "operational.failure",
                "operational.recovered",
                "owned-set.revised",
                "planning-session.approved",
                "planning-session.cancelled",
                "planning-session.candidate-submitted",
                "planning-session.previewed",
                "planning-session.question-answered",
                "planning-session.question-requested",
                "planning-session.revision-requested",
                "planning-session.started",
                "principal.key-granted",
                "principal.key-revoked",
                "publication.operation",
                "reconcile.fault",
                "record.repaired",
                "render.applied",
                "repair.applied",
                "resource.observed",
                "revision-proposal.applied",
                "revision-proposal.approved",
                "revision-proposal.cancelled",
                "revision-proposal.created",
                "rule.audited",
                "rule.set",
                "run-generation.created",
                "run-generation.state",
                "run-generation.superseded",
                "runtime.action.deadline-reached",
                "runtime.action.failed",
                "runtime.action.requested",
                "runtime.action.succeeded",
                "runtime.observed",
                "runtime.readiness-deadline-reached",
                "runtime.reconcile-decision",
                "runtime.restart-window-reset",
                "schedule.occurrence-cancelled",
                "schedule.occurrence-reached",
                "schedule.occurrence-scheduled",
                "schedule.work-failed",
                "schedule.work-requested",
                "schedule.work-started",
                "step-run.carried",
                "step-run.retried",
                "step-run.state",
                "subagent.appeared",
                "subagent.ended",
                "subagent.renewed",
                "subscription.batch-sent",
                "subscription.batched",
                "subscription.mission-deferred",
                "subscription.mission-failed",
                "subscription.mission-request-cancelled",
                "subscription.mission-request-released",
                "subscription.mission-requested",
                "subscription.mission-started",
                "subscription.state",
                "subscription.watch-ended",
                "terminal.input.requested",
                "terminal.input.result",
                "transport.observed",
                "work.claimed",
                "work.extended",
                "work.failed",
                "work.person-asked",
                "work.person-cancelled",
                "work.person-done",
                "work.progress",
                "work.released",
                "work.renewed",
                "work.submitted",
                "workspace.observed",
            ]
        );
        assert_eq!(registry.digest().len(), 64);
        assert_eq!(registry.digest(), registry.digest());
    }

    #[test]
    fn registry_rejects_the_removed_plan_names() {
        let registry = registry();
        for subject in ["plan/example", "plan-run/example"] {
            let error = registry
                .validate_subject(subject)
                .expect_err("an old subject family must fail");
            assert_eq!(error.code, "unknown-subject-family");
        }
        for claim in ["plan.published", "plan.produced", "plan-run.created"] {
            assert!(
                registry.claim(claim).is_none(),
                "old claim `{claim}` survived"
            );
        }
    }

    #[test]
    fn account_association_is_a_same_agent_state_transition() {
        let fields = BTreeMap::from([(
            "account".into(),
            Value::String("account/claude/team-a".into()),
        )]);
        let spec = registry()
            .validate_public_claim(
                "agent/run/worker",
                "agent.account",
                &fields,
                Some("agent/run/worker"),
            )
            .unwrap();
        assert_eq!(spec.cardinality, Cardinality::StateTransition);
        assert_eq!(spec.write_policy, WritePolicy::SameSubjectActor);

        assert_eq!(
            registry()
                .validate_public_claim(
                    "agent/run/worker",
                    "agent.account",
                    &fields,
                    Some("agent/run/other"),
                )
                .unwrap_err()
                .code,
            "claim-write-forbidden"
        );
    }

    #[test]
    fn account_association_validates_reference_syntax_without_existence() {
        let valid = BTreeMap::from([(
            "account".into(),
            Value::String("account/claude/missing-but-valid".into()),
        )]);
        registry()
            .validate_claim("agent/run/worker", "agent.account", &valid)
            .unwrap();

        for invalid in ["resource/account", "account", "account//team-a"] {
            let fields = BTreeMap::from([("account".into(), Value::String(invalid.into()))]);
            assert_eq!(
                registry()
                    .validate_claim("agent/run/worker", "agent.account", &fields)
                    .unwrap_err()
                    .code,
                "invalid-subject-reference"
            );
        }
    }

    #[test]
    fn subject_paths_and_file_subjects_are_strict() {
        for invalid in [
            "agent/",
            "agent/run//worker",
            "file/node/tmp/example",
            "file/node:relative/path",
            "file/:/tmp/example",
            "file/node:/tmp/../example",
        ] {
            assert!(registry().validate_subject(invalid).is_err(), "{invalid}");
        }
        registry()
            .validate_subject("file/node:/tmp/example")
            .unwrap();
    }

    #[test]
    fn runtime_emitter_shapes_match_the_registry() {
        let cases = [
            (
                "agent/run/worker",
                "agent.presence",
                BTreeMap::from([
                    ("presence".into(), Value::String("busy".into())),
                    ("reachability".into(), Value::String("indeterminate".into())),
                ]),
            ),
            (
                "file/node:/tmp/result",
                "file.observed",
                BTreeMap::from([
                    ("status".into(), Value::String("observed".into())),
                    ("path".into(), Value::String("/tmp/result".into())),
                    ("mode".into(), Value::from(0o644)),
                ]),
            ),
            (
                "daemon/node",
                "daemon.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("code".into(), Value::String("reconcile-failed".into())),
                    ("reason".into(), Value::String("the pass failed".into())),
                ]),
            ),
            (
                "schedule/run/reminder",
                "runtime.reconcile-decision",
                BTreeMap::from([("decision".into(), Value::String("raise".into()))]),
            ),
            (
                "observer/run/watch",
                "observer.state",
                BTreeMap::from([
                    ("state".into(), Value::String("unreachable".into())),
                    ("revision".into(), Value::String("claim/revision".into())),
                    ("next_check_unix_ms".into(), Value::String("1000".into())),
                ]),
            ),
        ];
        for (subject, kind, fields) in cases {
            registry().validate_claim(subject, kind, &fields).unwrap();
        }
    }

    #[test]
    fn participant_claims_require_dedicated_operations() {
        assert_eq!(
            registry()
                .claims
                .values()
                .filter(|claim| claim.write_policy == WritePolicy::AuthorizedParticipant)
                .map(|claim| claim.kind.as_str())
                .collect::<Vec<_>>(),
            [
                "attention.requested",
                "attention.resolved",
                "lane.approved",
                "lane.joined",
                "lane.left",
                "lane.marked",
                "lane.moved",
                "message.closed",
                "message.read",
                "planning-session.candidate-submitted",
                "planning-session.question-requested",
                "subscription.mission-request-cancelled",
                "subscription.mission-request-released",
                "work.claimed",
                "work.extended",
                "work.failed",
                "work.person-asked",
                "work.person-cancelled",
                "work.person-done",
                "work.progress",
                "work.released",
                "work.renewed",
                "work.submitted",
            ]
        );
        assert_eq!(
            registry()
                .validate_public_claim(
                    "message/example",
                    "message.read",
                    &BTreeMap::from([("status".into(), Value::String("read".into()))]),
                    Some("agent/run/recipient"),
                )
                .unwrap_err()
                .code,
            "claim-write-forbidden"
        );
    }

    #[test]
    fn terminal_input_accepts_more_than_one_request_and_result() {
        assert_eq!(
            registry()
                .claim("terminal.input.requested")
                .unwrap()
                .cardinality,
            Cardinality::Append
        );
        assert_eq!(
            registry()
                .claim("terminal.input.result")
                .unwrap()
                .cardinality,
            Cardinality::Append
        );
    }

    #[test]
    fn terminal_work_reports_are_once_per_attempt() {
        for kind in ["work.submitted", "work.failed"] {
            let spec = registry().claim(kind).unwrap();
            assert_eq!(spec.cardinality, Cardinality::OncePerAttempt);
            assert!(spec.fields.contains_key("attempt"));
        }
    }

    #[test]
    fn checked_in_schema_document_matches_the_registry() {
        assert_eq!(
            include_str!("../../../docs/st3/schema.md"),
            registry().markdown()
        );
    }

    #[test]
    fn custom_claims_need_both_custom_namespaces() {
        let fields = BTreeMap::new();
        registry()
            .validate_claim("custom/acme/fact", "custom.acme.found", &fields)
            .unwrap();
        assert_eq!(
            registry()
                .validate_claim("resource/acme", "custom.acme.found", &fields)
                .unwrap_err()
                .code,
            "invalid-custom-claim-subject"
        );
        assert_eq!(
            registry()
                .validate_claim("custom/acme/fact", "resource.observed", &fields)
                .unwrap_err()
                .code,
            "invalid-custom-subject-claim"
        );
        assert_eq!(
            registry()
                .validate_claim("custom/acme/fact", "custom.acme", &fields)
                .unwrap_err()
                .code,
            "invalid-custom-claim-kind"
        );
        assert_eq!(
            registry().validate_subject("custom/acme").unwrap_err().code,
            "invalid-custom-subject"
        );
    }

    #[test]
    fn custom_resource_kinds_need_a_namespace_and_name() {
        assert!(is_custom_resource_kind("custom.openai.account"));
        assert!(!is_custom_resource_kind("custom.account"));
        assert!(!is_custom_resource_kind("custom..account"));
    }

    #[test]
    fn gate_verdicts_are_strict() {
        let mut fields = BTreeMap::from([("verdict".into(), Value::String("maybe".into()))]);
        assert_eq!(
            registry()
                .validate_claim("gate-operation/run/step/gate", "gate.result", &fields)
                .unwrap_err()
                .code,
            "invalid-claim-field"
        );
        fields.insert("verdict".into(), Value::String("error".into()));
        registry()
            .validate_claim("gate-operation/run/step/gate", "gate.result", &fields)
            .unwrap();
    }

    #[test]
    fn issue_openers_are_optional_subject_references() {
        registry()
            .validate_resource_facts("vcs.issue", &BTreeMap::new())
            .unwrap();
        let facts = BTreeMap::from([
            ("opened_by".into(), Value::String("agent/node.author".into())),
            ("opened_by_run".into(), Value::String("mission-run/author".into())),
        ]);
        registry().validate_resource_facts("vcs.issue", &facts).unwrap();
        registry()
            .validate_public_claim(
                "resource/github/acme/demo/issue/8",
                "resource.observed",
                &BTreeMap::from([
                    ("kind".into(), Value::String("vcs.issue".into())),
                    ("facts".into(), serde_json::to_value(&facts).unwrap()),
                ]),
                Some("agent/node.author"),
            )
            .unwrap();
        for name in ["opened_by", "opened_by_run"] {
            let mut invalid = facts.clone();
            invalid.insert(name.into(), Value::String("not-a-subject".into()));
            assert!(registry().validate_resource_facts("vcs.issue", &invalid).is_err());
        }
    }

    #[test]
    fn built_in_resources_are_strict_and_custom_resources_are_open() {
        let pull = registry().resource("vcs.pull-request").unwrap();
        assert!(!pull.open_facts);
        assert!(pull.fields.contains_key("head"));
        let reference = registry().resource("vcs.ref").unwrap();
        assert!(!reference.open_facts);
        assert!(reference.fields.contains_key("head"));
        assert!(reference.fields.contains_key("ancestors"));
        registry()
            .validate_resource_facts(
                "vcs.ref",
                &BTreeMap::from([
                    ("head".into(), Value::String("abc123".into())),
                    (
                        "ancestors".into(),
                        Value::Array(vec![Value::String("refs/heads/topic".into())]),
                    ),
                ]),
            )
            .unwrap();
        assert!(
            registry()
                .validate_resource_kind("custom.acme.ticket")
                .unwrap()
                .open_facts
        );
        assert_eq!(
            registry()
                .validate_resource_facts(
                    "vcs.commit",
                    &BTreeMap::from([("unexpected".into(), Value::Bool(true))]),
                )
                .unwrap_err()
                .code,
            "unknown-resource-field"
        );
    }

    #[test]
    fn public_claim_admission_uses_the_registry_write_policy() {
        assert!(
            registry()
                .validate_public_claim(
                    "resource/example",
                    "resource.observed",
                    &BTreeMap::new(),
                    Some("person/requester"),
                )
                .is_ok()
        );
        let observed = BTreeMap::from([("status".into(), Value::String("running".into()))]);
        registry()
            .validate_public_claim(
                "exec/run/task",
                "runtime.observed",
                &observed,
                Some("exec/run/task"),
            )
            .unwrap();
        assert_eq!(
            registry()
                .validate_public_claim(
                    "exec/run/task",
                    "runtime.observed",
                    &observed,
                    Some("agent/run/other"),
                )
                .unwrap_err()
                .code,
            "claim-write-forbidden"
        );
        assert_eq!(
            registry()
                .validate_public_claim(
                    "host/example",
                    "transport.observed",
                    &BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    Some("host/example"),
                )
                .unwrap_err()
                .code,
            "claim-write-forbidden"
        );
    }
}
