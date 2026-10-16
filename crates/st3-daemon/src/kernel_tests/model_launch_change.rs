use crate::model::{LaunchSpec, MemberSpec};

fn claude(settings: &str, model: &str) -> MemberSpec {
    let intent = crate::graph::parse_intent(
        &format!(
            "version 2\nagent \"example/worker\" {{ workspace \"/work\"; harness \"claude\" {{ model {model:?}; }} }}"
        ),
        "example-host",
    )
    .unwrap();
    let mut member = intent.subjects["agent/example/worker"]
        .member
        .clone()
        .unwrap();
    // Stand in for another st build's hook registration.
    if let LaunchSpec::Argv(argv) = &mut member.launch {
        let at = argv.iter().position(|item| item == "--settings").unwrap();
        argv[at + 1] = settings.into();
    }
    member
}

#[test]
fn only_what_the_author_declares_changes_a_launch() {
    let launched = claude("{\"hooks\":{}}", "example-model");
    // Another st build's hook settings, a new run generation's environment and a new
    // restart policy launch the same harness.
    let mut same = claude("{\"hooks\":{\"Stop\":[]}}", "example-model");
    same.environment
        .insert("ST_RUN_GENERATION".into(), "next".into());
    same.restart = crate::model::RestartType::Never;
    same.one_shot = true;
    assert!(same.launch_changes(&launched).is_empty());

    assert_eq!(
        claude("{\"hooks\":{}}", "another-model").launch_changes(&launched),
        ["launch"]
    );
    let mut moved = launched.clone();
    moved.workspace = "/elsewhere".into();
    moved.cwd = "/elsewhere".into();
    assert_eq!(moved.launch_changes(&launched), ["workspace"]);
    let mut switched = launched.clone();
    switched.driver = Some("codex".into());
    assert_eq!(switched.launch_changes(&launched), ["harness"]);
    let mut command = launched.clone();
    command.launch = LaunchSpec::Shell("sleep 1".into());
    let mut other = command.clone();
    other.launch = LaunchSpec::Shell("sleep 2".into());
    assert_eq!(other.launch_changes(&command), ["launch"]);
}
