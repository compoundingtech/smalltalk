use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;

use st2::materialize::{
    materialize_catalog, materialize_catalog_against, parse_plan, validate_agent,
};
use st2::{AgentSpec, discover};

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn task_selector_refusal_initializes_only_the_persistent_coordination_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(tmp.path())
        .args([
            "--host",
            "host",
            "--materialize-only",
            "--task",
            "host.missing.task",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let root_entries = fs::read_dir(tmp.path())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(root_entries.len(), 1);
    assert_eq!(root_entries[0].file_name(), ".st2");
    let control_entries = fs::read_dir(root_entries[0].path())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(control_entries.len(), 1);
    assert_eq!(control_entries[0].file_name(), "catalog-authoring.lock");
    let lock = fs::symlink_metadata(control_entries[0].path()).unwrap();
    assert!(lock.is_file() && !lock.file_type().is_symlink());
}

#[test]
fn task_selector_materializes_only_owning_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let owner = tmp.path().join("owner");
    let sibling = tmp.path().join("sibling");
    fs::create_dir_all(&owner).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(&owner, r#"    copy "_templates/owner" "OWNER.txt""#),
    );
    write(
        &catalog.join("agents/Silber/pty/agent.kdl"),
        agent_kdl(&sibling, r#"    copy "_templates/sibling" "SIBLING.txt""#)
            .replace("agent \"cos\"", "agent \"pty\"")
            .replace("Silber.cos", "Silber.pty"),
    );
    write(&catalog.join("_templates/owner"), "owner");
    write(&catalog.join("_templates/sibling"), "sibling");
    let out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(&catalog)
        .args([
            "--host",
            "Silber",
            "--materialize-only",
            "--task",
            "Silber.cos.agent",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(owner.join("OWNER.txt")).unwrap(),
        "owner"
    );
    assert!(!sibling.join("SIBLING.txt").exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("materialized 1 operation"));
}

#[test]
fn shared_workspace_conflicting_copy_targets_fail_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("shared-workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        &catalog.join("agents/Silber/worker/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/worker.md" ".st2/PERSONA.md""#,
        ),
    );
    write(
        &catalog.join("agents/Silber/orchestrator/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/orchestrator.md" ".st2/PERSONA.md""#,
        )
        .replace("agent \"cos\"", "agent \"orchestrator\"")
        .replace("Silber.cos", "Silber.orchestrator"),
    );
    write(&catalog.join("_templates/worker.md"), "worker\n");
    write(
        &catalog.join("_templates/orchestrator.md"),
        "orchestrator\n",
    );

    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].contains("conflicting render ownership"),
        "{:?}",
        report.errors
    );
    assert_eq!(report.failed_agents.len(), 2);
    assert!(
        !workspace.join(".st2/PERSONA.md").exists(),
        "a conflicting fleet must be rejected before either owner writes"
    );
}

#[test]
fn selected_owner_cannot_bypass_a_shared_workspace_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("shared-workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        &catalog.join("agents/Silber/worker/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/worker.md" ".st2/PERSONA.md""#,
        ),
    );
    write(
        &catalog.join("agents/Silber/orchestrator/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/orchestrator.md" ".st2/PERSONA.md""#,
        )
        .replace("agent \"cos\"", "agent \"orchestrator\"")
        .replace("Silber.cos", "Silber.orchestrator"),
    );
    write(&catalog.join("_templates/worker.md"), "worker\n");
    write(
        &catalog.join("_templates/orchestrator.md"),
        "orchestrator\n",
    );
    let found = discover(&catalog);
    let worker = found
        .specs
        .iter()
        .find(|spec| spec.identity == "cos")
        .unwrap();

    let report = materialize_catalog_against(
        &catalog,
        std::slice::from_ref(worker),
        &found.specs,
        "Silber",
    );

    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.failed_agents.contains("Silber.cos"));
    assert!(!workspace.join(".st2/PERSONA.md").exists());
}

