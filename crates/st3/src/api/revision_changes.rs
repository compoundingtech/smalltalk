//! A revision card's changes: what a candidate mission revision adds, removes and changes, step
//! by step, in plain words (docs/clients/ui-contract.md, Revision changes).

use super::*;
use crate::model::MissionSpec;

/// The most lines a revision card names; past them, the card says it is truncated.
pub(crate) const REVISION_LINES: usize = 200;

/// Fields st derives from the others, or that name the revision itself: a change to one is a
/// change to what it comes from, which the card names instead.
/// `display_order` lists the steps: adding or removing one changes it, and a reorder of the
/// steps both revisions keep is named as the mission's step order instead.
const DERIVED: &[&str] = &[
    "id",
    "subject",
    "revision",
    "definition_hash",
    "steps",
    "display_order",
];

/// A declaration field as a person reads it.
fn word(field: &str) -> String {
    match field {
        "goals" => "goal".into(),
        "work_selector" => "assignee".into(),
        "dependencies" => "after".into(),
        "loop_spec" => "loop".into(),
        "timeout_ms" => "timeout".into(),
        other => other.replace('_', " "),
    }
}

/// A declaration's field when it is set. A field st serializes as null is not set: setting it
/// adds it, clearing it removes it.
fn set<'a>(declaration: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a Value> {
    declaration.get(field).filter(|value| !value.is_null())
}

/// The lines for the fields of one declaration, `name` (`mission` or a step path), that differ
/// between two revisions: `+` for the fields it gains, `-` for those it loses, `~` for those that
/// change, each with its fields in plain words, sorted.
fn field_lines(name: &str, before: &Value, after: &Value, changed: &[&str]) -> Vec<Value> {
    let none = serde_json::Map::new();
    let before = before.as_object().unwrap_or(&none);
    let after = after.as_object().unwrap_or(&none);
    let mut fields: [Vec<String>; 3] = Default::default();
    fields[2].extend(changed.iter().map(|field| (*field).to_owned()));
    for field in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        if DERIVED.contains(&field.as_str()) {
            continue;
        }
        let at = match (set(before, field), set(after, field)) {
            (None, Some(_)) => 0,
            (Some(_), None) => 1,
            (Some(old), Some(new)) if old != new => 2,
            _ => continue,
        };
        fields[at].push(word(field));
    }
    ["+", "-", "~"]
        .into_iter()
        .zip(fields)
        .filter(|(_, fields)| !fields.is_empty())
        .map(|(op, mut fields)| {
            fields.sort();
            fields.dedup();
            json!({"op": op, "text": format!("{name}: {}", fields.join(", "))})
        })
        .collect()
}

