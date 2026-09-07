#![cfg(unix)]
//! `st2 catalog archive` moves retired, runtime-free identities out of the live declaration plane.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A `pty list` shim whose JSON is the only runtime evidence the archive gate consults.
fn pty_shim(bin: &Path, sessions: &str) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join("pty");
    fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = list ]; then printf '{sessions}\\n'; fi\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A catalog, a `PATH` holding only the `pty` shim, and a `HOME`/`PTY_ROOT` outside the catalog.
fn fixture(temporary: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let catalog = temporary.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    (catalog, temporary.path().join("bin"))
}

fn st2(root: &Path, bin: &Path, args: &[&str]) -> Output {
    let home = root.parent().unwrap().join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args(args)
        .env("PATH", bin)
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("PTY_ROOT", home.join("pty"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}):\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// A retired seat that keeps every byte of its `resources/` payload (dotfiles#1535).
fn retired_seat(root: &Path, identity: &str, lifecycle: &str) {
    write(
        root,
        &format!("agents/h/{identity}/agent.kdl"),
        &format!("agent \"{identity}\" {{\n  host \"h\"\n  {lifecycle}\n  command \"true\"\n}}\n"),
    );
    write(
        root,
        &format!("agents/h/{identity}/resources/goal.md"),
        "# goal\n\nfinish the migration\n",
    );
    write(
        root,
        &format!("agents/h/{identity}/resources/inbox/1750000000000-abc123.md"),
        "from: h.boss\n\nlast word\n",
    );
}

const RETIRED: &str = "desired-state \"retired\" reason=\"Migration finished\"";

#[test]
fn archive_moves_a_retired_seat_out_of_discovery_with_its_resources_byte_identical() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);
    let goal = fs::read(root.join("agents/h/gone/resources/goal.md")).unwrap();
    let spec = fs::read(root.join("agents/h/gone/agent.kdl")).unwrap();

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "gone",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = json(&output);
    assert_eq!(receipt["schema"], "st2.catalog-archive.v1");
    assert_eq!(receipt["dryRun"], false);
    assert_eq!(receipt["archiveRoot"], ".st2/archive");
    let archived = receipt["archived"].as_array().unwrap();
    assert_eq!(archived.len(), 1, "{receipt:#}");
    assert_eq!(archived[0]["id"], "h.gone");
    assert_eq!(archived[0]["from"], "agents/h/gone");
    assert_eq!(archived[0]["to"], ".st2/archive/h/gone");
    assert_eq!(archived[0]["reason"], "Migration finished");
    assert!(archived[0]["archivedAt"].as_u64().unwrap() > 0);

    assert!(
        !root.join("agents/h/gone").exists(),
        "the identity must leave the live declaration plane"
    );
    assert_eq!(
        fs::read(root.join(".st2/archive/h/gone/agent.kdl")).unwrap(),
        spec
    );
    assert_eq!(
        fs::read(root.join(".st2/archive/h/gone/resources/goal.md")).unwrap(),
        goal
    );
    assert!(
        root.join(".st2/archive/h/gone/resources/inbox/1750000000000-abc123.md")
            .is_file(),
        "the whole identity directory moves, not just the spec"
    );

    let tombstone: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".st2/archive/h/gone.tombstone.json")).unwrap())
            .unwrap();
    assert_eq!(tombstone["schema"], "st2.catalog-archive-tombstone.v1");
    assert_eq!(tombstone["id"], "h.gone");
    assert_eq!(tombstone["archiveRoot"], ".st2/archive/h/gone");

    // Discovery must not see the archived spec at all: `.st2` is control space at any depth.
    let listed = st2(root, &bin, &["ls"]);
    assert!(listed.status.success());
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains("gone"),
        "archived specs stay undiscoverable:\n{}",
        String::from_utf8_lossy(&listed.stdout)
    );

    let graph = json(&st2(
        root,
        &bin,
        &["catalog", "graph", "--host", "h", "--json"],
    ));
    assert_eq!(graph["complete"], true, "{graph:#}");
    assert!(graph["agents"].as_array().unwrap().is_empty(), "{graph:#}");
    let rows = graph["archived"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{graph:#}");
    assert_eq!(rows[0]["id"], "h.gone");
    assert_eq!(rows[0]["reason"], "Migration finished");
    assert_eq!(rows[0]["archiveRoot"], ".st2/archive/h/gone");
    assert_eq!(rows[0]["archivedAt"], tombstone["archivedAt"]);
}

#[test]
fn archive_accepts_the_legacy_retirement_spelling() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "legacy", "retired #true");

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "legacy",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = json(&output);
    assert_eq!(receipt["archived"][0]["id"], "h.legacy");
    assert!(
        receipt["archived"][0]["reason"].is_null(),
        "a legacy declaration carries no rationale: {receipt:#}"
    );
    assert!(root.join(".st2/archive/h/legacy/agent.kdl").is_file());
}

