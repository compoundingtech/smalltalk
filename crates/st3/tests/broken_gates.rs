#![cfg(target_os = "linux")]
//! Exec gates that cannot answer hold their work for a revision instead of failing the run.
//! Exit-code field gates fail when a terminal exec cannot satisfy them.
//!
//! Each step replays a gate that failed a run whose work was done: a cargo gate whose PATH lacked
//! the linker (twice), a memory threshold that could never pass, a grep of a listing that showed
//! only its first page, and a listing limit st refuses. A revision of only the gates then passes
//! every step.
use std::fs::File;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const RUN: &str = "mission-run/orchid/replay";
const PUBLISHER: &str = "person/pat";

struct Daemon {
    root: PathBuf,
    child: Option<Child>,
}

impl Daemon {
    fn start(root: &Path) -> Self {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // These tests have no PTYs or external observers.
        for (name, source) in [
            ("pty", "#!/bin/sh\nprintf '[]\\n'\n"),
            ("gh", "#!/bin/sh\nexit 1\n"),
        ] {
            executable(&bin.join(name), source);
        }
        let log = File::create(root.join("daemon.log")).unwrap();
        let child = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
            .env_clear()
            .env("HOME", root)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .current_dir(root)
            .args(["up", "--node", "orchid", "--socket"])
            .arg(root.join("daemon.sock"))
            .arg("--client-gateway-socket")
            .arg(root.join("client.sock"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        std::fs::write(root.join("daemon.pid"), child.id().to_string()).unwrap();
        let mut daemon = Self {
            root: root.to_owned(),
            child: Some(child),
        };
        wait_for("the isolated daemon", || {
            assert!(
                daemon.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "{}",
                daemon.log()
            );
            std::os::unix::net::UnixStream::connect(root.join("daemon.sock")).is_ok()
        });
        daemon
    }

    fn command(&self, args: &[&str]) -> Value {
        let output = self.run_cli(args);
        assert!(
            output.status.success(),
            "{args:?}\n{}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.log()
        );
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }

    fn run_cli(&self, args: &[&str]) -> std::process::Output {
        st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
            .env_clear()
            .env("HOME", &self.root)
            .env("ST3_DAEMON_WAIT", "0")
            .current_dir(&self.root)
            .args(["--endpoint"])
            .arg(self.root.join("daemon.sock"))
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self) -> Value {
        self.command(&["missions", "show", RUN])
    }

    fn step(&self, path: &str) -> Value {
        self.run()["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["step"] == path)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Every `gate.result` claim so far, oldest first.
    fn gate_results(&self) -> Vec<Value> {
        let mut results = Vec::new();
        let mut after = 0;
        loop {
            let output = self.run_cli(&[
                "trace",
                "show",
                "--limit",
                "500",
                "--after-index",
                &after.to_string(),
            ]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let claims = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            let full = claims.len() == 500;
            after = claims
                .iter()
                .filter_map(|claim| claim["store_index"].as_u64())
                .max()
                .unwrap_or(after);
            results.extend(
                claims
                    .into_iter()
                    .filter(|claim| claim["kind"] == "gate.result"),
            );
            if !full {
                return results;
            }
        }
    }

    /// The newest result of the gate on step `path` in the run's current generation.
    fn gate_result(&self, path: &str) -> Option<Value> {
        let step = self.step(path)["subject"].as_str()?.to_owned();
        let operation = format!("gate-operation/{}/", step.replace('/', "."));
        self.gate_results().into_iter().rev().find(|claim| {
            claim["subject"]
                .as_str()
                .is_some_and(|subject| subject.starts_with(&operation))
        })
    }

    fn attention(&self) -> Vec<Value> {
        self.command(&["attention", "ls", "--as", PUBLISHER])["value"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("isolated daemon log: {}", self.log());
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait_for(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_terminal_exec_fails_its_unsatisfied_field_gate() {
    let root = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(root.path());
    let file = root.path().join("mission.kdl");
    std::fs::write(
        &file,
        r#"version 2
mission "orchid/replay" state="ready" {
  goal "Fail an impossible exit-code predicate."
  completion { when "all-steps-exhausted" }
  step "prepare" {
    agentless
    exec "prepare" {
      host "orchid"
      workspace "."
      command "exit 2"
      restart "never"
    }
    gate "prepared" {
      field "exit_code" "exec/${ST_MISSION_RUN}/prepare" is 0
    }
  }
  step "ship" {
    depends-on { step "prepare" completed }
    agentless
  }
}
"#,
    )
    .unwrap();
    daemon.command(&[
        "missions",
        "publish",
        file.to_str().unwrap(),
        "--as",
        PUBLISHER,
        "--no-gate-check",
    ]);
    daemon.command(&[
        "missions",
        "start",
        "orchid/replay",
        "--id",
        "orchid/replay",
        "--workspace",
        root.path().to_str().unwrap(),
        "--as",
        PUBLISHER,
    ]);
    wait_for("the failed field gate", || {
        daemon.step("prepare")["status"] == "failed"
    });
    let step = daemon.step("prepare");
    let reason = step["blocked_reason"].as_str().unwrap();
    assert!(reason.contains("exec/orchid/replay/prepare"), "{step}");
    assert!(reason.contains("exit code 2"), "{step}");
    wait_for("the run to fail", || daemon.run()["status"] == "failed");
    assert_ne!(daemon.step("ship")["status"], "completed");
    let results = daemon.gate_results();
    assert!(
        results.iter().any(|claim| {
            claim["body"]["fields"]["gate"] == "prepared"
                && claim["body"]["fields"]["verdict"] == "fail"
                && claim["body"]["fields"]["reason"] == reason
        }),
        "{results:?}"
    );
}

#[test]
fn missions_and_doctor_surface_a_terminal_field_gate_behind_a_pending_gate() {
    let root = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(root.path());
    let file = root.path().join("mission.kdl");
    std::fs::write(
        &file,
        r#"version 2
mission "orchid/replay" state="ready" {
  goal "Explain an impossible predicate behind a pending gate."
  step "prepare" {
    agentless
    exec "prepare" {
      host "orchid"
      workspace "."
      command "touch running; while [ ! -f release ]; do sleep 0.1; done; exit 2"
      restart "never"
    }
    gate "an external result exists" { exists "resource/orchid/result" }
    gate "prepared" { field "exit_code" "exec/${ST_MISSION_RUN}/prepare" is 0 }
  }
}
"#,
    )
    .unwrap();
    daemon.command(&[
        "missions",
        "publish",
        file.to_str().unwrap(),
        "--as",
        PUBLISHER,
        "--no-gate-check",
    ]);
    daemon.command(&[
        "missions",
        "start",
        "orchid/replay",
        "--id",
        "orchid/replay",
        "--workspace",
        root.path().to_str().unwrap(),
        "--as",
        PUBLISHER,
    ]);
    let check = || {
        // Other doctor checks can fail on a host without all runtime tools.
        let output = daemon.run_cli(&["doctor"]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "terminal-exec-gates")
            .unwrap()
            .clone()
    };
    wait_for("the exec to be running", || {
        root.path().join("running").exists()
    });
    assert!(daemon.run()["stuck_gates"].is_null());
    assert_eq!(check()["status"], "pass");
    std::fs::write(root.path().join("release"), "go").unwrap();
    wait_for("the terminal exec diagnostic", || {
        daemon.run()["stuck_gates"]
            .as_array()
            .is_some_and(|gates| !gates.is_empty())
    });
    let run = daemon.run();
    assert_eq!(run["status"], "running");
    let stuck = run["stuck_gates"][0].as_str().unwrap();
    assert!(
        stuck.contains(daemon.step("prepare")["subject"].as_str().unwrap()),
        "{stuck}"
    );
    assert!(stuck.contains("prepared"), "{stuck}");
    assert!(stuck.contains("exec/orchid/replay/prepare"), "{stuck}");
    assert!(stuck.contains("exit code 2"), "{stuck}");
    let health = check();
    assert_eq!(health["status"], "warn", "{health}");
    assert!(
        health["message"].as_str().unwrap().contains(stuck),
        "{health}"
    );
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        .env_clear()
        .env("HOME", root.path())
        .env("ST3_DAEMON_WAIT", "0")
        .args([
            "--endpoint",
            root.path().join("daemon.sock").to_str().unwrap(),
            "missions",
            "show",
            RUN,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("STUCK GATES") && human.contains(stuck),
        "{human}"
    );
    // Diagnostics must not decide the unreachable gate, and must drop an ended run.
    assert!(
        !daemon
            .gate_results()
            .iter()
            .any(|claim| claim["body"]["fields"]["gate"] == "prepared")
    );
    daemon.command(&[
        "missions",
        "cancel",
        RUN,
        "--reason",
        "the test is done",
        "--as",
        PUBLISHER,
    ]);
    wait_for("the run to be cancelled", || {
        daemon.run()["status"] == "cancelled"
    });
    assert!(daemon.run()["stuck_gates"].is_null());
    assert_eq!(check()["status"], "pass");
}

/// The replay mission. `fixed` selects the revised gates; nothing else differs.
fn mission(root: &Path, fixed: bool) -> String {
    let tools = root.join("tools");
    let linker = root.join("linker");
    let path = if fixed {
        format!("{}:{}", tools.display(), linker.display())
    } else {
        tools.display().to_string()
    };
    let suite = |name: &str| format!("export PATH={path}; cargo test -p orchid --test {name}");
    let threshold = if fixed { "8000000000" } else { "1200" };
    let memory = format!(
        "mem=$(awk '/^VmRSS:/ {{print $2 * 1024}}' /proc/$(cat {})/status); echo \"memory $mem\"; test \"$mem\" -lt {threshold}",
        root.join("daemon.pid").display()
    );
    let handoff = if fixed {
        "\"$ST3_BIN\" documents ls doc/orchid/stream-handoff | grep -q 'doc/orchid/stream-handoff'"
    } else {
        "\"$ST3_BIN\" documents ls | grep -q 'doc/orchid/stream-handoff'"
    };
    let limit = if fixed { "200" } else { "500" };
    let missions =
        format!("\"$ST3_BIN\" missions ls --limit {limit} | grep -q 'mission/orchid/replay'");
    let step = |id: &str, title: &str, command: &str| {
        format!(
            r#"  step {id:?} {{
    title {title:?}
    gate {title:?} {{
      exec {command:?}
      host "orchid"
      workspace "."
      time-limit "2m"
    }}
  }}
"#
        )
    };
    format!(
        "version 2\nmission \"orchid/replay\" state=\"ready\" {{\n  goal \"Replay gates that failed finished work.\"\n  completion {{ when \"all-steps-exhausted\" }}\n{}{}{}{}{}}}\n",
        step(
            "diet",
            "the log diet suite passes on main",
            &suite("log_diet")
        ),
        step(
            "isolation",
            "the fault-isolation suite passes on main",
            &suite("fault_isolation")
        ),
        step("memory", "the daemon uses under its memory budget", &memory),
        step("handoff", "the handoff document is published", handoff),
        step("listed", "the mission is listed", &missions),
    )
}

/// An isolated daemon whose host lacks the linker on the gates' PATH, holding a hundred documents
/// that sort before the handoff document.
fn replay_host(root: &Path) -> Daemon {
    // `cargo` needs the `mold` linker on PATH, as the gates' cargo builds did.
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::create_dir_all(root.join("linker")).unwrap();
    // Supply only the gate tools: mold must stay absent until the revision adds it.
    for name in ["awk", "cat", "grep", "sh", "env"] {
        let tool = st_runtime::resolve_executable(name, &std::env::vars().collect()).unwrap();
        std::os::unix::fs::symlink(tool, root.join("tools").join(name)).unwrap();
    }
    executable(
        &root.join("tools/cargo"),
        "#!/bin/sh\ncommand -v mold >/dev/null 2>&1 || { echo 'error: linker `mold` not found' >&2; exit 101; }\necho 'test result: ok. 12 passed'\n",
    );
    executable(&root.join("linker/mold"), "#!/bin/sh\nexit 0\n");
    let daemon = Daemon::start(root);
    // The published document sorts after the first hundred, as the handoff document did.
    let note = root.join("note.md");
    std::fs::write(&note, "A note.\n").unwrap();
    for index in 0..100 {
        daemon.command(&[
            "documents",
            "put",
            note.to_str().unwrap(),
            "--as",
            &format!("doc/orchid/note-{index:03}"),
        ]);
    }
    let handoff = root.join("handoff.md");
    std::fs::write(&handoff, "The handoff.\n").unwrap();
    daemon.command(&[
        "documents",
        "put",
        handoff.to_str().unwrap(),
        "--as",
        "doc/orchid/stream-handoff",
    ]);
    daemon
}

#[test]
fn broken_gates_wait_for_a_revision_that_then_passes_their_steps() {
    let root = tempfile::tempdir().unwrap();
    let daemon = replay_host(root.path());

    // Publish the broken gates as their runs had them; publish would otherwise refuse them.
    let file = root.path().join("replay.kdl");
    std::fs::write(&file, mission(root.path(), false)).unwrap();
    daemon.command(&[
        "missions",
        "publish",
        file.to_str().unwrap(),
        "--as",
        PUBLISHER,
        "--no-gate-check",
    ]);
    daemon.command(&[
        "missions",
        "start",
        "orchid/replay",
        "--id",
        "orchid/replay",
        "--workspace",
        root.path().to_str().unwrap(),
        "--as",
        PUBLISHER,
    ]);

    let broken = [
        ("diet", "exited 101", "mold"),
        ("isolation", "exited 101", "mold"),
        ("handoff", "listed 100 items and more exist", "documents ls"),
        (
            "listed",
            "the mission limit must be 1 through 200",
            "--limit 500",
        ),
    ];
    wait_for("every gate to answer", || {
        broken
            .iter()
            .map(|(path, _, _)| *path)
            .chain(["memory"])
            .all(|path| daemon.gate_result(path).is_some())
    });
    let run = daemon.run();
    assert_eq!(run["status"], "running", "{run}");
    for step in run["steps"].as_array().unwrap() {
        assert!(
            !matches!(
                step["status"].as_str(),
                Some("failed" | "cancelled" | "completed")
            ),
            "{step}"
        );
    }
    // The threshold that could never pass says not yet: its step waits and checks again.
    let memory = daemon.gate_result("memory").unwrap();
    assert_eq!(
        memory["body"]["fields"]["value"]["answer"], "not-yet",
        "{memory}"
    );
    for (path, reason, _) in broken {
        let result = daemon.gate_result(path).unwrap();
        assert_eq!(
            result["body"]["fields"]["value"]["answer"], "broken",
            "{result}"
        );
        let recorded = result["body"]["fields"]["reason"].as_str().unwrap();
        assert!(recorded.contains(reason), "{path}: {recorded}");
    }

    // The publisher has one item per broken gate, naming the gate, its host and its output.
    let items = daemon.attention();
    assert_eq!(items.len(), broken.len(), "{items:#?}");
    for (path, reason, output) in broken {
        let title = daemon.step(path)["title"].as_str().unwrap().to_owned();
        let item = items
            .iter()
            .find(|item| item["title"] == format!("Gate `{title}` is broken"))
            .unwrap_or_else(|| panic!("no item for {path}: {items:#?}"));
        let detail = item["detail"].as_str().unwrap();
        assert!(detail.contains("on host `orchid`"), "{detail}");
        assert!(detail.contains(reason), "{detail}");
        assert!(detail.contains(output), "{detail}");
        assert_eq!(item["mission_run_id"], RUN);
    }

    // Correcting only the gates passes every step without failing the run.
    std::fs::write(&file, mission(root.path(), true)).unwrap();
    daemon.command(&[
        "work",
        "revise",
        RUN,
        file.to_str().unwrap(),
        "--as",
        PUBLISHER,
        "--reason",
        "the gates now find mold, read a complete listing and use the real budget",
    ]);
    wait_for("the revised gates to pass", || {
        daemon.run()["status"] == "completed"
    });
    let run = daemon.run();
    for step in run["steps"].as_array().unwrap() {
        assert_eq!(step["status"], "completed", "{step}");
    }
    assert!(daemon.attention().is_empty(), "{:#?}", daemon.attention());
}

#[test]
fn a_check_answers_for_each_gate_and_publish_refuses_a_broken_one() {
    let root = tempfile::tempdir().unwrap();
    let daemon = replay_host(root.path());
    let workspace = root.path().to_str().unwrap();
    let file = root.path().join("replay.kdl");
    std::fs::write(&file, mission(root.path(), false)).unwrap();
    let file = file.to_str().unwrap();

    let checked = daemon.run_cli(&["missions", "check", file, "--workspace", workspace]);
    assert_eq!(checked.status.code(), Some(1), "{checked:?}");
    let view: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let answer = |owner: &str| {
        view["gates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|gate| gate["owner"] == owner)
            .cloned()
            .unwrap_or_else(|| panic!("no gate for {owner}: {view:#}"))
    };
    for (owner, expected, reason, output) in [
        ("step diet", "broken", "exited 101", "mold"),
        ("step isolation", "broken", "exited 101", "mold"),
        ("step memory", "not-yet", "", "memory"),
        (
            "step handoff",
            "broken",
            "listed 100 items and more exist",
            "",
        ),
        (
            "step listed",
            "broken",
            "the mission limit must be 1 through 200",
            "",
        ),
    ] {
        let gate = answer(owner);
        assert_eq!(gate["answer"], expected, "{gate:#}");
        assert_eq!(gate["host"], "orchid");
        assert!(
            gate["reason"].as_str().unwrap_or("").contains(reason),
            "{gate:#}"
        );
        assert!(
            gate["output"].as_str().unwrap().contains(output),
            "{gate:#}"
        );
    }

    // Publish runs the same check and refuses the broken gates, naming each one.
    let refused = daemon.run_cli(&[
        "missions",
        "publish",
        file,
        "--as",
        PUBLISHER,
        "--workspace",
        workspace,
    ]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("4 exec gates cannot answer as written"),
        "{stderr}"
    );
    assert!(
        stderr.contains("the log diet suite passes on main"),
        "{stderr}"
    );
    assert!(
        stderr.contains("error: linker `mold` not found"),
        "{stderr}"
    );
    assert!(stderr.contains("--no-gate-check"), "{stderr}");
    let listed = daemon.command(&["missions", "ls"]);
    assert!(!listed.to_string().contains("orchid/replay"), "{listed}");

    // The corrected gates answer; not yet does not stop a publication.
    let fixed = root.path().join("fixed.kdl");
    std::fs::write(&fixed, mission(root.path(), true)).unwrap();
    let fixed = fixed.to_str().unwrap();
    let checked = daemon.run_cli(&["missions", "check", fixed, "--workspace", workspace]);
    assert!(checked.status.success(), "{checked:?}");
    let view: Value = serde_json::from_slice(&checked.stdout).unwrap();
    for gate in view["gates"].as_array().unwrap() {
        let expected = if gate["owner"] == "step listed" {
            "not-yet"
        } else {
            "pass"
        };
        assert_eq!(gate["answer"], expected, "{gate:#}");
    }
    daemon.command(&[
        "missions",
        "publish",
        fixed,
        "--as",
        PUBLISHER,
        "--workspace",
        workspace,
    ]);
    let listed = daemon.command(&["missions", "ls"]);
    assert!(listed.to_string().contains("orchid/replay"), "{listed}");

    // A gate for another host, or in a workspace a run creates later, is unchecked, not broken.
    let elsewhere = root.path().join("elsewhere.kdl");
    std::fs::write(
        &elsewhere,
        r#"version 2
mission "orchid/elsewhere" state="ready" {
  goal "Check gates this host cannot run yet."
  step "remote" {
    gate "the build host has the artifact" { exec "true"; host "cobalt"; workspace "."; }
  }
  step "later" {
    gate "the checkout is clean" { exec "true"; host "orchid"; workspace "checkout"; }
  }
}
"#,
    )
    .unwrap();
    let checked = daemon.run_cli(&[
        "missions",
        "check",
        elsewhere.to_str().unwrap(),
        "--workspace",
        workspace,
    ]);
    assert!(checked.status.success(), "{checked:?}");
    let view: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let gates = view["gates"].as_array().unwrap();
    assert_eq!(gates.len(), 2, "{view:#}");
    assert!(
        gates.iter().all(|gate| gate["answer"] == "unchecked"),
        "{view:#}"
    );
    assert!(
        gates[0]["reason"]
            .as_str()
            .unwrap()
            .contains("host `cobalt`")
    );
    assert!(
        gates[1]["reason"]
            .as_str()
            .unwrap()
            .contains("does not exist here yet")
    );

    // A gate that reads an input runs with the value given, and is unchecked without one. A
    // built-in gate runs its `st gate` command; this host has no GitHub token, so it is broken.
    let inputs = root.path().join("inputs.kdl");
    std::fs::write(
        &inputs,
        r#"version 2
mission "orchid/inputs" state="ready" {
  goal "Check gates that read inputs."
  input "release" kind="text"
  input "pull_request" kind="text"
  step "tag" {
    gate "the release is 1.4.0" { exec "test '${input.release}' = 1.4.0"; host "orchid"; workspace "."; }
  }
  step "land" {
    gate "the fix merged" { merged "${input.pull_request}" }
  }
}
"#,
    )
    .unwrap();
    let inputs = inputs.to_str().unwrap();
    let check = |extra: &[&str]| {
        let mut args = vec!["missions", "check", inputs, "--workspace", workspace];
        args.extend_from_slice(extra);
        let output = daemon.run_cli(&args);
        let view: Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code(), view)
    };
    let (code, view) = check(&[]);
    assert_eq!(code, Some(0), "{view:#}");
    for gate in view["gates"].as_array().unwrap() {
        assert_eq!(gate["answer"], "unchecked", "{gate:#}");
        assert!(
            gate["reason"].as_str().unwrap().contains("--input "),
            "{gate:#}"
        );
    }
    let (code, view) = check(&[
        "--input",
        "release=1.4.0",
        "--input",
        "pull_request=acme/app#7",
    ]);
    assert_eq!(code, Some(1), "{view:#}");
    assert_eq!(view["gates"][0]["answer"], "pass", "{view:#}");
    let merged = &view["gates"][1];
    assert_eq!(merged["answer"], "broken", "{merged:#}");
    assert_eq!(merged["exit_code"], 3, "{merged:#}");
    assert!(
        merged["output"]
            .as_str()
            .unwrap()
            .contains("broken: GitHub observers have no token"),
        "{merged:#}"
    );
    let (_, view) = check(&["--input", "release=1.5.0"]);
    assert_eq!(view["gates"][0]["answer"], "not-yet", "{view:#}");
}
