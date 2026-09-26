use std::fs;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn declaration(identity: &str, supervisor: Option<&str>, managed_by: &str) -> String {
    let supervisor = supervisor
        .map(|value| format!("  supervisor {value:?}\n"))
        .unwrap_or_default();
    format!(
        "// unrelated comment\nagent {identity:?} {{\n  host \"h\"\n  meta {{ managed-by {managed_by:?}; keep \"unchanged\" }}\n{supervisor}  command \"sleep 300\"\n}}\n"
    )
}

fn run(root: &Path, command: &str, args: &[&str], actor: Option<&str>) -> std::process::Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_st2"));
    process
        .args(["--catalog", root.to_str().unwrap(), command])
        .args(args)
        .env_remove("ST_AGENT");
    if let Some(actor) = actor {
        process.env("ST_AGENT", actor);
    }
    process.output().unwrap()
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cli_sets_replaces_and_clears_fields_without_changing_identity_or_other_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let initial = declaration("worker", None, "catalog");
    write(root, "h/worker/agent.kdl", &initial);
    write(root, "h/worker/name", "obsolete sibling authority\n");

    for (command, field, value) in [
        ("rename", "name", "Build owner"),
        ("describe", "description", "Own build delivery"),
    ] {
        let output = run(
            root,
            command,
            &["h.worker", value, "--host", "h", "--json"],
            None,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["result"], "changed");
        assert_eq!(receipt["identity"], "h.worker");
        assert_eq!(receipt["field"], field);
        assert_eq!(receipt["value"], value);
    }

    let found = st2::discover(root);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let spec = &found.specs[0];
    assert_eq!(spec.identity, "worker");
    assert_eq!(spec.name.as_deref(), Some("Build owner"));
    assert_eq!(spec.description.as_deref(), Some("Own build delivery"));
    assert_eq!(
        fs::read_to_string(root.join("h/worker/name")).unwrap(),
        "obsolete sibling authority\n",
        "the retired sibling source is ignored, not rewritten or consulted"
    );

    let roster = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "agents",
            "--host",
            "h",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(roster.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&roster.stdout).unwrap();
    assert_eq!(rows[0]["identity"], "h.worker");
    assert_eq!(rows[0]["name"], "Build owner");
    assert_eq!(rows[0]["description"], "Own build delivery");

    let repeat = run(
        root,
        "rename",
        &["worker", "Build owner", "--host", "h", "--json"],
        None,
    );
    assert!(repeat.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&repeat.stdout).unwrap()["result"],
        "unchanged"
    );

    for command in ["rename", "describe"] {
        let clear = run(
            root,
            command,
            &["h.worker", "--clear", "--host", "h", "--json"],
            None,
        );
        assert!(clear.status.success());
    }
    assert_eq!(
        fs::read_to_string(root.join("h/worker/agent.kdl")).unwrap(),
        initial
    );
    let cleared_roster = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "agents",
            "--host",
            "h",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(cleared_roster.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&cleared_roster.stdout).unwrap();
    assert!(
        rows[0]["name"].is_null(),
        "retired sibling name file was consulted"
    );
    assert!(rows[0]["description"].is_null());
}

#[test]
fn cli_authors_positional_identity_without_an_existing_child_block() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/worker/agent.kdl", "agent \"worker\"\n");

    let output = run(
        root,
        "rename",
        &["h.worker", "Owner", "--host", "h", "--json"],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("h/worker/agent.kdl")).unwrap(),
        "agent \"worker\" { name \"Owner\" }\n"
    );
}

#[test]
fn cli_clears_a_presentation_field_from_compact_kdl() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        "agent \"worker\" { host \"h\"; name \"Owner\"; command \"x\" }\n",
    );

    let output = run(
        root,
        "rename",
        &["h.worker", "--clear", "--host", "h", "--json"],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let authored = fs::read_to_string(root.join("h/worker/agent.kdl")).unwrap();
    assert_eq!(authored, "agent \"worker\" { host \"h\"; command \"x\" }\n");
    let found = st2::discover(root);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert_eq!(found.specs[0].name, None);
}

#[test]
fn cli_preserves_declaration_mode_under_a_restrictive_umask() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let path = root.join("h/worker/agent.kdl");
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let mut process = Command::new(env!("CARGO_BIN_EXE_st2"));
    process
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "rename",
            "h.worker",
            "Owner",
            "--host",
            "h",
            "--json",
        ])
        .env_remove("ST_AGENT");
    unsafe {
        process.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = process.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o644);
}

