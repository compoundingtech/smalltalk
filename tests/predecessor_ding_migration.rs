//! Exact predecessor-to-current Ding migration gate.
//!
//! This ignored gate executes one source- and digest-pinned predecessor binary plus real `pty`
//! processes. Every state root and runtime id is test-owned. The predecessor is used only to drain
//! one exact legacy exec generation; the current binary then adopts the provider PTY and replaces
//! the dead numeric record with a strict generation.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agent_spec::spec::TaskKind;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const HOST: &str = "h";
const PROVIDER: &str = "h.provider";
const DING: &str = "h.provider.ding";
const SENTINEL: &str = "h.provider.ding-sentinel";
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
struct PredecessorPin {
    source_rev: &'static str,
    derivation: &'static str,
    input_sha256: &'static str,
    sha256: &'static str,
}

const PREDECESSOR_PIN: PredecessorPin = PredecessorPin {
    source_rev: "c6846f6239329f0803142afc06c15a07b93937c1",
    derivation: "docker.io/library/rust@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073: cargo build --locked --release --bin st2 at c6846f6239329f0803142afc06c15a07b93937c1",
    input_sha256: "56d7956f328d7525eea04c70f5767acb3bc207c9509e7e2cc332444a4ede2f3e",
    sha256: "6493b0284462a5cc8c7eba8d81c0995f6ea4f738ecfb2c848991847b0c0d9624",
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct PtyGeneration {
    pid: u32,
    created_at: String,
}

struct Fixture {
    _tmp: tempfile::TempDir,
    main_catalog: PathBuf,
    migration_catalog: PathBuf,
    xdg: PathBuf,
    pty_root: PathBuf,
    bin: PathBuf,
    predecessor: PathBuf,
    predecessor_ding: Option<(u32, u64)>,
}

impl Fixture {
    fn new(predecessor: PathBuf) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let main_catalog = tmp.path().join("main-catalog");
        let migration_catalog = tmp.path().join("migration-catalog");
        let xdg = tmp.path().join("xdg");
        let pty_root = tmp.path().join("pty");
        let bin = tmp.path().join("bin");
        for path in [&main_catalog, &migration_catalog, &xdg, &pty_root, &bin] {
            fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_st2"), bin.join("st2")).unwrap();
        Self {
            _tmp: tmp,
            main_catalog,
            migration_catalog,
            xdg,
            pty_root,
            bin,
            predecessor,
            predecessor_ding: None,
        }
    }

    fn isolated(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        let path = std::env::var_os("PATH").unwrap_or_default();
        command
            .env("XDG_STATE_HOME", &self.xdg)
            .env("PTY_ROOT", &self.pty_root)
            .env("ST_HOOKS", self.xdg.join("hooks"))
            .env("PATH", joined_path(&self.bin, &path))
            .env_remove("CATALOG")
            .env_remove("ST_ROOT");
        command
    }

    fn candidate(&self) -> Command {
        self.isolated(Path::new(env!("CARGO_BIN_EXE_st2")))
    }

    fn predecessor(&self) -> Command {
        self.isolated(&self.predecessor)
    }

    fn pty(&self) -> Command {
        let mut command = Command::new("pty");
        command.env("PTY_ROOT", &self.pty_root);
        command
    }

    fn exec_record(&self) -> PathBuf {
        self.xdg
            .join("st2")
            .join(HOST)
            .join("exec")
            .join(format!("{DING}.pid"))
    }

    fn write_catalogs(&self) {
        fs::write(
            self.main_catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                self.pty_root.display().to_string()
            ),
        )
        .unwrap();
        write_agent(
            &self.main_catalog,
            "provider",
            r#"
  ding
  pty "agent" {
    id "h.provider"
    lifecycle "adopt-only"
    argv "sleep" "120"
  }
"#,
        );
        fs::create_dir_all(self.main_catalog.join("agents/h/provider/resources/inbox")).unwrap();

        fs::write(
            self.migration_catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                self.pty_root.display().to_string()
            ),
        )
        .unwrap();
        write_agent(
            &self.migration_catalog,
            "provider",
            r#"
  retired #true
  ding
"#,
        );
        // Migration declarations are read-only, and their digest is checked
        // after predecessor teardown.
        for declaration in [
            self.migration_catalog.join("catalog.kdl"),
            self.migration_catalog.join("agents/h/provider/agent.kdl"),
        ] {
            let mut mode = fs::metadata(&declaration).unwrap().permissions();
            mode.set_mode(0o444);
            fs::set_permissions(declaration, mode).unwrap();
        }
    }

    fn run_pty(&self, id: &str) {
        let output = self
            .pty()
            .args([
                "run",
                "-d",
                "--force",
                "--id",
                id,
                "--no-display-name",
                "--tag",
                "keep=true",
                "--",
                "sleep",
                "120",
            ])
            .output()
            .unwrap();
        assert_success(&output, &format!("launch isolated PTY {id}"));
        poll_until(TIMEOUT, || {
            self.pty_generation(id)
                .is_some_and(|generation| generation.pid > 0)
        });
    }

    fn pty_generation(&self, id: &str) -> Option<PtyGeneration> {
        let output = self.pty().args(["list", "--json"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let rows: Value = serde_json::from_slice(&output.stdout).ok()?;
        let row = rows
            .as_array()?
            .iter()
            .find(|row| row["name"].as_str() == Some(id) && row["status"] == "running")?;
        Some(PtyGeneration {
            pid: u32::try_from(row["pid"].as_u64()?).ok()?,
            created_at: row["createdAt"].as_str()?.to_owned(),
        })
    }

    fn spawn_predecessor_ding(&mut self) -> Child {
        let mut command = self.predecessor();
        command
            .args([
                "ding",
                "--identity",
                PROVIDER,
                "--root",
                self.main_catalog.to_str().unwrap(),
                "--interval",
                "50",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        let start =
            process_start_ticks(pid).expect("predecessor Ding exited before identification");
        self.predecessor_ding = Some((pid, start));
        child
    }

    fn cleanup_pty(&self, id: &str) {
        let _ = self
            .pty()
            .args(["kill", id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let removed = self
                .pty()
                .args(["rm", id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if removed.is_ok_and(|status| status.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn cleanup_exec_record(&self) {
        let Ok(raw) = fs::read_to_string(self.exec_record()) else {
            return;
        };
        let generation = if let Ok(pid) = raw.trim().parse::<u32>() {
            process_start_ticks(pid).map(|start| (pid, start))
        } else {
            serde_json::from_str::<Value>(&raw).ok().and_then(|value| {
                Some((
                    u32::try_from(value["pid"].as_u64()?).ok()?,
                    value["startTimeTicks"].as_u64()?,
                ))
            })
        };
        if let Some((pid, start)) = generation
            && process_start_ticks(pid) == Some(start)
        {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = wait_process_gone(pid, start, Duration::from_secs(5));
        }
        let _ = fs::remove_file(self.exec_record());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cleanup_exec_record();
        if let Some((pid, start)) = self.predecessor_ding
            && process_start_ticks(pid) == Some(start)
        {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = wait_process_gone(pid, start, Duration::from_secs(5));
        }
        self.cleanup_pty(PROVIDER);
        self.cleanup_pty(SENTINEL);
    }
}

#[test]
#[ignore = "requires the pinned retained predecessor binary and real pty"]
fn predecessor_drains_only_legacy_ding_then_candidate_adopts_provider_and_replaces_it() {
    let predecessor = selected_predecessor();
    assert!(
        have_pty(),
        "selected predecessor Ding migration gate requires `pty` on PATH"
    );

    let mut fixture = Fixture::new(predecessor);
    fixture.write_catalogs();
    assert_migration_catalog_safe(&fixture.migration_catalog, &[DING]).unwrap();
    assert_adversarial_census_rejects_provider_task(&fixture);
    let declarations_before = declaration_digest(&fixture.migration_catalog);

    fixture.run_pty(PROVIDER);
    fixture.run_pty(SENTINEL);
    let provider_before = fixture.pty_generation(PROVIDER).unwrap();
    let sentinel_before = fixture.pty_generation(SENTINEL).unwrap();

    let mut predecessor_ding = fixture.spawn_predecessor_ding();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        predecessor_ding.try_wait().unwrap().is_none(),
        "predecessor Ding exited before its legacy generation could be seeded"
    );
    let (legacy_pid, legacy_start) = fixture.predecessor_ding.unwrap();
    fs::create_dir_all(fixture.exec_record().parent().unwrap()).unwrap();
    fs::write(fixture.exec_record(), format!("{legacy_pid}\n")).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let down = fixture
        .predecessor()
        .arg("--catalog")
        .arg(&fixture.migration_catalog)
        .args(["down", "--host", HOST])
        .output()
        .unwrap();
    assert_success(&down, "predecessor migration-catalog teardown");
    assert!(
        String::from_utf8_lossy(&down.stdout).contains(DING),
        "predecessor did not report exact Ding teardown:\n{}",
        String::from_utf8_lossy(&down.stdout)
    );
    assert!(
        wait_child_exit(&mut predecessor_ding, TIMEOUT),
        "predecessor left its exact Ding generation alive"
    );
    assert_ne!(
        process_start_ticks(legacy_pid),
        Some(legacy_start),
        "reaped predecessor Ding generation still appears live"
    );
    assert_eq!(
        fs::read_to_string(fixture.exec_record()).unwrap().trim(),
        legacy_pid.to_string(),
        "predecessor drain must leave the numeric record for candidate lifecycle GC"
    );
    assert_eq!(
        fixture.pty_generation(PROVIDER).unwrap(),
        provider_before,
        "predecessor migration replaced or killed the provider"
    );
    assert_eq!(
        fixture.pty_generation(SENTINEL).unwrap(),
        sentinel_before,
        "predecessor migration touched the adversarial provider sentinel"
    );
    assert_eq!(
        declaration_digest(&fixture.migration_catalog),
        declarations_before,
        "the immutable migration declarations changed during predecessor teardown"
    );

    let up = fixture
        .candidate()
        .arg("--catalog")
        .arg(&fixture.main_catalog)
        .args(["up", "--once", "--host", HOST])
        .output()
        .unwrap();
    assert_success(&up, "candidate adoption/replacement pass");
    let report = String::from_utf8_lossy(&up.stdout);
    assert!(
        report.contains(&format!("gc (1): {DING}")),
        "candidate did not GC the predecessor numeric record:\n{report}"
    );
    assert!(
        report.contains(&format!("launched (1): {DING}")),
        "candidate did not launch one strict Ding:\n{report}"
    );
    assert!(
        !report.contains("torn-down") && !report.contains("held"),
        "candidate reported an unexpected destructive or held task:\n{report}"
    );
    assert_eq!(
        fixture.pty_generation(PROVIDER).unwrap(),
        provider_before,
        "candidate replacement pass changed the adopted provider generation"
    );
    assert_eq!(
        fixture.pty_generation(SENTINEL).unwrap(),
        sentinel_before,
        "candidate replacement pass touched the undeclared provider sentinel"
    );

    let strict_raw = fs::read_to_string(fixture.exec_record()).unwrap();
    let strict: Value = serde_json::from_str(&strict_raw).expect("candidate record is not JSON");
    assert_eq!(strict["schema"], "st2.exec-generation.v1");
    let strict_pid = u32::try_from(strict["pid"].as_u64().unwrap()).unwrap();
    let strict_start = strict["startTimeTicks"].as_u64().unwrap();
    assert_ne!(
        strict_pid, legacy_pid,
        "candidate retained the predecessor PID"
    );
    assert_eq!(
        process_start_ticks(strict_pid),
        Some(strict_start),
        "strict record does not identify the live candidate Ding"
    );
    let cmdline = fs::read(format!("/proc/{strict_pid}/cmdline")).unwrap();
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        argv,
        [
            "/usr/bin/sh",
            "-c",
            "st2 ding --identity h.provider --root $ST_ROOT"
        ],
        "candidate launched a non-canonical derived Ding command"
    );

    let tasks = fixture
        .candidate()
        .arg("--catalog")
        .arg(&fixture.main_catalog)
        .args(["tasks", "--host", HOST, "--json"])
        .output()
        .unwrap();
    assert_success(&tasks, "strict post-migration inventory");
    let inventory: Value = serde_json::from_slice(&tasks.stdout).unwrap();
    assert_eq!(inventory["schema"], "st2.task-inventory.v2");
    assert_eq!(inventory["complete"], true);
    assert_eq!(inventory["errors"], serde_json::json!([]));
    let rows = inventory["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "unexpected desired-task census: {rows:#?}");
    let provider_row = row(rows, PROVIDER);
    assert_eq!(provider_row["kind"], "pty");
    assert_eq!(provider_row["lifecycle"], "adopt-only");
    assert_eq!(provider_row["runtime"]["state"], "running");
    assert_eq!(provider_row["runtime"]["pid"], provider_before.pid);
    assert_eq!(
        provider_row["runtime"]["createdAt"],
        provider_before.created_at
    );
    let ding_row = row(rows, DING);
    assert_eq!(ding_row["kind"], "exec");
    assert_eq!(ding_row["runtime"]["state"], "running");
    assert_eq!(ding_row["runtime"]["pid"], strict_pid);

    fixture.cleanup_exec_record();
    fixture.cleanup_pty(PROVIDER);
    fixture.cleanup_pty(SENTINEL);
    assert!(
        process_start_ticks(strict_pid).is_none(),
        "candidate Ding process residue remains after exact cleanup"
    );
    assert!(
        fixture.pty_generation(PROVIDER).is_none() && fixture.pty_generation(SENTINEL).is_none(),
        "PTY residue remains after exact isolated cleanup"
    );
}

fn write_agent(catalog: &Path, identity: &str, body: &str) {
    let path = catalog
        .join("agents")
        .join(HOST)
        .join(identity)
        .join("agent.kdl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n{body}}}\n"
        ),
    )
    .unwrap();
}

fn assert_migration_catalog_safe(catalog: &Path, expected_ids: &[&str]) -> Result<(), String> {
    let found = st2::discover(catalog);
    if !found.errors.is_empty() {
        return Err(format!("migration discovery errors: {:?}", found.errors));
    }
    let expected = expected_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    for spec in &found.specs {
        if !spec.desired_state.is_retired() {
            return Err(format!("migration agent {} is not retired", spec.identity));
        }
        if spec.tasks.len() != 1 {
            return Err(format!(
                "migration agent {} has {} tasks, expected one exact Ding",
                spec.identity,
                spec.tasks.len()
            ));
        }
        let task = &spec.tasks[0];
        let bus = spec.bus_id(HOST);
        let expected_id = format!("{bus}.ding");
        let expected_command = format!("st2 ding --identity {bus} --root $ST_ROOT");
        if task.kind != TaskKind::Exec
            || !task.derived
            || task.name != "ding"
            || task.id.as_deref() != Some(expected_id.as_str())
            || task.command.as_deref() != Some(expected_command.as_str())
            || task.argv.is_some()
        {
            return Err(format!(
                "migration task for {} is not the canonical derived Ding: {task:?}",
                spec.identity
            ));
        }
        actual.insert(task.id.as_deref().unwrap());
    }
    if actual != expected {
        return Err(format!(
            "migration Ding census {actual:?} != expected {expected:?}"
        ));
    }
    Ok(())
}

fn assert_adversarial_census_rejects_provider_task(fixture: &Fixture) {
    let adversarial = fixture._tmp.path().join("adversarial-migration-catalog");
    fs::create_dir_all(&adversarial).unwrap();
    fs::write(
        adversarial.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            fixture.pty_root.display().to_string()
        ),
    )
    .unwrap();
    write_agent(
        &adversarial,
        "provider",
        r#"
  retired #true
  ding
  pty "agent" {
    id "h.provider"
    argv "sleep" "120"
  }
"#,
    );
    let error = assert_migration_catalog_safe(&adversarial, &[DING]).unwrap_err();
    assert!(
        error.contains("has 2 tasks"),
        "provider-bearing migration catalog failed for an unexpected reason: {error}"
    );
}

fn declaration_digest(catalog: &Path) -> String {
    let mut hash = Sha256::new();
    for path in [
        catalog.join("catalog.kdl"),
        catalog.join("agents/h/provider/agent.kdl"),
    ] {
        let bytes = fs::read(path).unwrap();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

fn row<'a>(rows: &'a [Value], runtime_id: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["runtimeId"].as_str() == Some(runtime_id))
        .unwrap_or_else(|| panic!("inventory lacks runtime {runtime_id}: {rows:#?}"))
}

fn assert_success(output: &Output, action: &str) {
    assert!(
        output.status.success(),
        "{action} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn selected_predecessor() -> PathBuf {
    let pin = PREDECESSOR_PIN;
    validate_predecessor_pin(pin).unwrap_or_else(|error| panic!("{error}"));
    let path = std::env::var_os("ST2_PREDECESSOR_BIN")
        .map(PathBuf::from)
        .expect("selected predecessor Ding migration gate requires ST2_PREDECESSOR_BIN");
    verify_predecessor_digest(&path, pin.sha256).unwrap_or_else(|error| panic!("{error}"));

    let version = Command::new(&path)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "execute pinned predecessor {} from {}: {error}",
                path.display(),
                pin.derivation
            )
        });
    assert_success(&version, "read pinned predecessor version");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    let short_rev = &pin.source_rev[..pin.source_rev.len().min(7)];
    assert!(
        rendered.contains(short_rev) && !rendered.contains("dirty"),
        "pinned predecessor version does not identify clean source {} from {}: {rendered:?}",
        pin.source_rev,
        pin.derivation
    );
    path
}

fn validate_predecessor_pin(pin: PredecessorPin) -> Result<(), String> {
    if pin.source_rev.len() != 40
        || !pin
            .source_rev
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(
            "predecessor source revision must be 40 lowercase hexadecimal characters".into(),
        );
    }
    if pin.sha256.len() != 64
        || !pin
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("predecessor SHA-256 must be 64 lowercase hexadecimal characters".into());
    }
    if pin.derivation.is_empty() || !pin.derivation.contains(pin.source_rev) {
        return Err("predecessor derivation must include the exact source revision".into());
    }
    if pin.input_sha256.len() != 64
        || !pin
            .input_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(
            "predecessor derivation input SHA-256 must be 64 lowercase hexadecimal characters"
                .into(),
        );
    }
    Ok(())
}

fn verify_predecessor_digest(path: &Path, expected: &str) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!(
            "pinned predecessor {} is not a file",
            path.display()
        ));
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("read pinned predecessor {}: {error}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(format!(
            "pinned predecessor SHA-256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        ));
    }
    Ok(())
}

#[test]
fn predecessor_pin_is_complete() {
    validate_predecessor_pin(PREDECESSOR_PIN).unwrap();
}

#[test]
fn malformed_predecessor_pin_fails_closed() {
    let error = validate_predecessor_pin(PredecessorPin {
        source_rev: "unpinned",
        derivation: "github:compoundingtech/st2",
        input_sha256: "unpinned",
        sha256: "unpinned",
    })
    .unwrap_err();
    assert!(error.contains("source revision"), "{error}");
}

#[test]
fn predecessor_digest_mismatch_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("predecessor");
    fs::write(&path, b"not the retained predecessor").unwrap();
    let error = verify_predecessor_digest(&path, &"0".repeat(64)).unwrap_err();
    assert!(error.contains("SHA-256 mismatch"), "{error}");
}

fn have_pty() -> bool {
    Command::new("pty")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn joined_path(prefix: &Path, inherited: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut paths = vec![prefix.to_path_buf()];
    paths.extend(std::env::split_paths(inherited));
    std::env::join_paths(paths).unwrap()
}

fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn wait_process_gone(pid: u32, start: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_start_ticks(pid) != Some(start) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn poll_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition timed out after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
