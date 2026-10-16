use crate::publication::*;
use crate::model::{DesiredSubject, MemberLifecycle};
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
