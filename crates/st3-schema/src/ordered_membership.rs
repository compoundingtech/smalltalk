//! Ordered memberships: retained `(container, member)` pairs with one LWW `{bucket, key}` position.
//!
//! `ordered-membership.edited` is an append claim on a container whose lifecycle entry declares the
//! capability. Each atomic edit places or removes distinct members; a removal is a retained absent
//! value, never a physical deletion.
//!
//! Each pair record is bounded: container and member subjects pass the schema registry's
//! `validate_subject`, which caps every subject at 512 bytes; keys are at most `MAX_KEY_BYTES`;
//! an edit's operations are at most `MAX_EDIT_BYTES`.
use super::lifecycle::{self, EffectiveBucket};
use super::{ValidationError, arrangements, error};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_OPERATIONS: usize = arrangements::MAX_OPERATIONS;
pub const MAX_KEY_BYTES: usize = arrangements::MAX_KEY_BYTES;
pub const MAX_EDIT_BYTES: usize = arrangements::MAX_BODY_BYTES;

/// Local membership invalidation state. The frontier is opaque and comparable only
/// within one host; it is neither a canonical revision nor checkpoint authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub live_count: u64,
    pub changed_index: u64,
}

/// A member's position: `bucket: None` is the container root.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    #[serde(deserialize_with = "explicit_bucket")]
    pub bucket: Option<String>,
    pub key: String,
}

/// `bucket` must be present on the wire, `null` for the root. A plain `Option` field would read
/// a missing bucket as the root; `deserialize_with` makes serde report the field as missing.
fn explicit_bucket<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Operation {
    /// Place or move a member; `bucket` must be present, `null` for the root.
    Place {
        member: String,
        #[serde(deserialize_with = "explicit_bucket")]
        bucket: Option<String>,
        key: String,
    },
    /// Record the retained absent position.
    Remove { member: String },
}

impl Operation {
    pub fn member(&self) -> &str {
        match self {
            Self::Place { member, .. } | Self::Remove { member } => member,
        }
    }

    /// The written position, `None` for a removal.
    pub fn position(&self) -> Option<Position> {
        match self {
            Self::Place { bucket, key, .. } => Some(Position { bucket: bucket.clone(), key: key.clone() }),
            Self::Remove { .. } => None,
        }
    }
}

