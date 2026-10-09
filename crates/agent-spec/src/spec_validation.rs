use std::collections::BTreeMap;

use super::{AgentDesiredState, RawTask, Task, TaskKind, TaskLifecycle};
use super::{
    AGENT_ADDRESS_MAX_BYTES, AGENT_ADDRESS_SEGMENT_MAX_BYTES, AGENT_DESIRED_STATE_REASON_MAX_BYTES,
    AGENT_ID_MAX_BYTES,
};

pub(crate) fn lower_desired_state(
    retired: Option<bool>,
    state: Option<&str>,
    reason: Option<String>,
) -> anyhow::Result<AgentDesiredState> {
    anyhow::ensure!(
        retired.is_none() || state.is_none(),
        "agent declares both legacy `retired` and `desired-state`; choose one lifecycle form"
    );
    match (state, reason) {
        (None, None) => Ok(if retired == Some(true) {
            AgentDesiredState::Retired { reason: None }
        } else {
            AgentDesiredState::Running
        }),
        (None, Some(_)) => anyhow::bail!("agent lifecycle `reason` requires `desired-state`"),
        (Some("running"), None) => Ok(AgentDesiredState::Running),
        (Some("running"), Some(_)) => {
            anyhow::bail!("agent `desired-state \"running\"` forbids `reason`")
        }
        (Some("suspended"), Some(reason)) => {
            validate_desired_state_reason(&reason)?;
            Ok(AgentDesiredState::Suspended { reason })
        }
        (Some("retired"), Some(reason)) => {
            validate_desired_state_reason(&reason)?;
            Ok(AgentDesiredState::Retired {
                reason: Some(reason),
            })
        }
        (Some("suspended" | "retired"), None) => {
            anyhow::bail!("agent `desired-state {state:?}` requires `reason`")
        }
        (Some(other), _) => anyhow::bail!(
            "unknown agent desired state '{other}'; expected running, suspended, or retired"
        ),
    }
}


impl RawTask {
    pub(crate) fn lower(
        self,
        identity: &str,
        kind: TaskKind,
        name: String,
        inherited_env: &BTreeMap<String, String>,
    ) -> anyhow::Result<Task> {
        validate_launch(
            identity,
            self.command.as_ref(),
            self.argv.as_ref(),
            &format!("{kind:?} task '{name}'"),
        )?;
        let mut env = inherited_env.clone();
        env.extend(self.env);
        let lifecycle = parse_task_lifecycle(
            identity,
            &format!("{kind:?} task '{name}'"),
            self.lifecycle.as_deref(),
        )?;
        Ok(Task {
            kind,
            derived: false,
            name,
            id: self.id,
            command: self.command,
            argv: self.argv,
            cwd: self.cwd,
            tags: self.tags,
            env,
            keep: self.keep,
            lifecycle,
        })
    }
}

pub(crate) fn parse_task_lifecycle(
    identity: &str,
    location: &str,
    lifecycle: Option<&str>,
) -> anyhow::Result<TaskLifecycle> {
    match lifecycle {
        None | Some("service") => Ok(TaskLifecycle::Service),
        Some("adopt-only") => Ok(TaskLifecycle::AdoptOnly),
        Some(other) => {
            anyhow::bail!("agent '{identity}' {location} has unknown lifecycle '{other}'")
        }
    }
}

pub(crate) fn validate_launch(
    identity: &str,
    command: Option<&String>,
    argv: Option<&Vec<String>>,
    location: &str,
) -> anyhow::Result<()> {
    if command.is_some() && argv.is_some() {
        anyhow::bail!(
            "agent '{identity}' {location} declares both `command` and `argv`; choose one launch form"
        );
    }
    if argv.is_some_and(Vec::is_empty) {
        anyhow::bail!("agent '{identity}' {location} declares an empty `argv`");
    }
    if argv.is_some_and(|argv| argv.first().is_some_and(String::is_empty)) {
        anyhow::bail!("agent '{identity}' {location} declares an empty `argv` program");
    }
    Ok(())
}
/// Validate the rationale carried by a non-running desired state.
pub fn validate_desired_state_reason(reason: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !reason.is_empty() && reason.len() <= AGENT_DESIRED_STATE_REASON_MAX_BYTES,
        "agent desired-state `reason` must be 1..{AGENT_DESIRED_STATE_REASON_MAX_BYTES} UTF-8 bytes"
    );
    anyhow::ensure!(
        reason.trim() == reason
            && !reason
                .chars()
                .any(|character| character.is_control()
                    || matches!(character, '\u{2028}' | '\u{2029}')),
        "agent desired-state `reason` must have no surrounding Unicode whitespace, controls, or line separators"
    );
    Ok(())
}

