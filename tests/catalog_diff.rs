use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

fn st2() -> Command {
    Command::new(env!("CARGO_BIN_EXE_st2"))
}

fn write_catalog(catalog: &Path, spec: &str) {
    fs::create_dir_all(catalog.join("agents/host/worker")).unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty-root \"/tmp/st2-catalog-diff-test-pty\" }\n",
    )
    .unwrap();
    fs::write(catalog.join("agents/host/worker/agent.kdl"), spec).unwrap();
}

fn baseline_spec() -> &'static str {
    "agent \"worker\" {\n  host \"host\"\n  argv \"tool\" \"arg\"\n}\n"
}

fn snapshot(catalog: &Path, prepared: &Path) -> Value {
    let output = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            catalog.to_str().unwrap(),
            "--output",
            prepared.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "snapshot stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn diff(catalog: &Path, prepared: &Path, expected: &str) -> Output {
    st2()
        .args([
            "catalog",
            "diff",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--expect-sha256",
            expected,
            "--json",
        ])
        .output()
        .unwrap()
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let prepared = temp.path().join("prepared");
    write_catalog(&catalog, baseline_spec());
    let receipt = snapshot(&catalog, &prepared);
    let root = receipt["rootSha256"].as_str().unwrap().to_string();
    (temp, catalog, prepared, root)
}

fn parsed(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "diff stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn agent_fields(receipt: &Value) -> Vec<String> {
    receipt["agents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|agent| agent["fields"].as_array().unwrap())
        .map(|field| field["address"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn catalog_diff_cli_is_one_closed_read_only_mode() {
    let output = st2().args(["catalog", "diff", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in ["--prepared", "--expect-sha256", "--json"] {
        assert!(help.contains(flag), "catalog diff help omitted {flag}");
    }
    for forbidden in ["--resume", "--input-sha256", "--expect-absent"] {
        assert!(
            !help.contains(forbidden),
            "catalog diff exposed mutation mode {forbidden}"
        );
    }
}

#[test]
fn formatting_comments_and_explicit_defaults_are_a_normalized_agent_noop() {
    let (_temp, catalog, prepared, root) = fixture();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"// source formatting is not runtime meaning
agent "worker" {
  host "host"
  type "service"
  retired #false
  keep #false
  restart { attempts 3; interval "60s"; delay "0s"; mode "delay" }
  argv "tool" "arg"
  lifecycle "service"
}
"#,
    )
    .unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, &root));
    assert_eq!(receipt["schema"], "st2.catalog-diff.v1");
    assert!(receipt["agents"].as_array().unwrap().is_empty());
    assert_eq!(receipt["paths"].as_array().unwrap().len(), 1);
    assert_eq!(receipt["paths"][0]["path"], "agents/host/worker/agent.kdl");
}

#[test]
fn desired_state_and_reason_have_distinct_secret_safe_semantic_addresses() {
    let (_temp, catalog, prepared, root) = fixture();
    let reason = "Waiting for capacity";
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  desired-state \"suspended\" reason={reason:?}\n  argv \"tool\" \"arg\"\n}}\n"
        ),
    )
    .unwrap();

    let output = diff(&catalog, &prepared, &root);
    let receipt = parsed(&output);
    let fields = agent_fields(&receipt);
    assert!(fields.contains(&"/agents/host/worker/desired-state".to_string()));
    assert!(fields.contains(&"/agents/host/worker/desired-state/reason".to_string()));
    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(!rendered.contains(reason));
}

#[test]
fn declared_delivery_has_one_exact_semantic_address() {
    let (_temp, catalog, prepared, root) = fixture();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" {
  host "host"
  session-driver "codex"
  delivery-readiness "credential"
  deliver "app-server"
  session-driver "codex"
  delivery-readiness "credential"
  argv "tool" "arg"
}
"#,
    )
    .unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, &root));
    let fields = agent_fields(&receipt);
    assert_eq!(
        fields
            .iter()
            .filter(|field| field.as_str() == "/agents/host/worker/delivery")
            .count(),
        1
    );
}

