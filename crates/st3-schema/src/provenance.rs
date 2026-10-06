//! Fixed, inert metadata bound to one mission revision. No field is resolved or executed.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ValidationError, error};

pub const MAX_BYTES: usize = 16 * 1024;
pub const MAX_REASON_BYTES: usize = 2 * 1024;
pub const MAX_TEXT_BYTES: usize = 1024;
pub const MAX_REFERENCES: usize = 32;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub repository: String,
    pub commit: String,
    pub path: String,
    pub renderer: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub reason: String,
    pub source: Source,
    #[serde(default)]
    pub decision: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

fn invalid(message: impl Into<String>) -> ValidationError {
    error("invalid-mission-provenance", message)
}

impl Provenance {
    pub fn validate(&self) -> Result<(), ValidationError> {
        fn text(name: &str, value: &str, max: usize) -> Result<(), ValidationError> {
            if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
                return Err(invalid(format!(
                    "provenance `{name}` must be nonempty, at most {max} UTF-8 bytes, and contain no control characters"
                )));
            }
            Ok(())
        }
        text("reason", &self.reason, MAX_REASON_BYTES)?;
        for (name, value) in [
            ("repository", &self.source.repository),
            ("path", &self.source.path),
            ("renderer", &self.source.renderer),
        ] {
            text(name, value, MAX_TEXT_BYTES)?;
        }
        if !matches!(self.source.commit.len(), 40 | 64)
            || !self
                .source
                .commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid(
                "provenance `commit` must be a full 40- or 64-character hexadecimal commit id",
            ));
        }
        if self.source.path.starts_with('/')
            || self.source.path.contains('\\')
            || self
                .source
                .path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(invalid(
                "provenance `path` must be a repository-relative path without empty, dot or parent components",
            ));
        }
        for (name, references) in [("decision", &self.decision), ("evidence", &self.evidence)] {
            if references.len() > MAX_REFERENCES {
                return Err(invalid(format!(
                    "provenance allows at most {MAX_REFERENCES} `{name}` references"
                )));
            }
            for reference in references {
                text(name, reference, MAX_TEXT_BYTES)?;
            }
        }
        if serde_json::to_vec(self)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > MAX_BYTES
        {
            return Err(invalid("serialized provenance exceeds 16 KiB"));
        }
        Ok(())
    }
}

pub fn validate_claim(
    subject: &str,
    fields: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    let mission = fields
        .get("mission")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("provenance needs a mission subject"))?;
    let revision = fields
        .get("revision")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("provenance needs a revision"))?;
    if !mission.starts_with("mission/")
        || mission.contains('@')
        || revision.len() != 64
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        || subject != format!("{mission}@{revision}")
    {
        return Err(invalid(
            "provenance subject must be its exact mission subject followed by @ and the 64-character revision",
        ));
    }
    let value = fields
        .get("provenance")
        .ok_or_else(|| invalid("provenance block is missing"))?;
    if serde_json::to_vec(value)
        .map_err(|e| invalid(e.to_string()))?
        .len()
        > MAX_BYTES
    {
        return Err(invalid("serialized provenance exceeds 16 KiB"));
    }
    let provenance: Provenance = serde_json::from_value(value.clone())
        .map_err(|e| invalid(format!("malformed provenance: {e}")))?;
    provenance.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cardinality, Retention, WritePolicy, registry};
    use serde_json::json;

    #[test]
    fn provenance_claim_validates_the_exact_binding_and_nested_bounds() {
        let revision = "b".repeat(64);
        let subject = format!("mission/orchard@{revision}");
        let fields: BTreeMap<String, Value> = serde_json::from_value(json!({
            "mission": "mission/orchard", "revision": revision,
            "provenance": {"reason": "Approved source.", "source": {"repository": "orchard", "commit": "a".repeat(40), "path": "missions/orchard.kdl", "renderer": "orchard-v1"}, "decision": ["opaque"], "evidence": []}
        })).unwrap();
        let spec = registry()
            .validate_claim(&subject, "mission.provenance", &fields)
            .unwrap();
        assert_eq!(spec.cardinality, Cardinality::Once);
        assert_eq!(spec.retention, Retention::Durable);
        assert_eq!(spec.write_policy, WritePolicy::SystemOnly);
        assert!(
            registry()
                .validate_claim("mission/another", "mission.provenance", &fields)
                .is_err()
        );
        for invalid in [
            json!({"reason": "Missing source."}),
            json!({"reason": "Unknown field.", "source": fields["provenance"]["source"], "extra": "no"}),
            json!({"reason": "Bad type.", "source": fields["provenance"]["source"], "decision": [42]}),
            json!({"reason": "x".repeat(MAX_REASON_BYTES + 1), "source": fields["provenance"]["source"]}),
            json!({"reason": "Oversized.", "source": fields["provenance"]["source"], "decision": vec!["x".repeat(MAX_TEXT_BYTES); 16]}),
        ] {
            let mut malformed = fields.clone();
            malformed.insert("provenance".into(), invalid);
            assert!(
                registry()
                    .validate_claim(&subject, "mission.provenance", &malformed)
                    .is_err()
            );
        }
        assert!(
            registry()
                .validate_public_claim(
                    &subject,
                    "mission.provenance",
                    &fields,
                    Some("person/avery")
                )
                .is_err()
        );
    }
}
