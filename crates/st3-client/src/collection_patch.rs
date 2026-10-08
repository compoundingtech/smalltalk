use crate::{ClientError, Resource};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Replacement of top-level values in one existing row; null is a value. Identity and
/// resource kind are immutable. An unavailable base requires an authoritative snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionPatch {
    pub id: String,
    pub fields: Map<String, Value>,
    pub removed_fields: Vec<String>,
}

/// Validate every patch before changing any row. A malformed or missing base leaves the
/// held window unchanged; its caller marks it stale and resubscribes for a full snapshot.
pub fn apply_collection_patches(
    rows: &mut BTreeMap<String, Resource>,
    patches: &[CollectionPatch],
) -> Result<(), ClientError> {
    let fail =
        || ClientError::Protocol("collection field patch needs an authoritative snapshot".into());
    if patches.len() > 200 {
        return Err(fail());
    }
    let mut ids = BTreeSet::new();
    let mut prepared = Vec::new();
    for patch in patches {
        if !ids.insert(&patch.id) || patch.fields.len() > 128 || patch.removed_fields.len() > 128 {
            return Err(fail());
        }
        let base = rows.get(&patch.id).ok_or_else(fail)?;
        let mut value = serde_json::to_value(base).map_err(|_| fail())?;
        let object = value.as_object_mut().ok_or_else(fail)?;
        let mut removed = BTreeSet::new();
        for name in &patch.removed_fields {
            if matches!(name.as_str(), "id" | "kind")
                || patch.fields.contains_key(name)
                || !removed.insert(name)
            {
                return Err(fail());
            }
            object.remove(name);
        }
        for (name, value) in &patch.fields {
            if matches!(name.as_str(), "id" | "kind") {
                return Err(fail());
            }
            object.insert(name.clone(), value.clone());
        }
        let resource: Resource = serde_json::from_value(value).map_err(|_| fail())?;
        if resource.header().id != patch.id {
            return Err(fail());
        }
        prepared.push((patch.id.clone(), resource));
    }
    rows.extend(prepared);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn row() -> Resource {
        serde_json::from_value(json!({"id":"future/a","kind":"future-resource","revision":"r1","updated_at":"now","nullable":"before","removed":1,"unchanged":{"future":true}})).unwrap()
    }
    #[test]
    fn field_replacement_preserves_unknown_fields_and_distinguishes_null_from_removal() {
        let mut rows = BTreeMap::from([("future/a".into(), row())]);
        let patch:CollectionPatch=serde_json::from_value(json!({"id":"future/a","fields":{"nullable":null,"revision":"r2"},"removed_fields":["removed"]})).unwrap();
        apply_collection_patches(&mut rows, &[patch]).unwrap();
        let actual = serde_json::to_value(&rows["future/a"]).unwrap();
        assert_eq!(actual["revision"], "r2");
        assert_eq!(actual["nullable"], Value::Null);
        assert!(actual.get("removed").is_none());
        assert_eq!(actual["unchanged"], json!({"future":true}));
    }
    #[test]
    fn malformed_or_missing_patch_base_cannot_partially_update_the_window() {
        for bad in [
            json!({"id":"future/missing","fields":{},"removed_fields":[]}),
            json!({"id":"future/a","fields":{"id":"future/other"},"removed_fields":[]}),
            json!({"id":"future/a","fields":{},"removed_fields":["kind"]}),
            json!({"id":"future/a","fields":{"revision":"r2"},"removed_fields":["revision"]}),
        ] {
            let mut rows = BTreeMap::from([("future/a".into(), row())]);
            let original = rows.clone();
            let valid: CollectionPatch = serde_json::from_value(
                json!({"id":"future/a","fields":{"revision":"new"},"removed_fields":[]}),
            )
            .unwrap();
            let invalid = serde_json::from_value(bad).unwrap();
            assert!(apply_collection_patches(&mut rows, &[valid, invalid]).is_err());
            assert_eq!(rows, original);
        }
    }
}
