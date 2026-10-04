#![cfg(target_os = "linux")]
//! Cancellation owns both declared execs and directly launched gates, including after restart.
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

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
            let path = bin.join(name);
            std::fs::write(&path, source).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut daemon = Self {
            root: root.to_owned(),
            child: None,
        };
        daemon.launch();
        daemon
    }

    fn launch(&mut self) {
        let log = File::create(self.root.join("daemon.log")).unwrap();
        self.child = Some(
            st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
                .env_clear()
                .env("HOME", &self.root)
                    .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.root.join("bin").display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .env("XDG_CONFIG_HOME", self.root.join("config"))
                .env("XDG_STATE_HOME", self.root.join("state"))
                .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
                .current_dir(&self.root)
                .args(["up", "--node", "orchid", "--socket"])
                .arg(self.root.join("daemon.sock"))
                .arg("--client-gateway-socket")
                .arg(self.root.join("client.sock"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        wait_for("the isolated daemon", || {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "{}",
                self.log()
            );
            std::os::unix::net::UnixStream::connect(self.root.join("daemon.sock")).is_ok()
        });
    }

    fn restart(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        self.launch();
    }

    fn command(&self, args: &[&str]) -> Value {
        let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
            .env_clear()
            .env("HOME", &self.root)
            .env("ST3_DAEMON_WAIT", "0")
            .args(["--endpoint"])
            .arg(self.root.join("daemon.sock"))
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.log()
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn run(&self) -> Value {
        self.command(&["missions", "show", "mission-run/orchid/cancel"])
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("isolated daemon log: {}", self.log());
            for name in [
                "gate.pid",
                "exec.pid",
                "root-exec.pid",
                "completed-exec.pid",
                "final.pid",
                "final-exec.pid",
                "gate-child.pid",
            ] {
                if let Ok(pid) = std::fs::read_to_string(self.root.join(name)) {
                    eprintln!(
                        "{name}: {} {:?}",
                        pid.trim(),
                        std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
                    );
                }
            }
        }
        // Even a failed assertion must not leave the test's detached work alive.
        for name in [
            "gate.pid",
            "exec.pid",
            "root-exec.pid",
            "completed-exec.pid",
            "final.pid",
            "final-exec.pid",
            "gate-child.pid",
        ] {
            if let Ok(pid) = std::fs::read_to_string(self.root.join(name))
                && let Ok(pid) = pid.trim().parse::<i32>()
                && live(pid)
            {
                let group = unsafe { libc::getpgid(pid) };
                if group > 0 && group != unsafe { libc::getpgrp() } {
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn live(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, fields)| !fields.starts_with('Z'))
    })
}

fn cancellation_stops_owned_work(restart: bool) {
    let root = tempfile::tempdir().unwrap();
    let lane = File::create(root.path().join("lane.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut daemon = Daemon::start(root.path());
    let source = r#"version 2
mission "orchid/cancellation" state="ready" {
  goal "Stop owned work when cancelled."
  completion { when "all-steps-exhausted" }
  exec "root-worker" {
    command "trap '' TERM; echo $$ > root-exec.pid; while :; do sleep 60; done"
    restart "never"
  }
  step "setup-service" {
    agentless
    exec "service-worker" {
      command "trap '' TERM; echo $$ > completed-exec.pid; while :; do sleep 60; done"
      restart "never"
    }
  }
  step "wait" {
    agentless
    exec "worker" {
      command "trap '' TERM; echo $$ > exec.pid; while :; do sleep 60; done"
      restart "never"
    }
    gate "the lane opens" {
      exec "{ trap '' TERM; echo $$ > gate.pid; sleep 600 & echo $! > gate-child.pid; flock -x 9; echo started > forbidden; wait; } 9>lane.lock"
      host "orchid"
      workspace "."
      time-limit "2m"
    }
  }
  finally {
    step "report" {
      agentless
      exec "final-worker" {
        command "echo $$ > final-exec.pid; while [ ! -e finish ]; do sleep 0.05; done"
        restart "never"
      }
      gate "the final status is posted" {
        exec "echo $$ > final.pid; while [ ! -e finish ]; do sleep 0.05; done; exit 7"
        host "orchid"
        workspace "."
        time-limit "2m"
      }
    }
  }
}
"#;
    let file = root.path().join("mission.kdl");
    std::fs::write(&file, source).unwrap();
    daemon.command(&[
        "missions",
        "publish",
        file.to_str().unwrap(),
        "--as",
        "person/operator",
        // Its gates hold a lock and wait for the test; running them at publish would block.
        "--no-gate-check",
    ]);
    daemon.command(&[
        "missions",
        "start",
        "orchid/cancellation",
        "--id",
        "orchid/cancel",
        "--workspace",
        root.path().to_str().unwrap(),
        "--as",
        "person/operator",
    ]);
    wait_for("all owned processes", || {
        [
            "exec.pid",
            "root-exec.pid",
            "completed-exec.pid",
            "gate.pid",
            "gate-child.pid",
        ]
        .iter()
        .all(|name| {
            std::fs::read_to_string(root.path().join(name))
                .is_ok_and(|pid| pid.trim().parse::<i32>().is_ok())
        })
    });
    wait_for(
        "the setup step to complete with its service still live",
        || {
            daemon.run()["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["step"] == "setup-service" && step["status"] == "completed")
        },
    );
    let pids = [
        "exec.pid",
        "root-exec.pid",
        "completed-exec.pid",
        "gate.pid",
        "gate-child.pid",
    ]
    .map(|name| {
        std::fs::read_to_string(root.path().join(name))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap()
    });
    assert!(pids.iter().all(|pid| live(*pid)));
    assert!(!root.path().join("forbidden").exists());
    if restart {
        daemon.restart();
        assert!(
            pids.iter().all(|pid| live(*pid)),
            "the work must survive until cancellation"
        );
    }
    daemon.command(&[
        "missions",
        "cancel",
        "mission-run/orchid/cancel",
        "--reason",
        "the work is no longer needed",
        "--as",
        "person/operator",
    ]);
    wait_for("the final gate", || root.path().join("final.pid").exists());
    wait_for("the final exec", || {
        std::fs::read_to_string(root.path().join("final-exec.pid"))
            .is_ok_and(|pid| pid.trim().parse::<i32>().is_ok())
    });
    let final_exec = std::fs::read_to_string(root.path().join("final-exec.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    wait_for(
        "cancelled execs and gate descendants to die during finally",
        || pids.iter().all(|pid| !live(*pid)),
    );
    assert_eq!(daemon.run()["phase"], "final-cancelled");
    assert!(
        live(final_exec),
        "finally work must remain eligible during cancellation"
    );
    assert_eq!(unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_UN) }, 0);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !root.path().join("forbidden").exists(),
        "a cancelled gate acquired the lane later"
    );
    std::fs::write(root.path().join("finish"), "").unwrap();
    wait_for(
        "terminal cancellation despite the failed final gate",
        || daemon.run()["phase"] == "terminal",
    );
    let run = daemon.run();
    assert_eq!(run["status"], "cancelled", "{run}");
    let report = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["step"] == "report")
        .unwrap();
    assert_eq!(report["status"], "failed", "{run}");
    assert!(
        report["blocked_reason"]
            .as_str()
            .unwrap()
            .contains("the final status is posted")
    );
}

#[test]
fn cancellation_stops_execs_and_waiting_gates_before_finally_finishes() {
    cancellation_stops_owned_work(false);
}

#[test]
fn cancellation_adopts_and_stops_gate_processes_after_daemon_restart() {
    cancellation_stops_owned_work(true);
}