#[test]
fn shared_workspace_byte_identical_claims_are_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("shared-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let render = r#"    copy "_templates/shared.md" ".st2/bus.md""#;
    write(
        &catalog.join("agents/Silber/a/agent.kdl"),
        agent_kdl(&workspace, render),
    );
    write(
        &catalog.join("agents/Silber/b/agent.kdl"),
        agent_kdl(&workspace, render)
            .replace("agent \"cos\"", "agent \"b\"")
            .replace("Silber.cos", "Silber.b"),
    );
    write(&catalog.join("_templates/shared.md"), "shared\n");
    let found = discover(&catalog);

    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.failed_agents.is_empty());
    assert_eq!(
        fs::read_to_string(workspace.join(".st2/bus.md")).unwrap(),
        "shared\n"
    );
}

#[test]
fn shared_workspace_mode_claims_must_match() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("shared-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let regular = r#"    copy "_templates/shared" "bin/tool""#;
    let executable = r#"    copy "_templates/shared" "bin/tool" executable=#true"#;
    write(
        &catalog.join("agents/Silber/a/agent.kdl"),
        agent_kdl(&workspace, regular),
    );
    write(
        &catalog.join("agents/Silber/b/agent.kdl"),
        agent_kdl(&workspace, executable)
            .replace("agent \"cos\"", "agent \"b\"")
            .replace("Silber.cos", "Silber.b"),
    );
    write(&catalog.join("_templates/shared"), "same\n");
    let found = discover(&catalog);

    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("conflicting render ownership"));
    assert_eq!(report.failed_agents.len(), 2);
    assert!(!workspace.join("bin/tool").exists());
}

#[test]
fn task_selector_ambiguous_refuses_without_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let kdl = |id: &str, ws: &Path, marker: &str| {
        format!(
            "agent \"{id}\" {{\n host \"Silber\"\n type \"service\"\n workspace \"{}\"\n pty \"agent\" {{\n  id \"dup\"\n  command \"true\"\n }}\n render {{ file \"MARKER.txt\" \"{marker}\" }}\n}}\n",
            ws.display()
        )
    };
    write(
        &catalog.join("agents/Silber/a/agent.kdl"),
        kdl("a", &a, "a"),
    );
    write(
        &catalog.join("agents/Silber/b/agent.kdl"),
        kdl("b", &b, "b"),
    );
    let found = st2::discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert_eq!(found.specs.len(), 2);
    assert!(
        found
            .specs
            .iter()
            .all(|s| s.tasks.len() == 1 && s.tasks[0].id.as_deref() == Some("dup"))
    );
    let out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(&catalog)
        .args(["--host", "Silber", "--materialize-only", "--task", "dup"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
    assert!(!a.join("MARKER.txt").exists() && !b.join("MARKER.txt").exists());
}

#[test]
fn task_selector_wrong_host_refuses_without_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let target = tmp.path().join("hetz-target");
    fs::create_dir_all(&target).unwrap();
    let text = format!(
        "agent \"remote\" {{\n host \"Hetz\"\n type \"service\"\n workspace \"{}\"\n pty \"agent\" {{ id \"hetz.task\" command \"true\" }}\n render {{ file \"MARKER.txt\" \"remote\" }}\n}}\n",
        target.display()
    );
    write(&catalog.join("agents/Hetz/remote/agent.kdl"), text);
    let found = st2::discover(&catalog);
    assert!(found.errors.is_empty());
    assert_eq!(found.specs[0].tasks[0].id.as_deref(), Some("hetz.task"));
    let out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog"])
        .arg(&catalog)
        .args([
            "--host",
            "Silber",
            "--materialize-only",
            "--task",
            "hetz.task",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not resolve"));
    assert!(!target.join("MARKER.txt").exists());
}

#[test]
fn task_selector_cli_modes_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec![
            "up",
            "--catalog",
            tmp.path().to_str().unwrap(),
            "--task",
            "host.a.x",
        ],
        vec![
            "up",
            "--catalog",
            tmp.path().to_str().unwrap(),
            "--materialize-only",
            "--task",
            "host.a.x",
            "--agent",
            "a",
        ],
        vec![
            "up",
            "--catalog",
            tmp.path().to_str().unwrap(),
            "--agent",
            "a",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).trim().is_empty());
    }
}

