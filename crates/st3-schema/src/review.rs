//! Review identities are provider-independent; adapter availability is a runtime prerequisite.
use crate::{Registry, ValidationError, error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const KINDS: [&str; 4] = [
    "gate.head-bound", "gate.review-verdict", "gate.review-withdrawn", "gate.remediation-attached",
];
pub const REQUEST_FIELDS: [&str; 9] = [
    "gate_subject", "generation", "episode", "change", "head", "scope", "author", "binding_claim", "delta_ref",
];

/// Open, namespaced kind. New adapters do not require changing the claim vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub kind: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Head {
    #[serde(rename = "git.commit")]
    GitCommit { sha: String },
    #[serde(rename = "patch.manifest")]
    PatchManifest { digest: String },
}

impl Head {
    /// Structural admission accepts full SHA-1 or SHA-256 Git IDs. The observation adapter
    /// additionally checks the repository's object format before binding a head.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let (value, lengths): (&str, &[usize]) = match self {
            Self::GitCommit { sha } => (sha, &[40, 64]),
            Self::PatchManifest { digest } => (digest, &[64]),
        };
        if !lengths.contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(error("invalid-review-head", "review heads require a full lowercase object digest"));
        }
        Ok(())
    }
}

impl Change {
    pub fn validate(&self, registry: &Registry, head: &Head) -> Result<(), ValidationError> {
        let namespaced = self.kind.split('.').count() >= 2
            && self.kind.split('.').all(crate::valid_identifier_part);
        if self.kind != "patch" && !namespaced {
            return Err(error("invalid-review-change", "change kind must be namespaced (or patch)"));
        }
        let family = &registry.validate_subject(&self.subject)?.family;
        let compatible = match self.kind.as_str() {
            "github.pull-request" | "git.branch" => family == "resource" && matches!(head, Head::GitCommit { .. }),
            "patch" => family == "doc" && matches!(head, Head::PatchManifest { .. }),
            _ => matches!(family.as_str(), "resource" | "doc" | "custom"),
        };
        if !compatible {
            return Err(error("invalid-review-change", "change subject and head do not match its kind"));
        }
        head.validate()
    }

    /// Canonical tuple encoding for inclusion in request/fold identities; tags and full
    /// subjects are never omitted. Request generation/attempt/episode fences remain separate.
    pub fn canonical_identity(&self, registry: &Registry, head: &Head) -> Result<Vec<u8>, ValidationError> {
        self.validate(registry, head)?;
        serde_json::to_vec(&(self, head)).map_err(|e| error("invalid-review-change", e.to_string()))
    }
}

pub(crate) fn validate_claim(registry: &Registry, kind: &str, fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    if !KINDS.contains(&kind) && kind != "gate.requested" { return Ok(()); }
    let head_name = if matches!(kind, "gate.review-verdict" | "gate.remediation-attached") { "reviewed_head" } else { "head" };
    if let Some(value) = fields.get(head_name) {
        let head: Head = serde_json::from_value(value.clone()).map_err(|e| error("invalid-review-head", e.to_string()))?;
        head.validate()?;
        if let Some(change) = fields.get("change") {
            let change: Change = serde_json::from_value(change.clone()).map_err(|e| error("invalid-review-change", e.to_string()))?;
            change.validate(registry, &head)?;
        }
    } else if fields.contains_key("change") {
        return Err(error("invalid-review-head", "a review change requires its tagged head"));
    }
    if kind == "gate.requested" && REQUEST_FIELDS.iter().any(|name| fields.contains_key(*name)) {
        for name in REQUEST_FIELDS {
            if fields.get(name).is_none_or(Value::is_null) { return Err(error("missing-claim-field", format!("review request needs {name}"))); }
        }
    }
    for name in ["attempt", "boundary_attempt", "episode", "author_attempt"] {
        if fields.get(name).is_some_and(|value| value.as_u64().is_none()) {
            return Err(error("invalid-claim-field", format!("review fence {name} must be unsigned")));
        }
    }
    Ok(())
}

