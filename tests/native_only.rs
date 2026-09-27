use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn clean_path() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    symlink(env!("CARGO_BIN_EXE_st2"), bin.path().join("st2")).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| {
            path.is_file()
                && fs::read(path).is_ok_and(|bytes| {
                    !bytes
                        .windows(b"st2-recorder-wrapper".len())
                        .any(|window| window == b"st2-recorder-wrapper")
                })
        })
        .expect("the native authoring guide requires git on PATH");
    symlink(git, bin.path().join("git")).unwrap();
    executable(
        &bin.path().join("pty"),
        "#!/bin/sh\nif [ \"$1\" = \"list\" ]; then printf '[]\\n'; fi\nexit 0\n",
    );
    executable(&bin.path().join("codex"), "#!/bin/sh\nexit 0\n");
    executable(&bin.path().join("claude"), "#!/bin/sh\nexit 0\n");
    bin
}

fn clean_st2(bin: &Path, state: &Path, hooks: &Path) -> Command {
    let mut command = Command::new(bin.join("st2"));
    command
        .env("PATH", bin)
        .env("XDG_STATE_HOME", state)
        .env("ST_HOOKS", hooks)
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .env_remove("ST_AGENT");
    command
}

fn native_catalog(root: &Path, workspace: &Path) {
    let declaration = root.join("agents/h/worker/agent.kdl");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::write(
        declaration,
        format!(
            "agent \"worker\" {{\n  host \"h\"\n  role \"worker\"\n  workspace \"{}\"\n  \
             env {{ ST_AGENT \"h.worker\" }}\n  command \"true\"\n  ding\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
}

#[test]
fn clean_path_executes_the_maintained_native_authoring_guide() {
    let bin = clean_path();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let hooks = tmp.path().join("hooks");
    let catalog = tmp.path().join("catalog");
    let codex_bundle = tmp.path().join("codex-bundle");
    let claude_bundle = tmp.path().join("claude-bundle");
    let codex_workspace = tmp.path().join("codex-workspace");
    let claude_workspace = tmp.path().join("claude-workspace");
    fs::create_dir_all(&catalog).unwrap();
    fs::create_dir_all(codex_bundle.join("assets")).unwrap();
    fs::create_dir_all(claude_bundle.join("assets")).unwrap();
    fs::create_dir_all(&codex_workspace).unwrap();
    fs::create_dir_all(&claude_workspace).unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let codex = fs::read_to_string(manifest.join("examples/native/agent-codex.kdl"))
        .unwrap()
        .replace("<identity>", "codex")
        .replace("<host>", "clean")
        .replace("<workspace>", &codex_workspace.display().to_string())
        .replace("<boot prompt>", "boot");
    let claude = fs::read_to_string(manifest.join("examples/native/agent-claude.kdl"))
        .unwrap()
        .replace("<identity>", "claude")
        .replace("<host>", "clean")
        .replace("<workspace>", &claude_workspace.display().to_string())
        .replace("<boot prompt>", "boot");
    let bus = fs::read_to_string(manifest.join("templates/bus.st2.md")).unwrap();
    fs::write(codex_bundle.join("agent.kdl"), codex.as_bytes()).unwrap();
    fs::write(claude_bundle.join("agent.kdl"), claude.as_bytes()).unwrap();
    fs::write(
        codex_bundle.join("assets/AGENTS.md"),
        format!("# Clean Codex agent\n\n---\n\n{bus}"),
    )
    .unwrap();
    fs::write(
        claude_bundle.join("assets/PERSONA.md"),
        "# Clean Claude persona\n",
    )
    .unwrap();
    fs::write(claude_bundle.join("assets/bus.st2.md"), &bus).unwrap();

    for workspace in [&codex_workspace, &claude_workspace] {
        let git = Command::new(bin.path().join("git"))
            .args(["init", "-q"])
            .arg(workspace)
            .env("PATH", bin.path())
            .status()
            .unwrap();
        assert!(git.success());
    }

    for bundle in [&codex_bundle, &claude_bundle] {
        let digest = clean_st2(bin.path(), &state, &hooks)
            .args(["agent", "digest", "--bundle"])
            .arg(bundle)
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            digest.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&digest.stdout),
            String::from_utf8_lossy(&digest.stderr)
        );
        let digest: serde_json::Value = serde_json::from_slice(&digest.stdout).unwrap();
        let publish = clean_st2(bin.path(), &state, &hooks)
            .args(["agent", "publish", "--catalog"])
            .arg(&catalog)
            .arg("--bundle")
            .arg(bundle)
            .arg("--input-sha256")
            .arg(digest["sha256"].as_str().unwrap())
            .args(["--expect-absent", "--json"])
            .output()
            .unwrap();
        assert!(
            publish.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&publish.stdout),
            String::from_utf8_lossy(&publish.stderr)
        );
    }

    let install = clean_st2(bin.path(), &state, &hooks)
        .args(["hooks", "install"])
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let verify = clean_st2(bin.path(), &state, &hooks)
        .args(["hooks", "verify"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );

    let validate = clean_st2(bin.path(), &state, &hooks)
        .arg("validate")
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&validate.stdout),
        String::from_utf8_lossy(&validate.stderr)
    );

    let materialize = || {
        clean_st2(bin.path(), &state, &hooks)
            .arg("up")
            .arg("--catalog")
            .arg(&catalog)
            .args(["--host", "clean", "--materialize-only"])
            .output()
            .unwrap()
    };
    let first = materialize();
    assert!(
        first.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let codex_settings = fs::read(codex_workspace.join(".codex/hooks.json")).unwrap();
    let claude_settings = fs::read(claude_workspace.join(".claude/settings.local.json")).unwrap();
    let second = materialize();
    assert!(
        second.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read(codex_workspace.join(".codex/hooks.json")).unwrap(),
        codex_settings
    );
    assert_eq!(
        fs::read(claude_workspace.join(".claude/settings.local.json")).unwrap(),
        claude_settings
    );

    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(hooks.join("current.json")).unwrap()).unwrap();
    let selected = hooks.join(receipt["directory"].as_str().unwrap());
    for (settings, names) in [
        (
            serde_json::from_slice::<serde_json::Value>(&codex_settings).unwrap(),
            vec![
                "codex-session-start.sh",
                "codex-pre-compact.sh",
                "codex-stop.sh",
            ],
        ),
        (
            serde_json::from_slice::<serde_json::Value>(&claude_settings).unwrap(),
            vec![
                "claude-session-start.sh",
                "claude-pre-compact.sh",
                "claude-stop-failure.sh",
            ],
        ),
    ] {
        let encoded = settings.to_string();
        for name in names {
            assert!(
                encoded.contains(&selected.join(name).display().to_string()),
                "{name} did not resolve into {}:\n{encoded}",
                selected.display()
            );
        }
    }

    let codex_agents = fs::read_to_string(codex_workspace.join("AGENTS.md")).unwrap();
    assert!(codex_agents.starts_with("# Clean Codex agent\n\n---\n\n"));
    assert!(codex_agents.contains("Set `busy` immediately before actively executing"));
    assert_eq!(
        fs::read_to_string(claude_workspace.join(".st2/PERSONA.md")).unwrap(),
        "# Clean Claude persona\n"
    );
    let claude_bus = fs::read_to_string(claude_workspace.join(".st2/bus.md")).unwrap();
    assert!(claude_bus.contains("Set `busy` immediately before actively executing"));
    for workspace in [&codex_workspace, &claude_workspace] {
        let status = Command::new(bin.path().join("git"))
            .arg("-C")
            .arg(workspace)
            .args(["status", "--porcelain"])
            .env("PATH", bin.path())
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "{} was left dirty:\n{}",
            workspace.display(),
            String::from_utf8_lossy(&status.stdout)
        );
    }
}

