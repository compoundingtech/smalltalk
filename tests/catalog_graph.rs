use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn st2(root: &Path, args: &[&str], pty_root: Option<&Path>) -> Output {
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_st2"));
    command
        .args(["--catalog", root.to_str().unwrap()])
        .args(args)
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT");
    if let Some(pty_root) = pty_root {
        command.env("PTY_ROOT", pty_root);
    }
    command.output().unwrap()
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

#[test]
fn graph_preserves_valid_rows_broken_sources_conflicts_and_incompleteness() {
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    fs::create_dir_all(root.join("agents/h/lead/.workspace")).unwrap();
    write(
        root,
        "agents/h/lead/agent.kdl",
        "agent \"lead\" {\n  host \"h\"\n  role \"worker\"\n  supervisor \"h.boss\"\n  workspace \"./.workspace\"\n  session-driver \"claude\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"\n  command \"true\"\n}\n",
    );
    write(
        root,
        "alternate/lead.kdl",
        "agent \"lead\" { host \"h\"; command \"true\" }\n",
    );
    write(
        root,
        "broken/agent.kdl",
        "agent \"broken\" { host \"h\"; pty { command \"true\" } }\n",
    );
    symlink(root.join("missing-declaration.kdl"), root.join("unreadable.kdl")).unwrap();

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert_eq!(output.status.code(), Some(1));
    let graph = json(&output);
    assert_eq!(graph["schema"], "st2.catalog-graph.v2");
    assert_eq!(graph["complete"], false);
    assert_eq!(graph["roots"]["ptyRoot"], root.join("pty").display().to_string());

    let rows = graph["agents"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "valid duplicate rows remain visible: {graph:#}");
    let lead = rows
        .iter()
        .find(|row| row["source"]["path"] == "agents/h/lead/agent.kdl")
        .unwrap();
    assert_eq!(lead["id"], "h.lead");
    assert_eq!(lead["supervisor"], "h.boss");
    assert_eq!(lead["persona"], "worker");
    assert_eq!(lead["workspace"], "./.workspace");
    assert_eq!(lead["resolvedWorkspace"], root.join("agents/h/lead/.workspace").display().to_string());
    assert_eq!(lead["effectiveSessionDriver"], "claude");
    assert!(lead["parentId"].is_null());
    assert!(lead["rootId"].is_null());
    assert!(lead["depth"].is_null());
    assert!(lead["ancestorIds"].is_null());
    assert_eq!(lead["desiredState"], "suspended");
    assert_eq!(lead["desiredStateReason"], "Waiting for capacity");
    assert_eq!(lead["source"]["identityProvenance"], "declaration");
    assert_eq!(lead["source"]["hostProvenance"], "declaration");
    assert!(lead["runtime"]["observedState"].is_null());

    let broken = graph["declarations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "broken/agent.kdl")
        .unwrap();
    assert_eq!(broken["status"], "invalid");
    assert_eq!(broken["agents"][0]["identity"], "broken");
    assert!(graph["conflicts"].as_array().unwrap().iter().any(|conflict| {
        conflict["kind"] == "duplicateIdentity" && conflict["identity"] == "h.lead"
    }));
    let issue_codes = graph["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["code"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(issue_codes.contains(&"dup-id"));
    assert!(issue_codes.contains(&"catalog-incomplete"));
    assert!(issue_codes.contains(&"unknown-task-kind"));
}

#[test]
fn graph_exposes_admitted_topology_and_delivery_readiness_facts() {
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/root/agent.kdl",
        r#"agent "root" { host "h"; command "true" }"#,
    );
    write(
        root,
        "agents/h/worker/agent.kdl",
        r#"agent "worker" {
  host "h"
  supervisor "h.root"
  argv "axe" "agent" "launch"
  session-driver "omp"
  delivery-readiness "anonymous" "zeta" "alpha" "zeta" harness="omp"
}"#,
    );

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph = json(&output);
    assert_eq!(graph["complete"], true, "{graph:#}");
    let rows = graph["agents"].as_array().unwrap();
    let root_row = rows.iter().find(|row| row["id"] == "h.root").unwrap();
    assert!(root_row["parentId"].is_null());
    assert_eq!(root_row["rootId"], "h.root");
    assert_eq!(root_row["depth"], 0);
    assert_eq!(root_row["ancestorIds"], serde_json::json!([]));

    let worker = rows.iter().find(|row| row["id"] == "h.worker").unwrap();
    assert_eq!(worker["effectiveSessionDriver"], "omp");
    assert_eq!(worker["parentId"], "h.root");
    assert_eq!(worker["rootId"], "h.root");
    assert_eq!(worker["depth"], 1);
    assert_eq!(worker["ancestorIds"], serde_json::json!(["h.root"]));
    assert_eq!(
        worker["deliveryReadiness"],
        serde_json::json!({
            "kind": "anonymous",
            "harness": "omp",
            "models": ["alpha", "zeta"]
        })
    );
}

#[test]
fn graph_ignores_retired_roots_and_folds_legacy_retirement_into_declarations() {
    // #402 regression fixture: one active root plus root-shaped retired declarations — legacy
    // `retired #true` and new-style — must leave the host with exactly one counted root, admit
    // the active topology, and expose the fold in the declaration view.
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/cos/agent.kdl",
        r#"agent "cos" { host "h"; command "true" }"#,
    );
    write(
        root,
        "agents/h/old-legacy/agent.kdl",
        r#"agent "old-legacy" { host "h"; retired #true; command "true" }"#,
    );
    write(
        root,
        "agents/h/old-explicit/agent.kdl",
        r#"agent "old-explicit" { host "h"; desired-state "retired" reason="Replaced by cos"; command "true" }"#,
    );
    write(
        root,
        "agents/h/worker/agent.kdl",
        r#"agent "worker" { host "h"; supervisor "h.cos"; command "true" }"#,
    );

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert_eq!(output.status.code(), Some(0));
    let graph = json(&output);
    assert_eq!(graph["complete"], true, "{graph:#}");
    assert!(
        !graph["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["code"] == "root-count"),
        "retired roots must not hold the root slot: {graph:#}"
    );

    let rows = graph["agents"].as_array().unwrap();
    let cos = rows.iter().find(|row| row["id"] == "h.cos").unwrap();
    assert_eq!(cos["rootId"], "h.cos");
    assert_eq!(cos["depth"], 0);
    let worker = rows.iter().find(|row| row["id"] == "h.worker").unwrap();
    assert_eq!(worker["rootId"], "h.cos");
    assert_eq!(worker["parentId"], "h.cos");

    let declarations = graph["declarations"].as_array().unwrap();
    let legacy = declarations
        .iter()
        .find(|row| row["path"] == "agents/h/old-legacy/agent.kdl")
        .unwrap();
    assert_eq!(legacy["agents"][0]["desiredState"], "retired");
    // A declaration that states no lifecycle still folds to null (→ running), not "retired".
    let active = declarations
        .iter()
        .find(|row| row["path"] == "agents/h/cos/agent.kdl")
        .unwrap();
    assert!(active["agents"][0]["desiredState"].is_null());
}


#[test]
fn graph_is_incomplete_when_an_active_worker_descends_from_a_retired_root() {
    // #405 review: one counted root satisfies root-count, but a worker supervised by a retired
    // tombstone forms a second, dead-headed tree — the envelope must say so. The worker's own
    // row still reports its declared chain fact while the graph is incomplete.
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/live/agent.kdl",
        r#"agent "live" { host "h"; command "true" }"#,
    );
    write(
        root,
        "agents/h/dead/agent.kdl",
        r#"agent "dead" { host "h"; retired #true; command "true" }"#,
    );
    write(
        root,
        "agents/h/worker/agent.kdl",
        r#"agent "worker" { host "h"; supervisor "h.dead"; command "true" }"#,
    );

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert_eq!(output.status.code(), Some(1));
    let graph = json(&output);
    assert_eq!(graph["complete"], false, "{graph:#}");
    assert!(
        graph["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["code"] == "retired-root"),
        "expected a retired-root issue: {graph:#}"
    );
    let worker = graph["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "h.worker")
        .unwrap();
    assert_eq!(worker["rootId"], "h.dead");
}

#[test]
fn graph_rejects_missing_cycle_depth_and_per_host_root_count() {
    let issue_codes = |root: &Path| {
        let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
        assert_eq!(output.status.code(), Some(1));
        let graph = json(&output);
        assert_eq!(graph["complete"], false);
        graph["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| issue["code"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    let missing = tempfile::tempdir().unwrap();
    write(
        missing.path(),
        "agents/h/root/agent.kdl",
        r#"agent "root" { host "h"; command "true" }"#,
    );
    write(
        missing.path(),
        "agents/h/worker/agent.kdl",
        r#"agent "worker" { host "h"; supervisor "h.absent"; command "true" }"#,
    );
    assert!(issue_codes(missing.path()).contains(&"supervisor-missing".to_owned()));

    let cycle = tempfile::tempdir().unwrap();
    write(
        cycle.path(),
        "agents/h/one/agent.kdl",
        r#"agent "one" { host "h"; supervisor "h.two"; command "true" }"#,
    );
    write(
        cycle.path(),
        "agents/h/two/agent.kdl",
        r#"agent "two" { host "h"; supervisor "h.one"; command "true" }"#,
    );
    let cycle_codes = issue_codes(cycle.path());
    assert!(cycle_codes.contains(&"supervisor-cycle".to_owned()));
    assert!(cycle_codes.contains(&"root-count".to_owned()));

    let deep = tempfile::tempdir().unwrap();
    for index in 0..=64 {
        let supervisor = if index == 0 {
            String::new()
        } else {
            format!(" supervisor \"h.node{}\";", index - 1)
        };
        write(
            deep.path(),
            &format!("agents/h/node{index}/agent.kdl"),
            &format!(
                "agent \"node{index}\" {{ host \"h\";{supervisor} command \"true\" }}\n"
            ),
        );
    }
    assert!(issue_codes(deep.path()).contains(&"supervisor-depth".to_owned()));

    let roots = tempfile::tempdir().unwrap();
    for identity in ["one", "two"] {
        write(
            roots.path(),
            &format!("agents/h/{identity}/agent.kdl"),
            &format!("agent \"{identity}\" {{ host \"h\"; command \"true\" }}\n"),
        );
    }
    let roots_output = st2(
        roots.path(),
        &["catalog", "graph", "--host", "h", "--json"],
        None,
    );
    assert_eq!(roots_output.status.code(), Some(1));
    let roots_graph = json(&roots_output);
    assert!(roots_graph["issues"].as_array().unwrap().iter().any(|issue| {
        issue["code"] == "root-count"
    }));
    for row in roots_graph["agents"].as_array().unwrap() {
        assert!(row["parentId"].is_null());
        assert!(row["rootId"].is_null());
        assert!(row["depth"].is_null());
        assert!(row["ancestorIds"].is_null());
    }
}
#[test]
fn candidate_overlay_reports_conflict_on_stdout_and_never_publishes() {
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "legacy/worker.kdl",
        "agent { host \"h\"; command \"true\" }\n",
    );
    let candidate_dir = tempfile::tempdir().unwrap();
    let candidate = candidate_dir.path().join("agent.kdl.candidate");
    fs::write(
        &candidate,
        "agent \"worker\" { host \"h\"; command \"true\" }\n",
    )
    .unwrap();

    let output = st2(
        root,
        &[
            "validate",
            "--host",
            "h",
            "--candidate",
            candidate.to_str().unwrap(),
            "--strict",
            "--json",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(1));
    let receipt = json(&output);
    assert_eq!(receipt["schema"], "st2.validate.v2");
    assert!(receipt["issues"].as_array().unwrap().iter().any(|issue| {
        issue["code"] == "dup-id" && issue["agent"] == "worker"
    }));
    assert!(!root.join("agents/h/worker/agent.kdl").exists());
}

#[test]
fn graph_reports_default_relative_absolute_and_ambient_pty_roots() {
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/worker/agent.kdl",
        "agent \"worker\" { host \"h\"; command \"true\" }\n",
    );

    let default = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(default.status.success(), "{}", String::from_utf8_lossy(&default.stderr));
    assert_eq!(json(&default)["roots"]["ptyRoot"], root.join("pty").display().to_string());

    write(root, "catalog.kdl", "catalog { pty-root \"shared\" }\n");
    let relative = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(relative.status.success());
    assert_eq!(json(&relative)["roots"]["ptyRoot"], root.join("shared").display().to_string());

    let absolute_root = root.join("absolute-registry");
    write(
        root,
        "catalog.kdl",
        &format!("catalog {{ pty-root \"{}\" }}\n", absolute_root.display()),
    );
    let absolute = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(absolute.status.success());
    assert_eq!(json(&absolute)["roots"]["ptyRoot"], absolute_root.display().to_string());

    let ambient_root = root.join("ambient-shared-registry");
    let ambient = st2(
        root,
        &["catalog", "graph", "--host", "h", "--json"],
        Some(&ambient_root),
    );
    assert!(ambient.status.success());
    assert_eq!(json(&ambient)["roots"]["ptyRoot"], ambient_root.display().to_string());
}

/// DELTA-003 (R24/R35): topology keys on the effective agent ID, so a chain declared with
/// explicit immutable IDs reports those IDs in `id`, `parentId`, `rootId`, and `ancestorIds`
/// even though `supervisor` still names the mutable address. `identity` keeps its meaning.
#[test]
fn graph_topology_keys_on_explicit_agent_ids() {
    const ROOT_ID: &str = "0193b8f2-7c31-7a4e-9f11-4c2d6b8a35e7";
    const WORKER_ID: &str = "0193b8f2-9d02-7b15-8c44-1a7f0e5d92b3";
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/root/agent.kdl",
        &format!(
            "agent \"root\" {{\n  id \"{ROOT_ID}\"\n  address \"chief\"\n  host \"h\"\n  command \"true\"\n}}\n"
        ),
    );
    write(
        root,
        "agents/h/worker/agent.kdl",
        &format!(
            "agent \"worker\" {{\n  id \"{WORKER_ID}\"\n  host \"h\"\n  supervisor \"h.root\"\n  command \"true\"\n}}\n"
        ),
    );

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph = json(&output);
    assert_eq!(graph["schema"], "st2.catalog-graph.v2");
    assert_eq!(graph["complete"], true, "{graph:#}");
    let rows = graph["agents"].as_array().unwrap();

    let root_row = rows.iter().find(|row| row["identity"] == "root").unwrap();
    assert_eq!(root_row["id"], ROOT_ID);
    assert!(root_row["parentId"].is_null());
    assert_eq!(root_row["rootId"], ROOT_ID);
    assert_eq!(root_row["depth"], 0);
    assert_eq!(root_row["ancestorIds"], serde_json::json!([]));
    // The address namespace is separate and mutable: `chief`, not the ID and not `identity`.
    assert_eq!(root_row["address"], "chief");
    assert_eq!(root_row["busAddress"], "h.chief");

    let worker = rows.iter().find(|row| row["identity"] == "worker").unwrap();
    assert_eq!(worker["id"], WORKER_ID);
    assert_eq!(worker["parentId"], ROOT_ID);
    assert_eq!(worker["rootId"], ROOT_ID);
    assert_eq!(worker["depth"], 1);
    assert_eq!(worker["ancestorIds"], serde_json::json!([ROOT_ID]));
    // No explicit address: the positional identity remains the effective legacy address.
    assert_eq!(worker["address"], "worker");
    assert_eq!(worker["busAddress"], "h.worker");
}

/// DELTA-003: the same chain without any explicit `id` keeps the legacy projection unchanged —
/// an unmigrated subject's effective ID *is* its bus identity — and the three new fields are
/// appended after `runtime`, never woven into the existing field order.
#[test]
fn graph_without_explicit_ids_keeps_its_legacy_wire_shape() {
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/root/agent.kdl",
        "agent \"root\" { host \"h\"; command \"true\" }\n",
    );
    write(
        root,
        "agents/h/worker/agent.kdl",
        "agent \"worker\" { host \"h\"; supervisor \"h.root\"; command \"true\" }\n",
    );

    let output = st2(root, &["catalog", "graph", "--host", "h", "--json"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout.clone()).unwrap();

    // Field order is part of the contract, and a parsed `serde_json::Value` sorts its keys — so
    // read the emitted key sequence straight out of the pretty-printed bytes.
    let agents_slice = {
        let start = raw.find("\"agents\": [").unwrap();
        let end = raw[start..].find("\"archived\":").unwrap() + start;
        &raw[start..end]
    };
    // Six spaces of indent is exactly one agent row's own keys; nested objects sit deeper and
    // `runtime` is an opaque `Value` whose key order is not this contract.
    let emitted = agents_slice
        .lines()
        .filter(|line| {
            line.len() - line.trim_start().len() == 6 && line.starts_with("      \"")
        })
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('"')?;
            let key = rest.split_once('"')?.0;
            rest[key.len() + 1..].starts_with(':').then_some(key)
        })
        .collect::<Vec<_>>();
    let row_fields = [
        "id",
        "identity",
        "host",
        "name",
        "description",
        "supervisor",
        "persona",
        "workspace",
        "resolvedWorkspace",
        "effectiveSessionDriver",
        "deliveryReadiness",
        "parentId",
        "rootId",
        "depth",
        "ancestorIds",
        "desiredState",
        "desiredStateReason",
        "source",
        "resources",
        "runtime",
        // Appended by DELTA-003, strictly after every pre-existing field.
        "address",
        "busAddress",
    ];
    assert_eq!(
        emitted,
        row_fields
            .into_iter()
            .chain(row_fields)
            .collect::<Vec<_>>(),
        "existing fields must keep their order and the new ones must come last:\n{raw}"
    );

    // Values are the legacy projection: the effective ID is the bus identity and the effective
    // address is the positional identity.
    let graph = json(&output);
    let rows = graph["agents"].as_array().unwrap();
    let root_row = &rows[0];
    assert_eq!(root_row["id"], "h.root");
    assert_eq!(root_row["identity"], "root");
    assert_eq!(root_row["rootId"], "h.root");
    assert!(root_row["parentId"].is_null());
    assert_eq!(root_row["ancestorIds"], serde_json::json!([]));
    assert_eq!(root_row["address"], "root");
    assert_eq!(root_row["busAddress"], "h.root");
    let worker = &rows[1];
    assert_eq!(worker["id"], "h.worker");
    assert_eq!(worker["parentId"], "h.root");
    assert_eq!(worker["rootId"], "h.root");
    assert_eq!(worker["depth"], 1);
    assert_eq!(worker["ancestorIds"], serde_json::json!(["h.root"]));
    assert_eq!(worker["address"], "worker");
    assert_eq!(worker["busAddress"], "h.worker");
}

