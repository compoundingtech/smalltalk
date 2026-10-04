//! Data-only contracts for registered custom subjects. No executable projection hooks.
use super::{ValidationError, ValueType, error, registry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const REGISTRY_PREFIX: &str = "custom/st3-kinds/";
pub const REGISTERED: &str = "custom.st3-kinds.registered";
pub const LANGUAGE: &str = "custom-projection.v1";
pub const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub kind: String,
    pub namespace: String,
    pub version: u32,
    pub language: String,
    pub subject_prefix: String,
    pub creation_kind: String,
    pub claims: BTreeMap<String, ClaimSchema>,
    pub slots: BTreeMap<String, Slot>,
    pub fields: BTreeMap<String, Expr>,
    pub attention: Option<Attention>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimSchema {
    pub authority: Authority,
    pub fields: BTreeMap<String, Field>,
    #[serde(default)]
    pub additional_fields: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Authority {
    Creator,
    Owner,
    Recipient,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub value_type: ValueType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub reference_families: Vec<String>,
    #[serde(default)]
    pub document: bool,
    #[serde(default)]
    pub claim: bool,
    #[serde(default)]
    pub min_items: usize,
    #[serde(default = "default_items")]
    pub max_items: usize,
    #[serde(default = "default_bytes")]
    pub max_bytes: usize,
}
fn default_items() -> usize {
    256
}
fn default_bytes() -> usize {
    MAX_BYTES
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub kind: String,
    pub select: Select,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Select {
    First,
    Last,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Expr {
    Field { slot: String, field: String },
    Actor { slot: String },
    ClaimId { slot: String },
    Constant { value: Value },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Predicate {
    Exists { slot: String },
    Eq { left: Expr, right: Expr },
    All { args: Vec<Predicate> },
    Any { args: Vec<Predicate> },
    Not { arg: Box<Predicate> },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attention {
    pub when: Predicate,
    /// The first creation claim's field, never a later claim's recipient.
    pub recipient_field: String,
    pub title: Expr,
    pub detail: Expr,
    pub episode: Expr,
    pub reply: Reply,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub kind: String,
    pub fields: BTreeMap<String, Field>,
    #[serde(default)]
    pub bindings: BTreeMap<String, Expr>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Basis {
    pub subject: String,
    pub kinds: Vec<String>,
    pub revision: String,
}

impl Manifest {
    pub fn subject(&self) -> String {
        format!(
            "{REGISTRY_PREFIX}{}/v{}",
            self.kind.replace('.', "/"),
            self.version
        )
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        if serde_json::to_vec(self)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > MAX_BYTES
        {
            return Err(invalid("manifest exceeds 64 KiB"));
        }
        let identifier = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        };
        if !identifier(&self.namespace)
            || self.kind.len() > 128
            || !self.kind.split('.').all(identifier)
            || self.version == 0
            || matches!(self.namespace.as_str(), "client" | "st3-kinds")
            || !self
                .subject_prefix
                .starts_with(&format!("custom/{}/", self.namespace))
            || !self.subject_prefix.ends_with('/')
            || self.subject_prefix.len() > 512
            || registry()
                .validate_subject(&format!("{}sample", self.subject_prefix))
                .is_err()
        {
            return Err(invalid(
                "kind, namespace, version or exclusive subject prefix is invalid",
            ));
        }
        if self.language != LANGUAGE {
            return Err(error(
                "unsupported-custom-projection",
                "unsupported custom projection language",
            ));
        }
        if self.claims.is_empty()
            || self.claims.len() > 32
            || self.slots.is_empty()
            || self.slots.len() > 32
            || self.fields.len() > 64
        {
            return Err(invalid(
                "a manifest needs 1..32 claims/slots and at most 64 output fields",
            ));
        }
        let prefix = format!("custom.{}.", self.namespace);
        for (kind, schema) in &self.claims {
            if !kind.starts_with(&prefix)
                || !super::is_custom_claim_kind(kind)
                || schema.fields.len() > 64
            {
                return Err(invalid(
                    "claim kind is outside the namespace or has too many fields",
                ));
            }
            for (name, field) in &schema.fields {
                validate_field_spec(name, field)?;
            }
        }
        let creation = self
            .claims
            .get(&self.creation_kind)
            .ok_or_else(|| invalid("creation kind is undeclared"))?;
        if creation.authority != Authority::Creator
            || self
                .claims
                .iter()
                .any(|(k, s)| k != &self.creation_kind && s.authority == Authority::Creator)
        {
            return Err(invalid("only the creation kind may have creator authority"));
        }
        for (name, slot) in &self.slots {
            if !identifier(name) || !self.claims.contains_key(&slot.kind) {
                return Err(invalid("slot name or claim kind is invalid"));
            }
        }
        for (name, expr) in &self.fields {
            if !identifier(name) {
                return Err(invalid("output field name is invalid"));
            }
            self.validate_expr(expr)?;
        }
        if let Some(attention) = &self.attention {
            let field = creation
                .fields
                .get(&attention.recipient_field)
                .ok_or_else(|| invalid("recipient must be a creation field"))?;
            if !field.required
                || field.value_type != ValueType::SubjectReference
                || field.reference_families != ["person"]
            {
                return Err(invalid("recipient must be a required person reference"));
            }
            let mut count = 0;
            self.validate_predicate(&attention.when, 0, &mut count)?;
            for expr in [&attention.title, &attention.detail, &attention.episode] {
                self.validate_expr(expr)?;
            }
            let reply_schema = self
                .claims
                .get(&attention.reply.kind)
                .ok_or_else(|| invalid("reply kind is undeclared"))?;
            if reply_schema.authority != Authority::Recipient {
                return Err(invalid("reply needs recipient authority"));
            }
            if attention.reply.fields.len() > 32 || attention.reply.fields.is_empty() {
                return Err(invalid("reply needs 1..32 fields"));
            }
            for (name, field) in &attention.reply.fields {
                validate_field_spec(name, field)?;
                let target = reply_schema
                    .fields
                    .get(name)
                    .ok_or_else(|| invalid("reply field is undeclared"))?;
                if serde_json::to_value(target).ok() != serde_json::to_value(field).ok() {
                    return Err(invalid("reply field must match its claim schema"));
                }
                if attention.reply.bindings.contains_key(name) {
                    return Err(invalid("reply binding overlaps user input"));
                }
            }
            for (name, expr) in &attention.reply.bindings {
                if !reply_schema.fields.contains_key(name) {
                    return Err(invalid("reply binding is undeclared"));
                }
                self.validate_expr(expr)?;
            }
            if reply_schema.fields.iter().any(|(n, f)| {
                f.required
                    && !attention.reply.fields.contains_key(n)
                    && !attention.reply.bindings.contains_key(n)
            }) {
                return Err(invalid("reply cannot provide all required claim fields"));
            }
        } else if self
            .claims
            .values()
            .any(|s| s.authority == Authority::Recipient)
        {
            return Err(invalid("recipient authority needs an attention recipient"));
        }
        Ok(())
    }
    fn validate_expr(&self, expr: &Expr) -> Result<(), ValidationError> {
        let slot = match expr {
            Expr::Field { slot, field } => {
                let s = self
                    .slots
                    .get(slot)
                    .ok_or_else(|| invalid("expression names unknown slot"))?;
                if !self.claims[&s.kind].fields.contains_key(field) {
                    return Err(invalid("expression names undeclared field"));
                }
                slot
            }
            Expr::Actor { slot } | Expr::ClaimId { slot } => slot,
            Expr::Constant { value } => {
                if !value.is_null()
                    && !value.is_boolean()
                    && !value.is_number()
                    && !value.is_string()
                {
                    return Err(invalid("constants must be scalar"));
                }
                return Ok(());
            }
        };
        if !self.slots.contains_key(slot) {
            return Err(invalid("expression names unknown slot"));
        }
        Ok(())
    }
    fn validate_predicate(
        &self,
        p: &Predicate,
        depth: usize,
        count: &mut usize,
    ) -> Result<(), ValidationError> {
        *count += 1;
        if depth > 16 || *count > 128 {
            return Err(invalid("predicate exceeds its depth/node bound"));
        }
        match p {
            Predicate::Exists { slot } => {
                if !self.slots.contains_key(slot) {
                    return Err(invalid("predicate names unknown slot"));
                }
            }
            Predicate::Eq { left, right } => {
                self.validate_expr(left)?;
                self.validate_expr(right)?;
            }
            Predicate::All { args } | Predicate::Any { args } => {
                for arg in args {
                    self.validate_predicate(arg, depth + 1, count)?;
                }
            }
            Predicate::Not { arg } => self.validate_predicate(arg, depth + 1, count)?,
        }
        Ok(())
    }
}
pub fn validate_field_spec(name: &str, f: &Field) -> Result<(), ValidationError> {
    if name.starts_with('_')
        || name.is_empty()
        || name.len() > 128
        || f.max_bytes > MAX_BYTES
        || f.max_items > 256
        || f.min_items > f.max_items
        || f.values.len() > 256
    {
        return Err(invalid("field name or bounds are invalid"));
    }
    if (f.document || f.claim) && f.value_type != ValueType::String {
        return Err(invalid("document/claim references need string fields"));
    }
    if f.document && f.claim {
        return Err(invalid(
            "a field cannot be both document and claim reference",
        ));
    }
    Ok(())
}
pub fn validate_fields(
    schema: &ClaimSchema,
    fields: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    if serde_json::to_vec(fields)
        .map_err(|e| invalid(e.to_string()))?
        .len()
        > MAX_BYTES
    {
        return Err(invalid("claim exceeds 64 KiB"));
    }
    for (name, field) in &schema.fields {
        match fields.get(name) {
            None if field.required => return Err(invalid(format!("missing field {name}"))),
            Some(value) => validate_field(name, value, field)?,
            _ => {}
        }
    }
    if fields.keys().any(|k| {
        !schema.fields.contains_key(k)
            && k != "_registration"
            && k != "_basis"
            && !schema.additional_fields
    }) {
        return Err(invalid("unknown typed claim field"));
    }
    Ok(())
}
pub fn validate_field(name: &str, v: &Value, f: &Field) -> Result<(), ValidationError> {
    let valid = match f.value_type {
        ValueType::Any => true,
        ValueType::Boolean => v.is_boolean(),
        ValueType::Integer => v.as_i64().is_some() || v.as_u64().is_some(),
        ValueType::Number => v.is_number(),
        ValueType::String | ValueType::SubjectReference => v.is_string(),
        ValueType::Array => v.is_array(),
        ValueType::Object => v.is_object(),
    };
    if !valid
        || serde_json::to_vec(v)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > f.max_bytes
    {
        return Err(invalid(format!("wrong type or size for {name}")));
    }
    if let Some(items) = v.as_array() {
        if items.len() < f.min_items
            || items.len() > f.max_items
            || (!f.values.is_empty()
                && items
                    .iter()
                    .any(|v| !v.as_str().is_some_and(|s| f.values.iter().any(|a| a == s))))
        {
            return Err(invalid(format!("invalid selection/bounds for {name}")));
        }
    } else if !f.values.is_empty() && !v.as_str().is_some_and(|s| f.values.iter().any(|a| a == s)) {
        return Err(invalid(format!("invalid value for {name}")));
    }
    if f.value_type == ValueType::SubjectReference {
        let s = v.as_str().unwrap();
        let subject = registry().validate_subject(s)?;
        if !f.reference_families.is_empty() && !f.reference_families.contains(&subject.family) {
            return Err(invalid(format!("wrong reference family for {name}")));
        }
    }
    if f.document {
        let s = v.as_str().unwrap();
        if !s.starts_with("doc/")
            || !s
                .rsplit_once('@')
                .is_some_and(|(_, h)| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("document references must pin a hash"));
        }
    }
    if f.claim
        && !v
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid("claim references must name an immutable claim ID"));
    }
    Ok(())
}
fn invalid(message: impl Into<String>) -> ValidationError {
    error("invalid-custom-schema", message)
}
