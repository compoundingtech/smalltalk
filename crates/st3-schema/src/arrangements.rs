//! Person-owned arrangements: strict shared admission, independent of the real writer.
use super::{ValidationError, error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_OPERATIONS: usize = 1024;
pub const MAX_FOLDERS: usize = 1024;
pub const MAX_PLACEMENTS: usize = 4096;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Local projected resource bound leaves room in the client-v0 response envelope.
pub const MAX_RESOURCE_BYTES: usize = 512 * 1024;
pub const MAX_ARRANGEMENTS: usize = 100;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub enum Operation {
    #[serde(rename = "create")]
    Create { name: String },
    #[serde(rename = "rename")]
    Rename { name: String },
    #[serde(rename = "folder.create")]
    FolderCreate { id: String, name: String, parent: Option<String>, key: String },
    #[serde(rename = "folder.rename")]
    FolderRename { id: String, name: String },
    #[serde(rename = "folder.move")]
    FolderMove { id: String, parent: Option<String>, key: String },
    #[serde(rename = "folder.delete")]
    FolderDelete { id: String },
    #[serde(rename = "subject.place")]
    SubjectPlace { subject: String, folder: Option<String>, key: String },
    #[serde(rename = "retire")]
    Retire {},
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Register<T> { pub value: T, pub revision: String }
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Position { pub parent: Option<String>, pub key: String }
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement { pub folder: Option<String>, pub key: String }
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Folder {
    pub name: Register<String>,
    pub position: Register<Position>,
    pub tombstone: Option<Register<bool>>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub version: u8,
    pub name: Register<String>,
    pub folders: BTreeMap<String, Folder>,
    pub placements: BTreeMap<String, Register<Placement>>,
}
/// A folder-only arrangement: its placements are ordered memberships on the arrangement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BodyV2 {
    pub version: u8,
    pub name: Register<String>,
    pub folders: BTreeMap<String, Folder>,
}

/// An arrangement body decoded strictly by its `version`; there is no cross-version fallback.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum VersionedBody {
    V1(Body),
    V2(BodyV2),
}

impl VersionedBody {
    pub fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }
}

impl<'de> Deserialize<'de> for VersionedBody {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        body_for_read(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

pub fn body_for_read(value: &Value) -> Result<VersionedBody, ValidationError> {
    let invalid = |e: serde_json::Error| error("invalid-arrangement-body", e.to_string());
    match value.get("version").and_then(Value::as_u64) {
        Some(1) => Body::deserialize(value).map(VersionedBody::V1).map_err(invalid),
        Some(2) => BodyV2::deserialize(value).map(VersionedBody::V2).map_err(invalid),
        _ => Err(error("invalid-arrangement-body", "an arrangement body must declare version 1 or 2")),
    }
}

/// The layout an edit addresses: absent means version 1; version 2 is folder-only.
pub fn layout_version(fields: &BTreeMap<String, Value>) -> Result<u8, ValidationError> {
    match fields.get("version") {
        None => Ok(1),
        Some(value) => match value.as_u64() {
            Some(1) => Ok(1),
            Some(2) => Ok(2),
            _ => Err(error("invalid-arrangement-version", "arrangement version must be 1 or 2")),
        },
    }
}

/// Optional idempotent action metadata shared by arrangement and membership edits.
pub(crate) fn action_metadata(fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    match (fields.get("action_id"), fields.get("action_digest")) {
        (None, None) => Ok(()),
        (Some(id), Some(digest)) if id.as_str().is_some_and(|id| !id.trim().is_empty() && id.len() <= 512)
            && digest.as_str().is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) => Ok(()),
        _ => Err(error("invalid-arrangement-action", "action_id and lowercase SHA-256 action_digest must be supplied together")),
    }
}

pub fn valid_uuid(id: &str) -> bool {
    super::glasses::valid_uuid(id) && id.as_bytes()[14] == b'7'
        && matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
}
pub fn owner(subject: &str) -> Result<&str, ValidationError> {
    let tail = subject.strip_prefix("arrangement/").unwrap_or_default();
    let mut parts = tail.split('/');
    let person = parts.next();
    let name = parts.next().unwrap_or_default();
    let id = parts.next().unwrap_or_default();
    if person != Some("person") || name.is_empty() || !valid_uuid(id) || parts.next().is_some()
        || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(error("invalid-arrangement-subject", "an arrangement subject must be arrangement/person/NAME/lowercase-UUIDv7"));
    }
    Ok(&tail[..tail.len() - id.len() - 1])
}

