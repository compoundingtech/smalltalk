//! The conversation header (contract §3): every present field in fixed order.
//! Shared provenance appears once; minority sources are marked on their fields.
//! The shared age is the oldest field from that source, so stale data stays visible.

use serde_json::Value;

/// The header line for `header` (a page or delta `header` object). `now` is an RFC 3339
/// timestamp the caller owns, so the line is deterministic in tests and ages live in a UI.
/// Empty when no field has a value.
pub fn line(header: &Value, now: &str) -> String {
    let now = age_now(now);
    let mut parts: Vec<(String, &Value)> = Vec::new();
    for (name, render) in [
        ("model", model as fn(&str, &Value) -> Option<String>),
        ("context", context),
        ("cost", cost),
        ("todos", todos),
        ("jobs", count),
        ("subagents", count),
        ("ask", ask),
        ("working", working),
    ] {
        let Some(field) = header.get(name).filter(|field| field.is_object()) else {
            continue;
        };
        let Some(value) = render(name, &field["value"]) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        parts.push((value.to_owned(), field));
    }
    // At most eight fields: count sources without a separate map or copied source keys.
    // Ties keep the first source in the fixed field order.
    let mut common = "";
    let mut common_count = 0;
    for (_, field) in &parts {
        let source = field["source"].as_str().unwrap_or_default();
        let count = parts.iter().filter(|(_, other)| other["source"].as_str().unwrap_or_default() == source).count();
        if !source.is_empty() && count > common_count {
            common = source;
            common_count = count;
        }
    }
    let oldest = parts.iter()
        .filter(|(_, field)| field["source"].as_str() == Some(common))
        .min_by_key(|(_, field)| field["as_of"].as_str().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()))
        .map(|(_, field)| *field);
    let mut rendered = String::new();
    for (value, field) in &parts {
        if !rendered.is_empty() {
            rendered.push_str(" · ");
        }
        rendered.push_str(value);
        if field["source"].as_str().unwrap_or_default() != common {
            let marker = sourced(field, now.as_deref());
            if !marker.is_empty() {
                rendered.push_str(" [");
                rendered.push_str(&marker);
                rendered.push(']');
            }
        }
    }
    if let Some(field) = oldest {
        let marker = sourced(field, now.as_deref());
        if !marker.is_empty() {
            rendered.push_str(" · ");
            rendered.push_str(&marker);
        }
    }
    rendered
}

/// One field's value and, when the field says where it came from, its source and age.
fn sourced(field: &Value, now: Option<&str>) -> String {
    let marker = match (
        field
            .get("source")
            .and_then(Value::as_str)
            .filter(|source| !source.is_empty()),
        field
            .get("as_of")
            .and_then(Value::as_str)
            .and_then(|at| now.map(|now| (at, now))),
    ) {
        (Some(source), Some((at, now))) => {
            let age = age(at, now);
            if age.is_empty() {
                source.to_owned()
            } else {
                format!("{source} · {age} ago")
            }
        }
        (Some(source), None) => source.to_owned(),
        _ => return String::new(),
    };
    marker
}

fn model(_: &str, value: &Value) -> Option<String> {
    value.as_str().map(|model| format!("model {model}"))
}

fn context(_: &str, value: &Value) -> Option<String> {
    let tokens = value.get("tokens")?.as_u64()?;
    let window = value
        .get("window")
        .and_then(Value::as_u64)
        .map(|window| format!(" of {window}"))
        .unwrap_or_default();
    Some(format!("context {tokens} tokens{window}"))
}

fn cost(_: &str, value: &Value) -> Option<String> {
    let usd = value.get("usd")?.as_f64()?;
    Some(format!("cost ${usd:.2}"))
}

fn todos(_: &str, value: &Value) -> Option<String> {
    let mut total = 0;
    let mut done = 0;
    for phase in value.as_array()? {
        for item in phase.get("items").and_then(Value::as_array).into_iter().flatten() {
            total += 1;
            if item.get("status").and_then(Value::as_str) == Some("completed") {
                done += 1;
            }
        }
    }
    Some(format!("todo {done}/{total}"))
}

fn count(name: &str, value: &Value) -> Option<String> {
    let count = value.as_array()?.len();
    let label = match name {
        "jobs" => "jobs",
        _ => "agents",
    };
    Some(format!("{label} {count}"))
}

fn ask(_: &str, value: &Value) -> Option<String> {
    let question = value.get("questions")?.as_array()?.first()?;
    let text = question.get("question").and_then(Value::as_str)?;
    Some(format!("ask {text}"))
}