#[test]
fn task_selector_single_file_modes_refuse_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = tmp.path().join("spec.kdl");
    fs::write(&spec, "agent \"a\" { host \"Silber\" command \"true\" }\n").unwrap();
    let before = fs::read_to_string(&spec).unwrap();
    for extra in [
        ["--materialize-only", "--task", "Silber.a.agent"],
        ["--once", "--task", "Silber.a.agent"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["up", spec.to_str().unwrap()])
            .args(extra)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
    }
    assert_eq!(fs::read_to_string(&spec).unwrap(), before);
}

fn spec(catalog: &Path, identity: &str) -> AgentSpec {
    discover(catalog)
        .specs
        .into_iter()
        .find(|spec| spec.identity == identity)
        .unwrap()
}

fn agent_kdl(workspace: &Path, render: &str) -> String {
    format!(
        r##"agent "cos" {{
  host "Silber"
  workspace "{}"
  env {{ ST_AGENT "Silber.cos" }}
  command "true"
  ding
  render {{
{render}
  }}
}}
"##,
        workspace.display()
    )
}

fn init_git(workspace: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(workspace)
        .status()
        .unwrap();
    assert!(status.success());
}

fn track(workspace: &Path, path: &str) {
    let status = Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["add", "--", path])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn every_directive_materializes_in_order_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .arg(&workspace)
        .status()
        .unwrap();
    assert!(init.success());

    write(
        &catalog.join("_templates/AGENTS.md"),
        b"catalog-owned brief\n",
    );
    write(
        &workspace.join(".codex/hooks.json"),
        r#"{"keep":true,"nested":{"user":"value"}}"#,
    );
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r##"    copy "_templates/AGENTS.md" "AGENTS.md"
    file ".st2/env.txt" { content #"agent=$ST_AGENT root=$ST_ROOT"# }
    json-upsert ".codex/hooks.json" {
      content #"{"nested":{"st2":"value"},"hooks":{"Stop":[]}}"#
    }
    ensure-line ".codex/loader.md" "first"
    ensure-line ".codex/loader.md" "second"
    git-exclude "AGENTS.md" ".st2/"
"##,
        ),
    );

    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let plan = parse_plan(&found.specs[0]).unwrap();
    assert_eq!(plan.ops.len(), 7);

    for _ in 0..2 {
        let report = materialize_catalog(&catalog, &found.specs, "Silber");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    assert_eq!(
        fs::read(workspace.join("AGENTS.md")).unwrap(),
        b"catalog-owned brief\n"
    );
    let env = fs::read_to_string(workspace.join(".st2/env.txt")).unwrap();
    assert!(env.contains("agent=Silber.cos"));
    assert!(env.contains(&format!("root={}", catalog.display())));

    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(workspace.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(json["keep"], true);
    assert_eq!(json["nested"]["user"], "value");
    assert_eq!(json["nested"]["st2"], "value");
    assert_eq!(json["hooks"]["Stop"], serde_json::json!([]));

    assert_eq!(
        fs::read_to_string(workspace.join(".codex/loader.md")).unwrap(),
        "first\nsecond\n"
    );
    let exclude = fs::read_to_string(workspace.join(".git/info/exclude")).unwrap();
    assert_eq!(
        exclude.lines().filter(|line| *line == "AGENTS.md").count(),
        1
    );
    assert_eq!(exclude.lines().filter(|line| *line == ".st2/").count(), 1);
}

#[test]
fn ensure_line_preserves_contract_files_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        &workspace.join("CLAUDE.md"),
        "owner prose without a final newline",
    );
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    ensure-line "CLAUDE.md" "@.st2/CONTRACT.md"
    ensure-line "AGENTS.md" "@.st2/CONTRACT.md""#,
        ),
    );

    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);

    let first = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(first.is_clean(), "{:?}", first.errors);
    assert_eq!(first.materialized.len(), 2);
    assert_eq!(
        fs::read_to_string(workspace.join("CLAUDE.md")).unwrap(),
        "owner prose without a final newline\n@.st2/CONTRACT.md\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "@.st2/CONTRACT.md\n"
    );

    let second = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(second.is_clean(), "{:?}", second.errors);
    assert!(second.materialized.is_empty(), "{:?}", second.materialized);
}

