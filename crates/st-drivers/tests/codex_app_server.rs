use std::fs;
use std::path::Path;

use st_drivers::DeliveryTransport;
use st_drivers::reconcile::{TaskCompileContext, compile_generated_tasks};

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn context(root: &Path) -> TaskCompileContext {
    let executable = root.join("bin/st2");
    write(&executable, "test binary");
    TaskCompileContext::new(root.to_path_buf(), executable).unwrap()
}

#[test]
fn app_server_selector_wraps_the_canonical_argv_with_exact_owner_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let declaration = tmp.path().join("agents/h/worker/agent.kdl");
    write(
        &declaration,
        r#"agent "worker" {
  host "h"
  deliver "app-server"
  argv "codex" "--model" "gpt-test" "boot"
}
"#,
    );
    let mut found = st_drivers::discover(tmp.path());
    assert!(found.errors.is_empty(), "{:?}", found.errors);

    compile_generated_tasks(&mut found.specs, "h", &context(tmp.path())).unwrap();

    let spec = &found.specs[0];
    assert_eq!(spec.delivery, Some(DeliveryTransport::AppServer));
    let task = spec.tasks.iter().find(|task| task.name == "agent").unwrap();
    assert_eq!(task.command, None);
    assert_eq!(
        task.argv.as_deref(),
        Some(
            [
                tmp.path().join("bin/st2").display().to_string(),
                "--catalog".into(),
                tmp.path().display().to_string(),
                "codex-app-server".into(),
                "--identity".into(),
                "h.worker".into(),
                "--runtime-id".into(),
                "h.worker".into(),
                "--".into(),
                "codex".into(),
                "--model".into(),
                "gpt-test".into(),
                "boot".into(),
            ]
            .as_slice()
        )
    );
}

#[test]
fn codex_driver_matches_deliver_after_normalizing_only_the_subcommand_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy_path = tmp.path().join("legacy.kdl");
    let driver_path = tmp.path().join("driver.kdl");
    write(
        &legacy_path,
        r#"agent "worker" {
  host "h"
  lifecycle "adopt-only"
  env { CODEX_HOME "$CATALOG/codex" }
  deliver "app-server"
  argv "codex" "--model" "gpt-test" "-c" "model_reasoning_effort=xhigh" "--model" "override" "boot"
}
"#,
    );
    write(
        &driver_path,
        r#"agent "worker" {
  host "h"
  lifecycle "adopt-only"
  env { CODEX_HOME "$CATALOG/codex" }
  codex {
    model "gpt-test"
    effort "xhigh"
    prompt "boot"
    args "--model" "override"
  }
}
"#,
    );
    let (legacy, _) = st_drivers::discover_file(tmp.path(), &legacy_path).unwrap();
    let (driver, _) = st_drivers::discover_file(tmp.path(), &driver_path).unwrap();
    let mut legacy = legacy.into_iter().next().unwrap();
    let mut driver = driver.into_iter().next().unwrap();
    let compile_context = context(tmp.path());
    assert!(st_drivers::hooks::required_by_codex_agent(
        &driver,
        "h",
        tmp.path()
    ));

    compile_generated_tasks(std::slice::from_mut(&mut legacy), "h", &compile_context).unwrap();
    compile_generated_tasks(std::slice::from_mut(&mut driver), "h", &compile_context).unwrap();

    let legacy_task = legacy
        .tasks
        .iter()
        .find(|task| task.name == "agent")
        .unwrap()
        .clone();
    let mut driver_task = driver
        .tasks
        .iter()
        .find(|task| task.name == "agent")
        .unwrap()
        .clone();
    let argv = driver_task.argv.as_mut().unwrap();
    assert_eq!(&argv[3..5], ["driver", "codex"]);
    argv.splice(3..5, ["codex-app-server".to_string()]);

    assert_eq!(driver_task, legacy_task);
}

#[test]
fn app_server_selector_rejects_shell_and_pre_remote_launches_without_mutating_them() {
    for (name, launch, expected) in [
        (
            "shell",
            "command \"exec codex\"",
            "must use structured `argv`",
        ),
        (
            "remote",
            "argv \"codex\" \"--remote\" \"unix:///other.sock\"",
            "already declares `--remote`",
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(format!("agents/h/{name}/agent.kdl")),
            &format!("agent \"{name}\" {{ host \"h\"; deliver \"app-server\"; {launch} }}"),
        );
        let mut found = st_drivers::discover(tmp.path());
        assert!(found.errors.is_empty(), "{name}: {:?}", found.errors);
        let before = found.specs.clone();
        let error =
            compile_generated_tasks(&mut found.specs, "h", &context(tmp.path())).unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert_eq!(found.specs, before, "{name} compile failure mutated source");
    }
}

#[test]
fn mcp_selector_wraps_the_authored_claude_launch_with_session_ownership() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("agents/h/worker/agent.kdl"),
        r#"agent "worker" { host "h"; deliver "mcp"; argv "claude" "boot" }"#,
    );
    let mut found = st_drivers::discover(tmp.path());
    compile_generated_tasks(&mut found.specs, "h", &context(tmp.path())).unwrap();

    let task = &found.specs[0].tasks[0];
    assert_eq!(task.command, None);
    assert_eq!(
        task.argv.as_deref(),
        Some(
            [
                tmp.path().join("bin/st2").display().to_string(),
                "--catalog".into(),
                tmp.path().display().to_string(),
                "driver".into(),
                "claude-session".into(),
                "--identity".into(),
                "h.worker".into(),
                "--runtime-id".into(),
                "h.worker".into(),
                "--".into(),
                "claude".into(),
                "boot".into(),
            ]
            .as_slice()
        )
    );
}
