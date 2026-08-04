//! P2: `st2 up <spec>` boots an st2-spec's top-level team end-to-end through the real binary — the
//! spec is mapped to in-memory agents and spawned via the same reconcile/execute path as a catalog.
//! Benign `sleep` agents (no harness); isolated PTY_ROOT so it never touches the live fleet.
//!
//! Needs `pty` on PATH (the runner lists pty sessions) — HARD failure if absent unless
//! `ST2_ALLOW_PTY_SKIP` is set (a gate must not silently skip).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn pty_available() -> bool {
    Command::new("pty").arg("--help").output().map(|o| o.status.success()).unwrap_or(false)
}

fn pty_ids(pty_root: &Path) -> Vec<(String, String)> {
    let out = Command::new("pty").args(["list", "--json"]).env("PTY_ROOT", pty_root).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Array(vec![]));
    v.as_array()
        .map(|rows| {
            rows.iter()
                .map(|s| {
                    (
                        s.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        s.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn st2_up_boots_a_specs_team() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH — can't boot a team. Set ST2_ALLOW_PTY_SKIP=1 to skip."
        );
        eprintln!("SKIP st2_up_boots_a_specs_team: `pty` not on PATH");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap(); // short /tmp/.tmpXXXX path → PTY_ROOT fits the socket limit
    let spec_dir = tmp.path().join("cell");
    std::fs::create_dir_all(&spec_dir).unwrap();
    // A benign 2-agent team under a team prefix → session ids `t.a`, `t.b`.
    std::fs::write(
        spec_dir.join("cell.kdl"),
        r#"
env { ST_ROOT "$CATALOG/bus"; PTY_ROOT "$CATALOG/pty" }
team "t" {
  agent "a" { command "sleep 100000" }
  agent "b" { command "sleep 100000" }
}
"#,
    )
    .unwrap();

    let pty_root = tmp.path().join("pty");
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    let out = Command::new(bin)
        .args(["up"])
        .arg(&spec_dir)
        .arg("--once") // spec-up now SUPERVISES by default; --once does a single boot pass for the test
        .env("PATH", path)
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .env("PTY_ROOT", &pty_root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "st2 up <spec> failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("booted team from spec"), "no boot line:\n{stdout}");

    // The team is running under its team-prefixed ids, in the isolated PTY_ROOT.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let ids = pty_ids(&pty_root);
    let running: Vec<&str> = ids.iter().filter(|(_, s)| s == "running").map(|(n, _)| n.as_str()).collect();
    assert!(running.contains(&"t.a"), "t.a not running; sessions={ids:?}");
    assert!(running.contains(&"t.b"), "t.b not running; sessions={ids:?}");

    // Teardown — the team persists after st2 exits (nomad-decoupled), so clean it up ourselves.
    for id in ["t.a", "t.b"] {
        let _ = Command::new("pty").args(["kill", id]).env("PTY_ROOT", &pty_root).status();
        let _ = Command::new("pty").args(["rm", id]).env("PTY_ROOT", &pty_root).status();
    }
    // Stop any lingering per-task scopes (this spec's ids only).
    if let Ok(o) = Command::new("systemctl").args(["--user", "list-units", "--no-legend", "st2-t.*"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(unit) = line.split_whitespace().next() {
                let _ = Command::new("systemctl").args(["--user", "stop", unit]).status();
            }
        }
    }
}

/// File-level taxonomy (the maintainer): a file with NO agents is an eval-only ("job") file — `st2 up` must
/// refuse it (nothing to supervise) and point at `st2 eval`. No `pty` needed (it errors before any boot).
#[test]
fn st2_up_refuses_an_eval_only_file() {
    let bin = env!("CARGO_BIN_EXE_st2");
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    std::fs::create_dir_all(&cell).unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        "eval {\n  max-timeout \"5s\"\n  run \"x\" { command \"true\" }\n  judges { judge \"ok\" { exec \"true\" } }\n}\n",
    )
    .unwrap();
    let out = Command::new(bin)
        .args(["up", "--once"])
        .arg(&cell)
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "st2 up on an eval-only file must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("eval-only file") && stderr.contains("st2 eval"),
        "the refusal should point at `st2 eval`:\n{stderr}"
    );
}

/// `st2 down <spec>` is the symmetric teardown verb: after `st2 up <spec> --once` boots the team,
/// `st2 down <spec>` (same single file) stops the declared team's sessions. Sessions are nomad-decoupled
/// (they outlive the `up` process), so `down` is how a spec-based fleet is actually stopped.
#[test]
fn st2_down_tears_down_a_spec_fleet() {
    if !pty_available() {
        assert!(std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(), "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1");
        eprintln!("SKIP st2_down_tears_down_a_spec_fleet: `pty` not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("cell");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(
        spec_dir.join("cell.kdl"),
        r#"
env { ST_ROOT "$CATALOG/bus"; PTY_ROOT "$CATALOG/pty" }
team "down" {
  agent "a" { command "sleep 100000" }
  agent "b" { command "sleep 100000" }
}
"#,
    )
    .unwrap();
    let pty_root = tmp.path().join("pty");
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .arg(&spec_dir)
            .env("PATH", &path)
            .env("XDG_STATE_HOME", tmp.path().join("xdg"))
            .env("PTY_ROOT", &pty_root)
            .output()
            .unwrap()
    };

    // Boot the team, confirm it's running.
    let up = run(&["up", "--once"]);
    assert!(up.status.success(), "up failed: {}", String::from_utf8_lossy(&up.stderr));
    std::thread::sleep(Duration::from_millis(500));
    let running = |ids: &[(String, String)]| -> Vec<String> {
        ids.iter().filter(|(_, s)| s == "running").map(|(n, _)| n.clone()).collect()
    };
    let before = running(&pty_ids(&pty_root));
    assert!(before.contains(&"down.a".to_string()), "t.a not running before down; {before:?}");
    assert!(before.contains(&"down.b".to_string()), "t.b not running before down; {before:?}");

    // `st2 down <spec>` tears down the declared team.
    let down = run(&["down"]);
    let dstdout = String::from_utf8_lossy(&down.stdout);
    assert!(down.status.success(), "down failed: {}", String::from_utf8_lossy(&down.stderr));
    assert!(dstdout.contains("teardown of spec"), "no spec-teardown line:\n{dstdout}");
    assert!(dstdout.contains("down.a") && dstdout.contains("down.b"), "down did not report tearing down down.a/down.b:\n{dstdout}");

    // The sessions are no longer running.
    std::thread::sleep(Duration::from_millis(500));
    let after = running(&pty_ids(&pty_root));
    assert!(!after.contains(&"down.a".to_string()), "t.a still running after down; {after:?}");
    assert!(!after.contains(&"down.b".to_string()), "t.b still running after down; {after:?}");

    // Clean up the (now-stopped) sessions + any per-task scopes.
    for id in ["down.a", "down.b"] {
        let _ = Command::new("pty").args(["rm", id]).env("PTY_ROOT", &pty_root).status();
    }
    if let Ok(o) = Command::new("systemctl").args(["--user", "list-units", "--no-legend", "st2-down.*"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(unit) = line.split_whitespace().next() {
                let _ = Command::new("systemctl").args(["--user", "stop", unit]).status();
            }
        }
    }
}

/// A single `st2 up <spec> --once` pass RESPAWNS a hard-killed (kill -9) agent atomically — the corpse
/// is reaped and the agent comes back within that ONE pass, not "id already in use". Regression for the
/// reap-then-respawn race: after a hard-kill the just-reaped session id lingered microseconds in the pty
/// daemon, so the immediate respawn saw it in-use; a single `--once` had no later cycle to self-heal.
#[test]
fn st2_up_once_atomically_respawns_a_hard_killed_agent() {
    if !pty_available() {
        assert!(std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(), "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1");
        eprintln!("SKIP st2_up_once_atomically_respawns_a_hard_killed_agent: `pty` not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("cell");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(
        spec_dir.join("cell.kdl"),
        "env { PTY_ROOT \"$CATALOG/pty\" }\nagent \"raceonce\" { command \"sleep 100000\" }\n",
    )
    .unwrap();
    let pty_root = tmp.path().join("pty");
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    let once = || {
        Command::new(bin)
            .args(["up", "--once"])
            .arg(&spec_dir)
            .env("PATH", &path)
            .env("XDG_STATE_HOME", tmp.path().join("xdg"))
            .env("PTY_ROOT", &pty_root)
            .output()
            .unwrap()
    };
    let pid_of = |id: &str| -> Option<i64> {
        let out = Command::new("pty").args(["list", "--json"]).env("PTY_ROOT", &pty_root).output().ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v.as_array()?
            .iter()
            .find(|s| {
                s.get("name").and_then(|x| x.as_str()) == Some(id)
                    && s.get("status").and_then(|x| x.as_str()) == Some("running")
            })
            .and_then(|s| s.get("pid").and_then(|p| p.as_i64()))
    };

    // Boot, grab the pid, hard-kill the PROCESS (not `pty kill`) so the corpse lingers in the registry.
    assert!(once().status.success(), "initial boot failed");
    std::thread::sleep(Duration::from_millis(700));
    let pid1 = pid_of("raceonce").expect("agent 'raceonce' should be running after boot");
    let _ = Command::new("kill").args(["-9", &pid1.to_string()]).status();

    // A single `--once` pass right after the hard-kill must atomically reap + respawn — no "in use".
    let out = once();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "up --once failed after hard-kill:\n{stderr}");
    assert!(!stderr.contains("already in use"), "respawn hit the reap race:\n{stderr}");
    std::thread::sleep(Duration::from_millis(500));
    let pid2 = pid_of("raceonce").expect("agent 'raceonce' should be respawned by the same --once pass");
    assert_ne!(pid1, pid2, "respawn must be a NEW process");

    // Clean up.
    let _ = Command::new("pty").args(["kill", "raceonce"]).env("PTY_ROOT", &pty_root).status();
    let _ = Command::new("pty").args(["rm", "raceonce"]).env("PTY_ROOT", &pty_root).status();
    if let Ok(o) = Command::new("systemctl").args(["--user", "list-units", "--no-legend", "st2-raceonce*"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(unit) = line.split_whitespace().next() {
                let _ = Command::new("systemctl").args(["--user", "stop", unit]).status();
            }
        }
    }
}

/// `st2 up <spec>` (no --once) supervises the team: a killed agent is respawned. Boots
/// a benign agent under a fast interval, kills it, and confirms the supervise loop brings it back with a
/// NEW pid — then stops st2 (sessions persist, nomad-decoupled) and cleans up.
#[test]
fn st2_up_spec_supervises_and_respawns_a_killed_agent() {
    if !pty_available() {
        assert!(std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(), "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1");
        eprintln!("SKIP st2_up_spec_supervises_and_respawns_a_killed_agent: `pty` not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("cell");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(
        spec_dir.join("cell.kdl"),
        "env { ST_ROOT \"$CATALOG/custom-bus\"; PTY_ROOT \"$CATALOG/pty\" }\nagent \"a\" { command \"sleep 100000\" }\n",
    )
    .unwrap();
    let pty_root = tmp.path().join("pty");
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    // Supervise in the background with a fast reconcile interval.
    let mut child = std::process::Command::new(bin)
        .args(["up"]).arg(&spec_dir).args(["--interval", "1"])
        .env("PATH", &path)
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .env("PTY_ROOT", &pty_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let pid_of = |id: &str| -> Option<i64> {
        let out = std::process::Command::new("pty").args(["list", "--json"]).env("PTY_ROOT", &pty_root).output().ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v.as_array()?.iter().find(|s| s.get("name").and_then(|x| x.as_str()) == Some(id) && s.get("status").and_then(|x| x.as_str()) == Some("running"))
            .and_then(|s| s.get("pid").and_then(|p| p.as_i64()))
    };
    let wait_for = |id: &str, secs: u64| -> Option<i64> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(pid) = pid_of(id) { return Some(pid); }
            if Instant::now() > deadline { return None; }
            std::thread::sleep(Duration::from_millis(300));
        }
    };

    let pid1 = wait_for("a", 15).expect("agent 'a' should boot under supervision");
    // Kill it out from under the supervisor.
    let _ = std::process::Command::new("pty").args(["kill", "a"]).env("PTY_ROOT", &pty_root).status();
    // The supervise loop must bring it back (new pid) within a few reconcile intervals.
    let pid2 = wait_for("a", 15).expect("supervisor should RESPAWN the killed agent");
    assert_ne!(pid1, pid2, "respawn must be a NEW process, not the killed one");

    // Stop the supervisor; the session persists (nomad-decoupled). Clean up.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::process::Command::new("pty").args(["kill", "a"]).env("PTY_ROOT", &pty_root).status();
    let _ = std::process::Command::new("pty").args(["rm", "a"]).env("PTY_ROOT", &pty_root).status();
    for u in ["st2-a"] {
        if let Ok(o) = std::process::Command::new("systemctl").args(["--user", "list-units", "--no-legend", &format!("{u}*")]).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some(unit) = line.split_whitespace().next() {
                    let _ = std::process::Command::new("systemctl").args(["--user", "stop", unit]).status();
                }
            }
        }
    }
}