#[test]
fn ensure_line_verifies_tracked_contract_files_without_rewriting() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_git(&workspace);
    for name in ["CLAUDE.md", "AGENTS.md"] {
        write(&workspace.join(name), "owner prose\n@.st2/CONTRACT.md\n");
        fs::set_permissions(workspace.join(name), fs::Permissions::from_mode(0o644)).unwrap();
        track(&workspace, name);
    }
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    ensure-line "CLAUDE.md" "@.st2/CONTRACT.md"
    ensure-line "AGENTS.md" "@.st2/CONTRACT.md""#,
        ),
    );

    let found = discover(&catalog);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert!(report.is_clean(), "{:?}", report.errors);
    assert!(report.materialized.is_empty(), "{:?}", report.materialized);
    for name in ["CLAUDE.md", "AGENTS.md"] {
        assert_eq!(
            fs::read_to_string(workspace.join(name)).unwrap(),
            "owner prose\n@.st2/CONTRACT.md\n"
        );
    }
}

#[test]
fn ensure_line_refuses_missing_contract_line_in_a_tracked_file_before_any_write() {
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        init_git(&workspace);
        write(&workspace.join(name), "owner prose\n");
        track(&workspace, name);
        write(
            &catalog.join("agents/Silber/cos/agent.kdl"),
            agent_kdl(
                &workspace,
                &format!(
                    "    ensure-line \"{name}\" \"@.st2/CONTRACT.md\"\n    file \"must-not-exist\" \"blocked\""
                ),
            ),
        );

        let found = discover(&catalog);
        let report = materialize_catalog(&catalog, &found.specs, "Silber");

        assert_eq!(report.errors.len(), 1, "{name}: {:?}", report.errors);
        assert!(
            report.errors[0].contains("generated materialization would change Git-tracked target")
                && report.errors[0].contains(name),
            "{name}: {:?}",
            report.errors
        );
        assert_eq!(
            fs::read_to_string(workspace.join(name)).unwrap(),
            "owner prose\n"
        );
        assert!(!workspace.join("must-not-exist").exists());
    }
}