/// Claim-ID references checkpoint readers must preserve even when the referenced kind has
/// a drop rule. Artifact hashes and subject references are not claim IDs.
pub fn claim_references<'a>(kind: &str, body: &'a Value) -> impl Iterator<Item = &'a str> {
    let fields = (KINDS.contains(&kind) || kind == "gate.requested")
        .then(|| body.get("fields")).flatten();
    let direct = ["binding_claim", "previous_binding", "request", "verdict", "carry_forward_from", "attempt_receipt", "observation"]
        .into_iter().filter_map(move |name| fields?.get(name)?.as_str());
    let arrays = ["evidence", "finding_refs", "authorization_evidence"]
        .into_iter().filter_map(move |name| fields?.get(name)?.as_array())
        .flatten().filter_map(Value::as_str);
    direct.chain(arrays)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_registry_inventory_and_legacy_request_admission() {
        let registry = crate::registry();
        let mut older = registry.clone();
        for kind in KINDS {
            let spec = registry.claim(kind).unwrap();
            assert_eq!(spec.subjects, ["gate-operation"]);
            assert_eq!(spec.cardinality, crate::Cardinality::Append);
            assert_eq!(crate::retention::policy().kind(kind).class, crate::retention::Class::Durable);
            older.claims.remove(kind);
        }
        let request = older.claims.get_mut("gate.requested").unwrap();
        for name in REQUEST_FIELDS { assert!(request.fields.remove(name).is_some()); }
        assert_ne!(registry.digest(), older.digest());
        let mut fields = BTreeMap::from([
            ("gate_subject".into(),json!("gate-operation/tester")),
            ("generation".into(),json!("run-generation/tester")), ("episode".into(),json!(1)),
            ("change".into(),json!({"kind":"future.review","subject":"resource/project/review"})),
            ("head".into(),json!({"kind":"git.commit","sha":"a".repeat(64)})),
            ("scope".into(),json!({"coverage":["implementation"]})),
            ("author".into(),json!("agent/alex")), ("binding_claim".into(),json!("binding")),
            ("delta_ref".into(),json!("doc/project/delta")),
        ]);
        registry.validate_claim("gate-operation/tester","gate.requested",&fields).unwrap();
        assert_eq!(older.validate_claim("gate-operation/tester","gate.requested",&fields).unwrap_err().code,"unknown-claim-field");
        for name in REQUEST_FIELDS {
            let value = fields.remove(name).unwrap();
            assert!(registry.validate_claim("gate-operation/tester","gate.requested",&fields).is_err());
            assert!(!registry.claim("gate.requested").unwrap().fields[name].nullable);
            fields.insert(name.into(), Value::Null);
            assert!(registry.validate_claim("gate-operation/tester","gate.requested",&fields).is_err());
            fields.insert(name.into(),value);
        }
        fields.insert("mode".into(),json!("review-at-head"));
        assert_eq!(registry.validate_claim("gate-operation/tester","gate.requested",&fields).unwrap_err().code,"invalid-claim-field");
        assert_eq!(registry.validate_claim("gate-operation/tester","gate.requested",&BTreeMap::new()).unwrap().kind,"gate.requested");
    }

    #[test]
    fn typed_change_heads_and_open_kinds_have_distinct_canonical_identities() {
        let registry = crate::registry();
        let git = Head::GitCommit { sha: "a".repeat(40) };
        let patch = Head::PatchManifest { digest: "b".repeat(64) };
        let mut identities = std::collections::BTreeSet::new();
        for (kind, subject, head) in [
            ("github.pull-request", "resource/project/review", &git),
            ("git.branch", "resource/project/review", &git),
            ("patch", "doc/project/review", &patch),
            ("other.change", "custom/project/review", &git),
        ] {
            let change = Change { kind: kind.into(), subject: subject.into() };
            let encoded = change.canonical_identity(registry, head).unwrap();
            assert!(identities.insert(encoded.clone()));
            let decoded: (Change, Head) = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, (change, head.clone()));
        }
        for (kind, subject, head) in [
            ("github.pull-request", "doc/project/review", &git),
            ("git.branch", "resource/project/review", &patch),
            ("patch", "doc/project/review", &git),
            ("not-namespaced", "resource/project/review", &git),
        ] {
            assert!(Change { kind: kind.into(), subject: subject.into() }.validate(registry, head).is_err());
        }
        for head in [json!({"kind":"git.commit","sha":"abcdef"}), json!({"kind":"patch.manifest","digest":"x".repeat(64)}), json!({"kind":"git.commit","sha":"a".repeat(40),"extra":true})] {
            assert!(match serde_json::from_value::<Head>(head) { Ok(head) => head.validate().is_err(), Err(_) => true });
        }
    }
}