#[test]
fn effective_task_id_and_cwd_defaults_normalize_to_explicit_values() {
    let (_temp, catalog, prepared, _root) = fixture();
    fs::write(
        catalog.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" {
  host "host"
  workspace "/tmp"
  pty "agent" { argv "tool" }
}
"#,
    )
    .unwrap();
    let replacement = prepared.parent().unwrap().join("effective-default-base");
    let snap = snapshot(&catalog, &replacement);
    let root = snap["rootSha256"].as_str().unwrap();
    fs::remove_dir_all(&prepared).unwrap();
    fs::rename(replacement, &prepared).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" {
  host "host"
  workspace "/tmp"
  pty "agent" { id "host.worker.agent"; cwd "/tmp"; argv "tool" }
}
"#,
    )
    .unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, root));
    assert!(receipt["agents"].as_array().unwrap().is_empty());
}

#[test]
fn exact_argv_env_and_task_kind_addresses_are_rfc6901_escaped_and_secret_safe() {
    let (_temp, catalog, prepared, _root) = fixture();
    let before_secret = "before-velvet-secret-7f3a";
    let after_secret = "after-copper-secret-9c4b";
    let current = format!(
        r#"agent "worker" {{
  host "host"
  pty "same" {{ id "host.worker.pty"; argv "/bin/old" "arg"; env {{ "A/B~C" "{before_secret}" }} }}
  exec "same" {{ id "host.worker.exec"; command "old-exec" }}
}}
"#,
    );
    fs::write(catalog.join("agents/host/worker/agent.kdl"), &current).unwrap();
    let replacement = temp_snapshot(&catalog, prepared.parent().unwrap().join("replacement"));
    let root = replacement["rootSha256"].as_str().unwrap().to_string();
    fs::remove_dir_all(&prepared).unwrap();
    let desired = format!(
        r#"agent "worker" {{
  host "host"
  pty "same" {{ id "host.worker.pty"; argv "/bin/new" "arg"; env {{ "A/B~C" "{after_secret}" }} }}
  exec "same" {{ id "host.worker.exec"; command "new-exec" }}
}}
"#,
    );
    fs::create_dir_all(prepared.join("agents/host/worker")).unwrap();
    fs::write(
        prepared.join("catalog.kdl"),
        fs::read(catalog.join("catalog.kdl")).unwrap(),
    )
    .unwrap();
    fs::write(prepared.join("agents/host/worker/agent.kdl"), &desired).unwrap();

    let output = diff(&catalog, &prepared, &root);
    let receipt = parsed(&output);
    let fields = agent_fields(&receipt);
    assert!(fields.contains(&"/agents/host/worker/tasks/pty/same/argv/0".to_string()));
    assert!(fields.contains(&"/agents/host/worker/tasks/pty/same/env/A~1B~0C".to_string()));
    assert!(fields.contains(&"/agents/host/worker/tasks/exec/same/command".to_string()));

    let rendered = String::from_utf8(output.stdout).unwrap();
    for secret in [before_secret, after_secret] {
        assert!(!rendered.contains(secret));
        let digest = format!("{:x}", Sha256::digest(secret.as_bytes()));
        assert!(!rendered.contains(&digest));
    }
    for complete_spec in [&current, &desired] {
        let digest = format!("{:x}", Sha256::digest(complete_spec.as_bytes()));
        assert!(!rendered.contains(&digest));
    }
    assert!(!rendered.contains("beforeSha256"));
    assert!(!rendered.contains("afterSha256"));
    assert!(rendered.contains(r#""state": "present""#));
    assert!(rendered.contains(r#""type": "string""#));
}

fn temp_snapshot(catalog: &Path, output: PathBuf) -> Value {
    snapshot(catalog, &output)
}

#[test]
fn render_template_static_and_workspace_paths_remain_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let prepared = temp.path().join("prepared");
    let bundle = catalog.join("agents/host/worker");
    fs::create_dir_all(bundle.join(".workspace")).unwrap();
    fs::create_dir_all(catalog.join("_templates")).unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty-root \"/tmp/ext-pty\" }\n",
    )
    .unwrap();
    fs::write(catalog.join("_templates/prompt.md"), "template-v1").unwrap();
    fs::write(bundle.join("render.txt"), "render-v1").unwrap();
    fs::write(bundle.join("static.txt"), "static-v1").unwrap();
    fs::write(
        bundle.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  workspace "$CATALOG/agents/host/worker/.workspace"
  argv "tool"
  render { copy "agents/host/worker/render.txt" "rendered.txt" }
}
"#,
    )
    .unwrap();
    let snap = snapshot(&catalog, &prepared);
    let root = snap["rootSha256"].as_str().unwrap();
    fs::write(prepared.join("_templates/prompt.md"), "template-v2").unwrap();
    fs::write(prepared.join("agents/host/worker/render.txt"), "render-v2").unwrap();
    fs::write(prepared.join("agents/host/worker/static.txt"), "static-v2").unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, root));
    let classes = receipt["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            (
                change["path"].as_str().unwrap(),
                change["after"]["class"].as_str().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(classes["_templates/prompt.md"], "template");
    assert_eq!(classes["agents/host/worker/render.txt"], "render");
    assert_eq!(classes["agents/host/worker/static.txt"], "static");
}