#[test]
fn every_content_directive_refuses_to_change_a_tracked_target_before_any_write() {
    for (name, initial, directive, template) in [
        (
            "copy",
            "old\n",
            r#"copy "_templates/replacement" "target""#,
            Some("new\n"),
        ),
        ("file", "old\n", r#"file "target" "new\n""#, None),
        (
            "json-upsert",
            "{\"keep\":true}\n",
            r##"json-upsert "target" #"{"added":true}"#"##,
            None,
        ),
        (
            "ensure-line",
            "old\n",
            r#"ensure-line "target" "new""#,
            None,
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        init_git(&workspace);
        write(&workspace.join("target"), initial);
        track(&workspace, "target");
        if let Some(contents) = template {
            write(&catalog.join("_templates/replacement"), contents);
        }
        write(
            &catalog.join("agents/Silber/cos/agent.kdl"),
            agent_kdl(
                &workspace,
                &format!("    {directive}\n    file \"must-not-exist\" \"blocked with {name}\""),
            ),
        );

        let found = discover(&catalog);
        let report = materialize_catalog(&catalog, &found.specs, "Silber");
        assert_eq!(report.errors.len(), 1, "{name}: {:?}", report.errors);
        assert!(
            report.errors[0].contains("generated materialization would change Git-tracked target")
                && report.errors[0].contains("choose an untracked overlay target"),
            "{name}: {:?}",
            report.errors
        );
        assert_eq!(
            fs::read_to_string(workspace.join("target")).unwrap(),
            initial,
            "{name} changed the tracked target"
        );
        assert!(
            !workspace.join("must-not-exist").exists(),
            "{name} wrote a later operation after the gate failed"
        );
    }
}

#[test]
fn byte_identical_tracked_target_is_allowed_without_modification() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_git(&workspace);
    write(&workspace.join("AGENTS.md"), "same\n");
    fs::set_permissions(
        workspace.join("AGENTS.md"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    track(&workspace, "AGENTS.md");
    write(&catalog.join("_templates/AGENTS.md"), "same\n");
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(&workspace, r#"    copy "_templates/AGENTS.md" "AGENTS.md""#),
    );

    let found = discover(&catalog);
    let before = fs::metadata(workspace.join("AGENTS.md"))
        .unwrap()
        .modified()
        .unwrap();
    let report = materialize_catalog(&catalog, &found.specs, "Silber");
    let after = fs::metadata(workspace.join("AGENTS.md"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(report.is_clean(), "{:?}", report.errors);
    assert_eq!(before, after, "identical target was needlessly rewritten");
}

#[test]
fn mode_only_change_to_a_tracked_target_fails_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_git(&workspace);
    let target = workspace.join("tool");
    write(&target, "same\n");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    track(&workspace, "tool");
    write(&catalog.join("_templates/tool"), "same\n");
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/tool" "tool" executable=#true
    file "must-not-exist" "blocked""#,
        ),
    );

    let found = discover(&catalog);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("generated materialization would change Git-tracked target"));
    assert_eq!(
        fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(!workspace.join("must-not-exist").exists());
}

#[test]
fn executable_copy_repairs_a_byte_identical_destination_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(&catalog.join("_templates/wrapper"), "#!/bin/sh\nexit 0\n");
    let destination = workspace.join("bin/wrapper");
    write(&destination, "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "_templates/wrapper" "bin/wrapper" executable=#true"#,
        ),
    );

    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(report.is_clean(), "{:?}", report.errors);
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn regular_copy_repairs_a_byte_identical_executable_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(&catalog.join("_templates/plain"), "same\n");
    let destination = workspace.join("plain");
    write(&destination, "same\n");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o777)).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(&workspace, r#"    copy "_templates/plain" "plain""#),
    );

    let found = discover(&catalog);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");

    assert!(report.is_clean(), "{:?}", report.errors);
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn content_directives_create_exact_modes_under_a_restrictive_umask() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let template = catalog.join("_templates/tool");
    write(&template, "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&template, fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r##"    copy "_templates/tool" "bin/copied" executable=#true
    copy "_templates/tool" "plain-copy"
    file "bin/written" "#!/bin/sh\nexit 0\n" executable=#true
    json-upsert "bin/data.json" #"{"value":true}"# executable=#true
    ensure-line "bin/lines" "line" executable=#true
    file "plain" "plain"
"##,
        ),
    );

    let mut process = Command::new(env!("CARGO_BIN_EXE_st2"));
    process.args(["up", "--catalog"]).arg(&catalog).args([
        "--host",
        "Silber",
        "--materialize-only",
    ]);
    unsafe {
        process.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = process.output().unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for path in ["bin/copied", "bin/written", "bin/data.json", "bin/lines"] {
        assert_eq!(
            fs::metadata(workspace.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "{path}"
        );
    }
    for path in ["plain", "plain-copy"] {
        assert_eq!(
            fs::metadata(workspace.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "{path}"
        );
    }
    assert!(
        Command::new(workspace.join("bin/copied"))
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn inline_executable_content_is_exact_runnable_and_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#####"    file "bin/probe" executable=#true {
      content #"""
#!/bin/sh
printf 'hello:%s\n' "${ST_AGENT:-unknown}"

"""#
    }
    file "empty" ""
    file "quote-sequence" {
      content ##"""
before
"""# remains content

"""##
    }
"#####,
        ),
    );
    let expected_probe = b"#!/bin/sh\nprintf 'hello:%s\\n' \"${ST_AGENT:-unknown}\"\n".as_slice();
    let expected_quote = b"before\n\"\"\"# remains content\n".as_slice();

    let found = discover(&catalog);
    let first = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(first.is_clean(), "{:?}", first.errors);
    assert_eq!(first.materialized.len(), 3, "{:?}", first.materialized);

    let probe = workspace.join("bin/probe");
    assert_eq!(fs::read(&probe).unwrap(), expected_probe);
    assert_eq!(fs::read(workspace.join("empty")).unwrap(), b"");
    assert_eq!(
        fs::read(workspace.join("quote-sequence")).unwrap(),
        expected_quote
    );
    assert_eq!(
        fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let executed = Command::new(&probe)
        .env("ST_AGENT", "Silber.inline-proof")
        .output()
        .unwrap();
    assert!(
        executed.status.success(),
        "{}",
        String::from_utf8_lossy(&executed.stderr)
    );
    assert_eq!(executed.stdout, b"hello:Silber.inline-proof\n");

    let second = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(second.is_clean(), "{:?}", second.errors);
    assert!(
        second.materialized.is_empty(),
        "unchanged operations were reported as materialized: {:?}",
        second.materialized
    );
    assert_eq!(fs::read(probe).unwrap(), expected_probe);
}

#[test]
fn one_inline_target_repairs_mode_toggles_in_both_directions() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let declaration = |executable: bool| {
        agent_kdl(
            &workspace,
            &format!(
                "    file \"target\" \"same\" executable=#{}",
                if executable { "true" } else { "false" }
            ),
        )
    };

    for (executable, expected_mode) in [(true, 0o755), (false, 0o644), (true, 0o755)] {
        write(
            &catalog.join("agents/Silber/cos/agent.kdl"),
            declaration(executable),
        );
        let found = discover(&catalog);
        let report = materialize_catalog(&catalog, &found.specs, "Silber");
        assert!(report.is_clean(), "{:?}", report.errors);
        assert_eq!(report.materialized.len(), 1, "{:?}", report.materialized);
        assert_eq!(
            fs::metadata(workspace.join("target"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            expected_mode
        );
    }
}

#[test]
fn invalid_render_mode_properties_fail_validation() {
    for (directive, expected) in [
        (
            r#"copy "_templates/source" "target" executable="yes""#,
            "property 'executable' must be a boolean",
        ),
        (
            r#"file "target" "content" mode=#true"#,
            "unknown property 'mode'",
        ),
        (
            r#"git-exclude "target" executable=#true"#,
            "unknown property 'executable'",
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        write(&catalog.join("_templates/source"), "source\n");
        write(
            &catalog.join("agents/Silber/cos/agent.kdl"),
            agent_kdl(&workspace, &format!("    {directive}")),
        );
        let found = discover(&catalog);
        let error = parse_plan(&found.specs[0]).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn untracked_and_non_git_targets_remain_materializable() {
    for git in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        if git {
            init_git(&workspace);
        }
        write(&workspace.join("AGENTS.md"), "old\n");
        write(&catalog.join("_templates/AGENTS.md"), "new\n");
        write(
            &catalog.join("agents/Silber/cos/agent.kdl"),
            agent_kdl(&workspace, r#"    copy "_templates/AGENTS.md" "AGENTS.md""#),
        );

        let found = discover(&catalog);
        let report = materialize_catalog(&catalog, &found.specs, "Silber");
        assert!(report.is_clean(), "git={git}: {:?}", report.errors);
        assert_eq!(
            fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
            "new\n"
        );
    }
}

#[test]
fn suspended_declaration_does_not_materialize_workspace_content() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(&workspace.join("AGENTS.md"), "existing\n");
    write(&catalog.join("_templates/AGENTS.md"), "new\n");
    let declaration = agent_kdl(&workspace, r#"    copy "_templates/AGENTS.md" "AGENTS.md""#)
        .replace(
            "  host \"Silber\"\n",
            "  host \"Silber\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"\n",
        );
    write(&catalog.join("agents/Silber/cos/agent.kdl"), declaration);

    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");
    assert!(report.is_clean(), "{:?}", report.errors);
    assert_eq!(
        fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "existing\n"
    );
}

#[test]
fn running_render_validation_checks_source_presence_and_destination_safety() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        format!(
            r#"agent "cos" {{
  host "Silber"
  workspace "{}"
  command "true"
  render {{ copy "_templates/source" "safe" }}
}}
"#,
            workspace.display()
        ),
    );
    let found = discover(&catalog);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    let missing = validate_agent(&catalog, &found.specs[0], "Silber")
        .unwrap_err()
        .to_string();
    assert!(missing.contains("does not exist"), "{missing}");

    write(&catalog.join("_templates/source"), "source");
    let unsafe_declaration = fs::read_to_string(catalog.join("agents/Silber/cos/agent.kdl"))
        .unwrap()
        .replace(r#""safe""#, r#""../escape""#);
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        unsafe_declaration,
    );
    let unsafe_found = discover(&catalog);
    let error = validate_agent(&catalog, &unsafe_found.specs[0], "Silber")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("workspace-relative path without `..`"),
        "{error}"
    );
}

