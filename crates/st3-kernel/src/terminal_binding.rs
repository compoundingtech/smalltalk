//! A harness borrows a person's terminal. Its invocation fence is independent of the shell.
use serde_json::Value;

use crate::model::{DesiredSubject, MemberLifecycle, St3Error, TerminalBinding};

pub fn from_desired(desired: &Value) -> Result<Option<TerminalBinding>, St3Error> {
    let Some(node) = desired
        .get("children")
        .and_then(Value::as_array)
        .and_then(|children| children.iter().find(|node| node["name"] == "bind-terminal"))
    else {
        return Ok(None);
    };
    let string = |value: &Value| {
        value
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                St3Error::new(
                    "invalid-terminal-binding",
                    "a terminal binding field is missing",
                )
            })
    };
    Ok(Some(TerminalBinding {
        subject: string(&node["arguments"][0])?,
        incarnation: string(&node["properties"]["incarnation"])?,
        id: string(&node["properties"]["id"])?,
    }))
}

pub fn validate_declaration(
    desired: &DesiredSubject,
    actor: Option<&str>,
) -> Result<(), St3Error> {
    let binding = from_desired(&desired.desired)?;
    let member_binding = desired
        .member
        .as_ref()
        .and_then(|m| m.terminal_binding.as_ref());
    if binding.is_none()
        && member_binding.is_none()
        && desired
            .member
            .as_ref()
            .is_none_or(|m| m.lifecycle != MemberLifecycle::TerminalBound)
    {
        return Ok(());
    }
    let binding = binding.ok_or_else(|| {
        St3Error::new(
            "invalid-terminal-binding",
            "the declaration must name its bound terminal",
        )
    })?;
    st3_schema::owned_terminals::validate_declaration_owner(
        &binding.subject,
        "intent.desired",
        actor,
    )
    .map_err(|e| St3Error::new(e.code, e.message))?;
    if desired.kind != "agent"
        || desired.owner_run.is_some()
        || member_binding != Some(&binding)
        || desired
            .member
            .as_ref()
            .is_none_or(|m| m.lifecycle != MemberLifecycle::TerminalBound || !m.terminal)
        || st3_schema::owned_terminals::owner(&binding.subject)
            .map_err(|e| St3Error::new(e.code, e.message))?
            .is_none_or(|owner| !owner.starts_with("person/"))
        || uuid::Uuid::parse_str(&binding.id).is_err()
    {
        return Err(St3Error::new(
            "invalid-terminal-binding",
            "invalid bound terminal member",
        ));
    }
    Ok(())
}

pub fn validate_claim(
    kind: &str,
    body: &Value,
    actor: Option<&str>,
) -> Result<(), St3Error> {
    if kind == "intent.desired"
        && (body
            .pointer("/member/terminal_binding")
            .is_some_and(|v| !v.is_null())
            || body.pointer("/member/lifecycle").and_then(Value::as_str) == Some("terminal-bound")
            || from_desired(&body["desired"])?.is_some())
    {
        let desired = serde_json::from_value(body.clone())
            .map_err(|e| St3Error::new("invalid-terminal-binding", e.to_string()))?;
        validate_declaration(&desired, actor)?;
    }
    Ok(())
}