#[test]
fn inactive_render_inputs_do_not_require_ambient_source_existence_for_projection_or_diff() {
    for (name, lifecycle) in [
        ("suspended", r#"desired-state "suspended" reason="Waiting""#),
        (
            "canonical-retired",
            r#"desired-state "retired" reason="Finished""#,
        ),
        ("legacy-retired", "retired #true"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        let prepared = temp.path().join("prepared");
        fs::create_dir_all(catalog.join("agents/host/root")).unwrap();
        fs::create_dir_all(catalog.join(format!("agents/host/{name}/.workspace"))).unwrap();
        fs::write(
            catalog.join("catalog.kdl"),
            "catalog { pty-root \"/tmp/ext-pty\" }\n",
        )
        .unwrap();
        fs::write(
            catalog.join("agents/host/root/agent.kdl"),
            r#"agent "root" { host "host"; argv "true" }"#,
        )
        .unwrap();
        fs::write(
            catalog.join(format!("agents/host/{name}/agent.kdl")),
            format!(
                r#"agent "{name}" {{
  host "host"
  supervisor "host.root"
  {lifecycle}
  workspace ".workspace"
  render {{ copy "missing-source" "safe" }}
}}
"#
            ),
        )
        .unwrap();

        let receipt = snapshot(&catalog, &prepared);
        let root = receipt["rootSha256"].as_str().unwrap();
        let output = diff(&catalog, &prepared, root);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn classification_only_and_nested_agent_filename_changes_are_exact() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let prepared = temp.path().join("prepared");
    write_catalog(&catalog, baseline_spec());
    let bundle = catalog.join("agents/host/worker");
    fs::write(bundle.join("render.txt"), "same bytes").unwrap();
    fs::create_dir_all(bundle.join("docs")).unwrap();
    fs::write(bundle.join("docs/agent.kdl"), "note \"v1\"\n").unwrap();
    let snap = snapshot(&catalog, &prepared);
    let root = snap["rootSha256"].as_str().unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" {
  host "host"
  workspace "/tmp"
  argv "tool" "arg"
  render { copy "agents/host/worker/render.txt" "rendered.txt" }
}
"#,
    )
    .unwrap();
    fs::write(
        prepared.join("agents/host/worker/docs/agent.kdl"),
        "note \"v2\"\n",
    )
    .unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, root));
    let paths = receipt["paths"].as_array().unwrap();
    let render = paths
        .iter()
        .find(|change| change["path"] == "agents/host/worker/render.txt")
        .unwrap();
    assert_eq!(render["kind"], "modified");
    assert_eq!(render["before"]["class"], "static");
    assert_eq!(render["after"]["class"], "render");
    let nested = paths
        .iter()
        .find(|change| change["path"] == "agents/host/worker/docs/agent.kdl")
        .unwrap();
    assert_eq!(nested["before"]["class"], "static");
    assert_eq!(nested["after"]["class"], "static");
}