#[test]
fn cli_rejects_unicode_line_and_paragraph_separators_for_both_fields() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let initial = declaration("worker", None, "catalog");
    write(root, "h/worker/agent.kdl", &initial);

    for command in ["rename", "describe"] {
        for separator in ['\u{2028}', '\u{2029}'] {
            let value = format!("left{separator}right");
            let output = run(
                root,
                command,
                &["h.worker", &value, "--host", "h", "--json"],
                None,
            );
            assert!(
                !output.status.success(),
                "accepted {command} U+{:04X}",
                separator as u32
            );
            let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(receipt["result"], "error");
            assert_eq!(receipt["code"], "invalid-presentation");
        }
    }

    assert_eq!(
        fs::read_to_string(root.join("h/worker/agent.kdl")).unwrap(),
        initial
    );
}

#[test]
fn cli_enforces_agent_authority_and_nix_and_format_refusals() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/root/agent.kdl",
        &declaration("root", None, "catalog"),
    );
    write(
        root,
        "h/child/agent.kdl",
        &declaration("child", Some("root"), "catalog"),
    );
    write(
        root,
        "h/sibling/agent.kdl",
        &declaration("sibling", Some("root"), "catalog"),
    );
    write(
        root,
        "h/nix/agent.kdl",
        &declaration("nix", Some("root"), "nix"),
    );
    write(
        root,
        "h/json/agent.json",
        r#"{"identity":"json","host":"h","command":"sleep 300"}"#,
    );

    for (command, target, actor, code) in [
        (
            "rename",
            "h.sibling",
            Some("h.child"),
            "presentation-not-authorized",
        ),
        (
            "describe",
            "h.nix",
            Some("h.root"),
            "nix-managed-declaration",
        ),
        ("describe", "h.json", None, "unsupported-declaration-format"),
    ] {
        let output = run(
            root,
            command,
            &[target, "refused", "--host", "h", "--json"],
            actor,
        );
        assert!(!output.status.success());
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["result"], "error");
        assert_eq!(receipt["code"], code);
    }

    let allowed = run(
        root,
        "describe",
        &["h.child", "Owned by root", "--host", "h", "--json"],
        Some("h.root"),
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

#[test]
fn concurrent_cli_writers_serialize_without_losing_either_field() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    fs::create_dir(root.join(".st2")).unwrap();
    let lock_path = root.join(".st2/catalog-authoring.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let inode = fs::metadata(&lock_path).unwrap().ino();

    let spawn = |command: &str, value: &str| {
        Command::new(env!("CARGO_BIN_EXE_st2"))
            .args([
                "--catalog",
                root.to_str().unwrap(),
                command,
                "h.worker",
                value,
                "--host",
                "h",
                "--json",
            ])
            .env_remove("ST_AGENT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let rename = spawn("rename", "Build owner");
    let describe = spawn("describe", "Own build delivery");
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);

    for output in [
        rename.wait_with_output().unwrap(),
        describe.wait_with_output().unwrap(),
    ] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let found = st2::discover(root);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert_eq!(found.specs[0].name.as_deref(), Some("Build owner"));
    assert_eq!(
        found.specs[0].description.as_deref(),
        Some("Own build delivery")
    );
    assert_eq!(fs::metadata(lock_path).unwrap().ino(), inode);
}

#[test]
fn presentation_and_publication_contend_on_the_same_persistent_catalog_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let declaration_path = root.join("agents/h/worker/agent.kdl");
    write(
        &root,
        "agents/h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    fs::create_dir(root.join(".st2")).unwrap();
    let lock_path = root.join(".st2/catalog-authoring.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let inode = fs::metadata(&lock_path).unwrap().ino();

    let rename_attempt = temporary.path().join("rename-lock-attempt");
    let mut rename = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "rename",
            "h.worker",
            "Build owner",
            "--host",
            "h",
            "--json",
        ])
        .env("ST2_TEST_CATALOG_LOCK_ATTEMPT", &rename_attempt)
        .env_remove("ST_AGENT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_path(&rename_attempt);
    assert!(rename.try_wait().unwrap().is_none());
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    let output = rename.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let candidate = temporary.path().join("candidate.kdl");
    fs::copy(&declaration_path, &candidate).unwrap();
    let digest = st2::agent_publish::digest_source(st2::agent_publish::PublishSource::Spec(
        candidate.clone(),
    ))
    .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let publish_attempt = temporary.path().join("publish-lock-attempt");
    let mut publish = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "agent",
            "publish",
            "--catalog",
            root.to_str().unwrap(),
            "--spec",
            candidate.to_str().unwrap(),
            "--input-sha256",
            &digest.sha256,
            "--expect-absent",
            "--json",
        ])
        .env("ST2_TEST_CATALOG_LOCK_ATTEMPT", &publish_attempt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_path(&publish_attempt);
    assert!(publish.try_wait().unwrap().is_none());
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    let output = publish.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(fs::metadata(lock_path).unwrap().ino(), inode);
    assert!(
        fs::read_to_string(declaration_path)
            .unwrap()
            .contains("name \"Build owner\"")
    );
}

#[test]
fn catalog_lock_refuses_a_symlinked_control_directory() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    write(
        &root,
        "h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    symlink(&outside, root.join(".st2")).unwrap();

    let output = run(
        &root,
        "rename",
        &["h.worker", "Owner", "--host", "h", "--json"],
        None,
    );
    assert!(!output.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["code"], "catalog-lock-failed");
    assert!(!outside.join("catalog-authoring.lock").exists());
}

#[test]
fn presentation_crash_stages_only_in_the_control_plane() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let original = declaration("worker", None, "catalog");
    write(&root, "agents/h/worker/agent.kdl", &original);
    let crashed = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "rename",
            "h.worker",
            "Build owner",
            "--host",
            "h",
        ])
        .env_remove("ST_AGENT")
        .env("ST2_TEST_AGENT_AUTHOR_CRASH_AFTER_TEMP", "1")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    assert_eq!(
        crashed.status.signal(),
        Some(libc::SIGABRT),
        "status {:?}, stderr {}",
        crashed.status,
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("agents/h/worker/agent.kdl")).unwrap(),
        original
    );
    assert!(!root.join(".st2/catalog-generation").exists());
    assert!(
        fs::read_dir(root.join("agents/h/worker"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("agent-presentation-"))
    );
    let retry = run(
        &root,
        "rename",
        &["h.worker", "Build owner", "--host", "h", "--json"],
        None,
    );
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join(".st2/catalog-generation")).unwrap(),
        "1\n"
    );
}

