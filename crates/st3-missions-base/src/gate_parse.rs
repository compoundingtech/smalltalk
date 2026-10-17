//! Parsing a mission's gates: named predicates, built-in executable gates and baseline gates.
use std::collections::BTreeMap;

use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::model::{GateSpec, St3Error};

use crate::declaration_syntax::*;

const BUILT_IN_EXEC_GATES: &[&str] = &["merged", "ci-passed", "cargo-test"];

pub fn gate_name(gate: &GateSpec) -> &str {
    match gate {
        GateSpec::Exists { name, .. }
        | GateSpec::Empty { name, .. }
        | GateSpec::Field { name, .. }
        | GateSpec::Every { name, .. }
        | GateSpec::NotEvery { name, .. }
        | GateSpec::Has { name, .. }
        | GateSpec::Lacks { name, .. }
        | GateSpec::Deadline { name, .. }
        | GateSpec::Mechanical { name, .. }
        | GateSpec::Llm { name, .. }
        | GateSpec::Human { name, .. } => name,
    }
}

pub fn one_duration(node: &KdlNode) -> Result<u64, St3Error> {
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    let [entry] = node.entries() else {
        return Err(St3Error::new(
            "wrong-argument-count",
            format!("node `{}` needs one duration", node.name().value()),
        ));
    };
    match entry.value() {
        KdlValue::String(value) => parse_duration(value, true),
        KdlValue::Integer(value) if *value > 0 => u64::try_from(*value)
            .map(|value| value * 1_000)
            .map_err(|_| St3Error::new("duration-range", "duration is too large")),
        _ => Err(St3Error::new(
            "invalid-duration",
            "a required duration must be positive",
        )),
    }
}

pub fn parse_baseline_gate(node: &KdlNode, name: String) -> Result<GateSpec, St3Error> {
    reject_type(node)?;
    ensure_no_properties(node)?;
    parse_predicate_gate(node, name)
}

/// A built-in gate kind, stored as the exec gate it expands to: an `st gate` subcommand that
/// exits 0 to pass, 1 for not yet and 3 when the gate cannot answer. `host` defaults to `local`,
/// `workspace` to `${ST_WORKSPACE}`, and `time-limit` to two minutes, or an hour for `cargo-test`.
pub fn parse_built_in_gate(
    name: String,
    body: &KdlDocument,
    default_host: &str,
) -> Result<GateSpec, St3Error> {
    let kinds = body
        .nodes()
        .iter()
        .filter(|child| BUILT_IN_EXEC_GATES.contains(&child.name().value()))
        .collect::<Vec<_>>();
    let [check] = kinds.as_slice() else {
        return Err(St3Error::new(
            "invalid-gate-shape",
            format!("gate `{name}` needs exactly one built-in check"),
        ));
    };
    let kind = check.name().value();
    reject_unknown_children(
        body,
        &[kind, "host", "workspace", "time-limit"],
        "gate",
        &name,
    )?;
    for field in ["host", "workspace", "time-limit"] {
        unique_child(body, field)?;
    }
    reject_type(check)?;
    let word = |value: &str| crate::gate_kinds::shell_word(value);
    let required = |property: &str| -> Result<String, St3Error> {
        property_string(check, property)?.ok_or_else(|| {
            St3Error::new(
                "missing-gate-field",
                format!("gate `{name}` needs {kind} {property}=\"...\""),
            )
        })
    };
    let (command, default_time_limit_ms) = match kind {
        "merged" => {
            ensure_no_properties(check)?;
            let locator = one_string(check)?;
            if !locator.contains("${") {
                validate_pull_request_locator(&locator)?;
            }
            (
                format!("\"$ST3_BIN\" gate merged {}", word(&locator)),
                120_000,
            )
        }
        "ci-passed" => {
            ensure_only_properties(check, &["repo", "commit", "branch"])?;
            let name_of_check = one_string(check)?;
            let repository = required("repo")?;
            let reference = match (
                property_string(check, "commit")?,
                property_string(check, "branch")?,
            ) {
                (Some(reference), None) | (None, Some(reference)) => reference,
                _ => {
                    return Err(St3Error::new(
                        "invalid-gate-shape",
                        format!("gate `{name}` needs ci-passed commit=\"SHA\" or branch=\"NAME\""),
                    ));
                }
            };
            (
                format!(
                    "\"$ST3_BIN\" gate ci-passed {} --repo {} --ref {}",
                    word(&name_of_check),
                    word(&repository),
                    word(&reference)
                ),
                120_000,
            )
        }
        _ => {
            ensure_only_properties(check, &["package", "ref", "worktree"])?;
            let target = one_string(check)?;
            let package = required("package")?;
            let reference = property_string(check, "ref")?.unwrap_or_else(|| "origin/main".into());
            let worktree = property_string(check, "worktree")?
                .map(|worktree| format!(" --worktree {}", word(&worktree)))
                .unwrap_or_default();
            (
                format!(
                    "\"$ST3_BIN\" gate cargo-test {} --package {} --ref {}{worktree}",
                    word(&target),
                    word(&package),
                    word(&reference)
                ),
                3_600_000,
            )
        }
    };
    let time_limit_ms = child_string(body, "time-limit")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(default_time_limit_ms);
    Ok(GateSpec::Mechanical {
        name,
        command,
        host: placement_host(
            child_string(body, "host")?.unwrap_or_else(|| "local".into()),
            default_host,
        ),
        workspace: child_string(body, "workspace")?.unwrap_or_else(|| "${ST_WORKSPACE}".into()),
        environment: BTreeMap::new(),
        time_limit_ms,
    })
}

