//! Names and declaration ownership for a caller's standalone shell PTY.
use super::{ValidationError, error};

/// Other PTYs remain mission-owned. Standalone shells have a stable UUID below their person or local agent.
pub fn owner(subject: &str) -> Result<Option<&str>, ValidationError> {
    let Some(tail) = subject.strip_prefix("pty/") else {
        return Ok(None);
    };
    let Some((owner, id)) = tail.rsplit_once('/') else {
        return Ok(None);
    };
    let person = owner.starts_with("person/") && owner.matches('/').count() == 1;
    let agent = owner.starts_with("agent/") && owner.len() > "agent/".len();
    if !(person || agent) || owner.split('/').any(str::is_empty) || !super::glasses::valid_uuid(id)
    {
        // Other names remain mission-owned; malformed root declarations fail the graph parser.
        return Ok(None);
    }
    Ok(Some(owner))
}

pub fn validate_declaration_owner(
    subject: &str,
    kind: &str,
    actor: Option<&str>,
) -> Result<(), ValidationError> {
    if kind == "intent.desired"
        && let Some(owner) = owner(subject)?
        && actor != Some(owner)
    {
        return Err(error(
            "terminal-owner-forbidden",
            "only a terminal's creator may declare or end it",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn personal_terminal_ownership_is_exact_and_other_names_remain_mission_owned() {
        let subject = "pty/person/ada/019a0000-0000-7000-8000-000000000001";
        assert_eq!(owner(subject).unwrap(), Some("person/ada"));
        validate_declaration_owner(subject, "intent.desired", Some("person/ada")).unwrap();
        for actor in [
            None,
            Some("person/alex"),
            Some("agent/worker"),
            Some("daemon/runtime"),
        ] {
            assert_eq!(
                validate_declaration_owner(subject, "intent.desired", actor)
                    .unwrap_err()
                    .code,
                "terminal-owner-forbidden"
            );
        }
        validate_declaration_owner(subject, "runtime.observed", Some(subject)).unwrap();
        let agent_shell = "pty/agent/example/worker/019a0000-0000-7000-8000-000000000001";
        assert_eq!(owner(agent_shell).unwrap(), Some("agent/example/worker"));
        validate_declaration_owner(agent_shell, "intent.desired", Some("agent/example/worker"))
            .unwrap();
        assert!(
            validate_declaration_owner(agent_shell, "intent.desired", Some("agent/other")).is_err()
        );
        assert_eq!(owner("pty/person/ada/member").unwrap(), None);
        assert_eq!(owner("pty/run/member").unwrap(), None);
    }
}
