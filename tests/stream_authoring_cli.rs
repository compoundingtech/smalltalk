use st2::Runner as _;

static RUNTIME_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
use std::fs;
use std::path::Path;
use std::process::Command;

fn write_agent(root: &Path) {
    let directory = root.join("agents/hetz/worker");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("agent.kdl"),
        "agent \"worker\" {\n  host \"hetz\"\n  command \"agent\"\n}\n",
    )
    .unwrap();
    st2::event::publish_owner_binding_for_test(root, "hetz").unwrap();
}

fn st2(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(root)
        .args(args)
        .env("ST2_TEST_EVENT_HOST", "hetz")
        .env_remove("ST_AGENT")
        .output()
        .unwrap()
}

#[test]
fn stream_add_emit_and_rm_are_one_real_cli_workflow() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());

    let add = st2(
        catalog.path(),
        &[
            "stream",
            "add",
            "webhook",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--json",
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&add.stdout).unwrap();
    assert_eq!(receipt["result"], "changed");
    assert_eq!(receipt["name"], "webhook");
    assert!(receipt["launch"].is_null());

    let emit = st2(
        catalog.path(),
        &[
            "event",
            "emit",
            "hetz.worker",
            "--stream",
            "webhook",
            "--event-id",
            "delivery-1",
            "--message",
            "payload",
            "--host",
            "hetz",
            "--json",
        ],
    );
    assert!(
        emit.status.success(),
        "{}",
        String::from_utf8_lossy(&emit.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&emit.stdout).unwrap();
    assert_eq!(receipt["status"], "created");

    let remove = st2(
        catalog.path(),
        &[
            "stream",
            "rm",
            "webhook",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--json",
        ],
    );
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&remove.stdout).unwrap();
    assert_eq!(receipt["result"], "changed");
    let declaration =
        fs::read_to_string(catalog.path().join("agents/hetz/worker/agent.kdl")).unwrap();
    assert!(!declaration.contains("stream \"webhook\""));
}