fn working(_: &str, value: &Value) -> Option<String> {
    value.as_bool().map(|working| {
        if working {
            "working".into()
        } else {
            "idle".into()
        }
    })
}

/// `now` parsed once, so a line of several fields reads one clock.
fn age_now(now: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(now)
        .ok()
        .map(|at| at.to_rfc3339())
}

/// How old `as_of` is at `now`: seconds, then minutes, hours, days.
fn age(as_of: &str, now: &str) -> String {
    let Some(at) = chrono::DateTime::parse_from_rfc3339(as_of).ok() else {
        return String::new();
    };
    let Some(now) = chrono::DateTime::parse_from_rfc3339(now).ok() else {
        return String::new();
    };
    let seconds = now.signed_duration_since(at).num_seconds().max(0);
    let minutes = seconds.saturating_add(30) / 60;
    let hours = seconds.saturating_add(30 * 60) / (60 * 60);
    let (count, unit) = if seconds < 60 {
        (seconds, "s")
    } else if minutes < 60 {
        (minutes, "m")
    } else if hours < 24 {
        (hours, "h")
    } else {
        (seconds.saturating_add(12 * 60 * 60) / (24 * 60 * 60), "d")
    };
    format!("{count}{unit}")
}

/// The session a subagent card's `open <session>` row opens, when it has one.
pub fn open_session(output: &[String]) -> Option<&str> {
    let line = output.iter().find(|line| line.starts_with("open "))?;
    line.strip_prefix("open ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_provenance_uses_the_oldest_age_and_marks_register_cost() {
        let header = json!({
            "model": {"value": "m", "source": "transcript", "as_of": "2026-10-06T11:59:00Z"},
            "context": {"value": {"tokens": 50}, "source": "transcript", "as_of": "2026-10-06T11:00:00Z"},
            "cost": {"value": {"usd": 0.02}, "source": "register", "as_of": "2026-10-06T11:30:00Z"}
        });
        assert_eq!(
            line(&header, "2026-10-06T12:00:00Z"),
            "model m · context 50 tokens · cost $0.02 [register · 30m ago] · transcript · 1h ago"
        );
        assert_eq!(
            line(&json!({"todos": {"value": [], "source": "transcript", "as_of": "2026-10-06T12:00:00Z"}}), "2026-10-06T12:00:00Z"),
            "todo 0/0 · transcript · 0s ago"
        );
    }

    #[test]
    fn mixed_sources_mark_only_the_minority_field() {
        let header = json!({
            "model": {"value": "synthetic/model", "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "context": {"value": {"tokens": 50, "window": null}, "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "cost": {"value": {"usd": 0.02}, "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "todos": {"value": [{"phase": "Render", "items": [{"content": "Render cards", "status": "completed"}]}], "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "jobs": {"value": [{"id": "job-2", "state": "running"}], "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "subagents": {"value": [{"id": "child-live", "status": "running"}], "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "ask": {"value": {"call_id": "active", "questions": [{"question": "Continue?", "options": [], "multi": false}]}, "source": "transcript", "as_of": "2026-10-06T12:00:00Z"},
            "working": {"value": true, "source": "register", "as_of": "2026-10-06T12:00:00Z"}
        });
        assert_eq!(
            line(&header, "2026-10-06T12:00:00Z"),
            "model synthetic/model · context 50 tokens · cost $0.02 · todo 1/1 · jobs 1 \
             · agents 1 · ask Continue? · working [register · 0s ago] · transcript · 0s ago"
        );
    }

    #[test]
    fn absent_fields_are_absent_and_ages_step_up() {
        assert_eq!(line(&json!({}), "2026-10-06T12:00:00Z"), "");
        let header = json!({
            "model": {"value": "m", "source": "register", "as_of": "2026-10-06T11:00:30Z"},
            "working": {"value": false}
        });
        assert_eq!(
            line(&header, "2026-10-06T12:00:00Z"),
            "model m · idle · register · 1h ago"
        );
        assert_eq!(age("2026-10-05T12:00:00Z", "2026-10-06T12:00:00Z"), "1d");
    }

    #[test]
    fn a_card_opens_its_child_conversation() {
        let output = vec![
            "Review synthetic code".to_owned(),
            "duration 1200ms".to_owned(),
            "open session/child".to_owned(),
        ];
        assert_eq!(open_session(&output), Some("session/child"));
        assert_eq!(open_session(&["duration 1200ms".to_owned()]), None);
    }
}