pub fn parse_gate(node: &KdlNode, default_host: &str) -> Result<GateSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["type", "mode"])?;
    let name = one_string_with_children(node)?;
    if name.is_empty() || name.len() > 160 {
        return Err(St3Error::new(
            "invalid-gate-name",
            "a gate name must contain 1 through 160 bytes",
        ));
    }
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-gate-body", format!("gate `{name}` has no body")))?;
    let gate_type = property_string(node, "type")?;
    let mode = property_string(node, "mode")?;
    if mode.is_some() && gate_type.as_deref() != Some("human") {
        return Err(St3Error::new(
            "invalid-gate-mode",
            "only a human gate accepts a mode",
        ));
    }
    if gate_type.is_none()
        && body
            .nodes()
            .iter()
            .any(|child| BUILT_IN_EXEC_GATES.contains(&child.name().value()))
    {
        return parse_built_in_gate(name, body, default_host);
    }
    if gate_type
        .as_deref()
        .is_some_and(|kind| matches!(kind, "llm" | "human"))
        || body
            .nodes()
            .iter()
            .any(|child| child.name().value() == "exec")
    {
        return parse_running_gate(node, name, default_host);
    }
    if gate_type.is_some() {
        return Err(St3Error::new(
            "invalid-gate-type",
            format!("gate `{name}` has an invalid type"),
        ));
    }
    if body.nodes().len() != 1 {
        return Err(St3Error::new(
            "invalid-gate-shape",
            format!("gate `{name}` needs exactly one predicate"),
        ));
    }
    parse_predicate_gate(&body.nodes()[0], name)
}