#[test]
fn missing_git_executable_fails_closed_before_workspace_write() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    let hooks_root = tmp.path().join("hooks");
    let empty_path = tmp.path().join("empty-path");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&empty_path).unwrap();
    init_git(&workspace);
    write(&workspace.join("AGENTS.md"), "old\n");
    track(&workspace, "AGENTS.md");
    write(&catalog.join("_templates/AGENTS.md"), "new\n");
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(&workspace, r#"    copy "_templates/AGENTS.md" "AGENTS.md""#),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("up")
        .arg(&catalog)
        .args(["--host", "Silber", "--materialize-only"])
        .env("ST_HOOKS", &hooks_root)
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git rev-parse for materialization safety"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "old\n"
    );
}

#[test]
fn a_gating_failure_blocks_only_that_agent_but_git_exclude_is_advisory() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bad_workspace = tmp.path().join("bad-workspace");
    let okay_workspace = tmp.path().join("okay-workspace");
    fs::create_dir_all(&bad_workspace).unwrap();
    fs::create_dir_all(&okay_workspace).unwrap();

    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(
            &bad_workspace,
            r#"    copy "_templates/missing.md" "AGENTS.md""#,
        ),
    );
    let okay = agent_kdl(
        &okay_workspace,
        r#"    file "AGENTS.md" "okay"
    git-exclude "AGENTS.md""#,
    )
    .replacen(r#"agent "cos""#, r#"agent "worker""#, 1)
    .replace("Silber.cos", "Silber.worker");
    write(&catalog.join("agents/Silber/worker/agent.kdl"), okay);

    let found = discover(&catalog);
    let report = materialize_catalog(&catalog, &found.specs, "Silber");
    assert_eq!(report.errors.len(), 1);
    assert!(report.failed_agents.contains("Silber.cos"));
    assert!(!report.failed_agents.contains("Silber.worker"));
    assert_eq!(
        fs::read_to_string(okay_workspace.join("AGENTS.md")).unwrap(),
        "okay"
    );
    assert_eq!(report.warnings.len(), 1, "non-Git exclude is advisory");
}

