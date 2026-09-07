use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn fixture(pty_json: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let pty_root = tmp.path().join("pty-root");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(catalog.join("agents/h/worker")).unwrap();
    fs::create_dir(&pty_root).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            pty_root.display().to_string()
        ),
    )
    .unwrap();
    fs::write(
        catalog.join("agents/h/worker/agent.kdl"),
        r#"
agent "worker" {
  host "h"
  pty "agent" {
    id "h.worker"
    lifecycle "adopt-only"
    argv "agent-bin"
  }
}
"#,
    )
    .unwrap();
    write_executable(
        &bin.join("pty"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            pty_json.replace('\'', "'\"'\"'")
        ),
    );
    (tmp, catalog, bin)
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut mode = fs::metadata(path).unwrap().permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).unwrap();
}

fn tasks(catalog: &Path, bin: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["tasks", "--host", "h", "--json", "--catalog"])
        .arg(catalog)
        .env("PATH", bin)
        .env("XDG_STATE_HOME", state)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .output()
        .unwrap()
}

fn real_tasks(catalog: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["tasks", "--host", "h", "--json", "--catalog"])
        .arg(catalog)
        .env("XDG_STATE_HOME", state)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .output()
        .unwrap()
}

fn seeded_supervisor_scope(catalog: &Path, host: Option<&str>, state: &Path) -> PathBuf {
    let mut command = Command::new(env!("CARGO_BIN_EXE_st2"));
    command
        .args(["unpark", "h.worker", "--catalog"])
        .arg(catalog);
    if let Some(host) = host {
        command.args(["--host", host]);
    }
    let output = command
        .env("XDG_STATE_HOME", state)
        .env_remove("CATALOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let supervisors = state.join("st2/supervisors");
    let scope = fs::read_dir(&supervisors)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|scope| scope.join("unpark/h.worker").is_file())
        .expect("unpark identifies its supervisor scope");
    fs::remove_file(scope.join("unpark/h.worker")).unwrap();
    scope
}

fn execute_recovery(action: &serde_json::Value, bin: &Path, ambient_catalog: &Path, state: &Path) {
    let mut command = if let Some(shell) = action.as_str() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", shell]);
        command
    } else {
        let argv = action["argv"]
            .as_array()
            .expect("recovery action carries argv");
        let mut command = Command::new(env!("CARGO_BIN_EXE_st2"));
        command.args(argv.iter().skip(1).map(|arg| arg.as_str().unwrap()));
        command
    };
    let output = command
        .env("PATH", bin)
        .env("CATALOG", ambient_catalog)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn projected_recovery_targets_its_exact_catalog_and_host_despite_ambient_defaults() {
    let (tmp, selected_catalog, bin) = fixture("[]");
    symlink(env!("CARGO_BIN_EXE_st2"), bin.join("st2")).unwrap();
    let ambient_catalog = tmp.path().join("ambient-catalog");
    fs::create_dir(&ambient_catalog).unwrap();
    let state = tmp.path().join("state");

    let selected_scope = seeded_supervisor_scope(&selected_catalog, Some("h"), &state);
    let ambient_scope = seeded_supervisor_scope(&ambient_catalog, None, &state);
    assert_ne!(selected_scope, ambient_scope);
    for scope in [&selected_scope, &ambient_scope] {
        let projection = st2::park::ParkProjection::current(scope.join("parked")).unwrap();
        assert!(
            projection
                .publish(
                    &BTreeSet::from(["h.worker".to_string()]),
                    "same-id collision"
                )
                .is_empty()
        );
    }

    let output = tasks(&selected_catalog, &bin, &state);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inventory: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let recovery = &inventory["tasks"][0]["parked"]["recovery"];
    execute_recovery(recovery, &bin, &ambient_catalog, &state);

    let (selected, selected_errors) =
        st2::park::take_unpark_requests(&selected_scope.join("unpark"));
    let (ambient, ambient_errors) = st2::park::take_unpark_requests(&ambient_scope.join("unpark"));
    assert!(selected_errors.is_empty() && ambient_errors.is_empty());
    assert_eq!(selected, ["h.worker"]);
    assert!(
        ambient.is_empty(),
        "emitted recovery targeted the ambient supervisor scope"
    );
}

