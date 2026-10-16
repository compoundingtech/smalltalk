//! The launch boundary shared by publication and reconciliation.
use crate::model::{DesiredSubject, MemberLifecycle, MemberSpec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interruption {
    None,
    Stop,
    Restart(Vec<&'static str>),
    /// A running runtime is not proof of an unchanged launch. Callers that promise no
    /// interruption must refuse this result rather than substituting its declaration.
    Unknown,
}

/// Compare against the member that launched this incarnation, never the last declaration.
/// Observe-only declarations cannot restart a service. An explicit stop can interrupt any
/// running runtime, including an adopted runtime whose launch evidence is unavailable.
pub fn interruption(
    proposed: &DesiredSubject,
    running: bool,
    incarnation: Option<&str>,
    launched: Option<&MemberSpec>,
) -> Interruption {
    if !running {
        return Interruption::None;
    }
    if proposed.kind == "stop" {
        return Interruption::Stop;
    }
    let Some(member) = proposed
        .member
        .as_ref()
        .filter(|member| proposed.kind == "agent" && member.lifecycle == MemberLifecycle::Service)
    else {
        return Interruption::None;
    };
    let Some(launched) = incarnation.and(launched) else {
        return Interruption::Unknown;
    };
    let changes = member.launch_changes(launched);
    if changes.is_empty() {
        Interruption::None
    } else {
        Interruption::Restart(changes)
    }
}