/// DELTA-003 (R35): a duplicate explicit agent ID is a conflict, and every colliding row loses
/// its topology facts — even across hosts, where the legacy bus identities do not collide and
/// each host still has exactly one counted root.
#[test]
fn graph_reports_a_duplicate_explicit_agent_id_with_null_topology() {
    const SHARED_ID: &str = "0193b8f2-7c31-7a4e-9f11-4c2d6b8a35e7";
    let catalog = tempfile::tempdir().unwrap();
    let root = catalog.path();
    write(
        root,
        "agents/h/one/agent.kdl",
        &format!("agent \"one\" {{ id \"{SHARED_ID}\"; host \"h\"; command \"true\" }}\n"),
    );
    write(
        root,
        "agents/h2/two/agent.kdl",
        &format!("agent \"two\" {{ id \"{SHARED_ID}\"; host \"h2\"; command \"true\" }}\n"),
    );

    let graph = json(&st2(root, &["catalog", "graph", "--host", "h", "--json"], None));
    assert!(
        graph["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|conflict| conflict["kind"] == "duplicateIdentity"
                && conflict["identity"] == SHARED_ID),
        "{graph:#}"
    );
    let rows = graph["agents"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{graph:#}");
    for row in rows {
        assert_eq!(row["id"], SHARED_ID);
        assert!(row["parentId"].is_null(), "{row:#}");
        assert!(row["rootId"].is_null(), "{row:#}");
        assert!(row["depth"].is_null(), "{row:#}");
        assert!(row["ancestorIds"].is_null(), "{row:#}");
    }
    // The addresses do not collide: separate hosts, and each keeps its own legacy fallback.
    assert_eq!(rows[0]["busAddress"], "h.one");
    assert_eq!(rows[1]["busAddress"], "h2.two");
}