#[test]
fn explicit_multi_path_and_agent_sets_report_add_remove_and_modify() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let prepared = temp.path().join("prepared");
    write_catalog(&catalog, baseline_spec());
    fs::write(catalog.join("agents/host/worker/obsolete.txt"), "remove me").unwrap();
    fs::create_dir_all(catalog.join("agents/host/gone")).unwrap();
    fs::write(
        catalog.join("agents/host/gone/agent.kdl"),
        "agent \"gone\" { host \"host\"; supervisor \"host.worker\"; argv \"gone\" }\n",
    )
    .unwrap();
    let snap = snapshot(&catalog, &prepared);
    let root = snap["rootSha256"].as_str().unwrap();

    fs::remove_dir_all(prepared.join("agents/host/gone")).unwrap();
    fs::remove_file(prepared.join("agents/host/worker/obsolete.txt")).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; argv \"modified\" }\n",
    )
    .unwrap();
    fs::create_dir_all(prepared.join("agents/host/new")).unwrap();
    fs::write(
        prepared.join("agents/host/new/agent.kdl"),
        "agent \"new\" { host \"host\"; supervisor \"host.worker\"; argv \"new\" }\n",
    )
    .unwrap();
    fs::create_dir_all(prepared.join("_templates")).unwrap();
    fs::write(prepared.join("_templates/added.txt"), "added").unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, root));
    let path_kinds = receipt["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            (
                change["path"].as_str().unwrap(),
                change["kind"].as_str().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(path_kinds["_templates/added.txt"], "added");
    assert_eq!(path_kinds["agents/host/gone/agent.kdl"], "removed");
    assert_eq!(path_kinds["agents/host/worker/agent.kdl"], "modified");
    assert_eq!(path_kinds["agents/host/worker/obsolete.txt"], "removed");

    let agent_kinds = receipt["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| {
            (
                agent["identity"].as_str().unwrap(),
                agent["kind"].as_str().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(agent_kinds["gone"], "removed");
    assert_eq!(agent_kinds["new"], "added");
    assert_eq!(agent_kinds["worker"], "modified");

    let added_path = receipt["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "_templates/added.txt")
        .unwrap();
    assert!(added_path["before"].is_null());
    assert!(added_path["after"].is_object());
    let removed_path = receipt["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "agents/host/gone/agent.kdl")
        .unwrap();
    assert!(removed_path["before"].is_object());
    assert!(removed_path["after"].is_null());
}

#[test]
fn render_operations_are_normalized_separately_from_core_agent_fields() {
    let (_temp, catalog, prepared, _root) = fixture();
    fs::write(
        catalog.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" { host "host"; workspace "/tmp"; argv "tool"; render { file "a" "old" } }
"#,
    )
    .unwrap();
    let replacement = prepared.parent().unwrap().join("render-base");
    let snap = snapshot(&catalog, &replacement);
    let root = snap["rootSha256"].as_str().unwrap();
    fs::remove_dir_all(&prepared).unwrap();
    fs::rename(replacement, &prepared).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" { host "host"; workspace "/tmp"; argv "tool"; render { file "a" "new" } }
"#,
    )
    .unwrap();
    let receipt = parsed(&diff(&catalog, &prepared, root));
    assert!(agent_fields(&receipt).contains(&"/agents/host/worker/render/0/content".to_string()));
}

#[test]
fn render_executable_property_has_one_semantic_address() {
    let (_temp, catalog, prepared, _root) = fixture();
    fs::write(
        catalog.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" { host "host"; workspace "/tmp"; argv "tool"; render { file "a" "same" } }
"#,
    )
    .unwrap();
    let replacement = prepared.parent().unwrap().join("render-mode-base");
    let snap = snapshot(&catalog, &replacement);
    let root = snap["rootSha256"].as_str().unwrap();
    fs::remove_dir_all(&prepared).unwrap();
    fs::rename(replacement, &prepared).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        r#"agent "worker" { host "host"; workspace "/tmp"; argv "tool"; render { file "a" "same" executable=#true } }
"#,
    )
    .unwrap();

    let receipt = parsed(&diff(&catalog, &prepared, root));

    assert_eq!(
        agent_fields(&receipt),
        vec!["/agents/host/worker/render/0/executable"]
    );
}

#[test]
fn stale_root_malformed_ambiguous_and_special_prepared_inputs_fail_closed() {
    let (_temp, catalog, prepared, root) = fixture();
    let stale = diff(&catalog, &prepared, &"0".repeat(64));
    assert!(!stale.status.success());
    assert!(stale.stdout.is_empty());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("precondition failed"));

    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; argv \"x\" }\nagent \"other\" { host \"host\"; argv \"y\" }\n",
    )
    .unwrap();
    let ambiguous = diff(&catalog, &prepared, &root);
    assert!(!ambiguous.status.success());
    assert!(ambiguous.stdout.is_empty());

    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "not valid kdl (",
    )
    .unwrap();
    let malformed = diff(&catalog, &prepared, &root);
    assert!(!malformed.status.success());
    assert!(malformed.stdout.is_empty());

    fs::remove_file(prepared.join("agents/host/worker/agent.kdl")).unwrap();
    symlink(
        catalog.join("agents/host/worker/agent.kdl"),
        prepared.join("agents/host/worker/agent.kdl"),
    )
    .unwrap();
    let symlinked = diff(&catalog, &prepared, &root);
    assert!(!symlinked.status.success());
    assert!(symlinked.stdout.is_empty());
}

