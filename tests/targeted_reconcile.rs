//! R19 executable proofs for the `st2 up --once --task <exact-id>` CLI path.
//!
//! The exec-backed tests use a recording `pty` shim to prove refusal/listing order without touching
//! a real registry. The lifecycle test uses an isolated real `PTY_ROOT` and keeps an unrelated
//! sibling's PID + creation generation fixed across selected missing/live/replacement passes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = "targeted";
const OWNER: &str = "targeted.owner.work";
const SIBLING: &str = "targeted.sibling.work";

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn agent_kdl(identity: &str, kind: &str, task_id: &str, workspace: &Path, marker: &str) -> String {
    format!(
        r#"agent "{identity}" {{
  host "{HOST}"
  type "service"
  workspace "{}"
  {kind} "work" {{
    id "{task_id}"
    command "sleep 120"
  }}
  render {{
    file "{marker}" "{identity}"
  }}
}}
"#,
        workspace.display()
    )
}

fn write_two_agent_catalog(
    catalog: &Path,
    kind: &str,
    owner_workspace: &Path,
    sibling_workspace: &Path,
) {
    fs::create_dir_all(owner_workspace).unwrap();
    fs::create_dir_all(sibling_workspace).unwrap();
    write(
        &catalog.join("agents/targeted/owner/agent.kdl"),
        agent_kdl("owner", kind, OWNER, owner_workspace, "OWNER.txt"),
    );
    write(
        &catalog.join("agents/targeted/sibling/agent.kdl"),
        agent_kdl("sibling", kind, SIBLING, sibling_workspace, "SIBLING.txt"),
    );
}