#[test]
fn clean_path_supports_help_validate_env_and_doctor() {
    let bin = clean_path();
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    native_catalog(&catalog, &workspace);

    for absent in [["c", "onvoy"].concat(), ["s", "t"].concat()] {
        assert!(!bin.path().join(absent).exists());
    }

    let help = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--help")
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for command in [
        "validate",
        "message",
        "ding",
        "hooks",
        "agent",
        "completions",
    ] {
        assert!(help.contains(command), "missing {command} in help:\n{help}");
    }
    for removed in [
        ["a", "dd"].concat(),
        ["com", "pile"].concat(),
        ["ren", "der"].concat(),
        ["re", "move"].concat(),
        ["build", "-agent"].concat(),
        ["render", "-agent"].concat(),
    ] {
        let listed = help.lines().any(|line| {
            let line = line.trim_start();
            line == removed || line.starts_with(&format!("{removed} "))
        });
        assert!(!listed, "{removed} remained in help:\n{help}");
    }
    assert!(!Path::new("completions").exists());
    assert!(!Path::new("man").exists());

    let validate = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("validate")
        .arg("--catalog")
        .arg(&catalog)
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stdout)
    );

    let env = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("env")
        .arg("--catalog")
        .arg(&catalog)
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(env.status.success());
    let env = String::from_utf8_lossy(&env.stdout);
    let canonical = catalog.canonicalize().unwrap();
    assert!(env.contains(&format!("export ST_ROOT={}", canonical.display())));
    assert!(env.contains(&format!("export PTY_ROOT={}/pty", canonical.display())));

    let doctor_catalog = tmp.path().join("doctor-catalog");
    let doctor_agent = doctor_catalog.join("agents/h/missing/agent.kdl");
    fs::create_dir_all(doctor_agent.parent().unwrap()).unwrap();
    fs::write(&doctor_agent, "agent \"missing\" { host \"h\" }\n").unwrap();
    let mut owner = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    fs::write(
        doctor_catalog.join(".st2.h.lock"),
        format!("{}\n", owner.id()),
    )
    .unwrap();
    let missing = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(&doctor_catalog)
        .args(["--host", "h"])
        .env("PATH", bin.path())
        .env("PTY_ROOT", tmp.path().join("pty"))
        .output()
        .unwrap();
    assert!(
        !missing.status.success(),
        "a missing presence file passed doctor:\n{}",
        String::from_utf8_lossy(&missing.stdout)
    );
    assert!(
        String::from_utf8_lossy(&missing.stdout).contains("h.missing presence missing"),
        "stdout:\n{}",
        String::from_utf8_lossy(&missing.stdout)
    );

    fs::write(doctor_agent.parent().unwrap().join("status"), "offline\n").unwrap();
    let offline = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(&doctor_catalog)
        .args(["--host", "h"])
        .env("PATH", bin.path())
        .env("PTY_ROOT", tmp.path().join("pty"))
        .output()
        .unwrap();
    let _ = owner.kill();
    let _ = owner.wait();
    assert!(
        offline.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&offline.stdout),
        String::from_utf8_lossy(&offline.stderr)
    );
    assert!(
        String::from_utf8_lossy(&offline.stdout)
            .contains("h.missing presence fresh (is `offline`)"),
        "stdout:\n{}",
        String::from_utf8_lossy(&offline.stdout)
    );
}

#[test]
fn tracked_product_surface_contains_only_native_names() {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let paths: Vec<PathBuf> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| PathBuf::from(String::from_utf8(raw.to_vec()).unwrap()))
        .collect();

    let forbidden = [
        ["con", "voy"].concat(),
        [".con", "voy"].concat(),
        ["render", "-agent"].concat(),
        ["build", "-agent"].concat(),
        ["st2 ", "render"].concat(),
        ["st2 ", "add"].concat(),
        ["st2 ", "remove"].concat(),
    ];
    let mut violations = Vec::new();
    for path in paths {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let lower = contents.to_ascii_lowercase();
        for needle in &forbidden {
            if lower.contains(needle) {
                violations.push(format!("{} contains {needle:?}", path.display()));
            }
        }
        for needle in [
            ["st2 ", "compile "].concat(),
            ["`st2 ", "compile`"].concat(),
        ] {
            if lower.contains(&needle) {
                violations.push(format!("{} contains {needle:?}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "retired product surface returned:\n{}",
        violations.join("\n")
    );
}