/// Validate one optional presentation field at the shared parse/authoring boundary.
pub fn validate_presentation(
    field: &str,
    value: Option<&str>,
    max_chars: usize,
) -> anyhow::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    anyhow::ensure!(
        !value.is_empty(),
        "agent presentation `{field}` cannot be empty; omit it to clear it"
    );
    anyhow::ensure!(
        value.trim() == value,
        "agent presentation `{field}` cannot begin or end with whitespace"
    );
    anyhow::ensure!(
        !value.chars().any(|character| {
            character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
        }),
        "agent presentation `{field}` must be one printable line without control characters or Unicode line separators"
    );
    anyhow::ensure!(
        value.chars().count() <= max_chars,
        "agent presentation `{field}` exceeds the {max_chars}-character limit"
    );
    Ok(())
}

/// Validate an explicit immutable agent ID at the shared parse/authoring boundary (R24).
///
/// The ID is opaque, but not arbitrary: R26 reuses it verbatim as the canonical task ID and
/// therefore as a session socket path component, and IDs travel through shell-adjacent text (bus
/// messages, notices, journal lines), where a backtick has already executed a command on a live
/// host (schickling/dotfiles#1614). So the grammar is the closed safe set `[A-Za-z0-9._-]`, which
/// admits both admitted producers — a new subject's UUIDv7 and a migrated subject's frozen
/// `<host>.<identity>` bus identity — and refuses every shell metacharacter, path and host
/// separator, whitespace byte, and non-ASCII byte outright.
///
/// It stays wider than the address grammar on purpose: a frozen legacy ID keeps host-looking bytes
/// and whatever case and underscores its identity carried, so freezing an admissible identity can
/// never be refused here. Equal bytes in the two namespaces do not collide.
pub fn validate_agent_id(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "agent `id` cannot be empty");
    anyhow::ensure!(
        value.len() <= AGENT_ID_MAX_BYTES,
        "agent `id` '{value}' exceeds the {AGENT_ID_MAX_BYTES}-byte limit"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "agent `id` '{value}' must match [A-Za-z0-9._-]+"
    );
    anyhow::ensure!(
        !value.starts_with('.') && !value.ends_with('.'),
        "agent `id` '{value}' cannot begin or end with `.`"
    );
    Ok(())
}

/// Validate an explicit mutable agent address at the shared parse/authoring boundary (R24).
///
/// An explicit address is at most [`AGENT_ADDRESS_MAX_BYTES`] ASCII characters and is a dotted
/// sequence of 1-to-[`AGENT_ADDRESS_SEGMENT_MAX_BYTES`]-character segments. Each segment contains
/// only lowercase letters, digits, and hyphens and begins and ends with a letter or digit.
pub fn validate_agent_address(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty(),
        "agent `address` cannot be empty; omit it to fall back to the positional identity"
    );
    anyhow::ensure!(
        value.len() <= AGENT_ADDRESS_MAX_BYTES,
        "agent `address` '{value}' exceeds the {AGENT_ADDRESS_MAX_BYTES}-character limit"
    );
    anyhow::ensure!(value.is_ascii(), "agent `address` '{value}' must be ASCII");
    for segment in value.split('.') {
        anyhow::ensure!(
            !segment.is_empty(),
            "agent `address` '{value}' has an empty dotted segment"
        );
        anyhow::ensure!(
            segment.len() <= AGENT_ADDRESS_SEGMENT_MAX_BYTES,
            "agent `address` '{value}' segment '{segment}' exceeds {AGENT_ADDRESS_SEGMENT_MAX_BYTES} characters"
        );
        anyhow::ensure!(
            segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "agent `address` '{value}' segment '{segment}' must match [a-z0-9-]+"
        );
        let first = segment.as_bytes()[0];
        let last = segment.as_bytes()[segment.len() - 1];
        anyhow::ensure!(
            (first.is_ascii_lowercase() || first.is_ascii_digit())
                && (last.is_ascii_lowercase() || last.is_ascii_digit()),
            "agent `address` '{value}' segment '{segment}' must begin and end with a letter or digit"
        );
    }
    Ok(())
}