#[test]
fn prepared_hard_links_and_special_nodes_fail_before_a_receipt() {
    let (_temp, catalog, prepared, root) = fixture();
    fs::create_dir_all(prepared.join("_templates")).unwrap();
    fs::write(prepared.join("_templates/original"), "same inode").unwrap();
    fs::hard_link(
        prepared.join("_templates/original"),
        prepared.join("_templates/alias"),
    )
    .unwrap();
    let hard_linked = diff(&catalog, &prepared, &root);
    assert!(!hard_linked.status.success());
    assert!(hard_linked.stdout.is_empty());
    assert!(String::from_utf8_lossy(&hard_linked.stderr).contains("hard-linked"));

    fs::remove_dir_all(prepared.join("_templates")).unwrap();
    let fifo = prepared.join("agents/host/worker/special");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let special = diff(&catalog, &prepared, &root);
    assert!(!special.status.success());
    assert!(special.stdout.is_empty());
    assert!(String::from_utf8_lossy(&special.stderr).contains("special entry"));
}

#[test]
fn non_template_prepared_and_live_hard_links_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let prepared = temp.path().join("prepared");
    write_catalog(&catalog, baseline_spec());
    let snap = snapshot(&catalog, &prepared);
    let root = snap["rootSha256"].as_str().unwrap();

    let prepared_alias = temp.path().join("prepared-agent-alias.kdl");
    fs::rename(
        prepared.join("agents/host/worker/agent.kdl"),
        &prepared_alias,
    )
    .unwrap();
    fs::hard_link(
        &prepared_alias,
        prepared.join("agents/host/worker/agent.kdl"),
    )
    .unwrap();
    let prepared_result = diff(&catalog, &prepared, root);
    assert!(!prepared_result.status.success());
    assert!(prepared_result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&prepared_result.stderr).contains("hard-linked"));

    let live_alias = temp.path().join("live-agent-alias.kdl");
    fs::rename(catalog.join("agents/host/worker/agent.kdl"), &live_alias).unwrap();
    fs::hard_link(&live_alias, catalog.join("agents/host/worker/agent.kdl")).unwrap();
    fs::remove_file(prepared.join("agents/host/worker/agent.kdl")).unwrap();
    fs::copy(
        &prepared_alias,
        prepared.join("agents/host/worker/agent.kdl"),
    )
    .unwrap();
    let live_result = diff(&catalog, &prepared, root);
    assert!(!live_result.status.success());
    assert!(live_result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&live_result.stderr).contains("hard-linked"));
}