#[test]
fn source_can_be_relative_to_the_agent_file_for_blessed_catalog_compatibility() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(&catalog.join("_templates/brief.md"), "brief\n");
    write(
        &catalog.join("Silber/cos/agent.kdl"),
        agent_kdl(
            &workspace,
            r#"    copy "../../_templates/brief.md" "AGENTS.md""#,
        ),
    );
    let agent = spec(&catalog, "cos");
    let report = materialize_catalog(&catalog, &[agent], "Silber");
    assert!(report.is_clean(), "{:?}", report.errors);
    assert_eq!(
        fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "brief\n"
    );
}

#[test]
fn up_materialize_only_writes_the_overlay_without_needing_pty() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let workspace = tmp.path().join("workspace");
    let hooks_root = tmp.path().join("hooks");
    fs::create_dir_all(&workspace).unwrap();
    write(&catalog.join("_templates/brief.md"), "brief\n");
    write(
        &catalog.join("agents/Silber/cos/agent.kdl"),
        agent_kdl(&workspace, r#"    copy "_templates/brief.md" "AGENTS.md""#),
    );
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git is available on the test runner's PATH");
    symlink(git, bin.join("git")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up"])
        .arg(&catalog)
        .args(["--host", "Silber", "--materialize-only"])
        // Proves this path never tries the runtime's external pty backend.
        .env("ST_HOOKS", &hooks_root)
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "brief\n"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("materialized 1 operation"));
}