#[test]
fn archive_refuses_a_running_or_suspended_declaration() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    write(
        root,
        "agents/h/live/agent.kdl",
        "agent \"live\" { host \"h\"; command \"true\" }\n",
    );
    write(
        root,
        "agents/h/paused/agent.kdl",
        "agent \"paused\" { host \"h\"; desired-state \"suspended\" reason=\"Waiting\"; command \"true\" }\n",
    );

    for identity in ["live", "paused"] {
        let output = st2(
            root,
            &bin,
            &[
                "catalog",
                "archive",
                "--identity",
                identity,
                "--host",
                "h",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "archiving a non-retired declaration must fail: {identity}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not-retired"), "{stderr}");
        assert!(
            root.join(format!("agents/h/{identity}/agent.kdl"))
                .is_file(),
            "a refused archive must not move anything"
        );
    }
    assert!(!root.join(".st2/archive").exists());
}

#[test]
fn archive_refuses_while_any_declared_task_record_survives() {
    for (sessions, label) in [
        ("[{\"name\":\"h.gone\",\"status\":\"running\"}]", "alive"),
        ("[{\"name\":\"h.gone\",\"status\":\"exited\"}]", "dead"),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let (catalog, bin) = fixture(&temporary);
        let root = catalog.as_path();
        pty_shim(&bin, sessions);
        retired_seat(root, "gone", RETIRED);

        let output = st2(
            root,
            &bin,
            &[
                "catalog",
                "archive",
                "--identity",
                "gone",
                "--host",
                "h",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "a surviving {label} record must block archival"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("runtime-record-present"), "{stderr}");
        assert!(stderr.contains(&format!("h.gone ({label})")), "{stderr}");
        assert!(root.join("agents/h/gone/agent.kdl").is_file());
    }
}

#[test]
fn archive_refuses_an_identity_another_declaration_still_names_as_supervisor() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "boss", RETIRED);
    write(
        root,
        "agents/h/worker/agent.kdl",
        "agent \"worker\" { host \"h\"; supervisor \"h.boss\"; command \"true\" }\n",
    );

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "boss",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("supervisor-referenced"), "{stderr}");
    assert!(stderr.contains("h.worker"), "{stderr}");
    assert!(root.join("agents/h/boss/agent.kdl").is_file());
}

#[test]
fn dry_run_reports_the_plan_and_changes_nothing() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "gone",
            "--host",
            "h",
            "--dry-run",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = json(&output);
    assert_eq!(receipt["dryRun"], true);
    assert_eq!(receipt["archived"][0]["id"], "h.gone");
    assert_eq!(receipt["archived"][0]["archivedAt"], 0);

    assert!(root.join("agents/h/gone/agent.kdl").is_file());
    assert!(
        !root.join(".st2/archive").exists(),
        "a dry run must not create the archive root"
    );
}

#[test]
fn all_retired_archives_every_eligible_seat_and_reports_the_rest() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[{\"name\":\"h.busy\",\"status\":\"running\"}]");
    retired_seat(root, "gone", RETIRED);
    retired_seat(root, "busy", RETIRED);
    write(
        root,
        "agents/h/live/agent.kdl",
        "agent \"live\" { host \"h\"; command \"true\" }\n",
    );

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--all-retired",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "an ineligible member must not fail the sweep:\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = json(&output);
    let archived = receipt["archived"].as_array().unwrap();
    assert_eq!(archived.len(), 1, "{receipt:#}");
    assert_eq!(archived[0]["id"], "h.gone");
    let refused = receipt["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{receipt:#}");
    assert_eq!(refused[0]["id"], "h.busy");
    assert_eq!(refused[0]["code"], "runtime-record-present");

    assert!(root.join(".st2/archive/h/gone/agent.kdl").is_file());
    assert!(root.join("agents/h/busy/agent.kdl").is_file());
    assert!(root.join("agents/h/live/agent.kdl").is_file());
}

#[test]
fn all_retired_archives_a_retired_supervisor_together_with_its_retired_dependent() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "boss", RETIRED);
    write(
        root,
        "agents/h/worker/agent.kdl",
        &format!(
            "agent \"worker\" {{ host \"h\"; supervisor \"h.boss\"; {RETIRED}; command \"true\" }}\n"
        ),
    );

    let output = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--all-retired",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = json(&output);
    assert!(
        receipt["refused"].as_array().unwrap().is_empty(),
        "a dependent that leaves in the same run is not a live reference: {receipt:#}"
    );
    assert_eq!(receipt["archived"].as_array().unwrap().len(), 2);
    assert!(!root.join("agents/h/boss").exists());
    assert!(!root.join("agents/h/worker").exists());
}

