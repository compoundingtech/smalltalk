//! Shared admission validation for the private glass resource.
use super::{ValidationError, error};
use serde_json::Value;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_GLASSES: usize = 100;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 1024;

pub fn owner(subject: &str) -> Result<&str, ValidationError> {
    let tail = subject.strip_prefix("glass/").unwrap_or_default();
    let parts: Vec<_> = tail.split('/').collect();
    if parts.len() != 3 || parts[0] != "person" || parts[1].is_empty() || !valid_uuid(parts[2]) {
        return Err(error(
            "invalid-glass-subject",
            "a glass subject must be glass/person/NAME/UUID",
        ));
    }
    Ok(&tail[..tail.len() - parts[2].len() - 1])
}

pub fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

pub fn validate_owner(subject: &str, actor: Option<&str>) -> Result<(), ValidationError> {
    if subject.starts_with("glass/") && Some(owner(subject)?) != actor {
        return Err(error(
            "glass-owner-forbidden",
            "only a glass's person may write its claims",
        ));
    }
    Ok(())
}

pub fn validate_body(body: &Value) -> Result<(), ValidationError> {
    let invalid = || {
        error(
            "invalid-glass-body",
            "a glass needs a name and bounded tabs of opaque panes or binary splits",
        )
    };
    if serde_json::to_vec(body).map_err(|_| invalid())?.len() > MAX_BODY_BYTES {
        return Err(error(
            "glass-body-too-large",
            "a glass body must be at most 64 KiB",
        ));
    }
    let obj = body.as_object().ok_or_else(invalid)?;
    if obj.len() != 2
        || !obj
            .get("name")
            .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(invalid());
    }
    let tabs = obj
        .get("tabs")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if tabs.len() > MAX_NODES {
        return Err(invalid());
    }
    let mut stack = Vec::new();
    for tab in tabs {
        let tab = tab.as_object().ok_or_else(invalid)?;
        if tab.keys().any(|k| k != "layout" && k != "title")
            || tab.get("title").is_some_and(|v| !v.is_string())
        {
            return Err(invalid());
        }
        stack.push((tab.get("layout").ok_or_else(invalid)?, 1));
    }
    let mut nodes = 0;
    while let Some((layout, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_NODES || depth > MAX_DEPTH {
            return Err(invalid());
        }
        let obj = layout.as_object().ok_or_else(invalid)?;
        if obj.len() == 1
            && obj
                .get("pane")
                .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        {
            continue;
        }
        if obj.len() != 2
            || !obj
                .get("split")
                .is_some_and(|v| matches!(v.as_str(), Some("right" | "below")))
        {
            return Err(invalid());
        }
        let children = obj
            .get("children")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        if children.len() != 2 {
            return Err(invalid());
        }
        for child in children {
            stack.push((child, depth + 1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn glass_body_rejects_size_depth_and_invalid_structure() {
        let body = json!({"name":"A workspace","tabs":[{"title":"Home","layout":{"pane":"opaque:anything"}}]});
        validate_body(&body).unwrap();
        validate_body(&json!({"name":"Home only", "tabs":[]})).unwrap();
        assert!(validate_body(&json!({"name":"", "tabs":[]})).is_err());
        let mut exact = body.clone();
        exact["name"] = json!("");
        let overhead = serde_json::to_vec(&exact).unwrap().len();
        exact["name"] = json!("x".repeat(MAX_BODY_BYTES - overhead));
        assert_eq!(serde_json::to_vec(&exact).unwrap().len(), MAX_BODY_BYTES);
        validate_body(&exact).unwrap();
        for layout in [
            json!({"pane":"x","split":"right"}),
            json!({"split":"right","children":[{"pane":"x"}]}),
            json!({"split":"up","children":[{"pane":"x"},{"pane":"y"}]}),
            json!({"pane":""}),
        ] {
            assert!(validate_body(&json!({"name":"Main","tabs":[{"layout":layout}]})).is_err());
        }
        let mut deep = json!({"pane":"x"});
        for _ in 0..MAX_DEPTH {
            deep = json!({"split":"below","children":[deep,{"pane":"x"}]});
        }
        assert!(validate_body(&json!({"name":"Main","tabs":[{"layout":deep}]})).is_err());
        assert!(
            validate_body(
                &json!({"name":"x".repeat(MAX_BODY_BYTES),"tabs":[{"layout":{"pane":"x"}}]})
            )
            .is_err()
        );
        assert!(validate_body(&json!({"name":"Main","tabs":(0..MAX_NODES+1).map(|_|json!({"layout":{"pane":"x"}})).collect::<Vec<_>>()})).is_err());
        assert!(validate_body(&json!({"name":"Main","tabs":[],"focus":0})).is_err());
    }
}
