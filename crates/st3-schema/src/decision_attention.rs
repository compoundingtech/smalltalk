//! Source-owned decision metadata; private request and answer text never enter this contract.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ValidationError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAsk {
    pub key: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub decision_id: String,
    pub request_id: String,
    pub q: u64,
    pub source_sequence: u64,
    pub source_revision: String,
    pub person: String,
    pub decision_kind: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_id: Option<String>,
    pub revived: bool,
    pub activation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_ask: Option<NativeAsk>,
}

impl Observation {
    /// Transport recovery is not a new source decision and cannot change its resolution.
    pub fn same_source(&self, other: &Self) -> bool {
        self.decision_id == other.decision_id
            && self.request_id == other.request_id
            && self.q == other.q
            && self.source_sequence == other.source_sequence
            && self.source_revision == other.source_revision
            && self.person == other.person
            && self.decision_kind == other.decision_kind
            && self.state == other.state
            && self.answer_id == other.answer_id
            && self.revived == other.revived
            && self.activation == other.activation
    }
}

fn record_id(value: &str) -> bool {
    value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn reference(value: &str, family: &str) -> bool {
    value.len() <= 512
        && value.strip_prefix(family).is_some_and(|rest| !rest.is_empty()
            && rest.split('/').all(|part| !part.is_empty() && part != "." && part != ".."))
        && !value.chars().any(|character| character.is_whitespace() || character.is_control())
}

pub(crate) fn validate(fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    let invalid = || crate::error("invalid-decision-observation", "invalid bounded decision metadata");
    let text = |key: &str| fields.get(key).and_then(Value::as_str);
    let decision = text("decision_id").ok_or_else(invalid)?;
    let Some(identity) = decision.strip_prefix("resource/axe/decision/") else {
        return Err(invalid());
    };
    let Some((scope, request)) = identity.split_once('/') else { return Err(invalid()); };
    let number = |key: &str| fields.get(key).and_then(Value::as_u64)
        .is_some_and(|number| number > 0 && number <= 9_007_199_254_740_991);
    let answer = text("answer_id");
    if !hex(scope, 64) || Some(request) != text("request_id") || !record_id(request)
        || !number("q") || !number("source_sequence")
        || text("source_revision").is_none_or(|revision| !hex(revision, 64))
        || text("activation").is_none_or(|activation| !hex(activation, 16))
        || text("person").is_none_or(|person| !reference(person, "person/"))
        || answer.is_some_and(|answer| !record_id(answer))
        || (text("state") == Some("answered") && answer.is_none())
        || (text("state") == Some("pending") && answer.is_some())
    {
        return Err(invalid());
    }
    if let Some(value) = fields.get("native_ask") {
        let ask = value.as_object().ok_or_else(invalid)?;
        let text = |key: &str| ask.get(key).and_then(Value::as_str);
        if ask.keys().any(|key| !matches!(key.as_str(), "key" | "subject" | "run" | "episode" | "status"))
            || text("subject").is_none_or(|subject| !reference(subject, "step-run/"))
            || ask.get("run").is_some_and(|run| run.as_str().is_none_or(|run| !reference(run, "mission-run/")))
            || ask.get("episode").is_some_and(|episode| episode.as_str().is_none_or(|episode|
                episode.is_empty() || episode.len() > 512 || episode.chars().any(char::is_control)))
            || text("key").is_none_or(|key| key.len() > 512
                || key.strip_prefix("axe:decision:v1:").and_then(|rest| rest.strip_prefix(decision))
                    .is_none_or(|rest| !rest.starts_with(':'))
                || key.chars().any(|character| character.is_whitespace() || character.is_control()))
            || text("status").is_none_or(|status| status.is_empty() || status.len() > 64
                || status.chars().any(|character| character.is_whitespace() || character.is_control()))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields() -> BTreeMap<String, Value> {
        serde_json::from_value(json!({
            "decision_id": format!("resource/axe/decision/{}/q38req", "a".repeat(64)),
            "request_id": "q38req", "q": 38, "source_sequence": 1,
            "source_revision": "1".repeat(64), "person": "person/example",
            "decision_kind": "blocker", "state": "pending", "revived": false,
            "activation": "0000000000000000"
        })).unwrap()
    }

    #[test]
    fn admission_rejects_foreign_publishers_private_fields_and_unsafe_source_versions() {
        let registry = crate::registry();
        let subject = "agent/example/author";
        let fields = fields();
        assert!(registry.validate_public_claim(subject, "decision.observed", &fields, Some(subject)).is_ok());
        assert!(registry.validate_public_claim(subject, "decision.observed", &fields, Some("agent/example/other")).is_err());
        let mut private = fields.clone();
        private.insert("answer".into(), json!("private text"));
        assert!(registry.validate_claim(subject, "decision.observed", &private).is_err());
        for sequence in [0_u64, 9_007_199_254_740_992] {
            let mut unsafe_version = fields.clone();
            unsafe_version.insert("source_sequence".into(), json!(sequence));
            assert!(registry.validate_claim(subject, "decision.observed", &unsafe_version).is_err());
        }
        let mut contradictory = fields;
        contradictory.insert("answer_id".into(), json!("ans001"));
        assert!(registry.validate_claim(subject, "decision.observed", &contradictory).is_err());
        contradictory.insert("state".into(), json!("answered"));
        assert!(registry.validate_claim(subject, "decision.observed", &contradictory).is_ok());
    }

    #[test]
    fn native_link_must_name_its_own_source_and_cannot_smuggle_private_fields() {
        let registry = crate::registry();
        let mut fields = fields();
        let decision = fields["decision_id"].as_str().unwrap();
        let mut link = json!({
            "key":format!("axe:decision:v1:{decision}:1-q38req:ask:0000000000000000"),
            "subject":"step-run/example/ask", "status":"ready"
        });
        fields.insert("native_ask".into(), link.clone());
        assert!(registry.validate_claim("agent/example/author", "decision.observed", &fields).is_ok());
        link["body"] = json!("private request text");
        fields.insert("native_ask".into(), link.clone());
        assert!(registry.validate_claim("agent/example/author", "decision.observed", &fields).is_err());
        link.as_object_mut().unwrap().remove("body");
        link["key"] = json!("axe:decision:v1:resource/axe/decision/other/other1:1-other1:ask:0000000000000000");
        fields.insert("native_ask".into(), link);
        assert!(registry.validate_claim("agent/example/author", "decision.observed", &fields).is_err());
    }
}