#[test]
fn unarchive_restores_the_identity_byte_identically_and_clears_its_tombstone() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);
    // A counted root keeps the whole-catalog validation green on both sides of the round trip; a
    // retired declaration never holds the root slot.
    write(
        root,
        "agents/h/keeper/agent.kdl",
        "agent \"keeper\" { host \"h\"; command \"true\" }\n",
    );
    let spec = fs::read(root.join("agents/h/gone/agent.kdl")).unwrap();
    let goal = fs::read(root.join("agents/h/gone/resources/goal.md")).unwrap();

    let archived = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "gone",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(archived.status.success());

    let restored = st2(
        root,
        &bin,
        &["catalog", "unarchive", "gone", "--host", "h", "--json"],
    );
    assert!(
        restored.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let receipt = json(&restored);
    assert_eq!(receipt["schema"], "st2.catalog-unarchive.v1");
    assert_eq!(receipt["id"], "h.gone");
    assert_eq!(receipt["to"], "agents/h/gone");

    assert_eq!(
        fs::read(root.join("agents/h/gone/agent.kdl")).unwrap(),
        spec
    );
    assert_eq!(
        fs::read(root.join("agents/h/gone/resources/goal.md")).unwrap(),
        goal
    );
    assert!(!root.join(".st2/archive/h/gone").exists());
    assert!(!root.join(".st2/archive/h/gone.tombstone.json").exists());

    let graph = json(&st2(
        root,
        &bin,
        &["catalog", "graph", "--host", "h", "--json"],
    ));
    assert_eq!(graph["complete"], true, "{graph:#}");
    assert!(
        graph["archived"].as_array().unwrap().is_empty(),
        "{graph:#}"
    );
    assert_eq!(graph["agents"][0]["id"], "h.gone");
}

#[test]
fn an_archived_directory_without_a_tombstone_makes_the_graph_incomplete() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);
    assert!(
        st2(
            root,
            &bin,
            &[
                "catalog",
                "archive",
                "--identity",
                "gone",
                "--host",
                "h",
                "--json"
            ],
        )
        .status
        .success()
    );
    fs::remove_file(root.join(".st2/archive/h/gone.tombstone.json")).unwrap();

    let output = st2(root, &bin, &["catalog", "graph", "--host", "h", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let graph = json(&output);
    assert_eq!(graph["complete"], false, "{graph:#}");
    assert!(
        graph["archived"].as_array().unwrap().is_empty(),
        "{graph:#}"
    );
    let issue = graph["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["code"] == "archive-unexplained")
        .unwrap_or_else(|| panic!("no archive issue in {graph:#}"));
    assert_eq!(issue["severity"], "error");
    assert_eq!(issue["path"], ".st2/archive/h/gone");
}

#[test]
fn archive_refuses_an_unknown_identity_and_a_second_archive_of_the_same_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);

    let unknown = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "absent",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown-identity"),
        "stderr:\n{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    assert!(
        st2(
            root,
            &bin,
            &[
                "catalog",
                "archive",
                "--identity",
                "gone",
                "--host",
                "h",
                "--json"
            ],
        )
        .status
        .success()
    );
    // The declaration is gone from the live plane, so the identity is simply unknown again — the
    // archived copy is never silently replaced.
    let again = st2(
        root,
        &bin,
        &[
            "catalog",
            "archive",
            "--identity",
            "gone",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(!again.status.success());
    assert!(root.join(".st2/archive/h/gone/agent.kdl").is_file());
}

#[test]
fn unarchive_refuses_to_overwrite_a_live_declaration() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let root = catalog.as_path();
    pty_shim(&bin, "[]");
    retired_seat(root, "gone", RETIRED);
    assert!(
        st2(
            root,
            &bin,
            &[
                "catalog",
                "archive",
                "--identity",
                "gone",
                "--host",
                "h",
                "--json"
            ],
        )
        .status
        .success()
    );
    write(
        root,
        "agents/h/gone/agent.kdl",
        "agent \"gone\" { host \"h\"; command \"true\" }\n",
    );

    let output = st2(
        root,
        &bin,
        &["catalog", "unarchive", "gone", "--host", "h", "--json"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("live catalog already holds"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join(".st2/archive/h/gone/agent.kdl").is_file());
}