fn prepend_path(directory: &Path) -> String {
    format!(
        "{}:{}",
        directory.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn selected_once(catalog: &Path, xdg: &Path, pty_root: &Path, selector: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(catalog)
        .args(["--host", HOST, "--once", "--task", selector])
        .env("XDG_STATE_HOME", xdg)
        .env("PTY_ROOT", pty_root)
        .output()
        .unwrap()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_pid(path: &Path) -> Option<i32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok().or_else(|| {
        serde_json::from_str::<serde_json::Value>(&raw).ok()?["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
    })
}

fn kill_process_group(pid: i32) {
    for target in [format!("-{pid}"), pid.to_string()] {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct ExecCleanup {
    pidfiles: Vec<PathBuf>,
}

impl Drop for ExecCleanup {
    fn drop(&mut self) {
        for pidfile in &self.pidfiles {
            if let Some(pid) = read_pid(pidfile) {
                kill_process_group(pid);
            }
        }
    }
}

#[test]
fn targeted_once_cli_resolves_before_listing_and_runs_only_the_selected_exec() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let xdg = tmp.path().join("xdg");
    let pty_root = tmp.path().join("pty");
    let owner_workspace = tmp.path().join("owner-workspace");
    let sibling_workspace = tmp.path().join("sibling-workspace");
    let shim_bin = tmp.path().join("bin");
    let pty_calls = tmp.path().join("pty-calls");
    fs::create_dir_all(&shim_bin).unwrap();
    executable(
        &shim_bin.join("pty"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PTY_CALLS\"\n[ \"$1\" = list ] && printf '[]\\n' && exit 0\nexit 97\n",
    );
    write_two_agent_catalog(&catalog, "exec", &owner_workspace, &sibling_workspace);

    let owner_pidfile = xdg
        .join("st2")
        .join(HOST)
        .join("exec")
        .join(format!("{OWNER}.pid"));
    let sibling_pidfile = xdg
        .join("st2")
        .join(HOST)
        .join("exec")
        .join(format!("{SIBLING}.pid"));
    let _cleanup = ExecCleanup {
        pidfiles: vec![owner_pidfile.clone(), sibling_pidfile.clone()],
    };
    let invoke = |selector: &str| {
        Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["up", "--catalog"])
            .arg(&catalog)
            .args(["--host", HOST, "--once", "--task", selector])
            .env("PATH", prepend_path(&shim_bin))
            .env("PTY_CALLS", &pty_calls)
            .env("XDG_STATE_HOME", &xdg)
            .env("PTY_ROOT", &pty_root)
            .output()
            .unwrap()
    };

    let refused = invoke("targeted.missing.work");
    assert!(!refused.status.success());
    assert!(
        !pty_calls.exists(),
        "an unknown selector must refuse before `pty list`"
    );
    assert!(!owner_workspace.join("OWNER.txt").exists());
    assert!(!sibling_workspace.join("SIBLING.txt").exists());

    let launched = invoke(OWNER);
    assert_success(&launched, "selected exec launch failed");
    let stdout = String::from_utf8_lossy(&launched.stdout);
    assert!(
        stdout.contains(&format!("launched (1): {OWNER}")),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(owner_workspace.join("OWNER.txt")).unwrap(),
        "owner"
    );
    assert!(!sibling_workspace.join("SIBLING.txt").exists());
    assert!(owner_pidfile.exists());
    assert!(!sibling_pidfile.exists());
    assert_eq!(
        fs::read_to_string(&pty_calls)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["list --json"]
    );

    let owner_pid = read_pid(&owner_pidfile).unwrap();
    let adopted = invoke(OWNER);
    assert_success(&adopted, "selected live exec adoption failed");
    assert_eq!(read_pid(&owner_pidfile), Some(owner_pid));
    assert!(!sibling_pidfile.exists());
    assert!(
        String::from_utf8_lossy(&adopted.stdout).contains("adopted (1): owner"),
        "{}",
        String::from_utf8_lossy(&adopted.stdout)
    );
}

#[test]
fn targeted_once_cli_owner_render_failure_is_nonzero_before_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let xdg = tmp.path().join("xdg");
    let pty_root = tmp.path().join("pty");
    let workspace = tmp.path().join("workspace");
    let shim_bin = tmp.path().join("bin");
    let pty_calls = tmp.path().join("pty-calls");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&shim_bin).unwrap();
    executable(
        &shim_bin.join("pty"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PTY_CALLS\"\nprintf '[]\\n'\n",
    );
    write(
        &catalog.join("agents/targeted/owner/agent.kdl"),
        format!(
            r#"agent "owner" {{
  host "{HOST}"
  type "service"
  workspace "{}"
  exec "work" {{
    id "{OWNER}"
    command "sleep 120"
  }}
  render {{
    copy "_templates/missing" "OWNER.txt"
  }}
}}
"#,
            workspace.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(&catalog)
        .args(["--host", HOST, "--once", "--task", OWNER])
        .env("PATH", prepend_path(&shim_bin))
        .env("PTY_CALLS", &pty_calls)
        .env("XDG_STATE_HOME", &xdg)
        .env("PTY_ROOT", &pty_root)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        !pty_calls.exists(),
        "owner render refusal must occur before `pty list`"
    );
    assert!(!workspace.join("OWNER.txt").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("_templates/missing"), "{stderr}");
    assert!(
        stderr.contains("targeted one-shot reconcile pass reported errors"),
        "{stderr}"
    );
}

fn pty_available() -> bool {
    Command::new("pty")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn pty_gate(test: &str) -> bool {
    if pty_available() {
        return true;
    }
    assert!(
        std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
        "{test}: `pty` is not on PATH, so the real-PTY targeted reconcile gate is unproven. \
         Install `pty`, or set ST2_ALLOW_PTY_SKIP=1 for a local opt-out."
    );
    eprintln!("SKIP {test}: `pty` not on PATH (ST2_ALLOW_PTY_SKIP set)");
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PtyGeneration {
    pid: i64,
    created_at: String,
}

fn pty_generation(pty_root: &Path, id: &str) -> Option<PtyGeneration> {
    let output = Command::new("pty")
        .args(["list", "--json"])
        .env("PTY_ROOT", pty_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    rows.as_array()?
        .iter()
        .find(|row| row["name"].as_str() == Some(id) && row["status"].as_str() == Some("running"))
        .and_then(|row| {
            Some(PtyGeneration {
                pid: row["pid"].as_i64()?,
                created_at: row["createdAt"].as_str()?.to_owned(),
            })
        })
}

fn wait_for_generation(pty_root: &Path, id: &str) -> PtyGeneration {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(generation) = pty_generation(pty_root, id) {
            return generation;
        }
        assert!(
            Instant::now() < deadline,
            "session {id} did not become running in {}",
            pty_root.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct PtyCleanup {
    root: PathBuf,
    ids: Vec<&'static str>,
}

impl Drop for PtyCleanup {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = Command::new("pty")
                .args(["kill", id])
                .env("PTY_ROOT", &self.root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        std::thread::sleep(Duration::from_millis(600));
        for id in &self.ids {
            let _ = Command::new("pty")
                .args(["rm", id])
                .env("PTY_ROOT", &self.root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[test]
fn targeted_once_real_pty_preserves_sibling_generation_across_selected_lifecycle() {
    if !pty_gate("targeted_once_real_pty_preserves_sibling_generation_across_selected_lifecycle") {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let xdg = tmp.path().join("xdg");
    let pty_root = tmp.path().join("pty");
    let owner_workspace = tmp.path().join("owner-workspace");
    let sibling_workspace = tmp.path().join("sibling-workspace");
    fs::create_dir_all(&pty_root).unwrap();
    write_two_agent_catalog(&catalog, "pty", &owner_workspace, &sibling_workspace);
    let _cleanup = PtyCleanup {
        root: pty_root.clone(),
        ids: vec![OWNER, SIBLING],
    };

    let sibling_boot = Command::new("pty")
        .args(["run", "-d", "--id", SIBLING, "--", "sleep", "120"])
        .env("PTY_ROOT", &pty_root)
        .output()
        .unwrap();
    assert_success(&sibling_boot, "failed to seed the unrelated sibling PTY");
    let sibling_generation = wait_for_generation(&pty_root, SIBLING);

    let launched = selected_once(&catalog, &xdg, &pty_root, OWNER);
    assert_success(&launched, "selected missing PTY launch failed");
    let owner_generation = wait_for_generation(&pty_root, OWNER);
    assert_eq!(
        pty_generation(&pty_root, SIBLING),
        Some(sibling_generation.clone())
    );
    assert_eq!(
        fs::read_to_string(owner_workspace.join("OWNER.txt")).unwrap(),
        "owner"
    );
    assert!(!sibling_workspace.join("SIBLING.txt").exists());
    let stdout = String::from_utf8_lossy(&launched.stdout);
    assert!(
        stdout.contains(&format!("launched (1): {OWNER}")),
        "{stdout}"
    );

    let adopted = selected_once(&catalog, &xdg, &pty_root, OWNER);
    assert_success(&adopted, "selected live PTY adoption failed");
    assert_eq!(
        pty_generation(&pty_root, OWNER),
        Some(owner_generation.clone())
    );
    assert_eq!(
        pty_generation(&pty_root, SIBLING),
        Some(sibling_generation.clone())
    );
    assert!(
        String::from_utf8_lossy(&adopted.stdout).contains("adopted (1): owner"),
        "{}",
        String::from_utf8_lossy(&adopted.stdout)
    );

    let killed = Command::new("kill")
        .args(["-KILL", &owner_generation.pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "failed to hard-kill selected owner PTY");
    let relaunched = selected_once(&catalog, &xdg, &pty_root, OWNER);
    assert_success(&relaunched, "selected dead PTY relaunch failed");
    let replacement_generation = wait_for_generation(&pty_root, OWNER);
    assert_ne!(replacement_generation, owner_generation);
    assert_eq!(pty_generation(&pty_root, SIBLING), Some(sibling_generation));
    assert!(!sibling_workspace.join("SIBLING.txt").exists());
    let stdout = String::from_utf8_lossy(&relaunched.stdout);
    assert!(
        stdout.contains(&format!("restarted (1): {OWNER}")),
        "{stdout}"
    );
    assert!(!stdout.contains(SIBLING), "{stdout}");
}