pub fn parse_predicate_gate(child: &KdlNode, name: String) -> Result<GateSpec, St3Error> {
    reject_type(child)?;
    match child.name().value() {
        "exists" => {
            ensure_no_properties(child)?;
            let subject = one_string(child)?;
            validate_full_subject(&subject)?;
            Ok(GateSpec::Exists { name, subject })
        }
        // A named document exists: any version of `doc/NAME`, or the exact `doc/NAME@SHA256`.
        // Publication leaves the name as written; see `collect_document_refs`.
        "document" => {
            ensure_no_properties(child)?;
            let subject = one_string(child)?;
            if !subject.starts_with("doc/") {
                return Err(St3Error::new(
                    "invalid-document-reference",
                    format!("gate `{name}` names `{subject}`; a document gate needs doc/NAME"),
                ));
            }
            if !subject.contains("${") {
                validate_document_ref(&subject)?;
            }
            Ok(GateSpec::Exists { name, subject })
        }
        "empty" => {
            ensure_no_properties(child)?;
            let subject = one_string(child)?;
            if !subject.starts_with("mission-run/") {
                return Err(St3Error::new(
                    "invalid-empty-subject",
                    "empty requires a full mission run subject",
                ));
            }
            validate_full_subject(&subject)?;
            Ok(GateSpec::Empty { name, subject })
        }
        "has" | "lacks" => {
            let values = positional_strings(child)?;
            if values.len() != 2 {
                return Err(St3Error::new(
                    "invalid-text-predicate",
                    "has and lacks require a subject and text",
                ));
            }
            validate_full_subject(&values[0])?;
            if !matches!(
                values[0].split('/').next(),
                Some("file" | "doc" | "message")
            ) {
                return Err(St3Error::new(
                    "unsupported-predicate-subject",
                    "has and lacks require a file, document, or message subject",
                ));
            }
            Ok(if child.name().value() == "has" {
                GateSpec::Has {
                    name,
                    subject: values[0].clone(),
                    text: values[1].clone(),
                }
            } else {
                GateSpec::Lacks {
                    name,
                    subject: values[0].clone(),
                    text: values[1].clone(),
                }
            })
        }
        "field" => {
            ensure_no_properties(child)?;
            let entries = child
                .entries()
                .iter()
                .filter(|entry| entry.name().is_none())
                .map(|entry| entry.value())
                .collect::<Vec<_>>();
            if entries.len() != 4 {
                return Err(St3Error::new(
                    "invalid-field-predicate",
                    "field requires path, subject, operator, and value",
                ));
            }
            let path = value_string(entries[0])?;
            if !valid_field_path(&path) {
                return Err(St3Error::new(
                    "invalid-field-path",
                    format!("invalid field path `{path}`"),
                ));
            }
            let operator = value_string(entries[2])?;
            if !matches!(operator.as_str(), "is" | "starts-with" | "contains") {
                return Err(St3Error::new(
                    "invalid-field-operator",
                    format!("invalid field operator `{operator}`"),
                ));
            }
            let subject = value_string(entries[1])?;
            validate_full_subject(&subject)?;
            Ok(GateSpec::Field {
                name,
                path,
                subject,
                operator,
                value: json_value(entries[3])?,
            })
        }
        "every" | "not-every" => {
            ensure_no_properties(child)?;
            let entries = child
                .entries()
                .iter()
                .filter(|entry| entry.name().is_none())
                .map(|entry| entry.value())
                .collect::<Vec<_>>();
            if entries.len() != 2 {
                return Err(St3Error::new(
                    "invalid-quantified-predicate",
                    "every and not-every require a list field path and subject",
                ));
            }
            let path = value_string(entries[0])?;
            if !valid_field_path(&path) {
                return Err(St3Error::new(
                    "invalid-field-path",
                    format!("invalid field path `{path}`"),
                ));
            }
            let subject = value_string(entries[1])?;
            validate_full_subject(&subject)?;
            let body = child.children().ok_or_else(|| {
                St3Error::new(
                    "empty-quantified-predicate",
                    "every and not-every require at least one field predicate",
                )
            })?;
            if body.nodes().is_empty() {
                return Err(St3Error::new(
                    "empty-quantified-predicate",
                    "every and not-every require at least one field predicate",
                ));
            }
            let mut fields = Vec::with_capacity(body.nodes().len());
            for predicate in body.nodes() {
                if predicate.name().value() != "field" {
                    return Err(St3Error::new(
                        "invalid-quantified-predicate",
                        "every and not-every accept only field predicates",
                    ));
                }
                ensure_no_properties(predicate)?;
                ensure_no_children(predicate)?;
                let entries = predicate
                    .entries()
                    .iter()
                    .filter(|entry| entry.name().is_none())
                    .map(|entry| entry.value())
                    .collect::<Vec<_>>();
                if entries.len() != 3 {
                    return Err(St3Error::new(
                        "invalid-quantified-field",
                        "a quantified field requires path, operator, and value",
                    ));
                }
                let path = value_string(entries[0])?;
                if !valid_field_path(&path) {
                    return Err(St3Error::new(
                        "invalid-field-path",
                        format!("invalid field path `{path}`"),
                    ));
                }
                let operator = value_string(entries[1])?;
                if !matches!(operator.as_str(), "is" | "starts-with" | "contains") {
                    return Err(St3Error::new(
                        "invalid-field-operator",
                        format!("invalid field operator `{operator}`"),
                    ));
                }
                fields.push(crate::model::QuantifiedFieldSpec {
                    path,
                    operator,
                    value: json_value(entries[2])?,
                });
            }
            let gate = if child.name().value() == "every" {
                GateSpec::Every {
                    name,
                    path,
                    subject,
                    fields,
                }
            } else {
                GateSpec::NotEvery {
                    name,
                    path,
                    subject,
                    fields,
                }
            };
            Ok(gate)
        }
        "deadline" => {
            let duration = one_duration(child)?;
            Ok(GateSpec::Deadline {
                name,
                duration_ms: duration,
            })
        }
        other => Err(St3Error::new(
            "unknown-gate",
            format!("unknown gate predicate `{other}`"),
        )),
    }
}

