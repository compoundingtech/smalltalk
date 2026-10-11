//! Person-authored current context. A directive note informs; it never grants authority.
use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

use crate::{ValidationError, error};

pub const KIND: &str = "person.directive-note-set";
pub const MAX_TEXT_BYTES: usize = 4096;

pub fn validate_actor(subject: &str, kind: &str, actor: Option<&str>) -> Result<(), ValidationError> {
    if kind == KIND
        && (!subject.strip_prefix("person/").is_some_and(|name| !name.is_empty() && !name.contains('/'))
            || actor != Some(subject))
    {
        return Err(error(
            "directive-note-forbidden",
            "only the subject person may set or clear their directive note",
        ));
    }
    Ok(())
}

/// Accept RFC 3339 UTC timestamps, including the equivalent explicit +00:00 offset.
pub fn expiry(value: &str) -> Result<DateTime<FixedOffset>, ValidationError> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .filter(|time| time.offset().local_minus_utc() == 0)
        .filter(|_| value.ends_with('Z') || value.ends_with("+00:00"))
        .ok_or_else(|| error("invalid-directive-note", "expires_at must be a valid UTC timestamp"))
}

pub fn validate_fields(fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    match fields.get("text") {
        Some(Value::Null) => {}
        Some(Value::String(text)) if !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES => {}
        _ => return Err(error("invalid-directive-note", "text must be null or nonblank text of at most 4096 UTF-8 bytes")),
    }
    if let Some(value) = fields.get("expires_at") {
        expiry(value.as_str().ok_or_else(|| error("invalid-directive-note", "expires_at must be a UTC timestamp string"))?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;
    use serde_json::json;

    #[test]
    fn directive_note_schema_bounds_null_and_utc() {
        let text_field = &registry().claim(KIND).unwrap().fields["text"];
        assert!(text_field.required && text_field.nullable);
        assert_eq!(text_field.value_type, crate::ValueType::String);
        for text in [json!(null), json!("é".repeat(2048))] {
            assert!(registry().validate_claim("person/avery", KIND, &BTreeMap::from([("text".into(), text)])).is_ok());
        }
        for text in [json!(""), json!(" \n\t"), json!("é".repeat(2049)), json!(42), json!([])] {
            assert!(registry().validate_claim("person/avery", KIND, &BTreeMap::from([("text".into(), text)])).is_err());
        }
        assert!(registry().validate_claim("person/avery", KIND, &BTreeMap::new()).is_err());
        for at in ["2099-01-01T00:00:00Z", "2099-01-01T00:00:00.123+00:00"] {
            assert!(expiry(at).is_ok());
        }
        for at in ["2099-02-30T00:00:00Z", "2099-01-01", "2099-01-01T00:00:00-00:00", "2099-01-01T00:00:00+01:00"] {
            assert!(expiry(at).is_err());
        }
        for at in [Value::Null, json!(42)] {
            assert!(validate_fields(&BTreeMap::from([("text".into(), json!("context")), ("expires_at".into(), at)])).is_err());
        }
    }

    #[test]
    fn directive_note_schema_requires_exact_subject_person() {
        for actor in [None, Some("person/robin"), Some("agent/worker")] {
            assert!(validate_actor("person/avery", KIND, actor).is_err());
            assert!(registry().validate_public_claim("person/avery", KIND, &BTreeMap::from([("text".into(), json!("Approved"))]), actor).is_err());
        }
        assert!(validate_actor("person/avery", KIND, Some("person/avery")).is_ok());
        for subject in ["person/", "person/avery/other", "agent/worker"] {
            assert!(validate_actor(subject, KIND, Some(subject)).is_err());
        }
    }
}