#[test]
fn every_incomplete_authoring_marker_fences_diff_without_partial_json() {
    let (_temp, catalog, prepared, root) = fixture();
    for marker in ["catalog-apply-incomplete", "catalog-generation-incomplete"] {
        let path = catalog.join(".st2").join(marker);
        fs::write(&path, "malformed-but-authoritative").unwrap();
        let output = diff(&catalog, &prepared, &root);
        assert!(
            !output.status.success(),
            "marker {marker} did not fence diff"
        );
        assert!(
            output.stdout.is_empty(),
            "marker {marker} emitted partial JSON"
        );
        fs::remove_file(path).unwrap();
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    kind: &'static str,
    mode: u32,
    bytes: Vec<u8>,
}

fn tree(root: &Path) -> BTreeMap<String, TreeEntry> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, TreeEntry>) {
        let mut entries = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if metadata.is_dir() {
                out.insert(
                    relative,
                    TreeEntry {
                        kind: "dir",
                        mode: metadata.mode() & 0o7777,
                        bytes: Vec::new(),
                    },
                );
                walk(root, &path, out);
            } else if metadata.is_file() {
                out.insert(
                    relative,
                    TreeEntry {
                        kind: "file",
                        mode: metadata.permissions().mode() & 0o7777,
                        bytes: fs::read(path).unwrap(),
                    },
                );
            } else {
                out.insert(
                    relative,
                    TreeEntry {
                        kind: "special",
                        mode: metadata.mode() & 0o7777,
                        bytes: Vec::new(),
                    },
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn success_is_provably_read_only_for_live_state_control_and_workspace_bytes() {
    let (_temp, catalog, prepared, root) = fixture();
    let bundle = catalog.join("agents/host/worker");
    fs::create_dir_all(bundle.join("resources/inbox")).unwrap();
    fs::write(bundle.join("resources/inbox/state-sentinel"), "state").unwrap();
    fs::create_dir_all(bundle.join(".workspace/nested")).unwrap();
    fs::write(bundle.join(".workspace/nested/work-sentinel"), "workspace").unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; argv \"changed\" }\n",
    )
    .unwrap();
    let before = tree(&catalog);
    let generation_before = fs::read(catalog.join(".st2/catalog-generation")).ok();

    let receipt = parsed(&diff(&catalog, &prepared, &root));
    assert!(!receipt["paths"].as_array().unwrap().is_empty());
    assert_eq!(tree(&catalog), before);
    assert_eq!(
        fs::read(catalog.join(".st2/catalog-generation")).ok(),
        generation_before
    );
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    assert!(!catalog.join(".st2/catalog-generation-incomplete").exists());
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

#[test]
fn retained_prepared_capture_cannot_be_redirected_by_a_source_swap() {
    let (temp, catalog, prepared, root) = fixture();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; argv \"captured\" }\n",
    )
    .unwrap();
    let expected = parsed(&diff(&catalog, &prepared, &root));
    let expected_after = expected["afterRootSha256"].as_str().unwrap().to_string();
    let ready = temp.path().join("capture-ready");
    let release = temp.path().join("capture-release");
    let mut child = st2();
    child
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("ST2_TEST_PREPARED_CAPTURE_PAUSE_AT", "source-opened")
        .env("ST2_TEST_PREPARED_CAPTURE_READY", &ready)
        .env("ST2_TEST_PREPARED_CAPTURE_RELEASE", &release)
        .args([
            "catalog",
            "diff",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--expect-sha256",
            &root,
            "--json",
        ]);
    let child = child.spawn().unwrap();
    wait_for(&ready);
    let retained = temp.path().join("retained-source");
    fs::rename(&prepared, &retained).unwrap();
    fs::create_dir_all(prepared.join("agents/host/worker")).unwrap();
    fs::write(
        prepared.join("catalog.kdl"),
        fs::read(catalog.join("catalog.kdl")).unwrap(),
    )
    .unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; argv \"replacement\" }\n",
    )
    .unwrap();
    fs::write(&release, "go").unwrap();
    let output = child.wait_with_output().unwrap();
    let receipt = parsed(&output);
    assert_eq!(receipt["afterRootSha256"], expected_after);
}
