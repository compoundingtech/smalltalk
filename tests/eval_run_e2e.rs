//! P3+P4 end-to-end (the benign internal-proof milestone): `st2 eval <folder>` runs a whole eval
//! folder → VERDICT through the real binary. Benign shell "agents" choreograph the real flow on the
//! FLAT eval bus via `st2 message` (exercising the flat-bus bridge): kick → worker→sup report →
//! sup→requester confirmation (post-dating the report), then the judge engine grades the deliverable.
//! No real harness; st2 eval mints its own hermetic catalog + PTY_ROOT (never touches the live fleet).
//!
//! Needs `pty` on PATH — HARD failure if absent unless ST2_ALLOW_PTY_SKIP is set.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn pty_available() -> bool {
    Command::new("pty")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct RemoveDirOnDrop(std::path::PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[allow(dead_code)]
fn preserved_eval_catalog(output: &std::process::Output) -> std::path::PathBuf {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    text.lines()
        .find_map(|line| line.strip_prefix("catalog preserved (--keep): "))
        .map(std::path::PathBuf::from)
        .expect("--keep eval must report preserved catalog")
}

#[test]
fn st2_eval_runs_a_benign_folder_to_a_pass_verdict() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH — can't run an eval. Set ST2_ALLOW_PTY_SKIP=1 to skip."
        );
        eprintln!("SKIP st2_eval_runs_a_benign_folder_to_a_pass_verdict: `pty` not on PATH");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    std::fs::create_dir_all(fixture.join("scripts")).unwrap();
    std::fs::create_dir_all(fixture.join("sup")).unwrap();
    std::fs::create_dir_all(fixture.join("worker")).unwrap();

    // Native-default topology: no authored ST_ROOT. The kickoff, polling agents, built-in bare dings,
    // requester confirmation, and judges must all resolve the same canonical actor namespace.
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
env { PTY_ROOT "$CATALOG/pty" }
team "t" {
  agent "sup" {
    workspace "./sup"
    command "sh $CATALOG/scripts/sup.sh"
    ding
  }
  agent "worker" {
    workspace "./worker"
    supervisor "t.sup"
    command "sh $CATALOG/scripts/worker.sh"
    ding
  }
}
eval {
  copy "./fixture"
  message { from "requester"; to "t.sup"; content "relicense the widget" }
  max-timeout "30s"
  judges {
    judge "deliverable exists" { exec "test -f $CATALOG/worker/DONE" }
    judge "deliverable content" { file "worker/DONE" has "resolved" }
    judge "typed values" {
      json "worker/values.json" field "text" is "hello"
      json "worker/values.json" field "ok" is #true
      json "worker/values.json" field "count" is 7
    }
    judge "one native bus root" {
      exec "test \"$ST_ROOT\" = \"$CATALOG\" && test \"$(cat $CATALOG/sup/ST_ROOT)\" = \"$CATALOG\" && test \"$(cat $CATALOG/worker/ST_ROOT)\" = \"$CATALOG\""
    }
    judge "one canonical actor namespace" {
      exec "test \"$(cat $CATALOG/sup/ST_AGENT)\" = evalhost.t.sup && test \"$(cat $CATALOG/worker/ST_AGENT)\" = evalhost.t.worker && test \"$(cat $CATALOG/worker/ST_SUPERVISOR)\" = evalhost.t.sup && test -d $CATALOG/evalhost.t.sup/inbox && test ! -e $CATALOG/t.sup/inbox"
    }
  }
}
"#,
    )
    .unwrap();

    // worker: on boot, do the work (write the deliverable) + report to the sup.
    std::fs::write(
        fixture.join("scripts/worker.sh"),
        r#"#!/bin/sh
sleep 0.4
mkdir -p "$CATALOG/worker"
printf '%s\n' "$ST_ROOT" > "$CATALOG/worker/ST_ROOT"
printf '%s\n' "$ST_AGENT" > "$CATALOG/worker/ST_AGENT"
printf '%s\n' "$ST_SUPERVISOR" > "$CATALOG/worker/ST_SUPERVISOR"
echo "resolved by t.worker" > "$CATALOG/worker/DONE"
printf '%s\n' '{"text":"hello","ok":true,"count":7}' > "$CATALOG/worker/values.json"
st2 message send "$ST_SUPERVISOR" --root "$ST_ROOT" -m "worker done" >/dev/null 2>&1
sleep 60
"#,
    )
    .unwrap();
    // sup: wait for the kick, then the worker's report, then confirm to the requester (post-dating it).
    std::fs::write(
        fixture.join("scripts/sup.sh"),
        r#"#!/bin/sh
printf '%s\n' "$ST_ROOT" > "$CATALOG/sup/ST_ROOT"
printf '%s\n' "$ST_AGENT" > "$CATALOG/sup/ST_AGENT"
for _ in $(seq 1 150); do
  n=$(st2 message ls --root "$ST_ROOT" --from requester --count 2>/dev/null || echo 0)
  [ "$n" -gt 0 ] && break
  sleep 0.2
done
for _ in $(seq 1 150); do
  n=$(st2 message ls --root "$ST_ROOT" --from evalhost.t.worker --count 2>/dev/null || echo 0)
  [ "$n" -gt 0 ] && break
  sleep 0.2
done
st2 message send requester --root "$ST_ROOT" -m "done + verified: worker relicensed, commit clean" >/dev/null 2>&1
sleep 60
"#,
    )
    .unwrap();

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin)
        .args(["eval"])
        .arg(&cell)
        .env("PATH", &path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("team signalled done"),
        "the flow didn't reach done (kick → worker report → sup confirm):\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert!(
        stdout.contains("VERDICT: PASS"),
        "expected PASS:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert!(out.status.success(), "exit non-zero:\n{stdout}\n{stderr}");
    // Both judges (bash + declarative) passed.
    assert!(
        stdout.contains("SCORE: 5 PASS / 0 FAIL"),
        "expected 5/0:\n{stdout}"
    );

    let json_out = Command::new(bin)
        .args(["eval", "--json"])
        .arg(&cell)
        .env("PATH", &path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg-json"))
        .output()
        .unwrap();
    assert!(
        json_out.status.success(),
        "json eval should preserve exit 0: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&json_out.stdout).expect("--json emits EvalReport");
    assert_eq!(json["done"], true);
    assert!(json["judges"].as_array().is_some());

    let fail_cell = cell.join("fail.kdl");
    let fail_spec = std::fs::read_to_string(cell.join("cell.kdl"))
        .unwrap()
        .replace(
            "test -f $CATALOG/worker/DONE",
            "test -f $CATALOG/worker/NOPE",
        )
        .replace("field \"count\" is 7", "field \"count\" is 8");
    std::fs::write(&fail_cell, fail_spec).unwrap();
    let fail_out = Command::new(bin)
        .args(["eval", "--json"])
        .arg(&fail_cell)
        .env("PATH", &path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg-fail"))
        .output()
        .unwrap();
    assert!(!fail_out.status.success());
    let fail_json: serde_json::Value =
        serde_json::from_slice(&fail_out.stdout).expect("failed eval report");
    assert_eq!(fail_json["judges"][0]["passed"], false);
    assert!(
        fail_json["judges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["detail"].as_str().unwrap_or("").contains("count"))
    );
    assert!(!String::from_utf8_lossy(&fail_out.stdout).contains("=="));

    let invalid = Command::new(bin)
        .args(["eval", "--json"])
        .arg(tmp.path().join("missing.kdl"))
        .output()
        .unwrap();
    assert!(
        !invalid.status.success(),
        "invalid eval input must retain nonzero exit"
    );
}

#[test]
fn canonical_agents_run_from_the_hermetic_catalog_with_one_root_and_native_bus() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!(
            "SKIP canonical_agents_run_from_the_hermetic_catalog_with_one_root_and_native_bus"
        );
        return;
    }

    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    for path in [
        "agents/evalhost/sup",
        "agents/evalhost/worker",
        "scripts",
        "sup",
        "templates",
        "worker",
    ] {
        std::fs::create_dir_all(fixture.join(path)).unwrap();
    }
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
eval {
  copy "./fixture"
  canonical-agents
  supervise
  message { from "requester"; to "evalhost.sup"; content "do the bounded work" }
  max-timeout "30s"
  judges {
    judge "canonical team completed" { exec "test -f $CATALOG/worker/DONE" }
    judge "one eval-owned root" {
      exec "test -f $CATALOG/sup/roots-ok && test -f $CATALOG/worker/roots-ok"
    }
    judge "render materialized before launch" {
      exec "test -f $CATALOG/sup/render-seen-at-process-start"
    }
    judge "custom main id survived supervision" {
      exec "test -f $CATALOG/worker/restarted-once"
    }
    judge "kickoff used canonical inbox" {
      exec "test -d $CATALOG/agents/evalhost/sup/resources/inbox && test ! -e $CATALOG/evalhost.sup/inbox"
    }
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("agents/evalhost/sup/agent.kdl"),
        r#"agent "sup" {
  identity "sup"
  host "evalhost"
  workspace "$CATALOG/sup"
  env { ST_AGENT "evalhost.sup" }
  pty "agent" {
    id "canonical-sup-main"
    argv "sh" "$CATALOG/scripts/sup.sh"
  }
  render {
    copy "templates/proof.txt" "materialized.txt"
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("agents/evalhost/worker/agent.kdl"),
        r#"agent "worker" {
  identity "worker"
  name "requester"
  host "evalhost"
  workspace "$CATALOG/worker"
  supervisor "sup"
  env { ST_AGENT "evalhost.worker" }
  pty "agent" {
    id "canonical-worker-main"
    argv "sh" "$CATALOG/scripts/worker.sh"
  }
}
"#,
    )
    .unwrap();
    std::fs::write(fixture.join("templates/proof.txt"), "rendered\n").unwrap();
    std::fs::write(
        fixture.join("scripts/worker.sh"),
        r#"#!/bin/sh
echo "canonical worker main"
test "$CATALOG" = "$ST_ROOT" &&
  test "$PTY_ROOT" = "$CATALOG/pty" &&
  test "$ST_AGENT" = "evalhost.worker" &&
  : > "$CATALOG/worker/roots-ok"
if [ ! -e "$CATALOG/worker/restarted-once" ]; then
  : > "$CATALOG/worker/restarted-once"
  sleep 2
  exit 17
fi
: > "$CATALOG/worker/DONE"
st2 message send evalhost.sup --root "$ST_ROOT" --as evalhost.worker -m "worker done" >/dev/null 2>&1
exec sleep 60
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/sup.sh"),
        r#"#!/bin/sh
test "$(cat "$CATALOG/sup/materialized.txt")" = rendered || exit 41
: > "$CATALOG/sup/render-seen-at-process-start"
echo "canonical supervisor main"
test "$CATALOG" = "$ST_ROOT" &&
  test "$PTY_ROOT" = "$CATALOG/pty" &&
  test "$ST_AGENT" = "evalhost.sup" &&
  : > "$CATALOG/sup/roots-ok"
for _ in $(seq 1 150); do
  kick=$(st2 message ls evalhost.sup --root "$ST_ROOT" --from requester --count 2>/dev/null || echo 0)
  report=$(st2 message ls evalhost.sup --root "$ST_ROOT" --from evalhost.worker --count 2>/dev/null || echo 0)
  [ "$kick" -gt 0 ] && [ "$report" -gt 0 ] && break
  sleep 0.2
done
st2 message send requester --root "$ST_ROOT" --as evalhost.sup -m "done" >/dev/null 2>&1
exec sleep 60
"#,
    )
    .unwrap();

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let poison = tmp.path().join("ambient-poison");
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env("PATH", path)
        .env("CATALOG", poison.join("catalog"))
        .env("ST_ROOT", poison.join("bus"))
        .env("PTY_ROOT", poison.join("pty"))
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success()
            && stdout.contains("team signalled done")
            && stdout.contains("SCORE: 6 PASS / 0 FAIL"),
        "canonical eval did not close:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert!(
        !poison.exists(),
        "ambient CATALOG/ST_ROOT/PTY_ROOT leaked outside the eval-owned catalog"
    );
    assert_eq!(
        std::fs::read_to_string(catalog.join("sup/materialized.txt")).unwrap(),
        "rendered\n"
    );
    for id in ["canonical-sup-main", "canonical-worker-main"] {
        let log = catalog.join("logs").join(format!("{id}.log"));
        assert!(
            log.exists(),
            "custom main id did not flow into log capture: {}",
            log.display()
        );
    }
    let sessions = Command::new("pty")
        .args(["ls", "--json"])
        .env("PTY_ROOT", catalog.join("pty"))
        .output()
        .unwrap();
    let sessions = String::from_utf8_lossy(&sessions.stdout);
    assert!(
        !sessions.contains("canonical-sup-main") && !sessions.contains("canonical-worker-main"),
        "custom main ids survived teardown: {sessions}"
    );
}

#[test]
fn compact_agents_use_canonical_identity_while_fixture_agent_specs_stay_inert() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!(
            "SKIP compact_agents_use_canonical_identity_while_fixture_agent_specs_stay_inert"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    std::fs::create_dir_all(fixture.join("agents/evalhost/legacy")).unwrap();
    std::fs::create_dir_all(fixture.join("scripts")).unwrap();
    std::fs::write(
        fixture.join("agents/evalhost/legacy/agent.kdl"),
        r#"agent "legacy" {
  identity "legacy"
  host "evalhost"
  argv "sh" "-c" "touch \"$CATALOG/CANONICAL-SHOULD-NOT-LAUNCH\"; sleep 60"
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/legacy.sh"),
        r#"#!/bin/sh
for _ in $(seq 1 100); do
  set -- "$ST_ROOT/evalhost.legacy/inbox/"*.md
  if [ -e "$1" ] && [ "$ST_AGENT" = "evalhost.legacy" ]; then
    printf '%s\n' "$ST_AGENT" > "$CATALOG/SAW-CANONICAL-KICKOFF"
    break
  fi
  sleep 0.05
done
printf '%s\n' compact-runtime-active
exec sleep 60
"#,
    )
    .unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
agent "legacy" {
  command "sh $CATALOG/scripts/legacy.sh"
}
eval {
  copy "./fixture"
  message { from "requester"; to "legacy"; content "go" }
  max-timeout "2s"
  judges {
    judge "compact identity is canonical and fixture authority stays inert" {
      exec "test \"$(cat $CATALOG/SAW-CANONICAL-KICKOFF)\" = evalhost.legacy && test ! -e $CATALOG/CANONICAL-SHOULD-NOT-LAUNCH && test ! -e $CATALOG/agents/evalhost/legacy/resources/inbox"
    }
  }
}
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env("PATH", path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "colliding fixture Agent Spec changed legacy eval semantics:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        catalog.join("logs/evalhost.legacy.log").exists()
            && !catalog.join("logs/legacy.log").exists(),
        "compact runtime did not use the canonical PTY identity"
    );
}

#[test]
fn canonical_agents_accept_path_independent_local_tasks_and_ignore_remote_projection() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!(
            "SKIP canonical_agents_accept_path_independent_local_tasks_and_ignore_remote_projection"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    let local_dir = fixture.join("organization/.managed/arbitrary/declaration");
    let remote_dir = fixture.join("fleet/remote/declaration");
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(fixture.join("scripts")).unwrap();
    std::fs::write(
        local_dir.join("agent.kdl"),
        r#"agent "local" {
  identity "local"
  host "evalhost"
  pty "work" {
    id "custom-local-task"
    argv "sh" "$CATALOG/scripts/local.sh"
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        remote_dir.join("agent.kdl"),
        r#"agent "remote" {
  identity "remote"
  host "other"
  pty "work" {
    id "remote-task"
    command "touch \"$CATALOG/REMOTE-SPAWNED\"; sleep 60"
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/local.sh"),
        r#"#!/bin/sh
for _ in $(seq 1 100); do
  set -- "$CATALOG/organization/.managed/arbitrary/declaration/resources/inbox/"*.md
  if [ -e "$1" ]; then
    st2 message send requester --root "$ST_ROOT" --as evalhost.local -m "done" >/dev/null 2>&1
    exec sleep 60
  fi
  sleep 0.05
done
exit 42
"#,
    )
    .unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
eval {
  copy "./fixture"
  canonical-agents
  message { from "requester"; to "evalhost.local"; content "go" }
  max-timeout "10s"
  judges {
    judge "local projection only" {
      exec "test -d $CATALOG/organization/.managed/arbitrary/declaration/resources/inbox && test ! -e $CATALOG/REMOTE-SPAWNED"
    }
  }
}
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env("PATH", path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success()
            && stdout.contains("team signalled done")
            && stdout.contains("SCORE: 2 PASS / 0 FAIL"),
        "path-independent local projection failed:\n{stdout}\n{stderr}"
    );
    assert!(catalog.join("logs/custom-local-task.log").is_file());
}

#[test]
fn canonical_agents_reject_an_unknown_kickoff_target_before_spawn() {
    let bin = env!("CARGO_BIN_EXE_st2");
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    std::fs::create_dir_all(fixture.join("agents/evalhost/valid")).unwrap();
    std::fs::write(
        fixture.join("agents/evalhost/valid/agent.kdl"),
        r#"agent "valid" {
  identity "valid"
  host "evalhost"
  argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60"
}
"#,
    )
    .unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
eval {
  copy "./fixture"
  canonical-agents
  message { from "requester"; to "evalhost.missing"; content "go" }
  max-timeout "10s"
  judges { judge "never reached" { exec "false" } }
}
"#,
    )
    .unwrap();
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "unknown kickoff target was accepted:\n{combined}"
    );
    assert!(
        combined.contains("kickoff target `evalhost.missing`") && combined.contains("found 0"),
        "wrong refusal:\n{combined}"
    );
    assert!(
        !catalog.join("SPAWNED").exists(),
        "task spawned before kickoff admission"
    );
}

#[test]
fn canonical_agents_freeze_the_admitted_route_across_post_boot_catalog_mutation() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!(
            "SKIP canonical_agents_freeze_the_admitted_route_across_post_boot_catalog_mutation"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    std::fs::create_dir_all(fixture.join("agents/evalhost/interviewer")).unwrap();
    std::fs::create_dir_all(fixture.join("scripts")).unwrap();
    std::fs::create_dir_all(fixture.join("workspace")).unwrap();
    std::fs::write(
        fixture.join("agents/evalhost/interviewer/agent.kdl"),
        r#"agent "interviewer" {
  identity "interviewer"
  host "evalhost"
  workspace "$CATALOG/workspace"
  argv "sh" "$CATALOG/scripts/interviewer.sh"
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/interviewer.sh"),
        r#"#!/bin/sh
rm "$CATALOG/agents/evalhost/interviewer/agent.kdl"
for _ in $(seq 1 100); do
  set -- "$CATALOG/agents/evalhost/interviewer/resources/inbox/"*.md
  if [ -e "$1" ]; then
    sleep 0.02
    st2 message send requester --root "$ST_ROOT" --as evalhost.interviewer -m "done"
    echo "completed through frozen route"
    break
  fi
  sleep 0.05
done
exec sleep 60
"#,
    )
    .unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
eval {
  copy "./fixture"
  canonical-agents
  message { from "requester"; to "evalhost.interviewer"; content "go" }
  max-timeout "10s"
  judges {
    judge "mutation happened after admission" {
      exec "test ! -e $CATALOG/agents/evalhost/interviewer/agent.kdl"
    }
  }
}
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env("PATH", path)
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let log =
        std::fs::read_to_string(catalog.join("logs/evalhost.interviewer.log")).unwrap_or_default();
    assert!(
        out.status.success()
            && stdout.contains("team signalled done")
            && stdout.contains("SCORE: 2 PASS / 0 FAIL"),
        "frozen canonical route did not survive declaration removal:\n{stdout}\n{stderr}\n--log--\n{log}"
    );
}

#[test]
fn canonical_agents_fail_closed_matrix_is_pre_spawn_and_non_vacuous() {
    type Case<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);

    let bin = env!("CARGO_BIN_EXE_st2");
    let cases: Vec<Case<'_>> = vec![
        (
            "unknown-type",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; type "srvice"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
            )],
        ),
        (
            "unknown-task-kind",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60"; pty { command "true" } }"#,
            )],
        ),
        (
            "supervisor-missing",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; supervisor "missing"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
            )],
        ),
        (
            "bad-path",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; workspace "$CATALOG/missing"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
            )],
        ),
        (
            "materialization warnings",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; workspace "$CATALOG/workspace"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60"; render { git-exclude ".st2/" } }"#,
            )],
        ),
        (
            "duplicate runtime task id",
            "evalhost.one",
            vec![
                (
                    "one",
                    r#"agent "one" { identity "one"; host "evalhost"; pty "agent" { id "shared"; command "touch \"$CATALOG/SPAWNED\"; sleep 60" } }"#,
                ),
                (
                    "two",
                    r#"agent "two" { identity "two"; host "evalhost"; pty "agent" { id "two-main"; command "sleep 60" }; exec "poison" { id "shared"; command "touch \"$CATALOG/SPAWNED\"; sleep 60" } }"#,
                ),
            ],
        ),
        (
            "non-running Agent Spec `evalhost.worker` (retired)",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; retired #true; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
            )],
        ),
        (
            "no local canonical Agent Specs",
            "other.worker",
            vec![(
                "../other/worker",
                r#"agent "worker" { identity "worker"; host "other"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
            )],
        ),
        (
            "override eval-owned `ST_ROOT`",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; pty "agent" { command "touch \"$CATALOG/SPAWNED\"; sleep 60"; env { ST_ROOT "/tmp/poison" } } }"#,
            )],
        ),
        (
            "runtime task id must be nonempty",
            "evalhost.worker",
            vec![(
                "worker",
                r#"agent "worker" { identity "worker"; host "evalhost"; pty "agent" { id ""; command "touch \"$CATALOG/SPAWNED\"; sleep 60" } }"#,
            )],
        ),
        (
            "duplicate canonical route",
            "worker",
            vec![
                (
                    "worker",
                    r#"agent "worker" { identity "worker"; host "evalhost"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
                ),
                (
                    "qualified",
                    r#"agent "evalhost.worker" { identity "evalhost.worker"; host "evalhost"; argv "sh" "-c" "sleep 60" }"#,
                ),
            ],
        ),
    ];
    for (expected, target, declarations) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let cell = tmp.path().join("cell");
        let fixture = cell.join("fixture");
        std::fs::create_dir_all(fixture.join("workspace")).unwrap();
        for (identity, declaration) in declarations {
            let dir = fixture.join(format!("agents/evalhost/{identity}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("agent.kdl"), declaration).unwrap();
        }
        std::fs::write(
            cell.join("cell.kdl"),
            format!(
                r#"
host "evalhost"
eval {{
  copy "./fixture"
  canonical-agents
  message {{ from "requester"; to "{target}"; content "go" }}
  max-timeout "10s"
  judges {{ judge "never reached" {{ exec "false" }} }}
}}
"#
            ),
        )
        .unwrap();
        let child = Command::new(bin)
            .args(["eval", "--keep", "--host", "evalhost"])
            .arg(&cell)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
        let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
        let out = child.wait_with_output().unwrap();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.status.success(),
            "`{expected}` case launched:\n{combined}"
        );
        assert!(
            combined.contains(expected),
            "`{expected}` case produced the wrong refusal:\n{combined}"
        );
        assert!(
            !catalog.join("SPAWNED").exists(),
            "`{expected}` case allowed a pre-admission side effect"
        );
    }

    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    let agent_dir = fixture.join("agents/evalhost/worker");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        fixture.join("catalog.kdl"),
        "catalog { pty_root \"/tmp/poison\" }\n",
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" { identity "worker"; host "evalhost"; argv "sh" "-c" "touch \"$CATALOG/SPAWNED\"; sleep 60" }"#,
    )
    .unwrap();
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
host "evalhost"
eval {
  copy "./fixture"
  canonical-agents
  message { from "requester"; to "evalhost.worker"; content "go" }
  max-timeout "10s"
  judges { judge "never reached" { exec "false" } }
}
"#,
    )
    .unwrap();
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "malformed catalog config launched:\n{combined}"
    );
    assert!(
        combined.contains("catalog-config") && combined.contains("pty_root"),
        "malformed catalog config produced the wrong refusal:\n{combined}"
    );
    assert!(
        !catalog.join("SPAWNED").exists(),
        "malformed catalog config allowed a pre-admission side effect"
    );
}

