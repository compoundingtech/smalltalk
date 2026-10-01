//! Names and declaration ownership for a person's standalone shell PTY.
use super::{ValidationError, error};

/// Other PTYs remain mission-owned. Personal shells have a stable UUID below their person.
pub fn owner(subject: &str) -> Result<Option<&str>, ValidationError> {
    if !subject.starts_with("pty/person/") {
        return Ok(None);
    }
    let tail = subject.strip_prefix("pty/").expect("checked PTY prefix");
    let parts: Vec<_> = tail.split('/').collect();
    if parts.len() != 3 || parts[1].is_empty() || !super::glasses::valid_uuid(parts[2]) {
        // A mission may already have a similarly named member. Only the complete personal
        // namespace is reserved; malformed root personal declarations still fail the graph parser.
        return Ok(None);
    }
    Ok(Some(&tail[..tail.len() - parts[2].len() - 1]))
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
            "only a terminal's person may declare or end it",
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
        assert_eq!(owner("pty/person/ada/member").unwrap(), None);
        assert_eq!(owner("pty/run/member").unwrap(), None);
    }
}