/// Free mode allows a real person owner or a trusted fleet agent, never anonymous/system actors.
/// Fleet trust is established by the enclosing authenticated transport/replication envelope.
pub fn validate_actor(subject: &str, actor: Option<&str>) -> Result<(), ValidationError> {
    if !subject.starts_with("arrangement/") { return Ok(()); }
    if actor == Some(owner(subject)?) || actor.is_some_and(|a| a.starts_with("agent/") && super::registry().validate_subject(a).is_ok()) {
        return Ok(());
    }
    Err(error("arrangement-owner-forbidden", "only the person owner or a trusted fleet agent may edit an arrangement as its real actor"))
}

/// The canonical base-62 fractional-indexing alphabet, with a variable-length integer prefix.
pub fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    let Some(&head) = bytes.first() else { return false; };
    let integer_len = match head { b'a'..=b'z' => usize::from(head - b'a') + 2, b'A'..=b'Z' => usize::from(b'Z' - head) + 2, _ => return false };
    bytes.len() <= MAX_KEY_BYTES && bytes.len() >= integer_len
        && bytes[1..].iter().all(u8::is_ascii_alphanumeric)
        && (bytes.len() == integer_len || bytes.last() != Some(&b'0'))
        && !(head == b'A' && bytes[1..integer_len].iter().all(|b| *b == b'0'))
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum Touched<'a> {
    Name,
    FolderName(&'a str),
    FolderPosition(&'a str),
    FolderTombstone(&'a str),
    Placement(&'a str),
    Retired,
}

type OperationRegisters<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>, Option<&'a str>, [Option<Touched<'a>>; 2]);

pub fn operations(subject: &str, fields: &BTreeMap<String, Value>) -> Result<Vec<Operation>, ValidationError> {
    if fields.get("owner").and_then(Value::as_str) != Some(owner(subject)?) {
        return Err(error("arrangement-owner-forbidden", "owner must match the immutable person in the arrangement subject"));
    }
    action_metadata(fields)?;
    let version = layout_version(fields)?;
    let value = fields.get("operations").ok_or_else(|| error("missing-claim-field", "arrangement edits require operations"))?;
    if serde_json::to_vec(value).map_err(|e| error("invalid-arrangement-operations", e.to_string()))?.len() > MAX_BODY_BYTES {
        return Err(error("arrangement-body-too-large", "arrangement operations exceed 1 MiB"));
    }
    if value.as_array().is_some_and(|ops| ops.iter().any(|op| {
        match op.get("op").and_then(Value::as_str) {
            Some("folder.create" | "folder.move") => op.get("parent").is_none(),
            Some("subject.place") => op.get("folder").is_none(),
            _ => false,
        }
    })) {
        return Err(error("invalid-arrangement-operations", "nullable parent/folder fields must be explicitly present"));
    }
    let operations: Vec<Operation> = Vec::deserialize(value).map_err(|e| error("invalid-arrangement-operations", e.to_string()))?;
    if operations.is_empty() || operations.len() > MAX_OPERATIONS {
        return Err(error("invalid-arrangement-operations", "an edit requires 1 through 1024 operations"));
    }
    let mut touched = BTreeSet::new();
    for op in &operations {
        let (id, name, target, key, registers): OperationRegisters<'_> = match op {
            Operation::Create { name } | Operation::Rename { name } => (None, Some(name), None, None, [Some(Touched::Name), None]),
            Operation::FolderCreate { id, name, parent, key } => (Some(id), Some(name), parent.as_deref(), Some(key), [Some(Touched::FolderName(id)), Some(Touched::FolderPosition(id))]),
            Operation::FolderRename { id, name } => (Some(id), Some(name), None, None, [Some(Touched::FolderName(id)), None]),
            Operation::FolderMove { id, parent, key } => (Some(id), None, parent.as_deref(), Some(key), [Some(Touched::FolderPosition(id)), None]),
            Operation::FolderDelete { id } => (Some(id), None, None, None, [Some(Touched::FolderTombstone(id)), None]),
            Operation::SubjectPlace { subject, folder, key } => {
                super::registry().validate_subject(subject).map_err(|_| error("invalid-subject-reference", "placement requires a registered graph subject"))?;
                if subject.starts_with("pty/") || subject.starts_with("session/") {
                    return Err(error("invalid-subject-reference", "placements address stable graph subjects, never PTY or session IDs"));
                }
                (None, None, folder.as_deref(), Some(key), [Some(Touched::Placement(subject)), None])
            }
            Operation::Retire {} => (None, None, None, None, [Some(Touched::Retired), None]),
        };
        if id.is_some_and(|id| !valid_uuid(id)) || target.is_some_and(|id| !valid_uuid(id)) {
            return Err(error("invalid-arrangement-folder", "folder IDs must be lowercase UUIDv7"));
        }
        if name.is_some_and(|n| n.trim().is_empty() || n.len() > MAX_NAME_BYTES || n.chars().any(char::is_control)) {
            return Err(error("invalid-arrangement-name", "names must be nonblank, at most 256 bytes, and contain no control characters"));
        }
        if key.is_some_and(|k| !valid_key(k)) { return Err(error("invalid-arrangement-key", "position keys must be canonical base-62 fractional keys")); }
        if registers.into_iter().flatten().any(|r| !touched.insert(r)) { return Err(error("invalid-arrangement-operations", "an atomic edit may touch each register only once")); }
    }
    if operations.iter().any(|op| matches!(op, Operation::Retire {})) && operations.len() != 1 {
        return Err(error("invalid-arrangement-operations", "retirement must be the only operation"));
    }
    if version == 2 && operations.iter().any(|op| matches!(op, Operation::SubjectPlace { .. })) {
        return Err(error("invalid-arrangement-operations", "version 2 arrangements place subjects through ordered memberships"));
    }
    drop(touched);
    Ok(operations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const SUBJECT: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
    #[test]
    fn strict_operations_identity_and_canonical_keys() {
        for key in ["a0", "a0V", "a1", "Zz", "b00"] { assert!(valid_key(key)); }
        for key in ["", "0", "a", "a00", "a0!", "A00000000000000000000000000"] { assert!(!valid_key(key)); }
        for value in [json!([{"op":"rename","name":"ok","extra":true}]), json!([{"op":"rename","name":" "}]), json!([{"op":"rename","name":"one"},{"op":"rename","name":"two"}])] {
            let fields = serde_json::from_value(json!({"owner":"person/ada","operations":value})).unwrap();
            assert!(operations(SUBJECT, &fields).is_err());
        }
        assert!(validate_actor(SUBJECT, Some("agent/fleet/fixture-arrangements/seat")).is_ok());
        assert!(validate_actor(SUBJECT, Some("person/other")).is_err());
        assert!(validate_actor(SUBJECT, None).is_err());
        assert!(owner("arrangement/person/ada/019a0000-0000-4000-8000-000000000001").is_err());
    }
    #[test]
    fn operation_boundaries_references_and_action_metadata_are_shared_admission() {
        let fields = |ops| serde_json::from_value::<BTreeMap<String,Value>>(json!({"owner":"person/ada","operations":ops})).unwrap();
        assert_eq!(operations(SUBJECT,&fields(json!([{"op":"rename","name":"x".repeat(MAX_NAME_BYTES)}]))).unwrap(),vec![Operation::Rename { name:"x".repeat(MAX_NAME_BYTES) }]);
        for ops in [
            json!([{"op":"rename","name":"x".repeat(MAX_NAME_BYTES+1)}]),
            json!([{"op":"folder.move","id":"019a0000-0000-7000-8000-000000000010","key":"a0"}]),
            json!([{"op":"subject.place","subject":"pty/transient","folder":null,"key":"a0"}]),
            json!([{"op":"subject.place","subject":"session/transient","folder":null,"key":"a0"}]),
            json!([{"op":"folder.delete","id":"019a0000-0000-4000-8000-000000000010"}]),
            json!([{"op":"retire"},{"op":"rename","name":"gone"}]),
            json!([]),
            json!((0..=MAX_OPERATIONS).map(|id| json!({"op":"subject.place","subject":format!("agent/fleet/{id}"),"folder":null,"key":"a0"})).collect::<Vec<_>>()),
        ] { assert!(operations(SUBJECT,&fields(ops)).is_err()); }
        let mut action = fields(json!([{"op":"rename","name":"ok"}]));
        action.insert("action_id".into(),json!("action-one"));
        assert_eq!(operations(SUBJECT,&action).unwrap_err().code,"invalid-arrangement-action");
        action.insert("action_digest".into(),json!("0".repeat(64)));
        assert_eq!(operations(SUBJECT,&action).unwrap(),vec![Operation::Rename { name:"ok".into() }]);
    }
    #[test]
    fn version_two_is_explicit_folder_only_and_bodies_decode_strictly() {
        let fields = |version: Value, ops: Value| {
            let mut fields = serde_json::from_value::<BTreeMap<String, Value>>(json!({"owner":"person/ada","operations":ops})).unwrap();
            if !version.is_null() { fields.insert("version".into(), version); }
            fields
        };
        let place = json!([{"op":"subject.place","subject":"agent/fleet/seat","folder":null,"key":"a0"}]);
        let create = json!([{"op":"create","name":"Sidebar"}]);
        assert_eq!(layout_version(&fields(Value::Null, create.clone())).unwrap(), 1);
        assert!(operations(SUBJECT, &fields(Value::Null, place.clone())).is_ok());
        assert!(operations(SUBJECT, &fields(json!(1), place.clone())).is_ok());
        assert_eq!(operations(SUBJECT, &fields(json!(2), place)).unwrap_err().code, "invalid-arrangement-operations");
        assert_eq!(layout_version(&fields(json!(2), create.clone())).unwrap(), 2);
        assert!(operations(SUBJECT, &fields(json!(2), create.clone())).is_ok());
        for version in [json!(0), json!(3), json!("2"), json!(2.0), json!(null)] {
            let mut invalid = fields(Value::Null, create.clone());
            invalid.insert("version".into(), version);
            assert_eq!(operations(SUBJECT, &invalid).unwrap_err().code, "invalid-arrangement-version");
        }
        let name = json!({"value":"Sidebar","revision":"r1"});
        let v1 = json!({"version":1,"name":name,"folders":{},"placements":{}});
        let v2 = json!({"version":2,"name":name,"folders":{}});
        assert_eq!(body_for_read(&v1).unwrap().version(), 1);
        assert_eq!(body_for_read(&v2).unwrap().version(), 2);
        assert_eq!(serde_json::from_value::<VersionedBody>(v2.clone()).unwrap(), body_for_read(&v2).unwrap());
        assert_eq!(serde_json::to_value(body_for_read(&v2).unwrap()).unwrap(), v2);
        for invalid in [
            json!({"version":2,"name":name,"folders":{},"placements":{}}),
            json!({"version":1,"name":name,"folders":{}}),
            json!({"version":3,"name":name,"folders":{}}),
            json!({"name":name,"folders":{}}),
        ] {
            assert_eq!(body_for_read(&invalid).unwrap_err().code, "invalid-arrangement-body");
            assert!(serde_json::from_value::<VersionedBody>(invalid).is_err());
        }
        let registry = super::super::registry();
        assert_eq!(
            registry.claims["arrangement.edited"].fields.keys().map(String::as_str).collect::<Vec<_>>(),
            ["action_digest", "action_id", "operations", "owner", "version"]
        );
        assert!(registry.validate_claim(SUBJECT, "arrangement.edited", &fields(json!(2), create)).is_ok());
    }
}