pub fn parse_running_gate(
    node: &KdlNode,
    name: String,
    default_host: &str,
) -> Result<GateSpec, St3Error> {
    ensure_only_properties(node, &["type", "mode"])?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-gate-body", format!("gate `{name}` has no body")))?;
    let gate_type = property_string(node, "type")?;
    let mode = property_string(node, "mode")?;
    let allowed: &[&str] = match gate_type.as_deref() {
        None => &["exec", "host", "workspace", "env", "time-limit"],
        Some("llm") => &[
            "model",
            "host",
            "workspace",
            "tools",
            "env",
            "token-budget",
            "time-limit",
            "prompt",
        ],
        Some("human") => &["reviewer", "question", "review"],
        Some(other) => {
            return Err(St3Error::new(
                "invalid-gate-type",
                format!("gate `{name}` has invalid type `{other}`"),
            ));
        }
    };
    reject_unknown_children(body, allowed, "gate", &name)?;
    for child in allowed.iter().filter(|child| **child != "review") {
        unique_child(body, child)?;
    }
    if gate_type.as_deref() == Some("human") {
        let mode = mode.unwrap_or_else(|| "approve".into());
        if !matches!(mode.as_str(), "approve" | "feedback") {
            return Err(St3Error::new(
                "invalid-human-gate-mode",
                format!("human gate `{name}` has invalid mode `{mode}`"),
            ));
        }
        let reviewer = required_child_string(body, "reviewer", &name)?;
        if !reviewer.starts_with("person/") {
            return Err(St3Error::new(
                "invalid-human-reviewer",
                "a human gate needs a full person subject",
            ));
        }
        let question = child_string(body, "question")?;
        let mut review_targets = Vec::new();
        for field in body
            .nodes()
            .iter()
            .filter(|field| field.name().value() == "review")
        {
            let target = one_string(field)?;
            validate_full_subject(&target)?;
            if review_targets.contains(&target) {
                return Err(St3Error::new(
                    "duplicate-human-review-target",
                    format!("human review target `{target}` repeats"),
                ));
            }
            review_targets.push(target);
        }
        return Ok(GateSpec::Human {
            name,
            reviewer,
            mode: Some(mode),
            question,
            review_targets,
        });
    }
    let host = placement_host(required_child_string(body, "host", &name)?, default_host);
    let workspace = required_child_string(body, "workspace", &name)?;
    let environment = parse_map_child(body, "env")?;
    if let Some(env) = unique_child(body, "env")? {
        validate_string_map(env, true)?;
    }
    match gate_type.as_deref() {
        None => {
            let time_limit_ms = child_string(body, "time-limit")?
                .map(|value| parse_duration(&value, true))
                .transpose()?
                .unwrap_or(120_000);
            Ok(GateSpec::Mechanical {
                name: name.clone(),
                command: required_child_string(body, "exec", &name)?,
                host,
                workspace,
                environment,
                time_limit_ms,
            })
        }
        Some("llm") => {
            let tools = child_strings(body, "tools")?.ok_or_else(|| {
                St3Error::new("missing-gate-field", format!("gate `{name}` needs tools"))
            })?;
            let token_budget = child_integer(body, "token-budget")?.ok_or_else(|| {
                St3Error::new(
                    "missing-gate-field",
                    format!("gate `{name}` needs token-budget"),
                )
            })?;
            if token_budget <= 0 {
                return Err(St3Error::new(
                    "invalid-token-budget",
                    "an LLM token budget must be positive",
                ));
            }
            for tool in &tools {
                if !matches!(tool.as_str(), "shell" | "git" | "gh" | "network") {
                    return Err(St3Error::new(
                        "unsupported-capability",
                        format!("gate tool `{tool}` is not registered"),
                    ));
                }
            }
            let time_limit_ms = child_string(body, "time-limit")?
                .ok_or_else(|| {
                    St3Error::new(
                        "missing-gate-field",
                        format!("gate `{name}` needs time-limit"),
                    )
                })
                .and_then(|value| parse_duration(&value, true))?;
            Ok(GateSpec::Llm {
                name: name.clone(),
                model: required_child_string(body, "model", &name)?,
                host,
                workspace,
                tools,
                environment,
                token_budget: token_budget as u64,
                time_limit_ms,
                prompt: required_child_string(body, "prompt", &name)?,
            })
        }
        Some("human") => unreachable!("human gates return above"),
        Some(_) => unreachable!("gate type was validated"),
    }
}

pub fn validate_pull_request_locator(locator: &str) -> Result<(), St3Error> {
    let valid = locator
        .rsplit_once('#')
        .is_some_and(|(repository, number)| {
            number.parse::<u64>().is_ok()
                && repository.split_once('/').is_some_and(|(owner, name)| {
                    !owner.is_empty() && !name.is_empty() && !name.contains('/')
                })
        });
    if valid {
        Ok(())
    } else {
        Err(St3Error::new(
            "invalid-pull-request",
            format!("`{locator}` is not a pull request; write OWNER/REPO#NUMBER"),
        ))
    }
}
