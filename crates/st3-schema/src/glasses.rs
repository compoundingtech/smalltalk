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
            "a glass needs a name and a bounded tree of tab groups or binary splits",
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
    let mut stack = vec![(obj.get("layout").ok_or_else(invalid)?, 1)];
    let mut nodes = 0;
    while let Some((layout, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(invalid());
        }
        let obj = layout.as_object().ok_or_else(invalid)?;
        if obj.len() == 1 && obj.contains_key("tabs") {
            let tabs = obj["tabs"].as_array().ok_or_else(invalid)?;
            nodes += tabs.len();
            if nodes > MAX_NODES {
                return Err(invalid());
            }
            for tab in tabs {
                let tab = tab.as_object().ok_or_else(invalid)?;
                if tab.keys().any(|k| k != "pane" && k != "title")
                    || tab.get("title").is_some_and(|v| !v.is_string())
                    || !tab
                        .get("pane")
                        .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                {
                    return Err(invalid());
                }
            }
            continue;
        }
        nodes += 1;
        if nodes > MAX_NODES
            || obj
                .keys()
                .any(|k| k != "split" && k != "children" && k != "ratio")
            || obj
                .get("ratio")
                .is_some_and(|v| !v.as_f64().is_some_and(|ratio| (0.1..=0.9).contains(&ratio)))
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

/// Durable version-0 claims remain valid history. Client version-1 writes use
/// `validate_body`; projections translate old tabs of split panes without rewriting claims.
pub fn body_for_read(body: &Value) -> Result<Value, ValidationError> {
    if validate_body(body).is_ok() {
        return Ok(body.clone());
    }
    validate_legacy_body(body)?;
    let mut tabs = Vec::new();
    for tab in body["tabs"].as_array().expect("validated legacy tabs") {
        let mut title = tab.get("title").cloned();
        let mut stack = vec![&tab["layout"]];
        while let Some(layout) = stack.pop() {
            if let Some(pane) = layout.get("pane") {
                let mut tab = serde_json::json!({"pane": pane});
                if let Some(title) = title.take() {
                    tab["title"] = title;
                }
                tabs.push(tab);
            } else {
                let children = layout["children"]
                    .as_array()
                    .expect("validated binary split");
                // Stack visits left/top before right/bottom, preserving pane order.
                stack.push(&children[1]);
                stack.push(&children[0]);
            }
        }
    }
    Ok(serde_json::json!({"name": body["name"], "layout": {"tabs": tabs}}))
}

fn validate_legacy_body(body: &Value) -> Result<(), ValidationError> {
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
    fn legacy_bodies_project_all_panes_in_order_and_keep_titles_on_first_panes() {
        let old = json!({"name":"Main","tabs":[
            {"title":"Work","layout":{"split":"right","children":[
                {"pane":"agent:a"}, {"split":"below","children":[{"pane":"agent:b"},{"pane":"agent:c"}]}
            ]}},
            {"layout":{"pane":"agent:d"}},
            {"title":"","layout":{"pane":"agent:e"}}
        ]});
        let expected = json!({"name":"Main","layout":{"tabs":[
            {"title":"Work","pane":"agent:a"},{"pane":"agent:b"},{"pane":"agent:c"},
            {"pane":"agent:d"},{"title":"","pane":"agent:e"}
        ]}});
        assert_eq!(body_for_read(&old).unwrap(), expected);
        assert_eq!(body_for_read(&expected).unwrap(), expected);
        assert!(
            validate_body(&old).is_err(),
            "new clients must write the new shape"
        );
        assert_eq!(
            body_for_read(&json!({"name":"Empty","tabs":[]})).unwrap(),
            json!({"name":"Empty","layout":{"tabs":[]}})
        );
        for invalid in [
            json!({"name":"Main","tabs":[{"title":null,"layout":{"pane":"agent:a"}}]}),
            json!({"name":"Main","tabs":[{"layout":{"split":"right","children":[{"pane":"agent:a"}]}}]}),
            json!({"name":"Main","tabs":[],"layout":{"tabs":[]}}),
        ] {
            assert!(body_for_read(&invalid).is_err());
        }
        let mut deep = json!({"pane":"a"});
        for _ in 1..MAX_DEPTH {
            deep = json!({"split":"right","children":[{"pane":"b"},deep]});
        }
        body_for_read(&json!({"name":"Depth limit","tabs":[{"layout":deep}]})).unwrap();
        let too_deep = json!({"split":"right","children":[{"pane":"b"},deep]});
        assert!(body_for_read(&json!({"name":"Too deep","tabs":[{"layout":too_deep}]})).is_err());
        let mut large = json!({"name":"x".repeat(MAX_BODY_BYTES),"tabs":[]});
        assert!(body_for_read(&large).is_err());
        large["name"] = json!("x".repeat(MAX_BODY_BYTES - 21));
        assert_eq!(serde_json::to_vec(&large).unwrap().len(), MAX_BODY_BYTES);
        body_for_read(&large).unwrap();
    }
    fn body(layout: Value) -> Value {
        json!({"name":"Main", "layout":layout})
    }
    #[test]
    fn glass_groups_validate_structure_and_reject_previous_shape() {
        let group =
            json!({"tabs":[{"title":"Work","pane":"opaque:anything"},{"pane":"opaque:second"}]});
        validate_body(&body(group.clone())).unwrap();
        validate_body(&body(json!({"tabs":[]}))).unwrap();
        validate_body(&body(
            json!({"split":"right","children":[group,{"tabs":[]}]}),
        ))
        .unwrap();
        for layout in [
            json!({"pane":"x"}),
            json!({"tabs":[],"split":"right"}),
            json!({"split":"up","children":[{"tabs":[]},{"tabs":[]}]}),
            json!({"split":"right","children":[{"tabs":[]}]}),
            json!({"split":"right","children":[{"tabs":[]},{"tabs":[]},{"tabs":[]}]}),
            json!({"tabs":[{"pane":""}]}),
            json!({"tabs":[{"pane":"x","title":null}]}),
            json!({"tabs":[{"pane":"x","layout":{"tabs":[]}}]}),
            json!({"tabs":[{"pane":"x","selected":true}]}),
            json!({"tabs":[],"focus":0}),
        ] {
            assert!(validate_body(&body(layout)).is_err());
        }
        assert!(validate_body(&json!({"name":"Main","tabs":[]})).is_err());
        assert!(validate_body(&json!({"name":"","layout":{"tabs":[]}})).is_err());
    }
    #[test]
    fn split_ratios_are_bounded_and_survive_stored_projection() {
        for ratio in [0.1, 0.5, 0.9] {
            let value =
                body(json!({"split":"right", "ratio":ratio, "children":[{"tabs":[]},{"tabs":[]}]}));
            validate_body(&value).unwrap();
            assert_eq!(body_for_read(&value).unwrap(), value);
        }
        for ratio in [
            json!(0.099),
            json!(0.901),
            json!(1),
            json!(-1),
            json!(null),
            json!("0.5"),
            json!(true),
        ] {
            assert!(
                validate_body(&body(
                    json!({"split":"below", "ratio":ratio, "children":[{"tabs":[]},{"tabs":[]}]})
                ))
                .is_err()
            );
        }
        assert!(validate_body(&body(json!({"tabs":[], "ratio":0.5}))).is_err());
        assert!(validate_body(&body(json!({"split":"right", "ratio":0.5, "focus":0, "children":[{"tabs":[]},{"tabs":[]}]}))).is_err());
    }
    #[test]
    fn glass_groups_enforce_exact_byte_depth_and_combined_node_bounds() {
        let mut exact = body(json!({"tabs":[]}));
        exact["name"] = json!("");
        let overhead = serde_json::to_vec(&exact).unwrap().len();
        exact["name"] = json!("x".repeat(MAX_BODY_BYTES - overhead));
        validate_body(&exact).unwrap();
        exact["name"] = json!("x".repeat(MAX_BODY_BYTES - overhead + 1));
        assert_eq!(
            validate_body(&exact).unwrap_err().code,
            "glass-body-too-large"
        );
        let mut deep = json!({"tabs":[]});
        for _ in 1..MAX_DEPTH {
            deep = json!({"split":"below","children":[deep,{"tabs":[]}]});
        }
        validate_body(&body(deep.clone())).unwrap();
        assert!(
            validate_body(&body(
                json!({"split":"below","children":[deep,{"tabs":[]}]})
            ))
            .is_err()
        );
        let tabs = (0..MAX_NODES)
            .map(|_| json!({"pane":"x"}))
            .collect::<Vec<_>>();
        validate_body(&body(json!({"tabs":tabs}))).unwrap();
        // A split consumes the remaining node: empty leaf groups consume no tab nodes.
        let tabs = (0..MAX_NODES - 1)
            .map(|_| json!({"pane":"x"}))
            .collect::<Vec<_>>();
        let mut layout = json!({"split":"right","children":[{"tabs":tabs},{"tabs":[]}]});
        validate_body(&body(layout.clone())).unwrap();
        layout["children"][1]["tabs"] = json!([{"pane":"x"}]);
        assert!(validate_body(&body(layout)).is_err());
    }
}