#[test]
fn a_direct_adapter_launch_executes_the_exact_event_cli_contract() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    let binary = env!("CARGO_BIN_EXE_st2");
    let add = st2(
        catalog.path(),
        &[
            "stream",
            "add",
            "adapter",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--",
            binary,
            "--catalog",
            catalog.path().to_str().unwrap(),
            "event",
            "emit",
            "hetz.worker",
            "--stream",
            "adapter",
            "--event-id",
            "adapter-1",
            "--message",
            "from-adapter",
            "--host",
            "hetz",
            "--json",
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let spec = st2::discover(catalog.path()).specs.remove(0);
    let adapter = spec
        .tasks
        .iter()
        .find(|task| task.name == "stream-adapter")
        .unwrap();
    assert!(adapter.derived);
    let argv = adapter.argv.as_ref().unwrap();
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .env("ST2_TEST_EVENT_HOST", "hetz")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "created");
    assert_eq!(
        st2::message::list_inbox(&st2::message::inbox_dir(
            &catalog.path().join("agents/hetz/worker")
        ))
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn direct_adapter_argv_preserves_spaces_and_metacharacters_exactly() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    let expected = [
        "/bin/example adapter",
        "argument with spaces",
        "$HOME",
        "$(never-executed)",
        "semi;colon",
        "quote\"and\\slash",
        "--looks-like-a-flag",
    ];
    let mut args = vec![
        "stream",
        "add",
        "exact-argv",
        "--agent",
        "hetz.worker",
        "--host",
        "hetz",
        "--",
    ];
    args.extend(expected);

    let add = st2(catalog.path(), &args);

    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let spec = st2::discover(catalog.path()).specs.remove(0);
    let stream = spec
        .streams
        .iter()
        .find(|stream| stream.name == "exact-argv")
        .unwrap();
    assert_eq!(
        stream.launch,
        Some(st2::spec::StreamLaunch::Argv(
            expected.iter().map(|value| (*value).to_owned()).collect()
        ))
    );
}

#[test]
fn command_and_direct_argv_are_mutually_exclusive() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());

    let add = st2(
        catalog.path(),
        &[
            "stream",
            "add",
            "ambiguous",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--command",
            "echo shell",
            "--",
            "/bin/echo",
            "direct",
        ],
    );

    assert!(!add.status.success());
    assert!(
        String::from_utf8_lossy(&add.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
}

#[test]
fn launched_stream_removal_retires_runtime_before_source_publication() {
    let _runtime = RUNTIME_TEST.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    let add = st2(
        catalog.path(),
        &[
            "stream",
            "add",
            "live",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--command",
            "sleep 60",
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let runner = st2::SystemRunner::new(catalog.path().to_path_buf(), st2::exec_state_dir("hetz"));
    for _ in 0..3 {
        let report = st2::up_once(catalog.path(), "hetz", &runner).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        if runner
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| session.alive && session.pty_id == "hetz.worker.stream-live")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        runner
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| { session.alive && session.pty_id == "hetz.worker.stream-live" })
    );

    let remove = st2(
        catalog.path(),
        &[
            "stream",
            "rm",
            "live",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
        ],
    );

    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let mut retired = false;
    for _ in 0..100 {
        retired = runner
            .list_sessions()
            .unwrap()
            .iter()
            .all(|session| !session.alive || session.pty_id != "hetz.worker.stream-live");
        if retired {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        retired,
        "stream adapter remained alive after lifecycle-first removal"
    );
    assert!(
        !fs::read_to_string(catalog.path().join("agents/hetz/worker/agent.kdl"))
            .unwrap()
            .contains("stream \"live\"")
    );
}

#[test]
fn launched_stream_removal_escalates_an_ignoring_adapter_before_forgetting_it() {
    let _runtime = RUNTIME_TEST.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    let add = st2(
        catalog.path(),
        &[
            "stream",
            "add",
            "stubborn",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
            "--command",
            "trap '' TERM; while :; do sleep 1; done",
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let runner = st2::SystemRunner::new(catalog.path().to_path_buf(), st2::exec_state_dir("hetz"));
    assert!(
        st2::up_once(catalog.path(), "hetz", &runner)
            .unwrap()
            .errors
            .is_empty()
    );
    let runtime_id = "hetz.worker.stream-stubborn";
    for _ in 0..100 {
        if runner
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| session.alive && session.pty_id == runtime_id)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let started = std::time::Instant::now();
    let remove = st2(
        catalog.path(),
        &[
            "stream",
            "rm",
            "stubborn",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
        ],
    );
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    assert!(
        runner
            .list_sessions()
            .unwrap()
            .iter()
            .all(|session| session.pty_id != runtime_id),
        "retirement must not erase the record until the ignoring process group exits"
    );
    assert!(
        !fs::read_to_string(catalog.path().join("agents/hetz/worker/agent.kdl"))
            .unwrap()
            .contains("stream \"stubborn\"")
    );
}

#[test]
fn failed_source_publish_after_stop_keeps_declaration_relaunchable() {
    let _runtime = RUNTIME_TEST.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    assert!(
        st2(
            catalog.path(),
            &[
                "stream",
                "add",
                "live",
                "--agent",
                "hetz.worker",
                "--host",
                "hetz",
                "--command",
                "sleep 60"
            ]
        )
        .status
        .success()
    );
    let runner = st2::SystemRunner::new(catalog.path().to_path_buf(), st2::exec_state_dir("hetz"));
    assert!(
        st2::up_once(catalog.path(), "hetz", &runner)
            .unwrap()
            .errors
            .is_empty()
    );
    let failed = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(catalog.path())
        .args([
            "stream",
            "rm",
            "live",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
        ])
        .env("ST2_TEST_AGENT_AUTHOR_FAIL_BEFORE_PUBLISH", "1")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        fs::read_to_string(catalog.path().join("agents/hetz/worker/agent.kdl"))
            .unwrap()
            .contains("stream \"live\"")
    );

    for _ in 0..3 {
        let report = st2::up_once(catalog.path(), "hetz", &runner).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        if runner
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| session.alive && session.pty_id == "hetz.worker.stream-live")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        runner
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| { session.alive && session.pty_id == "hetz.worker.stream-live" })
    );
    assert!(
        st2(
            catalog.path(),
            &[
                "stream",
                "rm",
                "live",
                "--agent",
                "hetz.worker",
                "--host",
                "hetz"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn external_stream_removal_performs_no_runtime_operation() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());
    assert!(
        st2(
            catalog.path(),
            &[
                "stream",
                "add",
                "external",
                "--agent",
                "hetz.worker",
                "--host",
                "hetz"
            ]
        )
        .status
        .success()
    );

    let remove = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(catalog.path())
        .args([
            "stream",
            "rm",
            "external",
            "--agent",
            "hetz.worker",
            "--host",
            "hetz",
        ])
        .env("PATH", "")
        .env_remove("ST_AGENT")
        .output()
        .unwrap();

    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
}

#[test]
fn a_bare_actor_can_self_author_on_the_selected_host() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path());

    let add = st2(
        catalog.path(),
        &[
            "stream", "add", "webhook", "--as", "worker", "--host", "hetz",
        ],
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let remove = st2(
        catalog.path(),
        &[
            "stream", "rm", "webhook", "--as", "worker", "--host", "hetz",
        ],
    );
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
}