#[test]
fn tasks_cli_emits_stable_complete_generation_without_mutation() {
    let (tmp, catalog, bin) = fixture(
        r#"[{"name":"h.worker","status":"running","pid":4000000,"createdAt":"2026-07-31T10:00:00.000Z"},{"name":"human.scratch","status":"running","pid":4000001,"createdAt":"2026-07-31T10:00:01.000Z"}]"#,
    );
    let agent = catalog.join("agents/h/worker/agent.kdl");
    let catalog_config = catalog.join("catalog.kdl");
    let before_agent = fs::read(&agent).unwrap();
    let before_catalog = fs::read(&catalog_config).unwrap();
    let state = tmp.path().join("state");
    let first = tasks(&catalog, &bin, &state);
    let second = tasks(&catalog, &bin, &state);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout, "unchanged generation drifted");
    let value: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["schema"], "st2.task-inventory.v2");
    assert_eq!(value["complete"], true);
    assert_eq!(value["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(value["tasks"][0]["agent"], "h.worker");
    assert_eq!(value["tasks"][0]["task"], "agent");
    assert_eq!(value["tasks"][0]["runtimeId"], "h.worker");
    assert_eq!(value["tasks"][0]["desiredState"], "running");
    assert_eq!(value["tasks"][0]["agentDesiredState"], "running");
    assert!(value["tasks"][0]["agentDesiredStateReason"].is_null());
    assert_eq!(value["tasks"][0]["runtime"]["pid"], 4_000_000);
    assert_eq!(
        value["tasks"][0]["runtime"]["createdAt"],
        "2026-07-31T10:00:00.000Z"
    );
    assert!(
        value["tasks"][0]["runtime"]["generationId"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        value["tasks"][0]["runtime"]["resourceTarget"],
        serde_json::json!({
            "type": "unavailable",
            "reason": "processUnavailable"
        })
    );
    assert_eq!(fs::read(agent).unwrap(), before_agent);
    assert_eq!(fs::read(catalog_config).unwrap(), before_catalog);
    assert!(!state.exists(), "read-only inventory created runtime state");
}

#[test]
fn driver_and_deliver_have_identical_end_to_end_inventory() {
    let (tmp, catalog, bin) = fixture("[]");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let legacy = r#"
agent "worker" {
  host "h"
  deliver "mcp"
  argv "claude" "--model" "opus" "--effort" "xhigh" "boot"
}
"#;
    let driver = r#"
agent "worker" {
  host "h"
  claude {
    model "opus"
    effort "xhigh"
    prompt "boot"
  }
}
"#;
    fs::write(&declaration, legacy).unwrap();
    let legacy = tasks(&catalog, &bin, &tmp.path().join("state"));
    assert!(
        legacy.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    fs::write(&declaration, driver).unwrap();
    let driver = tasks(&catalog, &bin, &tmp.path().join("state"));
    assert!(
        driver.status.success(),
        "{}",
        String::from_utf8_lossy(&driver.stderr)
    );

    let legacy: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
    let driver: serde_json::Value = serde_json::from_slice(&driver.stdout).unwrap();
    assert_eq!(driver, legacy);
    assert_eq!(driver["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(driver["tasks"][0]["agent"], "h.worker");
    assert_eq!(driver["tasks"][0]["task"], "agent");
    assert_eq!(driver["tasks"][0]["kind"], "pty");
    assert_eq!(driver["tasks"][0]["lifecycle"], "service");
}

#[test]
fn suspended_agent_projects_task_absence_and_agent_rationale_separately() {
    let (tmp, catalog, bin) = fixture("[]");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let authored = fs::read_to_string(&declaration).unwrap().replace(
        "  host \"h\"\n",
        "  host \"h\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"\n",
    );
    fs::write(declaration, authored).unwrap();

    let output = tasks(&catalog, &bin, &tmp.path().join("state"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let row = &value["tasks"][0];
    assert_eq!(row["retired"], false);
    assert_eq!(row["desiredState"], "absent");
    assert_eq!(row["agentDesiredState"], "suspended");
    assert_eq!(row["agentDesiredStateReason"], "Waiting for capacity");
    assert_eq!(row["runtime"]["state"], "absent");
}

#[test]
fn completed_catalog_aba_during_runtime_observation_is_incomplete() {
    let (tmp, catalog, bin) = fixture("[]");
    let prepared_a = tmp.path().join("prepared-a");
    let prepared_b = tmp.path().join("prepared-b");
    let snapshot = |output: &Path| {
        let result = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args([
                "catalog",
                "snapshot",
                "--catalog",
                catalog.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()
    };
    let root_a = snapshot(&prepared_a)["rootSha256"]
        .as_str()
        .unwrap()
        .to_string();
    snapshot(&prepared_b);
    let b_spec = prepared_b.join("agents/h/worker/agent.kdl");
    let bytes = fs::read_to_string(&b_spec)
        .unwrap()
        .replace("agent \"worker\" {", "agent \"worker\" {\n  name \"temporary\"");
    fs::write(&b_spec, bytes).unwrap();

    let observer_ready = tmp.path().join("observer-ready");
    let observer_release = tmp.path().join("observer-release");
    write_executable(
        &bin.join("pty"),
        &format!(
            "#!/bin/sh\n: > {:?}\nwhile [ ! -e {:?} ]; do :; done\nprintf '[]\\n'\n",
            observer_ready, observer_release
        ),
    );
    let inventory = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["tasks", "--host", "h", "--json", "--catalog"])
        .arg(&catalog)
        .env("PATH", &bin)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !observer_ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "runtime observer did not start"
        );
        std::thread::yield_now();
    }

    let apply = |prepared: &Path, expected: &str| {
        let input_sha256 = st2::catalog_transaction::digest_prepared(&catalog, prepared)
            .unwrap()
            .root_sha256;
        Command::new(env!("CARGO_BIN_EXE_st2"))
            .args([
                "catalog",
                "apply",
                "--catalog",
                catalog.to_str().unwrap(),
                "--prepared",
                prepared.to_str().unwrap(),
                "--input-sha256",
                &input_sha256,
                "--expect-sha256",
                expected,
                "--json",
            ])
            .output()
            .unwrap()
    };
    let to_b = apply(&prepared_b, &root_a);
    assert!(
        to_b.status.success(),
        "{}",
        String::from_utf8_lossy(&to_b.stderr)
    );
    let root_b = serde_json::from_slice::<serde_json::Value>(&to_b.stdout).unwrap()["afterSha256"]
        .as_str()
        .unwrap()
        .to_string();
    let to_a = apply(&prepared_a, &root_b);
    assert!(
        to_a.status.success(),
        "{}",
        String::from_utf8_lossy(&to_a.stderr)
    );
    fs::write(&observer_release, "").unwrap();

    let inventory = inventory.wait_with_output().unwrap();
    assert!(!inventory.status.success());
    let value: serde_json::Value = serde_json::from_slice(&inventory.stdout).unwrap();
    assert_eq!(value["complete"], false);
    assert!(value["errors"].as_array().unwrap().iter().any(|error| {
        error
            .as_str()
            .is_some_and(|error| error.contains("generation changed"))
    }));
}

#[test]
fn completed_single_agent_writer_abas_during_runtime_observation_are_incomplete() {
    for writer in ["publish", "presentation"] {
        let (tmp, catalog, bin) = fixture("[]");
        let spec = catalog.join("agents/h/worker/agent.kdl");
        let original = fs::read_to_string(&spec).unwrap();
        let observer_ready = tmp.path().join("observer-ready");
        let observer_release = tmp.path().join("observer-release");
        write_executable(
            &bin.join("pty"),
            &format!(
                "#!/bin/sh\n: > {:?}\nwhile [ ! -e {:?} ]; do :; done\nprintf '[]\\n'\n",
                observer_ready, observer_release
            ),
        );
        let inventory = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["tasks", "--host", "h", "--json", "--catalog"])
            .arg(&catalog)
            .env("PATH", &bin)
            .env("XDG_STATE_HOME", tmp.path().join("state"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !observer_ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "runtime observer did not start"
            );
            std::thread::yield_now();
        }

        if writer == "publish" {
            let source_a = tmp.path().join("source-a.kdl");
            let source_b = tmp.path().join("source-b.kdl");
            fs::write(&source_a, &original).unwrap();
            fs::write(
                &source_b,
                original.replace(
                    "agent \"worker\" {",
                    "agent \"worker\" {\n  name \"temporary\"",
                ),
            )
            .unwrap();
            let digest = |source: &Path| {
                let output = Command::new(env!("CARGO_BIN_EXE_st2"))
                    .args(["agent", "digest", "--spec"])
                    .arg(source)
                    .arg("--json")
                    .output()
                    .unwrap();
                assert!(output.status.success());
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["sha256"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let digest_a = digest(&source_a);
            let digest_b = digest(&source_b);
            for (source, input, expected) in [
                (&source_b, digest_b.as_str(), digest_a.as_str()),
                (&source_a, digest_a.as_str(), digest_b.as_str()),
            ] {
                let output = Command::new(env!("CARGO_BIN_EXE_st2"))
                    .args([
                        "agent",
                        "publish",
                        "--catalog",
                        catalog.to_str().unwrap(),
                        "--spec",
                        source.to_str().unwrap(),
                        "--input-sha256",
                        input,
                        "--expect-sha256",
                        expected,
                    ])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } else {
            for args in [
                vec!["rename", "h.worker", "temporary", "--host", "h"],
                vec!["rename", "h.worker", "--clear", "--host", "h"],
            ] {
                let output = Command::new(env!("CARGO_BIN_EXE_st2"))
                    .arg("--catalog")
                    .arg(&catalog)
                    .args(args)
                    .env_remove("ST_AGENT")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        assert_eq!(fs::read_to_string(&spec).unwrap(), original);
        assert_eq!(
            fs::read_to_string(catalog.join(".st2/catalog-generation")).unwrap(),
            "2\n"
        );
        fs::write(&observer_release, "").unwrap();
        let inventory = inventory.wait_with_output().unwrap();
        assert!(
            !inventory.status.success(),
            "{writer} ABA was reported complete"
        );
        let value: serde_json::Value = serde_json::from_slice(&inventory.stdout).unwrap();
        assert_eq!(value["complete"], false);
        assert!(value["errors"].as_array().unwrap().iter().any(|error| {
            error
                .as_str()
                .is_some_and(|error| error.contains("generation changed"))
        }));
    }
}

#[test]
fn incomplete_or_malformed_pty_generation_is_json_nonzero_and_never_absent() {
    for pty_json in [
        r#"[{"name":"h.worker","status":"running","pid":77}]"#,
        r#"[{"name":"h.worker","status":"running","pid":0,"createdAt":"not-a-time"}]"#,
        "not-json",
    ] {
        let (tmp, catalog, bin) = fixture(pty_json);
        let output = tasks(&catalog, &bin, &tmp.path().join("state"));
        assert!(!output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["tasks"][0]["runtime"]["state"], "indeterminate");
        assert_ne!(value["tasks"][0]["runtime"]["state"], "absent");
    }
}

#[test]
fn failed_and_timed_out_pty_commands_are_typed_incomplete() {
    let (tmp, catalog, bin) = fixture("[]");
    write_executable(
        &bin.join("pty"),
        "#!/bin/sh\nprintf 'fixture failure' >&2\nexit 7\n",
    );
    let failed = tasks(&catalog, &bin, &tmp.path().join("failed-state"));
    assert!(!failed.status.success());
    let failed_json: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed_json["complete"], false);
    assert!(
        failed_json["errors"][0]
            .as_str()
            .unwrap()
            .contains("fixture failure")
    );

    let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sleep"))
        .find(|path| path.is_file())
        .expect("sleep executable on the test PATH");
    write_executable(
        &bin.join("pty"),
        &format!("#!/bin/sh\nexec {:?} 30\n", sleep.display().to_string()),
    );
    let started = std::time::Instant::now();
    let timed_out = tasks(&catalog, &bin, &tmp.path().join("timeout-state"));
    assert!(!timed_out.status.success());
    assert!(started.elapsed() < Duration::from_secs(10));
    let timeout_json: serde_json::Value = serde_json::from_slice(&timed_out.stdout).unwrap();
    assert_eq!(timeout_json["complete"], false);
    assert!(
        timeout_json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error
                    .as_str()
                    .is_some_and(|error| error.contains("timed out"))
            })
    );
}

#[test]
fn legacy_exec_generation_is_joined_read_only_without_calling_pty() {
    let (tmp, catalog, bin) = fixture("[]");
    fs::write(
        catalog.join("agents/h/worker/agent.kdl"),
        r#"
agent "worker" {
  host "h"
  exec "ding" {
    command "sleep 30"
  }
}
"#,
    )
    .unwrap();
    write_executable(&bin.join("pty"), "#!/bin/sh\nexit 99\n");
    std::thread::sleep(Duration::from_millis(20));
    let state = tmp.path().join("state");
    let exec_state = state.join("st2/h/exec");
    fs::create_dir_all(&exec_state).unwrap();
    let pid_file = exec_state.join("h.worker.ding.pid");
    let legacy = std::process::id().to_string();
    fs::write(&pid_file, &legacy).unwrap();
    let before_entries = fs::read_dir(&exec_state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();

    let output = tasks(&catalog, &bin, &state);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["complete"], true);
    assert_eq!(value["tasks"][0]["kind"], "exec");
    assert_eq!(value["tasks"][0]["runtime"]["state"], "running");
    assert_eq!(
        value["tasks"][0]["runtime"]["pid"],
        u64::from(std::process::id())
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        value["tasks"][0]["runtime"]["resourceTarget"],
        serde_json::json!({
            "type": "darwinProcessTree",
            "rootPid": std::process::id()
        })
    );
    #[cfg(target_os = "linux")]
    {
        let target = &value["tasks"][0]["runtime"]["resourceTarget"];
        assert_eq!(target["type"], "linuxCgroupV2");
        assert!(target["path"].as_str().unwrap().starts_with('/'));
    }
    assert_eq!(fs::read_to_string(&pid_file).unwrap(), legacy);
    let after_entries = fs::read_dir(&exec_state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        after_entries, before_entries,
        "inventory changed exec state"
    );
}

#[test]
fn missing_pty_root_is_positive_absence_without_creation_or_invocation() {
    let (tmp, catalog, bin) = fixture("[]");
    let missing = tmp.path().join("missing-root");
    fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            missing.display().to_string()
        ),
    )
    .unwrap();
    write_executable(
        &bin.join("pty"),
        &format!(
            "#!/bin/sh\ntouch {:?}\nexit 99\n",
            tmp.path().join("PTY-WAS-CALLED").display().to_string()
        ),
    );
    let output = tasks(&catalog, &bin, &tmp.path().join("state"));
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["tasks"][0]["runtime"]["state"], "absent");
    assert_eq!(
        value["tasks"][0]["runtime"]["resourceTarget"],
        serde_json::json!({
            "type": "unavailable",
            "reason": "notRunning"
        })
    );
    assert!(!missing.exists());
    assert!(!tmp.path().join("PTY-WAS-CALLED").exists());
}

#[test]
fn catalog_semantic_drift_during_observation_fails_closed() {
    let (tmp, catalog, bin) = fixture("[]");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let replacement = r#"agent "worker" {
  host "h"
  pty "replacement" { id "h.replacement"; argv "agent-bin" }
}
"#;
    write_executable(
        &bin.join("pty"),
        &format!(
            "#!/bin/sh\nprintf '%s' '{}' > {:?}\nprintf '%s\\n' '[]'\n",
            replacement.replace('\'', "'\"'\"'"),
            declaration.display().to_string()
        ),
    );
    let output = tasks(&catalog, &bin, &tmp.path().join("state"));
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["complete"], false);
    assert!(value["errors"].as_array().unwrap().iter().any(|error| {
        error
            .as_str()
            .is_some_and(|error| error.contains("changed during task observation"))
    }));
}

#[test]
fn missing_catalog_and_missing_json_flag_fail_explicitly() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing-catalog");
    let missing_output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["tasks", "--host", "h", "--json", "--catalog"])
        .arg(&missing)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .output()
        .unwrap();
    assert!(!missing_output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&missing_output.stdout).unwrap();
    assert_eq!(value["schema"], "st2.task-inventory.v2");
    assert_eq!(value["complete"], false);
    assert!(value["tasks"].as_array().unwrap().is_empty());

    let (fixture_tmp, catalog, bin) = fixture("[]");
    let no_json = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["tasks", "--host", "h", "--catalog"])
        .arg(&catalog)
        .env("PATH", bin)
        .env("XDG_STATE_HOME", fixture_tmp.path().join("state"))
        .output()
        .unwrap();
    assert!(!no_json.status.success());
    assert!(String::from_utf8_lossy(&no_json.stderr).contains("requires --json"));
}