/// Under `supervise`, teardown reaps RUNTIME-spawned seats too (the team-standup pattern: a seat spins
/// up an undeclared peer mid-run), not just the declared team. The seat spawns an undeclared `rtpeer`
/// into the eval's hermetic PTY_ROOT; after the eval, no orphan carrying the peer's marker survives.
const RUNTIME_PEER_SPEC: &str = r#"
env { ST_ROOT "$CATALOG/custom-bus"; PTY_ROOT "$CATALOG/pty" }
agent "sup" {
  command "sh -c 'pty run -d --id rtpeer -- sleep 100000; for _ in $(seq 1 100); do test -s $PTY_ROOT/rtpeer.pid && break; sleep 0.05; done; cat $PTY_ROOT/rtpeer.pid > $CATALOG/runtime-peer.pid; exec sleep 100000'"
}
eval {
  message { from "runner"; to "sup"; content "go" }
  max-timeout "6s"
  supervise
  judges { judge "trivial" { exec "exit 0" } }
}
"#;

fn supervise_teardown_reaps_a_runtime_spawned_seat_case(judge_command: &str, expect_success: bool) {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!("SKIP supervise_teardown_reaps_a_runtime_spawned_seat: `pty` not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    std::fs::create_dir_all(&cell).unwrap();
    // The seat spawns an UNDECLARED peer into this eval's hermetic PTY_ROOT and records that peer's
    // daemon pid inside the preserved eval catalog. No done signal → the eval times out → supervised
    // teardown must reap both the declared seat and the runtime peer. The proof below uses only this
    // invocation's root + exact pid; concurrent evals cannot be observed or killed.
    let spec_text = RUNTIME_PEER_SPEC;
    assert_eq!(spec_text.len(), 434);
    let sentinel = "judge \"trivial\" { exec \"exit 0\" }";
    assert_eq!(spec_text.matches(sentinel).count(), 1);
    std::fs::write(
        cell.join("cell.kdl"),
        spec_text.replace(
            sentinel,
            &format!("judge \"trivial\" {{ exec \"{judge_command}\" }}"),
        ),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep"])
        .arg(&cell)
        .env("PATH", &path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains(&format!(
            "catalog preserved (--keep): {}",
            catalog.display()
        )),
        "eval did not preserve its expected per-process catalog:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert_eq!(out.status.success(), expect_success);
    if expect_success {
        assert!(
            stdout.contains("VERDICT: PASS"),
            "expected human PASS verdict:\n{stdout}\n{stderr}"
        );
    } else {
        assert!(
            stderr.contains("VERDICT: FAIL"),
            "expected human FAIL verdict on stderr:\n{stdout}\n{stderr}"
        );
        assert!(!out.status.success(), "human FAIL must be nonzero");
    }

    let peer_pid: i32 = std::fs::read_to_string(catalog.join("runtime-peer.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let peer_alive = unsafe { libc::kill(peer_pid, 0) == 0 };
    assert!(
        !peer_alive,
        "runtime peer still alive before post-teardown assertions (pid {peer_pid})"
    );
    let sessions = Command::new("pty")
        .args(["--root"])
        .arg(catalog.join("pty"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(
        sessions.status.success(),
        "could not inspect the eval-scoped PTY registry:\n{}",
        String::from_utf8_lossy(&sessions.stderr)
    );
    let session_json: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert!(
        !catalog.join("pty/rtpeer.pid").exists()
            && !catalog.join("pty/rtpeer.sock").exists()
            && session_json
                .as_array()
                .is_some_and(|sessions| sessions.is_empty()),
        "runtime-spawned seat leaked after supervise teardown (pid {peer_pid}, registry {session_json}):\n\
         --stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
}

#[test]
fn supervise_teardown_reaps_a_runtime_spawned_seat() {
    supervise_teardown_reaps_a_runtime_spawned_seat_case("exit 0", true);
}

#[test]
fn supervise_teardown_runtime_peer_human_failure() {
    supervise_teardown_reaps_a_runtime_spawned_seat_case("exit 1", false);
}

struct SignalCaseFailureGuard {
    child: Option<std::process::Child>,
    pty_root: PathBuf,
    peer_id: String,
    peer_pid: Option<i32>,
    armed: bool,
    receipt: Arc<Mutex<SignalCleanupReceipt>>,
}

fn pty_session_pid(root: &Path, id: &str) -> (i32, serde_json::Value) {
    let out = Command::new("pty")
        .args(["--root"])
        .arg(root)
        .args(["stats", "--json", id])
        .output()
        .unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stats parse failed: {e}; raw={}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    fn find(v: &serde_json::Value) -> Option<i32> {
        match v {
            serde_json::Value::Object(m) => m
                .get("process")
                .and_then(|p| p.get("pid"))
                .and_then(|p| p.as_i64())
                .map(|p| p as i32)
                .or_else(|| m.values().find_map(find)),
            serde_json::Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    let pid = find(&raw).unwrap_or_else(|| panic!("stats missing process.pid: {raw}"));
    (pid, raw)
}

#[derive(Clone, Debug, Default)]
struct SignalCleanupReceipt {
    success: bool,
    diagnostics: String,
    child_pid: Option<u32>,
    child_dead: bool,
    peer_pid: Option<i32>,
    peer_dead: bool,
    registry_empty: bool,
    pid_absent: bool,
    socket_absent: bool,
}

impl SignalCaseFailureGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
    fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        self.child.as_mut()
    }
    fn take_child(&mut self) -> Option<std::process::Child> {
        self.child.take()
    }
}

impl Drop for SignalCaseFailureGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let child_pid = self.child.as_ref().map(|c| c.id());
        let peer_pid = self.peer_pid;
        let mut ok = false;
        let mut diagnostics = String::new();
        for _ in 0..5 {
            let _ = Command::new("pty")
                .args(["--root"])
                .arg(&self.pty_root)
                .args(["kill", &self.peer_id])
                .status();
            if let Some(pid) = peer_pid
                && unsafe { libc::kill(pid, 0) == 0 }
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            let _ = Command::new("pty")
                .args(["--root"])
                .arg(&self.pty_root)
                .args(["rm", &self.peer_id])
                .status();
            match Command::new("pty")
                .args(["--root"])
                .arg(&self.pty_root)
                .args(["list", "--json"])
                .output()
            {
                Ok(out) => {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                        if json.as_array().is_some_and(|v| v.is_empty()) {
                            ok = true;
                            break;
                        }
                        diagnostics = json.to_string();
                    } else {
                        diagnostics = String::from_utf8_lossy(&out.stderr).into_owned();
                    }
                }
                Err(e) => diagnostics = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let child_dead = child_pid.is_some_and(|p| unsafe { libc::kill(p as i32, 0) != 0 });
        let peer_dead = peer_pid.is_none_or(|p| unsafe { libc::kill(p, 0) != 0 });
        let pid_absent = !self.pty_root.join(format!("{}.pid", self.peer_id)).exists();
        let socket_absent = !self
            .pty_root
            .join(format!("{}.sock", self.peer_id))
            .exists();
        if !ok && diagnostics.is_empty() {
            diagnostics = "registry did not converge empty".into();
        }
        if let Ok(mut receipt) = self.receipt.lock() {
            receipt.child_pid = child_pid;
            receipt.child_dead = child_dead;
            receipt.peer_pid = peer_pid;
            receipt.peer_dead = peer_dead;
            receipt.registry_empty = ok;
            receipt.pid_absent = pid_absent;
            receipt.socket_absent = socket_absent;
            receipt.success = child_dead && peer_dead && ok && pid_absent && socket_absent;
            receipt.diagnostics = diagnostics;
        }
    }
}

fn runtime_peer_signal_case(sig: libc::c_int) {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "pty not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!("SKIP runtime_peer_signal_case: pty not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    std::fs::create_dir_all(&cell).unwrap();
    std::fs::write(cell.join("cell.kdl"), RUNTIME_PEER_SPEC).unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep"])
        .arg(&cell)
        .env("PATH", path)
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _guard = RemoveDirOnDrop(catalog.clone());
    let receipt = Arc::new(Mutex::new(SignalCleanupReceipt::default()));
    let mut failure = SignalCaseFailureGuard {
        child: Some(child),
        pty_root: catalog.join("pty"),
        peer_id: "rtpeer".into(),
        peer_pid: None,
        armed: true,
        receipt,
    };
    let marker = catalog.join("runtime-peer.pid");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if !marker.exists() {
        let status = failure
            .child_mut()
            .and_then(|c| c.try_wait().ok())
            .flatten();
        panic!(
            "marker timeout status={status:?} catalog={}",
            catalog.display()
        );
    }
    assert!(catalog.is_dir());
    let (session_pid, _stats) = pty_session_pid(&catalog.join("pty"), "rtpeer");
    let child_id = failure.child_mut().unwrap().id();
    assert_eq!(unsafe { libc::kill(child_id as i32, sig) }, 0);
    let out = failure.take_child().unwrap().wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "status={:?} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("eval interrupted by SIGINT/SIGTERM"),
        "missing interruption contract: {combined}"
    );
    let peer: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    failure.peer_pid = Some(peer);
    assert!(unsafe { libc::kill(peer, 0) != 0 });
    assert!(unsafe { libc::kill(session_pid, 0) != 0 });
    let listed = Command::new("pty")
        .args(["--root"])
        .arg(catalog.join("pty"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    let registry: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(registry.as_array().is_some_and(|v| v.is_empty()));
    assert!(!catalog.join("pty/rtpeer.pid").exists());
    assert!(!catalog.join("pty/rtpeer.sock").exists());
    failure.disarm();
}

#[test]
fn supervise_runtime_peer_sigterm_reaps() {
    runtime_peer_signal_case(libc::SIGTERM);
}
#[test]
fn supervise_runtime_peer_sigint_reaps() {
    runtime_peer_signal_case(libc::SIGINT);
}

#[test]
fn signal_case_failure_guard_reaps_on_unwind() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "pty not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!("SKIP signal_case_failure_guard_reaps_on_unwind: pty not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let receipt = Arc::new(Mutex::new(SignalCleanupReceipt::default()));
    let receipt_out = receipt.clone();
    let session_pid_out: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let session_pid_capture = session_pid_out.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::fs::create_dir_all(catalog.join("pty")).unwrap();
        let _catalog = RemoveDirOnDrop(catalog.clone());
        let root = catalog.join("pty");
        let peer = Command::new("pty")
            .args(["--root"])
            .arg(&root)
            .args(["run", "-d", "--id", "rtpeer", "--", "sleep", "1000"])
            .status()
            .unwrap();
        assert!(peer.success());
        let pid_path = root.join("rtpeer.pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let peer_pid: i32 = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let (session_pid, _) = pty_session_pid(&root, "rtpeer");
        *session_pid_capture.lock().unwrap() = Some(session_pid);
        let child = Command::new("sh")
            .args(["-c", "sleep 1000"])
            .spawn()
            .unwrap();
        let guard = SignalCaseFailureGuard {
            child: Some(child),
            pty_root: root,
            peer_id: "rtpeer".into(),
            peer_pid: Some(peer_pid),
            armed: true,
            receipt: receipt.clone(),
        };
        let _guard = guard;
        panic!("representative post-signal assertion");
    }));
    assert!(result.is_err());
    let receipt = receipt_out.lock().unwrap();
    assert!(
        receipt.success,
        "cleanup diagnostics: {}",
        receipt.diagnostics
    );
    assert!(receipt.diagnostics.is_empty());
    assert!(
        receipt.child_pid.is_some()
            && receipt.child_dead
            && receipt.peer_pid.is_some()
            && receipt.peer_dead
            && receipt.registry_empty
            && receipt.pid_absent
            && receipt.socket_absent
    );
    let session_pid = session_pid_out.lock().unwrap().unwrap();
    assert!(unsafe { libc::kill(session_pid, 0) != 0 });
    assert!(!catalog.exists());
}

/// A TEAM-LESS eval (no agents, no kickoff) runs its `run` steps to completion, captures each step's
/// stdout/stderr/exit for the judges, and the judges own the verdict. By DEFAULT a step must exit 0 (so
/// `make` gates the verdict); `allow-nonzero` opts a step out (so `probe`'s deliberate exit 3 is fine
/// and a judge asserts it). Flat run-collapse form. No `pty` needed (no seats are booted).
#[test]
fn team_less_run_stage_captures_and_judges() {
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    std::fs::create_dir_all(&cell).unwrap();
    // `make` succeeds (the default must-exit-0 gate) and writes a file; `probe` deliberately exits 3 with
    // `allow-nonzero`, so that is NOT a failure — the judges assert the file, probe's captured exit (via
    // $RUNS_DIR and $RUN_<id>_EXIT), and probe's captured stdout. Written in the flat run-collapse form.
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
env { ST_ROOT "$CATALOG/custom-bus" }
eval {
  max-timeout "20s"
  run "make"  { command "echo hi > $CATALOG/out.txt" }
  run "probe" { command "echo scanning; exit 3"; allow-nonzero }
  judges {
    judge "file-made"    { exec "test -f $CATALOG/out.txt" }
    judge "probe-exit3"  { exec "test \"$(cat $RUNS_DIR/probe.exit)\" = 3" }
    judge "probe-stdout" { exec "grep -q scanning $RUNS_DIR/probe.out" }
    judge "exit-var"     { exec "test \"$RUN_probe_EXIT\" = 3" }
    judge "probe-log"    { exec "grep -q scanning $LOGS_DIR/probe.log" }
  }
}
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin)
        .args(["eval"])
        .arg(&cell)
        .env("PATH", path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("team-less eval: 2 run step(s)"),
        "not the team-less path:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("run step probe → exit 3"),
        "probe's non-zero exit not captured:\n{stdout}"
    );
    assert!(
        stdout.contains("SCORE: 6 PASS / 0 FAIL"),
        "team-less run-stage eval should be 6/0 (make must-exit-0 gate + 5 judges incl. $LOGS_DIR):\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert!(
        stdout.contains("VERDICT: PASS") && out.status.success(),
        "expected PASS:\n{stdout}\n{stderr}"
    );
}

/// crash-ding: under `supervise`, a seat that CRASHES (non-zero/killed/vanished) is respawned AND its
/// crash escalates up the supervisor chain — a "worker crash: <id>" bus ding to the direct supervisor
/// AND the root cos (walking the `supervisor` field transitively). A seat that exits CLEANLY (0) stays
/// SILENT (a false ding on a routine finish fails as hard as a missed crash). Keys on the pty session
/// dying → harness-agnostic. Needs `pty` (seats boot).
#[test]
fn supervise_crash_dings_up_the_chain_and_is_silent_on_clean_exit() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!(
            "SKIP supervise_crash_dings_up_the_chain_and_is_silent_on_clean_exit: `pty` not on PATH"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    let fixture = cell.join("fixture");
    std::fs::create_dir_all(fixture.join("scripts")).unwrap();
    // Chain: cd.worker → cd.sup → cd.cos (root). worker CRASHES (exit 1) → ding up to sup AND cos;
    // `clean` exits 0 → stays silent. A clean sentinel exits only after kickoff; its supervisor-driven
    // respawn proves that at least one supervise tick observed worker + clean alive. Only then does the
    // sup release those two seats, so neither can race the boot gate or the `ever_alive` classification
    // under load. Their second launches report that both exits were observed and respawned.
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
env { ST_ROOT "$CATALOG/custom-bus"; PTY_ROOT "$CATALOG/pty" }
team "cd" {
  agent "gate"   { supervisor "cd.cos"; command "sh $CATALOG/scripts/gate.sh" }
  agent "worker" { supervisor "cd.sup"; command "sh $CATALOG/scripts/worker.sh" }
  agent "clean"  { supervisor "cd.cos"; command "sh $CATALOG/scripts/clean.sh" }
  agent "sup"    { supervisor "cd.cos"; command "sh $CATALOG/scripts/sup.sh" }
  agent "cos"    { command "sleep 100000" }
}
eval {
  copy "./fixture"
  message { from "runner"; to "cd.sup"; content "go" }
  max-timeout "20s"
  supervise
  judges { judge "ok" { exec "true" } }
}
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/gate.sh"),
        r#"#!/bin/sh
while [ ! -e "$CATALOG/release-gate-after-kickoff" ]; do sleep 0.05; done
if mkdir "$CATALOG/gate-exited-once" 2>/dev/null; then
  exit 0
fi
st2 message send evalhost.cd.sup --root "$ST_ROOT" --as "$ST_AGENT" -m "supervise tick completed" >/dev/null 2>&1
sleep 100000
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/worker.sh"),
        r#"#!/bin/sh
while [ ! -e "$CATALOG/release-after-supervise-tick" ]; do sleep 0.05; done
if mkdir "$CATALOG/worker-exited-once" 2>/dev/null; then
  exit 1
fi
st2 message send evalhost.cd.sup --root "$ST_ROOT" --as "$ST_AGENT" -m "worker respawned after crash" >/dev/null 2>&1
sleep 100000
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/clean.sh"),
        r#"#!/bin/sh
while [ ! -e "$CATALOG/release-after-supervise-tick" ]; do sleep 0.05; done
if mkdir "$CATALOG/clean-exited-once" 2>/dev/null; then
  exit 0
fi
st2 message send evalhost.cd.sup --root "$ST_ROOT" --as "$ST_AGENT" -m "clean seat respawned" >/dev/null 2>&1
sleep 100000
"#,
    )
    .unwrap();
    std::fs::write(
        fixture.join("scripts/sup.sh"),
        r#"#!/bin/sh
wait_from() {
  from=$1
  for _ in $(seq 1 160); do
    count=$(st2 message ls "$ST_AGENT" --root "$ST_ROOT" --from "$from" --count 2>/dev/null || echo 0)
    [ "$count" -gt 0 ] 2>/dev/null && return 0
    sleep 0.05
  done
  return 1
}

wait_from runner || exit 2
: > "$CATALOG/release-gate-after-kickoff"
wait_from evalhost.cd.gate || exit 3
: > "$CATALOG/release-after-supervise-tick"
wait_from st2 || exit 4
wait_from evalhost.cd.worker || exit 5
wait_from evalhost.cd.clean || exit 6
st2 message send runner --root "$ST_ROOT" --as "$ST_AGENT" -m "both supervised exits classified" >/dev/null 2>&1
sleep 100000
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(bin)
        .args(["eval", "--keep", "--host", "evalhost"])
        .arg(&cell)
        .env("PATH", path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let catalog = std::env::temp_dir().join(format!("st2e-{}", child.id()));
    let _catalog_cleanup = RemoveDirOnDrop(catalog.clone());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("team signalled done"),
        "post-boot exit/respawn handshake didn't complete:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    // (b) the crash escalates to BOTH the direct supervisor and the root cos.
    assert!(
        stdout.contains("crash-ding: evalhost.cd.worker → evalhost.cd.sup"),
        "crash didn't ding the supervisor:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    assert!(
        stdout.contains("crash-ding: evalhost.cd.worker → evalhost.cd.cos"),
        "crash didn't reach the root cos:\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    // (c) the clean-exit seat produces NO ding.
    assert!(
        !stdout.contains("crash-ding: evalhost.cd.clean"),
        "a clean exit (0) must NOT crash-ding:\n{stdout}"
    );
    assert!(
        !stdout.contains("crash-ding: evalhost.cd.gate"),
        "the clean gate exit (0) must NOT crash-ding:\n{stdout}"
    );
}

/// A seat whose command exits at boot (127 command-not-found, crash) must fail the eval FAST + loudly,
/// NOT hang until max-timeout waiting for a confirmation that can never come. (CoS robustness finding.)
#[test]
fn st2_eval_fails_fast_when_a_seat_exits_at_boot() {
    if !pty_available() {
        assert!(
            std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
            "`pty` not on PATH; set ST2_ALLOW_PTY_SKIP=1"
        );
        eprintln!("SKIP st2_eval_fails_fast_when_a_seat_exits_at_boot: `pty` not on PATH");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cell = tmp.path().join("cell");
    std::fs::create_dir_all(cell.join("fixture")).unwrap();
    // A seat command that doesn't exist → the pty session exits ~immediately. max-timeout is huge, so a
    // hang would take 10 min; fail-fast must make this return in seconds.
    std::fs::write(
        cell.join("cell.kdl"),
        r#"
env { ST_ROOT "$CATALOG/custom-bus"; PTY_ROOT "$CATALOG/pty" }
agent "bad" { command "definitely-not-a-real-binary-xyz123" }
eval {
  copy "./fixture"
  message { from "requester"; to "bad"; content "go" }
  max-timeout "600s"
  judges { judge "t" { exec "true" } }
}
"#,
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let start = Instant::now();
    let out = Command::new(bin)
        .args(["eval"])
        .arg(&cell)
        .env("PATH", path)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env("XDG_STATE_HOME", tmp.path().join("xdg"))
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "a dead-at-boot seat must fail the eval:\n{combined}"
    );
    assert!(
        combined.contains("exited at boot"),
        "expected a clear fail-fast error:\n{combined}"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "must fail FAST, not wait max-timeout — took {elapsed:?}"
    );
}