#[test]
fn presentation_post_commit_generation_failure_is_fenced_and_recovered() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    write(
        &root,
        "agents/h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    let failed = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "rename",
            "h.worker",
            "Build owner",
            "--host",
            "h",
        ])
        .env_remove("ST_AGENT")
        .env("ST2_TEST_GENERATION_FAIL_AFTER_COMMIT", "1")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        fs::read_to_string(root.join("agents/h/worker/agent.kdl"))
            .unwrap()
            .contains("name \"Build owner\"")
    );
    assert!(root.join(".st2/catalog-generation-incomplete").is_file());
    assert!(!root.join(".st2/catalog-generation").exists());
    let shared = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["agents", "--catalog", root.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(!shared.status.success());

    let recovered = run(
        &root,
        "rename",
        &["h.worker", "Build owner", "--host", "h", "--json"],
        None,
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join(".st2/catalog-generation")).unwrap(),
        "1\n"
    );
    assert!(!root.join(".st2/catalog-generation-incomplete").exists());
}

#[test]
fn control_directory_swap_cannot_redirect_presentation_staging() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    write(
        &root,
        "agents/h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    let ready = temporary.path().join("ready");
    let release = temporary.path().join("release");
    let writer = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            root.to_str().unwrap(),
            "rename",
            "h.worker",
            "Build owner",
            "--host",
            "h",
        ])
        .env_remove("ST_AGENT")
        .env("ST2_TEST_CATALOG_LOCK_HELD_READY", &ready)
        .env("ST2_TEST_CATALOG_LOCK_HELD_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_path(&ready);
    let retained = temporary.path().join("retained-control");
    fs::rename(root.join(".st2"), &retained).unwrap();
    let outside = temporary.path().join("outside-control");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(".st2")).unwrap();
    fs::write(&release, "").unwrap();
    let writer = writer.wait_with_output().unwrap();
    assert!(
        writer.status.success(),
        "{}",
        String::from_utf8_lossy(&writer.stderr)
    );
    assert!(
        fs::read_to_string(root.join("agents/h/worker/agent.kdl"))
            .unwrap()
            .contains("name \"Build owner\"")
    );
    assert!(outside.read_dir().unwrap().next().is_none());
    assert!(retained.join("catalog-generation").is_file());
}

#[test]
fn catalog_lock_refuses_a_symlinked_lock_file() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let outside = temporary.path().join("outside-lock");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join(".st2")).unwrap();
    fs::write(&outside, "unchanged").unwrap();
    write(
        &root,
        "h/worker/agent.kdl",
        &declaration("worker", None, "catalog"),
    );
    symlink(&outside, root.join(".st2/catalog-authoring.lock")).unwrap();

    let output = run(
        &root,
        "describe",
        &["h.worker", "Owner", "--host", "h", "--json"],
        None,
    );
    assert!(!output.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["code"], "catalog-lock-failed");
    assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
}