struct RealPtyCleanup {
    root: PathBuf,
    id: &'static str,
}

impl Drop for RealPtyCleanup {
    fn drop(&mut self) {
        for verb in ["kill", "rm"] {
            let _ = Command::new("pty")
                .args([verb, self.id])
                .env("PTY_ROOT", &self.root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn remove_real_pty(root: &Path, id: &str) {
    let killed = Command::new("pty")
        .args(["kill", id])
        .env("PTY_ROOT", root)
        .output()
        .unwrap();
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let removed = Command::new("pty")
            .args(["rm", id])
            .env("PTY_ROOT", root)
            .output()
            .unwrap();
        if removed.status.success() {
            return;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn packaged_tasks_tracks_real_pty_generation_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let pty_root = tmp.path().join("pty");
    fs::create_dir_all(catalog.join("agents/h/worker")).unwrap();
    fs::create_dir(&pty_root).unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            pty_root.display().to_string()
        ),
    )
    .unwrap();
    fs::write(
        catalog.join("agents/h/worker/agent.kdl"),
        r#"agent "worker" {
  host "h"
  pty "agent" { id "h.worker"; argv "agent-bin" }
}
"#,
    )
    .unwrap();
    let _cleanup = RealPtyCleanup {
        root: pty_root.clone(),
        id: "h.worker",
    };
    let launch = || {
        Command::new("pty")
            .args(["run", "-d", "--id", "h.worker", "--", "sleep", "120"])
            .env("PTY_ROOT", &pty_root)
            .output()
            .unwrap()
    };
    let first_launch = launch();
    assert!(
        first_launch.status.success(),
        "{}",
        String::from_utf8_lossy(&first_launch.stderr)
    );
    let state = tmp.path().join("state");
    let first = real_tasks(&catalog, &state);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_generation = first_json["tasks"][0]["runtime"]["generationId"]
        .as_str()
        .unwrap()
        .to_owned();
    #[cfg(target_os = "macos")]
    assert_eq!(
        first_json["tasks"][0]["runtime"]["resourceTarget"],
        serde_json::json!({
            "type": "darwinProcessTree",
            "rootPid": first_json["tasks"][0]["runtime"]["pid"]
        })
    );
    #[cfg(target_os = "linux")]
    {
        let target = &first_json["tasks"][0]["runtime"]["resourceTarget"];
        assert_eq!(target["type"], "linuxCgroupV2");
        assert!(target["path"].as_str().unwrap().starts_with('/'));
    }

    remove_real_pty(&pty_root, "h.worker");
    let second_launch = launch();
    assert!(
        second_launch.status.success(),
        "{}",
        String::from_utf8_lossy(&second_launch.stderr)
    );
    let second = real_tasks(&catalog, &state);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_ne!(
        second_json["tasks"][0]["runtime"]["generationId"]
            .as_str()
            .unwrap(),
        first_generation
    );
}

#[test]
fn a_derived_stream_task_is_reported_honestly_alongside_its_agent() {
    let (tmp, catalog, bin) = fixture(
        r#"[{"name":"h.streamed","status":"running","pid":91,"createdAt":"2026-08-20T10:00:00.000Z"}]"#,
    );
    fs::create_dir_all(catalog.join("agents/h/streamed")).unwrap();
    fs::write(
        catalog.join("agents/h/streamed/agent.kdl"),
        r#"
agent "streamed" {
  host "h"
  command "true"
  stream "gh-ci" { command "poll-gh-ci.sh" }
}
"#,
    )
    .unwrap();
    let output = tasks(&catalog, &bin, &tmp.path().join("state"));
    let inventory: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = inventory["tasks"].as_array().unwrap();
    let stream = rows
        .iter()
        .find(|row| row["task"] == "stream-gh-ci")
        .unwrap_or_else(|| {
            panic!(
                "no stream row in {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(stream["agent"], "h.streamed");
    assert_eq!(stream["runtimeId"], "h.streamed.stream-gh-ci");
    assert_eq!(stream["kind"], "exec");
    assert_eq!(stream["runtime"]["state"], "absent");
    assert!(stream.get("parked").is_none() || stream["parked"].is_null());
}
