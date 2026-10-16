//! The launch boundary shared by publication and reconciliation.
use crate::model::{DesiredSubject, MemberLifecycle, MemberSpec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Interruption {
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
pub(crate) fn interruption(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::parse_intent;

    fn seat() -> DesiredSubject {
        parse_intent(
            "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; }",
            "node",
        )
        .unwrap()
        .subjects
        .remove("agent/example/worker")
        .unwrap()
    }

    #[test]
    fn launch_boundary_distinguishes_stop_start_observe_and_unknown() {
        let mut proposed = seat();
        let launched = proposed.member.clone().unwrap();
        assert_eq!(
            interruption(&proposed, false, None, None),
            Interruption::None
        );
        assert_eq!(
            interruption(&proposed, true, Some("one"), Some(&launched)),
            Interruption::None
        );
        assert_eq!(
            interruption(&proposed, true, None, Some(&launched)),
            Interruption::Unknown
        );
        assert_eq!(
            interruption(&proposed, true, Some("one"), None),
            Interruption::Unknown
        );
        proposed
            .member
            .as_mut()
            .unwrap()
            .environment
            .insert("EXAMPLE".into(), "new".into());
        assert_eq!(
            interruption(&proposed, true, Some("one"), Some(&launched)),
            Interruption::None
        );
        proposed.member.as_mut().unwrap().workspace = "/example/changed".into();
        assert_eq!(
            interruption(&proposed, true, Some("one"), Some(&launched)),
            Interruption::Restart(vec!["workspace"])
        );
        proposed.member.as_mut().unwrap().lifecycle = MemberLifecycle::AdoptOnly;
        assert_eq!(
            interruption(&proposed, true, Some("one"), None),
            Interruption::None
        );
        proposed.kind = "stop".into();
        assert_eq!(
            interruption(&proposed, true, Some("one"), None),
            Interruption::Stop
        );
        assert_eq!(
            interruption(&proposed, false, None, None),
            Interruption::None
        );
    }
}