/// The lines between two revisions of a mission: `mission: …` for its own fields, then by step
/// path `+ step PATH`, `- step PATH` or the fields of `PATH: …`; at most [`REVISION_LINES`], and
/// whether there were more.
pub(crate) fn revision_lines(
    from: &MissionSpec,
    to: &MissionSpec,
) -> anyhow::Result<(Vec<Value>, bool)> {
    let line = |op: &str, text: String| json!({"op": op, "text": text});
    let mut lines = Vec::new();
    // The steps both revisions keep, in each one's order.
    let kept = |order: &[String], other: &MissionSpec| {
        order
            .iter()
            .filter(|path| other.steps.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>()
    };
    let reordered = kept(&from.display_order, to) != kept(&to.display_order, from);
    lines.extend(field_lines(
        "mission",
        &serde_json::to_value(from)?,
        &serde_json::to_value(to)?,
        if reordered { &["step order"] } else { &[] },
    ));
    let paths = from
        .steps
        .keys()
        .chain(to.steps.keys())
        .collect::<BTreeSet<_>>();
    for path in paths {
        match (from.steps.get(path), to.steps.get(path)) {
            (None, Some(_)) => lines.push(line("+", format!("step {path}"))),
            (Some(_), None) => lines.push(line("-", format!("step {path}"))),
            (Some(before), Some(after)) => lines.extend(field_lines(
                path,
                &serde_json::to_value(before)?,
                &serde_json::to_value(after)?,
                &[],
            )),
            (None, None) => {}
        }
    }
    let truncated = lines.len() > REVISION_LINES;
    lines.truncate(REVISION_LINES);
    Ok((lines, truncated))
}

/// A revision approval's changes, `{from_revision, to_revision, reason, lines, truncated}`:
/// from the revision its run runs now to the candidate. `None` when st no longer has the
/// proposal, its run's generation or either revision.
pub(crate) fn revision_card(store: &Store, row: &Value) -> anyhow::Result<Option<Value>> {
    let (Some(source), Some(mission)) = (row["source_id"].as_str(), row["mission_id"].as_str())
    else {
        return Ok(None);
    };
    let Some(proposal) = store.revision_proposal(source)? else {
        return Ok(None);
    };
    let Some(generation) = store.run_generation(&proposal.source_generation)? else {
        return Ok(None);
    };
    let mission = mission.strip_prefix("mission/").unwrap_or(mission);
    let (Some(from), Some(to)) = (
        store.mission_spec(mission, Some(&generation.revision))?,
        store.mission_spec(mission, Some(&proposal.candidate_revision))?,
    ) else {
        return Ok(None);
    };
    let (lines, truncated) = revision_lines(&from, &to)?;
    Ok(Some(json!({
        "from_revision": generation.revision,
        "to_revision": proposal.candidate_revision,
        "reason": proposal.reason,
        "lines": lines,
        "truncated": truncated,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mission(source: &str) -> MissionSpec {
        let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
        intent.missions.into_values().next().unwrap()
    }

    #[test]
    fn a_revision_names_each_step_it_adds_removes_and_changes() {
        let from = mission(
            r#"version 2
mission "audit" state="ready" {
  goal "Audit the build."
  step "prepare" { goal "Prepare."; }
  step "review" { goal "Review."; }
  step "retire" { goal "Retire."; }
}
"#,
        );
        let to = mission(
            r#"version 2
mission "audit" state="ready" {
  goal "Audit the build and the release."
  step "prepare" { goal "Prepare."; }
  step "review" timeout="10m" { goal "Review twice."; }
  step "publish" { goal "Publish."; }
}
"#,
        );
        let (lines, truncated) = revision_lines(&from, &to).unwrap();
        assert!(!truncated);
        assert_eq!(
            lines,
            [
                json!({"op": "~", "text": "mission: goal"}),
                json!({"op": "+", "text": "step publish"}),
                json!({"op": "-", "text": "step retire"}),
                json!({"op": "+", "text": "review: timeout"}),
                json!({"op": "~", "text": "review: goal"}),
            ]
        );
        // An unchanged revision changes nothing.
        assert_eq!(revision_lines(&from, &from).unwrap(), (vec![], false));
    }

    #[test]
    fn a_reorder_of_the_steps_it_keeps_is_the_missions_step_order() {
        let from = mission(
            "version 2\nmission \"order\" state=\"ready\" {\n  goal \"Order.\"\n  step \"a\" { goal \"A.\"; }\n  step \"b\" { goal \"B.\"; }\n}\n",
        );
        let to = mission(
            "version 2\nmission \"order\" state=\"ready\" {\n  goal \"Order.\"\n  step \"b\" { goal \"B.\"; }\n  step \"a\" { goal \"A.\"; }\n}\n",
        );
        assert_eq!(
            revision_lines(&from, &to).unwrap().0,
            [json!({"op": "~", "text": "mission: step order"})]
        );
    }

    #[test]
    fn a_revision_past_the_line_limit_says_it_is_truncated() {
        let steps = |count: usize| {
            (0..count)
                .map(|step| format!("  step \"s{step:03}\" {{ goal \"Step.\"; }}\n"))
                .collect::<String>()
        };
        let from = mission("version 2\nmission \"wide\" state=\"ready\" {\n  goal \"Wide.\"\n}\n");
        let to = mission(&format!(
            "version 2\nmission \"wide\" state=\"ready\" {{\n  goal \"Wide.\"\n{}}}\n",
            steps(REVISION_LINES + 5)
        ));
        let (lines, truncated) = revision_lines(&from, &to).unwrap();
        assert!(truncated);
        assert_eq!(lines.len(), REVISION_LINES);
    }
}