/// Strict admission for `ordered-membership.edited` on `subject`.
pub fn operations(subject: &str, fields: &BTreeMap<String, Value>) -> Result<Vec<Operation>, ValidationError> {
    let (_, capability) = lifecycle::registry().container(subject)?;
    let (owner, valid_bucket): (&str, fn(&str) -> bool) = match capability.effective_bucket {
        EffectiveBucket::ArrangementFolders { .. } => (arrangements::owner(subject)?, arrangements::valid_uuid),
    };
    if fields.get("owner").and_then(Value::as_str) != Some(owner) {
        return Err(error("membership-owner-forbidden", "owner must match the immutable person in the container subject"));
    }
    arrangements::action_metadata(fields)?;
    let value = fields
        .get("operations")
        .ok_or_else(|| error("missing-claim-field", "membership edits require operations"))?;
    if serde_json::to_vec(value)
        .map_err(|e| error("invalid-membership-operations", e.to_string()))?
        .len()
        > MAX_EDIT_BYTES
    {
        return Err(error("membership-edit-too-large", "membership operations exceed 1 MiB"));
    }
    let operations: Vec<Operation> =
        Vec::deserialize(value).map_err(|e| error("invalid-membership-operations", e.to_string()))?;
    if operations.is_empty() || operations.len() > MAX_OPERATIONS {
        return Err(error("invalid-membership-operations", "an edit requires 1 through 1024 operations"));
    }
    let mut members = BTreeSet::new();
    for op in &operations {
        let member = op.member();
        if member.starts_with("pty/") || member.starts_with("session/") {
            return Err(error("invalid-membership-member", "memberships address stable graph subjects, never PTY or session IDs"));
        }
        lifecycle::registry()
            .member(member)
            .map_err(|e| error("invalid-membership-member", e.message))?;
        if member == subject {
            return Err(error("invalid-membership-member", "a container cannot be its own member"));
        }
        if !members.insert(member) {
            return Err(error("invalid-membership-operations", "an atomic edit may touch each member only once"));
        }
        if let Operation::Place { bucket, key, .. } = op {
            if bucket.as_deref().is_some_and(|id| !valid_bucket(id)) {
                return Err(error("invalid-membership-bucket", "buckets must be the container's valid bucket IDs"));
            }
            if !arrangements::valid_key(key) {
                return Err(error("invalid-membership-key", "position keys must be canonical base-62 fractional keys"));
            }
        }
    }
    Ok(operations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUBJECT: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
    const FOLDER: &str = "019a0000-0000-7000-8000-000000000010";

    fn fields(ops: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(json!({"owner":"person/ada","operations":ops})).unwrap()
    }

    #[test]
    fn accepts_atomic_unique_places_and_removals() {
        let ops = operations(
            SUBJECT,
            &fields(json!([
                {"op":"place","member":"agent/fleet/seat","bucket":null,"key":"a0"},
                {"op":"place","member":"mission/m1","bucket":FOLDER,"key":"a1"},
                {"op":"remove","member":"doc/readme"},
            ])),
        )
        .unwrap();
        assert_eq!(
            ops,
            [
                Operation::Place { member: "agent/fleet/seat".into(), bucket: None, key: "a0".into() },
                Operation::Place { member: "mission/m1".into(), bucket: Some(FOLDER.into()), key: "a1".into() },
                Operation::Remove { member: "doc/readme".into() },
            ]
        );
        assert_eq!(ops[1].position(), Some(Position { bucket: Some(FOLDER.into()), key: "a1".into() }));
        assert_eq!(ops[2].position(), None);
        let mut action = fields(json!([{"op":"remove","member":"agent/fleet/seat"}]));
        action.insert("action_id".into(), json!("action-one"));
        assert_eq!(operations(SUBJECT, &action).unwrap_err().code, "invalid-arrangement-action");
        action.insert("action_digest".into(), json!("0".repeat(64)));
        assert!(operations(SUBJECT, &action).is_ok());
    }

    #[test]
    fn rejects_malformed_edits() {
        for (ops, code) in [
            (json!([]), "invalid-membership-operations"),
            (json!([{"op":"place","member":"agent/fleet/seat","key":"a0"}]), "invalid-membership-operations"),
            (json!([{"op":"place","member":"agent/fleet/seat","bucket":null,"key":"a0","extra":1}]), "invalid-membership-operations"),
            (json!([{"op":"move","member":"agent/fleet/seat"}]), "invalid-membership-operations"),
            (json!([{"op":"remove","member":"agent/fleet/seat","bucket":null}]), "invalid-membership-operations"),
            (json!([{"op":"remove","member":"agent/fleet/seat"},{"op":"place","member":"agent/fleet/seat","bucket":null,"key":"a0"}]), "invalid-membership-operations"),
            (json!([{"op":"place","member":"pty/run/one","bucket":null,"key":"a0"}]), "invalid-membership-member"),
            (json!([{"op":"remove","member":"session/abc"}]), "invalid-membership-member"),
            (json!([{"op":"remove","member":"daemon/node"}]), "invalid-membership-member"),
            (json!([{"op":"remove","member":"nope/x"}]), "invalid-membership-member"),
            (json!([{"op":"remove","member":"agent/"}]), "invalid-membership-member"),
            (json!([{"op":"remove","member":SUBJECT}]), "invalid-membership-member"),
            (json!([{"op":"place","member":"agent/fleet/seat","bucket":"019a0000-0000-4000-8000-000000000010","key":"a0"}]), "invalid-membership-bucket"),
            (json!([{"op":"place","member":"agent/fleet/seat","bucket":null,"key":"a00"}]), "invalid-membership-key"),
            (
                json!((0..=MAX_OPERATIONS).map(|id| json!({"op":"remove","member":format!("agent/fleet/{id}")})).collect::<Vec<_>>()),
                "invalid-membership-operations",
            ),
        ] {
            assert_eq!(operations(SUBJECT, &fields(ops.clone())).unwrap_err().code, code, "{ops}");
        }
        let mut wrong_owner = fields(json!([{"op":"remove","member":"agent/fleet/seat"}]));
        wrong_owner.insert("owner".into(), json!("person/bob"));
        assert_eq!(operations(SUBJECT, &wrong_owner).unwrap_err().code, "membership-owner-forbidden");
        assert_eq!(
            operations("glass/person/ada/019a0000-0000-7000-8000-000000000001", &fields(json!([{"op":"remove","member":"agent/fleet/seat"}])))
                .unwrap_err()
                .code,
            "unsupported-membership-container"
        );
        let long_container = format!("arrangement/person/{}/019a0000-0000-7000-8000-000000000001", "a".repeat(512));
        assert_eq!(
            operations(&long_container, &fields(json!([{"op":"remove","member":"agent/fleet/seat"}]))).unwrap_err().code,
            "invalid-claim-subject"
        );
        let long_member = format!("agent/fleet/{}", "a".repeat(512));
        assert_eq!(
            operations(SUBJECT, &fields(json!([{"op":"remove","member":long_member}]))).unwrap_err().code,
            "invalid-membership-member"
        );
        let long_key = format!("a0{}", "1".repeat(MAX_KEY_BYTES - 1));
        assert_eq!(
            operations(SUBJECT, &fields(json!([{"op":"place","member":"agent/fleet/seat","bucket":null,"key":long_key}]))).unwrap_err().code,
            "invalid-membership-key"
        );
        let max_key = format!("a0{}", "1".repeat(MAX_KEY_BYTES - 2));
        assert!(operations(SUBJECT, &fields(json!([{"op":"place","member":"agent/fleet/seat","bucket":null,"key":max_key}]))).is_ok());
        let oversized = (0..MAX_OPERATIONS)
            .map(|id| json!({"op":"remove","member":format!("agent/fleet/{id}-{}", "a".repeat(1000))}))
            .collect::<Vec<_>>();
        assert_eq!(operations(SUBJECT, &fields(json!(oversized))).unwrap_err().code, "membership-edit-too-large");
    }

    #[test]
    fn bucket_must_be_explicit_and_null_is_the_root() {
        let place = serde_json::from_value::<Operation>(json!({"op":"place","member":"agent/fleet/seat","bucket":null,"key":"a0"})).unwrap();
        assert_eq!(place.position(), Some(Position { bucket: None, key: "a0".into() }));
        let error = serde_json::from_value::<Operation>(json!({"op":"place","member":"agent/fleet/seat","key":"a0"})).unwrap_err();
        assert!(error.to_string().contains("missing field `bucket`"), "{error}");
        let root: Position = serde_json::from_value(json!({"bucket":null,"key":"a0"})).unwrap();
        assert_eq!(root.bucket, None);
        assert_eq!(serde_json::to_value(&root).unwrap(), json!({"bucket":null,"key":"a0"}));
        let error = serde_json::from_value::<Position>(json!({"key":"a0"})).unwrap_err();
        assert!(error.to_string().contains("missing field `bucket`"), "{error}");
    }

    #[test]
    fn registry_admits_only_ordinary_actor_checked_membership_claims() {
        let registry = super::super::registry();
        let spec = registry.claim(lifecycle::MEMBERSHIP_CLAIM).unwrap();
        assert_eq!(spec.subjects, ["arrangement"]);
        assert_eq!(spec.write_policy, super::super::WritePolicy::OrdinaryClient);
        assert_eq!(spec.cardinality, super::super::Cardinality::Append);
        assert_eq!(spec.retention, super::super::Retention::Durable);
        assert_eq!(spec.fields.keys().map(String::as_str).collect::<Vec<_>>(), ["action_digest", "action_id", "operations", "owner"]);
        let ops = fields(json!([{"op":"remove","member":"agent/fleet/seat"}]));
        assert!(registry.validate_public_claim(SUBJECT, lifecycle::MEMBERSHIP_CLAIM, &ops, Some("person/ada")).is_ok());
        assert!(registry.validate_public_claim(SUBJECT, lifecycle::MEMBERSHIP_CLAIM, &ops, Some("agent/fleet/seat")).is_ok());
        assert!(registry.validate_public_claim(SUBJECT, lifecycle::MEMBERSHIP_CLAIM, &ops, Some("person/bob")).is_err());
        assert!(registry.validate_public_claim(SUBJECT, lifecycle::MEMBERSHIP_CLAIM, &ops, None).is_err());
        assert_eq!(
            registry.validate_claim(SUBJECT, lifecycle::MEMBERSHIP_CLAIM, &fields(json!([]))).unwrap_err().code,
            "invalid-membership-operations"
        );
        assert_eq!(
            registry.validate_claim("agent/fleet/seat", lifecycle::MEMBERSHIP_CLAIM, &ops).unwrap_err().code,
            "invalid-claim-subject"
        );
    }
}
