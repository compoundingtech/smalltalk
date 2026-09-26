mod support;

use std::ffi::CString;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use support::{CommandOwnedChildGroupExt as _, OwnedChildGroup};

fn st2() -> Command {
    Command::new(env!("CARGO_BIN_EXE_st2"))
}

const DEMO_WASM_SRC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/crates/agent-spec/tests/fixtures/demo_resolver.wasm"
);

fn agent(identity: &str, retired: bool) -> String {
    format!("agent \"{identity}\" {{\n  host \"host\"\n  retired #{retired}\n  argv \"true\"\n}}\n")
}

fn agent_dir(catalog: &Path, identity: &str) -> PathBuf {
    catalog.join("agents/host").join(identity)
}

fn write_agent(catalog: &Path, identity: &str, retired: bool) {
    write_agent_for_host(catalog, "host", identity, retired);
}

fn write_agent_for_host(catalog: &Path, host: &str, identity: &str, retired: bool) {
    let dir = catalog.join("agents").join(host).join(identity);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("agent.kdl"),
        format!(
            "agent \"{identity}\" {{\n  host \"{host}\"\n  retired #{retired}\n  argv \"true\"\n}}\n"
        ),
    )
    .unwrap();
}

fn write_invalid_agent(catalog: &Path, identity: &str) {
    write_agent(catalog, identity, false);
    let path = agent_dir(catalog, identity).join("agent.kdl");
    let invalid = fs::read_to_string(&path).unwrap().replace(
        "  retired #false\n",
        "  desired-state \"running\" because=\"unsupported\"\n",
    );
    fs::write(path, invalid).unwrap();
}

fn ensure_external_pty_config(catalog: &Path) {
    let config = catalog.join("catalog.kdl");
    if !config.exists() {
        fs::write(
            config,
            "catalog { pty-root \"/tmp/st2-catalog-transaction-test-pty\" }\n",
        )
        .unwrap();
    }
}

fn profile_catalog_config(profiles: &[(&str, &str)]) -> String {
    let mut config = "catalog { pty-root \"/tmp/st2-catalog-transaction-test-pty\" }\n".to_string();
    for (scheme, module) in profiles {
        config.push_str(&format!("profile {scheme:?} {{ wasm {module:?} }}\n"));
    }
    config
}

fn snapshot(catalog: &Path, output: &Path) -> Value {
    ensure_external_pty_config(catalog);
    let result = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            catalog.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "snapshot stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn raw_snapshot(catalog: &Path, output: &Path) -> Output {
    ensure_external_pty_config(catalog);
    st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            catalog.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--raw-preimage",
            "--json",
        ])
        .output()
        .unwrap()
}

fn prepared_root_sha256(catalog: &Path, prepared: &Path) -> String {
    st2::catalog_transaction::digest_prepared(catalog, prepared)
        .map(|digest| digest.root_sha256)
        // Negative apply fixtures deliberately cannot be digested. A well-formed mismatch lets
        // the apply command itself reach and report the more specific structural rejection.
        .unwrap_or_else(|_| "0".repeat(64))
}

fn apply(catalog: &Path, prepared: &Path, expected: &str) -> Output {
    let input_sha256 = prepared_root_sha256(catalog, prepared);
    st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            expected,
            "--json",
        ])
        .output()
        .unwrap()
}

fn raw_apply(catalog: &Path, prepared: &Path, expected: &str) -> Output {
    let input_sha256 = prepared_root_sha256(catalog, prepared);
    st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            expected,
            "--raw-preimage",
            "--json",
        ])
        .output()
        .unwrap()
}

fn bootstrap(catalog: &Path, prepared: &Path, input_sha256: &str) -> Output {
    st2()
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            input_sha256,
            "--json",
        ])
        .output()
        .unwrap()
}

fn resume(catalog: &Path) -> Output {
    st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--resume",
            "--json",
        ])
        .output()
        .unwrap()
}

#[test]
fn catalog_apply_cli_exposes_exactly_the_three_closed_modes() {
    let help = st2().args(["catalog", "apply", "--help"]).output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for flag in [
        "--prepared",
        "--input-sha256",
        "--expect-sha256",
        "--raw-preimage",
        "--resume",
        "--json",
    ] {
        assert!(help.contains(flag), "catalog apply help omitted {flag}");
    }
    assert!(!help.contains("--expect-absent"));

    for args in [
        vec!["catalog", "apply"],
        vec!["catalog", "apply", "--prepared", "/tmp/prepared"],
        vec![
            "catalog",
            "apply",
            "--prepared",
            "/tmp/prepared",
            "--expect-sha256",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ],
        vec![
            "catalog",
            "apply",
            "--resume",
            "--prepared",
            "/tmp/prepared",
        ],
        vec![
            "catalog",
            "apply",
            "--resume",
            "--expect-sha256",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ],
        vec!["catalog", "apply", "--resume", "--raw-preimage"],
    ] {
        let rejected = st2().args(args).output().unwrap();
        assert!(
            !rejected.status.success(),
            "catalog apply accepted an incomplete or ambiguous mode"
        );
    }
}

#[test]
fn apply_rejects_a_prepared_projection_mutated_after_digest_without_live_mutation() {
    for raw_preimage in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        if raw_preimage {
            write_invalid_agent(&catalog, "worker");
        } else {
            write_agent(&catalog, "worker", false);
        }
        ensure_external_pty_config(&catalog);
        let prepared = temp.path().join("prepared");
        let before = if raw_preimage {
            let output = raw_snapshot(&catalog, &temp.path().join("raw-preimage"));
            assert!(output.status.success());
            let before = serde_json::from_slice::<Value>(&output.stdout).unwrap();
            let desired = temp.path().join("desired");
            write_agent(&desired, "worker", false);
            snapshot(&desired, &prepared);
            before
        } else {
            snapshot(&catalog, &prepared)
        };
        let approved = prepared_root_sha256(&catalog, &prepared);
        fs::write(
            prepared.join("agents/host/worker/agent.kdl"),
            agent("worker", true),
        )
        .unwrap();

        let mut command = st2();
        command.args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &approved,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
            "--json",
        ]);
        if raw_preimage {
            command.arg("--raw-preimage");
        }
        let rejected = command.output().unwrap();
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("catalog apply input precondition failed"),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
        let live = fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap();
        if raw_preimage {
            assert!(live.contains("because=\"unsupported\""));
        } else {
            assert_eq!(live, agent("worker", false));
        }
    }
}

#[test]
fn catalog_digest_is_a_typed_read_only_projection_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let control_before = fs::read_dir(catalog.join(".st2"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();

    let digest = st2()
        .args([
            "catalog",
            "digest",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        digest.status.success(),
        "{}",
        String::from_utf8_lossy(&digest.stderr)
    );
    let digest: Value = serde_json::from_slice(&digest.stdout).unwrap();
    assert_eq!(digest["schema"], "st2.catalog-digest.v1");
    assert_eq!(
        digest["catalog"],
        catalog.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        digest["prepared"],
        prepared.canonicalize().unwrap().to_str().unwrap()
    );

    let diff = st2()
        .args([
            "catalog",
            "diff",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff: Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_eq!(digest["rootSha256"], diff["afterRootSha256"]);
    let control_after = fs::read_dir(catalog.join(".st2"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(control_after, control_before);
}

#[test]
fn apply_input_fence_survives_source_free_crash_resume() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let digest = st2()
        .args([
            "catalog",
            "digest",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_WORKSPACE", ".workspace")
        .output()
        .unwrap();
    assert!(digest.status.success());
    let digest: Value = serde_json::from_slice(&digest.stdout).unwrap();
    let input_sha256 = digest["rootSha256"].as_str().unwrap().to_string();
    let interrupted = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "marker-created")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let marker_path = catalog.join(".st2/catalog-apply-incomplete");
    let mut marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
    assert_eq!(marker["preparedRootSha256"], input_sha256);
    assert!(
        marker
            .as_object_mut()
            .unwrap()
            .remove("originalProfileModules")
            .is_some(),
        "the current writer must include the field before emulating an older v1 marker"
    );
    fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
    fs::remove_dir_all(&prepared).unwrap();

    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        agent("worker", true)
    );
}

#[test]
fn catalog_bootstrap_cli_is_create_only_and_source_bound() {
    let help = st2()
        .args(["catalog", "bootstrap", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for flag in ["--prepared", "--input-sha256", "--json"] {
        assert!(help.contains(flag), "catalog bootstrap help omitted {flag}");
    }
    for forbidden in ["--resume", "--expect-sha256", "--expect-absent"] {
        assert!(
            !help.contains(forbidden),
            "catalog bootstrap exposed redundant or unsafe mode {forbidden}"
        );
    }
}

#[test]
fn bootstrap_atomically_publishes_an_absent_catalog_and_replays_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");

    let first = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["schema"], "st2.catalog-bootstrap.v1");
    assert_eq!(first["status"], "created");
    assert_eq!(first["rootSha256"], captured["rootSha256"]);
    assert_eq!(
        fs::read_to_string(target.join(".st2/catalog-generation")).unwrap(),
        "1\n"
    );
    assert!(target.join(".st2/catalog-authoring.lock").is_file());
    let verified = snapshot(&target, &temp.path().join("verified"));
    assert_eq!(verified["rootSha256"], captured["rootSha256"]);

    fs::create_dir_all(agent_dir(&target, "worker").join("resources/inbox")).unwrap();
    fs::write(
        agent_dir(&target, "worker").join("resources/inbox/message.md"),
        "state survives replay",
    )
    .unwrap();
    let replay = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["status"], "unchanged");
    assert_eq!(
        fs::read_to_string(agent_dir(&target, "worker").join("resources/inbox/message.md"))
            .unwrap(),
        "state survives replay"
    );
}

#[test]
fn bootstrap_rejects_a_different_existing_catalog_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "desired", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let incumbent_source = temp.path().join("incumbent-source");
    write_agent(&incumbent_source, "incumbent", false);
    let incumbent_prepared = temp.path().join("incumbent-prepared");
    let incumbent = snapshot(&incumbent_source, &incumbent_prepared);
    let target = temp.path().join("target");
    let created = bootstrap(
        &target,
        &incumbent_prepared,
        incumbent["rootSha256"].as_str().unwrap(),
    );
    assert!(created.status.success());
    let before = fs::read_to_string(agent_dir(&target, "incumbent").join("agent.kdl")).unwrap();

    let rejected = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("already exists with root sha256"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent_dir(&target, "incumbent").join("agent.kdl")).unwrap(),
        before
    );
    assert!(!agent_dir(&target, "desired").exists());
}

#[test]
fn bootstrap_requires_an_explicit_external_pty_root_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    for case in ["missing", "catalog-local"] {
        let source = temp.path().join(format!("source-{case}"));
        write_agent(&source, "worker", false);
        if case == "catalog-local" {
            fs::write(
                source.join("catalog.kdl"),
                "catalog { pty-root \"$CATALOG/pty\" }\n",
            )
            .unwrap();
        }
        let prepared = temp.path().join(format!("prepared-{case}"));
        let captured = st2()
            .args([
                "catalog",
                "snapshot",
                "--catalog",
                source.to_str().unwrap(),
                "--output",
                prepared.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(captured.status.success());
        let captured: Value = serde_json::from_slice(&captured.stdout).unwrap();
        let target = temp.path().join(format!("target-{case}"));
        let rejected = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
        assert!(!rejected.status.success());
        assert!(!target.exists());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            stderr.contains("requires an explicit external pty-root")
                || stderr.contains("requires pty-root outside"),
            "{stderr}"
        );
    }
}

#[test]
fn concurrent_bootstrap_has_one_publication_and_one_exact_replay() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");

    let children = (0..2)
        .map(|_| {
            st2()
                .args([
                    "catalog",
                    "bootstrap",
                    "--catalog",
                    target.to_str().unwrap(),
                    "--prepared",
                    prepared.to_str().unwrap(),
                    "--input-sha256",
                    captured["rootSha256"].as_str().unwrap(),
                    "--json",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn_owned()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut statuses = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    statuses.sort();
    assert_eq!(statuses, ["created", "unchanged"]);
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".st2-catalog-bootstrap-")
    }));
}

#[test]
fn bootstrap_crash_boundaries_replay_from_absent_or_complete_only() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let expected = captured["rootSha256"].as_str().unwrap();

    let before_target = temp.path().join("before-target");
    let before = st2()
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            before_target.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            expected,
            "--json",
        ])
        .env("ST2_TEST_CATALOG_BOOTSTRAP_CRASH_AT", "before-publish")
        .output()
        .unwrap();
    assert!(!before.status.success());
    assert!(!before_target.exists());
    let recovered_before = bootstrap(&before_target, &prepared, expected);
    assert!(recovered_before.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&recovered_before.stdout).unwrap()["status"],
        "created"
    );

    let after_target = temp.path().join("after-target");
    let after = st2()
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            after_target.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            expected,
            "--json",
        ])
        .env(
            "ST2_TEST_CATALOG_BOOTSTRAP_CRASH_AT",
            "after-publish-before-parent-sync",
        )
        .output()
        .unwrap();
    assert!(!after.status.success());
    let replay_ready = temp.path().join("replay-parent-synced");
    let replay_release = temp.path().join("replay-release");
    let recovered_after = paused_bootstrap(
        &after_target,
        &prepared,
        expected,
        "after-replay-parent-sync",
        &replay_ready,
        &replay_release,
    );
    wait_for(&replay_ready);
    fs::write(&replay_release, "").unwrap();
    let recovered_after = recovered_after.wait_with_output().unwrap();
    assert!(recovered_after.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&recovered_after.stdout).unwrap()["status"],
        "unchanged"
    );
}

#[test]
fn bootstrap_publishes_its_lock_before_readers_can_enter() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");
    let ready = temp.path().join("bootstrap-ready");
    let release = temp.path().join("bootstrap-release");
    let bootstrap_child = paused_bootstrap(
        &target,
        &prepared,
        captured["rootSha256"].as_str().unwrap(),
        "after-publish-before-parent-sync",
        &ready,
        &release,
    );
    wait_for(&ready);

    let lock_attempt = temp.path().join("reader-lock-attempt");
    let mut reader = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            target.to_str().unwrap(),
            "--output",
            temp.path().join("observed").to_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_CATALOG_LOCK_ANY_ATTEMPT", &lock_attempt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_owned()
        .unwrap();
    wait_for(&lock_attempt);
    assert!(
        reader.try_wait().unwrap().is_none(),
        "reader crossed the staged lock before bootstrap became durable"
    );
    fs::write(&release, "").unwrap();
    let published = bootstrap_child.wait_with_output().unwrap();
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let observed = reader.wait_with_output().unwrap();
    assert!(
        observed.status.success(),
        "{}",
        String::from_utf8_lossy(&observed.stderr)
    );
}

#[test]
fn bootstrap_capture_is_not_redirected_by_a_source_swap() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");
    let ready = temp.path().join("capture-ready");
    let release = temp.path().join("capture-release");
    let mut child = st2();
    child
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            target.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            captured["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_PREPARED_CAPTURE_PAUSE_AT", "source-opened")
        .env("ST2_TEST_PREPARED_CAPTURE_READY", &ready)
        .env("ST2_TEST_PREPARED_CAPTURE_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().unwrap();
    wait_for(&ready);
    let retained = temp.path().join("retained-prepared");
    fs::rename(&prepared, &retained).unwrap();
    write_agent(&prepared, "redirected", false);
    ensure_external_pty_config(&prepared);
    fs::write(&release, "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(agent_dir(&target, "worker").is_dir());
    assert!(!agent_dir(&target, "redirected").exists());
}

#[test]
fn bootstrap_never_touches_the_external_pty_root() {
    let temp = tempfile::tempdir().unwrap();
    let pty_root = temp.path().join("shared-pty");
    fs::create_dir(&pty_root).unwrap();
    fs::write(pty_root.join("sentinel"), "unchanged").unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    fs::write(
        source.join("catalog.kdl"),
        format!("catalog {{ pty-root {:?} }}\n", pty_root),
    )
    .unwrap();
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");
    let output = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(pty_root.join("sentinel")).unwrap(),
        "unchanged"
    );
    assert_eq!(fs::read_dir(&pty_root).unwrap().count(), 1);
}

#[test]
fn bootstrap_rejects_wrong_input_and_uninitialized_existing_targets_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let absent = temp.path().join("absent");
    let wrong = bootstrap(&absent, &prepared, &"0".repeat(64));
    assert!(!wrong.status.success());
    assert!(!absent.exists());

    let existing = temp.path().join("existing");
    fs::create_dir(&existing).unwrap();
    let rejected = bootstrap(
        &existing,
        &prepared,
        captured["rootSha256"].as_str().unwrap(),
    );
    assert!(!rejected.status.success());
    assert!(!existing.join(".st2").exists());
}

#[test]
fn bootstrap_composes_with_the_next_root_cas_apply_generation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let target = temp.path().join("target");
    let created = bootstrap(&target, &prepared, captured["rootSha256"].as_str().unwrap());
    assert!(created.status.success());

    let update = temp.path().join("update");
    let before = snapshot(&target, &update);
    fs::write(
        update.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" { host \"host\"; role \"updated\"; argv \"true\" }\n",
    )
    .unwrap();
    let applied = apply(&target, &update, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read_to_string(target.join(".st2/catalog-generation")).unwrap(),
        "2\n"
    );
}

#[test]
fn bootstrap_target_capability_cannot_be_redirected_by_symlink_swaps() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    let prepared = temp.path().join("prepared");
    let captured = snapshot(&source, &prepared);
    let expected = captured["rootSha256"].as_str().unwrap();
    let external = temp.path().join("external");
    fs::create_dir(&external).unwrap();

    let final_symlink = temp.path().join("final-symlink");
    std::os::unix::fs::symlink(&external, &final_symlink).unwrap();
    let rejected = bootstrap(&final_symlink, &prepared, expected);
    assert!(!rejected.status.success());
    assert!(external.read_dir().unwrap().next().is_none());

    let parent = temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let target = parent.join("target");
    let ready = temp.path().join("target-ready");
    let release = temp.path().join("target-release");
    let child = paused_bootstrap(
        &target,
        &prepared,
        expected,
        "before-stage",
        &ready,
        &release,
    );
    wait_for(&ready);
    let retained_parent = temp.path().join("retained-parent");
    fs::rename(&parent, &retained_parent).unwrap();
    fs::create_dir(&parent).unwrap();
    std::os::unix::fs::symlink(&external, &target).unwrap();
    fs::write(&release, "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(agent_dir(&retained_parent.join("target"), "worker").is_dir());
    assert!(external.read_dir().unwrap().next().is_none());
}

#[test]
fn bootstrap_replay_validates_the_same_catalog_capability_it_locked() {
    let temp = tempfile::tempdir().unwrap();
    let desired_source = temp.path().join("desired-source");
    write_agent(&desired_source, "desired", false);
    let desired_prepared = temp.path().join("desired-prepared");
    let desired = snapshot(&desired_source, &desired_prepared);
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let target = parent.join("target");
    assert!(
        bootstrap(
            &target,
            &desired_prepared,
            desired["rootSha256"].as_str().unwrap(),
        )
        .status
        .success()
    );

    let incumbent_lock = st2::CatalogLock::exclusive(&target).unwrap();
    let attempt = temp.path().join("replay-lock-attempt");
    let mut child = st2();
    child
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            target.to_str().unwrap(),
            "--prepared",
            desired_prepared.to_str().unwrap(),
            "--input-sha256",
            desired["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_CATALOG_LOCK_ANY_ATTEMPT", &attempt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().unwrap();
    wait_for(&attempt);

    let displaced = parent.join("displaced");
    fs::rename(&target, &displaced).unwrap();
    assert!(
        bootstrap(
            &target,
            &desired_prepared,
            desired["rootSha256"].as_str().unwrap(),
        )
        .status
        .success()
    );
    drop(incumbent_lock);

    let replay = child.wait_with_output().unwrap();
    assert!(!replay.status.success());
    assert!(agent_dir(&displaced, "desired").is_dir());
    assert!(agent_dir(&target, "desired").is_dir());
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn assert_no_writer_temporaries(root: &Path) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".agent.kdl.presentation-")
                && !name.starts_with(".agent.kdl.publish-")
                && !name.starts_with(".catalog-apply-file-"),
            "writer temporary escaped into the declaration plane: {}",
            path.display()
        );
        if path.is_dir() && name != ".st2" {
            assert_no_writer_temporaries(&path);
        }
    }
}

fn write_test_marker(catalog: &Path, original_paths: &[&str]) {
    fs::create_dir_all(catalog.join(".st2")).unwrap();
    let hash = "0".repeat(64);
    let mut original_paths = original_paths.to_vec();
    original_paths.sort();
    let marker = serde_json::json!({
        "schema": "st2.catalog-apply-incomplete.v1",
        "stageName": format!("catalog-apply-stage-{hash}"),
        "expectedRootSha256": hash,
        "preparedRootSha256": "0".repeat(64),
        "originalProfileModules": [],
        "originalPaths": original_paths,
    });
    fs::write(
        catalog.join(".st2/catalog-apply-incomplete"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
}

fn send(catalog: &Path, recipient: &str, body: &str) -> Output {
    let mut child = st2()
        .args([
            "message",
            "send",
            recipient,
            "--catalog",
            catalog.to_str().unwrap(),
            "--as",
            "host.sender",
            "--host",
            "host",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_owned()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_with_stdin(args: &[&str], body: &str) -> Output {
    let mut child = st2()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_owned()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn paused_apply(
    catalog: &Path,
    prepared: &Path,
    expected: &str,
    point: &str,
    ready: &Path,
    release: &Path,
) -> OwnedChildGroup {
    let mut command = st2();
    let input_sha256 = prepared_root_sha256(catalog, prepared);
    command
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            expected,
            "--json",
        ])
        .stdin(Stdio::null())
        .env("ST2_TEST_CATALOG_APPLY_PAUSE_AT", point)
        .env("ST2_TEST_CATALOG_APPLY_READY", ready)
        .env("ST2_TEST_CATALOG_APPLY_RELEASE", release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn_owned().unwrap()
}

fn paused_bootstrap(
    catalog: &Path,
    prepared: &Path,
    input_sha256: &str,
    point: &str,
    ready: &Path,
    release: &Path,
) -> OwnedChildGroup {
    let mut command = st2();
    command
        .args([
            "catalog",
            "bootstrap",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            input_sha256,
            "--json",
        ])
        .stdin(Stdio::null())
        .env("ST2_TEST_CATALOG_BOOTSTRAP_PAUSE_AT", point)
        .env("ST2_TEST_CATALOG_BOOTSTRAP_READY", ready)
        .env("ST2_TEST_CATALOG_BOOTSTRAP_RELEASE", release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn_owned().unwrap()
}

#[test]
fn paused_catalog_child_is_reaped_when_test_unwinds() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let child_pid = std::cell::Cell::new(0_i32);

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child = paused_apply(
            &catalog,
            &prepared,
            before["rootSha256"].as_str().unwrap(),
            "marker-created",
            &ready,
            &release,
        );
        child_pid.set(child.id() as i32);
        wait_for(&ready);
        panic!("exercise panic-safe owned-child cleanup");
    }));
    assert!(unwound.is_err());

    let child_pid = child_pid.get();
    let deadline = Instant::now() + Duration::from_secs(2);
    while st2::host_lock::process_alive(child_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let survived = st2::host_lock::process_alive(child_pid);
    if survived {
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
            libc::waitpid(child_pid, std::ptr::null_mut(), 0);
        }
    }
    assert!(!survived, "paused catalog child survived test unwind");
}

#[test]
fn snapshot_is_typed_deterministic_and_excludes_state_and_workspaces() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let dir = agent_dir(&catalog, "worker");
    let workspace = temp.path().join("external-workspace");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir_all(catalog.join("_templates")).unwrap();
    fs::write(catalog.join("_templates/prompt.md"), "prompt").unwrap();
    fs::write(
        dir.join("agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n  render {{ copy \"_templates/prompt.md\" \"prompt.md\" }}\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.join("resources/inbox")).unwrap();
    fs::write(dir.join("resources/inbox/live.md"), "state").unwrap();
    fs::write(dir.join("status"), "busy").unwrap();
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::write(dir.join("assets/tool.sh"), "#!/bin/sh\ntrue\n").unwrap();
    fs::create_dir_all(catalog.join("workspaces/repo")).unwrap();
    fs::write(catalog.join("workspaces/repo/owned.txt"), "workspace").unwrap();

    let output = temp.path().join("snapshot");
    let first = snapshot(&catalog, &output);
    assert_eq!(first["schema"], "st2.catalog-snapshot.v1");
    assert_eq!(first["status"], "created");
    assert!(output.join("agents/host/worker/agent.kdl").is_file());
    assert!(output.join("agents/host/worker/assets/tool.sh").is_file());
    assert!(output.join("_templates/prompt.md").is_file());
    assert!(!output.join("agents/host/worker/resources").exists());
    assert!(!output.join("agents/host/worker/status").exists());
    assert!(!output.join("workspaces").exists());

    let second = snapshot(&catalog, &output);
    assert_eq!(second["status"], "unchanged");
    assert_eq!(second["rootSha256"], first["rootSha256"]);
}

#[test]
fn snapshot_ignores_and_preserves_an_exact_legacy_harness_context_staging_file() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let first_output = temp.path().join("snapshot-before-legacy-stage");
    let first = snapshot(&catalog, &first_output);
    let legacy = catalog
        .join("agents/host")
        .join(".harness-context.tmp-123-456");
    fs::write(&legacy, b"stale legacy staging bytes").unwrap();

    let second_output = temp.path().join("snapshot-after-legacy-stage");
    let second = snapshot(&catalog, &second_output);

    assert_eq!(second["rootSha256"], first["rootSha256"]);
    assert!(
        !second_output
            .join("agents/host/.harness-context.tmp-123-456")
            .exists()
    );
    assert_eq!(
        fs::read(&legacy).unwrap(),
        b"stale legacy staging bytes",
        "snapshotting must not clean another process's file"
    );
}

#[test]
fn harness_runtime_records_never_change_the_declaration_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let agent = agent_dir(&catalog, "worker");
    let baseline_output = temp.path().join("snapshot-before-runtime");
    let baseline = snapshot(&catalog, &baseline_output);
    let runtime_names = [
        "harness-context",
        "harness-state",
        ".harness-state.seq",
        ".harness-state.lock",
        ".harness-context.lock",
    ];

    for name in runtime_names {
        fs::write(agent.join(name), format!("first {name}\n")).unwrap();
    }
    let first_runtime_output = temp.path().join("snapshot-with-runtime");
    let first_runtime = snapshot(&catalog, &first_runtime_output);
    assert_eq!(first_runtime["rootSha256"], baseline["rootSha256"]);
    for name in runtime_names {
        assert!(
            !first_runtime_output
                .join("agents/host/worker")
                .join(name)
                .exists(),
            "{name} must not enter the declaration snapshot"
        );
        fs::write(agent.join(name), format!("changed {name}\n")).unwrap();
    }

    let changed_runtime = snapshot(&catalog, &temp.path().join("snapshot-changed-runtime"));
    assert_eq!(changed_runtime["rootSha256"], baseline["rootSha256"]);

    let near_miss = agent.join("harness-context.backup");
    fs::write(&near_miss, b"declaration-owned bytes").unwrap();
    let with_near_miss_output = temp.path().join("snapshot-with-near-miss");
    let with_near_miss = snapshot(&catalog, &with_near_miss_output);
    assert_ne!(with_near_miss["rootSha256"], baseline["rootSha256"]);
    assert_eq!(
        fs::read(with_near_miss_output.join("agents/host/worker/harness-context.backup")).unwrap(),
        b"declaration-owned bytes"
    );
}

#[test]
fn snapshot_projects_relative_profile_modules_once_and_hashes_their_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    fs::create_dir(catalog.join("resolvers")).unwrap();
    fs::write(catalog.join("resolvers/goal.wasm"), b"module-v1").unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        profile_catalog_config(&[
            ("dev.example.goal", "resolvers/goal.wasm"),
            ("dev.example.alias", "resolvers/./goal.wasm"),
        ]),
    )
    .unwrap();

    let first_output = temp.path().join("first");
    let first = snapshot(&catalog, &first_output);
    assert!(first_output.join("catalog.kdl").is_file());
    assert_eq!(
        fs::read(first_output.join("resolvers/goal.wasm")).unwrap(),
        b"module-v1"
    );
    assert_eq!(first["entries"], 3);

    fs::write(catalog.join("resolvers/goal.wasm"), b"module-v2").unwrap();
    let second = snapshot(&catalog, &temp.path().join("second"));
    assert_ne!(first["rootSha256"], second["rootSha256"]);
}

#[test]
fn absolute_profile_module_is_an_external_input_and_is_not_bundled() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let module = catalog.join("external-by-declaration.wasm");
    fs::write(&module, b"external").unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.external", module.to_str().unwrap())]),
    )
    .unwrap();

    let output = temp.path().join("snapshot");
    let result = snapshot(&catalog, &output);
    assert_eq!(result["entries"], 2);
    assert!(!output.join("external-by-declaration.wasm").exists());
}

#[test]
fn relative_profile_modules_reject_unsafe_missing_and_unprojected_inputs() {
    let temp = tempfile::tempdir().unwrap();
    for case in ["traversal", "missing", "symlink", "fifo", "oversized"] {
        let catalog = temp.path().join(format!("catalog-{case}"));
        write_agent(&catalog, "worker", false);
        fs::create_dir(catalog.join("resolvers")).unwrap();
        let declared = match case {
            "traversal" => "../escape.wasm",
            _ => "resolvers/goal.wasm",
        };
        fs::write(
            catalog.join("catalog.kdl"),
            profile_catalog_config(&[("dev.example.goal", declared)]),
        )
        .unwrap();
        match case {
            "traversal" | "missing" => {}
            "symlink" => {
                let external = temp.path().join("external.wasm");
                fs::write(&external, b"external").unwrap();
                std::os::unix::fs::symlink(&external, catalog.join("resolvers/goal.wasm")).unwrap();
            }
            "fifo" => {
                let fifo = catalog.join("resolvers/goal.wasm");
                let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            }
            "oversized" => {
                let module = fs::File::create(catalog.join("resolvers/goal.wasm")).unwrap();
                module
                    .set_len(agent_spec::profile::DEFAULT_MODULE_LIMIT_BYTES as u64 + 1)
                    .unwrap();
            }
            _ => unreachable!(),
        }

        let output = st2()
            .args([
                "catalog",
                "snapshot",
                "--catalog",
                catalog.to_str().unwrap(),
                "--output",
                temp.path()
                    .join(format!("snapshot-{case}"))
                    .to_str()
                    .unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "profile module case {case} unexpectedly succeeded"
        );
    }

    let missing = temp.path().join("catalog-validation-missing");
    write_agent(&missing, "worker", false);
    fs::write(
        missing.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "missing.wasm")]),
    )
    .unwrap();
    assert!(
        st2::validate::validate(&missing)
            .issues
            .iter()
            .any(|issue| issue.code == "profile-module")
    );

    let catalog = temp.path().join("catalog-extra");
    write_agent(&catalog, "worker", false);
    fs::create_dir(catalog.join("resolvers")).unwrap();
    fs::write(catalog.join("resolvers/goal.wasm"), b"goal").unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/goal.wasm")]),
    )
    .unwrap();
    let prepared = temp.path().join("prepared-extra");
    let before = snapshot(&catalog, &prepared);
    fs::write(prepared.join("resolvers/extra.wasm"), b"extra").unwrap();
    let rejected = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unprojected file"));
}

#[test]
fn raw_preimage_repairs_catalogs_with_unadmitted_profile_modules() {
    let temp = tempfile::tempdir().unwrap();
    for case in ["traversal", "missing", "symlink", "fifo", "oversized"] {
        let catalog = temp.path().join(format!("raw-module-{case}"));
        write_agent(&catalog, "worker", false);
        fs::create_dir(catalog.join("resolvers")).unwrap();
        let declared = if case == "traversal" {
            "../escape.wasm"
        } else {
            "resolvers/goal.wasm"
        };
        fs::write(
            catalog.join("catalog.kdl"),
            profile_catalog_config(&[("dev.example.goal", declared)]),
        )
        .unwrap();
        match case {
            "traversal" | "missing" => {}
            "symlink" => {
                let external = temp.path().join("raw-external.wasm");
                fs::write(&external, b"external").unwrap();
                std::os::unix::fs::symlink(&external, catalog.join("resolvers/goal.wasm")).unwrap();
            }
            "fifo" => {
                let fifo = catalog.join("resolvers/goal.wasm");
                let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            }
            "oversized" => {
                let module = fs::File::create(catalog.join("resolvers/goal.wasm")).unwrap();
                module
                    .set_len(agent_spec::profile::DEFAULT_MODULE_LIMIT_BYTES as u64 + 1)
                    .unwrap();
            }
            _ => unreachable!(),
        }

        let raw_output = temp.path().join(format!("raw-module-capture-{case}"));
        let captured = raw_snapshot(&catalog, &raw_output);
        assert!(
            captured.status.success(),
            "raw capture {case}: {}",
            String::from_utf8_lossy(&captured.stderr)
        );
        let captured: Value = serde_json::from_slice(&captured.stdout).unwrap();
        assert!(raw_output.join("catalog.kdl").is_file());
        assert!(raw_output.join("agents/host/worker/agent.kdl").is_file());
        assert!(!raw_output.join("resolvers/goal.wasm").exists());

        let desired = temp.path().join(format!("raw-module-desired-{case}"));
        write_agent(&desired, "worker", false);
        fs::create_dir(desired.join("resolvers")).unwrap();
        fs::copy(DEMO_WASM_SRC, desired.join("resolvers/repaired.wasm")).unwrap();
        fs::write(
            desired.join("catalog.kdl"),
            profile_catalog_config(&[("dev.example.goal", "resolvers/repaired.wasm")]),
        )
        .unwrap();
        let prepared = temp.path().join(format!("raw-module-prepared-{case}"));
        snapshot(&desired, &prepared);

        let repaired = raw_apply(
            &catalog,
            &prepared,
            captured["rootSha256"].as_str().unwrap(),
        );
        assert!(
            repaired.status.success(),
            "raw repair {case}: {}",
            String::from_utf8_lossy(&repaired.stderr)
        );
        assert!(catalog.join("resolvers/repaired.wasm").is_file());
        assert!(
            fs::read_to_string(catalog.join("catalog.kdl"))
                .unwrap()
                .contains("resolvers/repaired.wasm")
        );
    }
}

#[test]
fn profile_modules_reject_reserved_catalog_paths_before_apply_writes() {
    let temp = tempfile::tempdir().unwrap();
    let reserved = [
        ".st2/generation",
        ".git/x.wasm",
        "pty/x.wasm",
        "workspace/x.wasm",
        "workspaces/x.wasm",
        "resources/x.wasm",
        "agents/host/worker/.workspace/x.wasm",
        "agents/host/worker/resources/x.wasm",
        "agents/host/worker/archive/x.wasm",
        "agents/host/worker/inbox/x.wasm",
        "agents/host/worker/status/x.wasm",
        "_templates/resources/x.wasm",
    ];
    for (index, declared) in reserved.into_iter().enumerate() {
        let source = temp.path().join(format!("reserved-source-{index}"));
        write_agent(&source, "worker", false);
        let module = source.join(declared);
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, b"module").unwrap();
        fs::write(
            source.join("catalog.kdl"),
            profile_catalog_config(&[("dev.example.reserved", declared)]),
        )
        .unwrap();

        let snapshot_output = temp.path().join(format!("reserved-snapshot-{index}"));
        let rejected_snapshot = st2()
            .args([
                "catalog",
                "snapshot",
                "--catalog",
                source.to_str().unwrap(),
                "--output",
                snapshot_output.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            !rejected_snapshot.status.success()
                && String::from_utf8_lossy(&rejected_snapshot.stderr)
                    .contains("reserved control/state path"),
            "reserved module {declared}: {}",
            String::from_utf8_lossy(&rejected_snapshot.stderr)
        );
        assert!(!snapshot_output.exists());

        let catalog = temp.path().join(format!("reserved-live-{index}"));
        write_agent(&catalog, "worker", false);
        let before = snapshot(
            &catalog,
            &temp.path().join(format!("reserved-before-{index}")),
        );
        let live_config = fs::read(catalog.join("catalog.kdl")).unwrap();
        let live_spec = fs::read(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap();
        let rejected_apply = apply(&catalog, &source, before["rootSha256"].as_str().unwrap());
        assert!(
            !rejected_apply.status.success(),
            "reserved module apply unexpectedly succeeded: {declared}"
        );
        assert_eq!(fs::read(catalog.join("catalog.kdl")).unwrap(), live_config);
        assert_eq!(
            fs::read(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
            live_spec
        );
        assert!(!catalog.join(declared).exists());
        assert!(!catalog.join(".st2/catalog-generation").exists());
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    }
}

#[test]
fn prepared_profile_bundle_applies_and_loads_from_live_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let before = snapshot(&catalog, &temp.path().join("before"));

    let source = temp.path().join("source");
    write_agent(&source, "worker", false);
    fs::create_dir(source.join("resolvers")).unwrap();
    fs::copy(DEMO_WASM_SRC, source.join("resolvers/goal.wasm")).unwrap();
    fs::write(
        source.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/goal.wasm")]),
    )
    .unwrap();
    let prepared = temp.path().join("prepared-profile");
    snapshot(&source, &prepared);

    let applied = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read(catalog.join("resolvers/goal.wasm")).unwrap(),
        fs::read(DEMO_WASM_SRC).unwrap()
    );
    let registry = st2::catalog::declared_profiles(&catalog).unwrap();
    assert_eq!(
        registry.get("dev.example.goal").unwrap().module(),
        Some(catalog.join("resolvers/goal.wasm").as_path())
    );
}
#[test]
fn profile_module_publication_is_superset_biased_around_catalog_kdl() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    fs::create_dir(catalog.join("resolvers")).unwrap();
    fs::write(catalog.join("resolvers/old.wasm"), b"old").unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/old.wasm")]),
    )
    .unwrap();
    let before = snapshot(&catalog, &temp.path().join("before-publication"));

    let source = temp.path().join("publication-source");
    write_agent(&source, "worker", false);
    fs::create_dir(source.join("resolvers")).unwrap();
    fs::write(source.join("resolvers/new.wasm"), b"new").unwrap();
    fs::write(
        source.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/new.wasm")]),
    )
    .unwrap();
    let prepared = temp.path().join("publication-prepared");
    snapshot(&source, &prepared);

    let ready = temp.path().join("publication-ready");
    let release = temp.path().join("publication-release");
    let child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "mid-write",
        &ready,
        &release,
    );
    wait_for(&ready);
    assert!(
        fs::read_to_string(catalog.join("catalog.kdl"))
            .unwrap()
            .contains("resolvers/old.wasm")
    );
    assert!(catalog.join("resolvers/old.wasm").is_file());
    assert!(catalog.join("resolvers/new.wasm").is_file());

    fs::write(&release, "").unwrap();
    let applied = child.wait_with_output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        fs::read_to_string(catalog.join("catalog.kdl"))
            .unwrap()
            .contains("resolvers/new.wasm")
    );
    assert!(!catalog.join("resolvers/old.wasm").exists());
    assert!(catalog.join("resolvers/new.wasm").is_file());
}

#[test]
fn profile_module_recovery_admits_only_recorded_projected_module_paths() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog-recovery-profile");
    write_agent(&catalog, "worker", false);
    fs::create_dir(catalog.join("resolvers")).unwrap();
    fs::write(catalog.join("resolvers/old.wasm"), b"old").unwrap();
    fs::write(
        catalog.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/old.wasm")]),
    )
    .unwrap();
    let before = snapshot(&catalog, &temp.path().join("recovery-profile-before"));

    let source = temp.path().join("recovery-profile-source");
    write_agent(&source, "worker", false);
    fs::create_dir(source.join("resolvers")).unwrap();
    fs::write(source.join("resolvers/new.wasm"), b"new").unwrap();
    fs::write(
        source.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.goal", "resolvers/new.wasm")]),
    )
    .unwrap();
    let prepared = temp.path().join("recovery-profile-prepared");
    let desired = snapshot(&source, &prepared);
    let crashed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            desired["rootSha256"].as_str().unwrap(),
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "marker-created")
        .output()
        .unwrap();
    assert!(!crashed.status.success());

    let marker = catalog.join(".st2/catalog-apply-incomplete");
    let marker_json: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(
        marker_json["originalProfileModules"],
        serde_json::json!(["resolvers/old.wasm"])
    );
    assert!(
        marker_json["originalPaths"]
            .as_array()
            .unwrap()
            .contains(&Value::String("resolvers/old.wasm".to_owned()))
    );

    let forged_resolver = catalog.join("resolvers/forged.wasm");
    fs::write(&forged_resolver, b"preserve").unwrap();
    let mut forged_extra = marker_json.clone();
    forged_extra["originalPaths"]
        .as_array_mut()
        .unwrap()
        .push(Value::String("resolvers/forged.wasm".to_owned()));
    forged_extra["originalPaths"]
        .as_array_mut()
        .unwrap()
        .sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    fs::write(&marker, serde_json::to_vec(&forged_extra).unwrap()).unwrap();
    let rejected_extra = resume(&catalog);
    assert!(
        !rejected_extra.status.success()
            && String::from_utf8_lossy(&rejected_extra.stderr).contains("unowned declaration path"),
        "{}",
        String::from_utf8_lossy(&rejected_extra.stderr)
    );
    assert_eq!(fs::read(&forged_resolver).unwrap(), b"preserve");

    let reserved_module = catalog.join(".git/forged.wasm");
    fs::create_dir_all(reserved_module.parent().unwrap()).unwrap();
    fs::write(&reserved_module, b"preserve").unwrap();
    let mut forged_reserved = marker_json.clone();
    for field in ["originalPaths", "originalProfileModules"] {
        forged_reserved[field]
            .as_array_mut()
            .unwrap()
            .push(Value::String(".git/forged.wasm".to_owned()));
        forged_reserved[field]
            .as_array_mut()
            .unwrap()
            .sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    }
    fs::write(&marker, serde_json::to_vec(&forged_reserved).unwrap()).unwrap();
    let rejected_reserved = resume(&catalog);
    assert!(
        !rejected_reserved.status.success()
            && String::from_utf8_lossy(&rejected_reserved.stderr)
                .contains("reserved profile module path"),
        "{}",
        String::from_utf8_lossy(&rejected_reserved.stderr)
    );
    assert_eq!(fs::read(&reserved_module).unwrap(), b"preserve");

    fs::write(&marker, serde_json::to_vec(&marker_json).unwrap()).unwrap();
    fs::remove_dir_all(&prepared).unwrap();
    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(!marker.exists());
    assert!(!catalog.join("resolvers/old.wasm").exists());
    assert_eq!(
        fs::read(catalog.join("resolvers/new.wasm")).unwrap(),
        b"new"
    );
}

#[test]
fn raw_preimage_repairs_an_invalid_catalog_and_preserves_mutable_state() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_invalid_agent(&catalog, "worker");
    ensure_external_pty_config(&catalog);
    let dir = agent_dir(&catalog, "worker");
    fs::create_dir_all(dir.join("resources/inbox")).unwrap();
    fs::write(
        dir.join("resources/inbox/message.md"),
        "keep resource state",
    )
    .unwrap();
    fs::create_dir_all(dir.join("archive")).unwrap();
    fs::write(dir.join("archive/old.md"), "keep archive state").unwrap();
    fs::write(dir.join("status"), "busy").unwrap();

    let strict_snapshot = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            catalog.to_str().unwrap(),
            "--output",
            temp.path().join("strict-invalid").to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!strict_snapshot.status.success());

    let raw_capture_dir = temp.path().join("raw-capture");
    let raw_capture = raw_snapshot(&catalog, &raw_capture_dir);
    assert!(
        raw_capture.status.success(),
        "{}",
        String::from_utf8_lossy(&raw_capture.stderr)
    );
    let raw_capture: Value = serde_json::from_slice(&raw_capture.stdout).unwrap();
    assert_eq!(
        raw_capture["schema"],
        "st2.catalog-raw-preimage-snapshot.v1"
    );
    assert!(
        !raw_capture_dir
            .join("agents/host/worker/resources")
            .exists()
    );
    assert!(!raw_capture_dir.join("agents/host/worker/archive").exists());
    assert!(!raw_capture_dir.join("agents/host/worker/status").exists());

    let desired_source = temp.path().join("desired-source");
    write_agent(&desired_source, "worker", false);
    let prepared = temp.path().join("prepared");
    snapshot(&desired_source, &prepared);

    let strict = apply(
        &catalog,
        &prepared,
        raw_capture["rootSha256"].as_str().unwrap(),
    );
    assert!(!strict.status.success());
    assert!(
        fs::read_to_string(dir.join("agent.kdl"))
            .unwrap()
            .contains("because=\"unsupported\"")
    );

    let repaired = raw_apply(
        &catalog,
        &prepared,
        raw_capture["rootSha256"].as_str().unwrap(),
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let repaired: Value = serde_json::from_slice(&repaired.stdout).unwrap();
    assert_eq!(repaired["schema"], "st2.catalog-raw-preimage-apply.v1");
    assert_eq!(repaired["status"], "applied");
    assert_eq!(repaired["beforeSha256"], raw_capture["rootSha256"]);
    assert_eq!(
        fs::read_to_string(dir.join("resources/inbox/message.md")).unwrap(),
        "keep resource state"
    );
    assert_eq!(
        fs::read_to_string(dir.join("archive/old.md")).unwrap(),
        "keep archive state"
    );
    assert_eq!(fs::read_to_string(dir.join("status")).unwrap(), "busy");
    assert!(
        !fs::read_to_string(dir.join("agent.kdl"))
            .unwrap()
            .contains("because=\"unsupported\"")
    );
}

#[test]
fn raw_preimage_repairs_a_catalog_whose_declared_workspace_fact_is_runtime_only() {
    // The deployer's repair path: raw-snapshot the invalid live plane, publish valid declaration
    // bytes into that snapshot, then bind the apply to the opaque preimage. A raw preimage
    // captures no runtime directory, so the prepared plane it produces has none for a declared
    // workspace fact — admission must not demand one.
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    ensure_external_pty_config(&catalog);
    let dir = agent_dir(&catalog, "worker");
    fs::write(
        dir.join("agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  desired-state \"running\" because=\"unsupported\"\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let workspace = dir.join(".workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("session.txt"), "live runtime state").unwrap();

    let strict_snapshot = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            catalog.to_str().unwrap(),
            "--output",
            temp.path().join("strict-invalid").to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        !strict_snapshot.status.success(),
        "invalid live plane unexpectedly passed the strict projection"
    );

    let prepared = temp.path().join("raw-capture");
    let raw_capture = raw_snapshot(&catalog, &prepared);
    assert!(
        raw_capture.status.success(),
        "{}",
        String::from_utf8_lossy(&raw_capture.stderr)
    );
    let raw_capture: Value = serde_json::from_slice(&raw_capture.stdout).unwrap();
    assert_eq!(
        raw_capture["schema"],
        "st2.catalog-raw-preimage-snapshot.v1"
    );
    assert!(!prepared.join("agents/host/worker/.workspace").exists());

    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  retired #false\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();

    let repaired = raw_apply(
        &catalog,
        &prepared,
        raw_capture["rootSha256"].as_str().unwrap(),
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let repaired: Value = serde_json::from_slice(&repaired.stdout).unwrap();
    assert_eq!(repaired["schema"], "st2.catalog-raw-preimage-apply.v1");
    assert_eq!(repaired["status"], "applied");
    assert_eq!(repaired["beforeSha256"], raw_capture["rootSha256"]);
    assert!(
        !fs::read_to_string(dir.join("agent.kdl"))
            .unwrap()
            .contains("because=\"unsupported\"")
    );
    assert!(workspace.is_dir());
    assert_eq!(
        fs::read_to_string(workspace.join("session.txt")).unwrap(),
        "live runtime state"
    );
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());

    // The repaired plane is strictly projectable again, workspace fact included.
    let strict = snapshot(&catalog, &temp.path().join("strict-repaired"));
    assert!(strict["rootSha256"].as_str().unwrap().len() == 64);
}

#[test]
fn raw_preimage_migrates_legacy_argv_profile_to_a_component_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let pty_root = temp.path().join("shared-pty");
    let pty_root = pty_root.to_str().unwrap();
    let legacy_config = format!(
        r#"catalog {{ pty-root {pty_root:?} }}
profile "dev.example.observe" {{
  wasm "resolvers/observe.wasm"
  runtime {{
    argv "legacy-provider" "--unsafe"
  }}
}}
"#
    );
    fs::write(catalog.join("catalog.kdl"), &legacy_config).unwrap();
    let legacy_error = st2::catalog::load(&catalog).unwrap_err();
    assert!(
        legacy_error
            .to_string()
            .contains("runtime field 'argv' is unknown"),
        "{legacy_error:#}"
    );

    let agent = agent_dir(&catalog, "worker");
    fs::create_dir_all(agent.join("resources/context")).unwrap();
    fs::write(
        agent.join("resources/context/now.md"),
        "preserve mutable context",
    )
    .unwrap();
    fs::write(agent.join("status"), "busy").unwrap();
    fs::create_dir_all(agent.join(".workspace")).unwrap();
    fs::write(
        agent.join(".workspace/session.txt"),
        "preserve live workspace",
    )
    .unwrap();

    let raw_capture_dir = temp.path().join("raw-capture-legacy");
    let captured = raw_snapshot(&catalog, &raw_capture_dir);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let captured: Value = serde_json::from_slice(&captured.stdout).unwrap();
    assert_eq!(
        fs::read_to_string(raw_capture_dir.join("catalog.kdl")).unwrap(),
        legacy_config
    );
    assert!(
        !raw_capture_dir
            .join("agents/host/worker/.workspace")
            .exists()
    );

    let desired = temp.path().join("desired-component");
    write_agent(&desired, "worker", false);
    fs::create_dir_all(desired.join("resolvers")).unwrap();
    fs::create_dir_all(desired.join("providers")).unwrap();
    fs::copy(DEMO_WASM_SRC, desired.join("resolvers/observe.wasm")).unwrap();
    fs::copy(
        DEMO_WASM_SRC,
        desired.join("providers/observe.component.wasm"),
    )
    .unwrap();
    fs::write(
        desired.join("catalog.kdl"),
        format!(
            r#"catalog {{ pty-root {pty_root:?} }}
profile "dev.example.observe" {{
  wasm "resolvers/observe.wasm"
  runtime {{
    component "providers/observe.component.wasm"
    pty-stats executable="/bin/true" cwd="/" deadline-ms=1000
  }}
}}
"#
        ),
    )
    .unwrap();
    let prepared = temp.path().join("prepared-component");
    snapshot(&desired, &prepared);

    let repaired = raw_apply(
        &catalog,
        &prepared,
        captured["rootSha256"].as_str().unwrap(),
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent.join("resources/context/now.md")).unwrap(),
        "preserve mutable context"
    );
    assert_eq!(fs::read_to_string(agent.join("status")).unwrap(), "busy");
    assert_eq!(
        fs::read_to_string(agent.join(".workspace/session.txt")).unwrap(),
        "preserve live workspace"
    );
    let applied = st2::catalog::load(&catalog).unwrap();
    assert_eq!(applied.pty_root.as_deref(), Some(pty_root));
    assert_eq!(
        applied.profiles[0]
            .runtime
            .as_ref()
            .map(|runtime| runtime.component.as_str()),
        Some("providers/observe.component.wasm")
    );
    assert!(
        !fs::read_to_string(catalog.join("catalog.kdl"))
            .unwrap()
            .contains("argv")
    );
    assert_eq!(
        fs::read(catalog.join("providers/observe.component.wasm")).unwrap(),
        fs::read(DEMO_WASM_SRC).unwrap()
    );
}

#[test]
fn raw_preimage_accepts_valid_bytes_and_wrong_cas_preserves_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let valid = temp.path().join("valid");
    write_agent(&valid, "worker", false);
    fs::write(
        agent_dir(&valid, "worker").join("agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let workspace = agent_dir(&valid, "worker").join(".workspace");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir_all(valid.join("resolvers")).unwrap();
    fs::copy(DEMO_WASM_SRC, valid.join("resolvers/observe.wasm")).unwrap();
    fs::write(
        valid.join("catalog.kdl"),
        profile_catalog_config(&[("dev.example.observe", "resolvers/observe.wasm")]),
    )
    .unwrap();
    let valid_prepared = temp.path().join("valid-prepared");
    snapshot(&valid, &valid_prepared);
    let valid_raw_snapshot = raw_snapshot(&valid, &temp.path().join("valid-raw"));
    assert!(
        valid_raw_snapshot.status.success(),
        "{}",
        String::from_utf8_lossy(&valid_raw_snapshot.stderr)
    );
    let valid_raw_snapshot: Value = serde_json::from_slice(&valid_raw_snapshot.stdout).unwrap();
    let generation_before = fs::read(valid.join(".st2/catalog-generation")).ok();
    let valid_raw_apply = raw_apply(
        &valid,
        &valid_prepared,
        valid_raw_snapshot["rootSha256"].as_str().unwrap(),
    );
    assert!(
        valid_raw_apply.status.success(),
        "{}",
        String::from_utf8_lossy(&valid_raw_apply.stderr)
    );
    let valid_raw_apply: Value = serde_json::from_slice(&valid_raw_apply.stdout).unwrap();
    assert_eq!(valid_raw_apply["status"], "unchanged");
    assert!(!valid.join(".st2/catalog-apply-incomplete").exists());
    assert_eq!(
        fs::read(valid.join(".st2/catalog-generation")).ok(),
        generation_before
    );
    assert!(workspace.is_dir());

    fs::write(valid.join("resolvers/observe.wasm"), b"stale module").unwrap();
    let stale_module_apply = raw_apply(
        &valid,
        &valid_prepared,
        valid_raw_snapshot["rootSha256"].as_str().unwrap(),
    );
    assert!(
        stale_module_apply.status.success(),
        "{}",
        String::from_utf8_lossy(&stale_module_apply.stderr)
    );
    let stale_module_apply: Value = serde_json::from_slice(&stale_module_apply.stdout).unwrap();
    assert_eq!(stale_module_apply["status"], "applied");
    assert_eq!(
        fs::read(valid.join("resolvers/observe.wasm")).unwrap(),
        fs::read(DEMO_WASM_SRC).unwrap()
    );
    assert!(workspace.is_dir());

    let live_module = valid.join("resolvers/observe.wasm");
    let module_alias = temp.path().join("observe-alias.wasm");
    fs::hard_link(&live_module, &module_alias).unwrap();
    let hard_linked_module_apply = raw_apply(
        &valid,
        &valid_prepared,
        valid_raw_snapshot["rootSha256"].as_str().unwrap(),
    );
    assert!(
        hard_linked_module_apply.status.success(),
        "{}",
        String::from_utf8_lossy(&hard_linked_module_apply.stderr)
    );
    let hard_linked_module_apply: Value =
        serde_json::from_slice(&hard_linked_module_apply.stdout).unwrap();
    assert_eq!(hard_linked_module_apply["status"], "applied");
    fs::write(&module_alias, b"mutated alias").unwrap();
    assert_eq!(
        fs::read(&live_module).unwrap(),
        fs::read(DEMO_WASM_SRC).unwrap()
    );

    let invalid = temp.path().join("invalid");
    write_invalid_agent(&invalid, "worker");
    fs::create_dir(agent_dir(&invalid, "worker").join(".workspace")).unwrap();
    ensure_external_pty_config(&invalid);
    let declaration = agent_dir(&invalid, "worker").join("agent.kdl");
    let context = agent_dir(&invalid, "worker").join("resources/context/now.md");
    fs::create_dir_all(context.parent().unwrap()).unwrap();
    fs::write(&context, "state before wrong CAS").unwrap();
    let writer_temporary = agent_dir(&invalid, "worker").join(".agent.kdl.publish-test");
    fs::write(&writer_temporary, "unfinished writer bytes").unwrap();
    let before = fs::read(&declaration).unwrap();
    let wrong = raw_apply(
        &invalid,
        &valid_prepared,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("precondition failed"));
    assert_eq!(fs::read(&declaration).unwrap(), before);
    assert_eq!(
        fs::read_to_string(&context).unwrap(),
        "state before wrong CAS"
    );
    assert_eq!(
        fs::read_to_string(&writer_temporary).unwrap(),
        "unfinished writer bytes"
    );
    assert!(!invalid.join(".st2/catalog-apply-incomplete").exists());
    assert!(fs::read_dir(invalid.join(".st2")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("catalog-apply-stage-")
    }));
}

#[test]
fn raw_preimage_rejects_hard_linked_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_invalid_agent(&catalog, "worker");
    ensure_external_pty_config(&catalog);
    let declaration = agent_dir(&catalog, "worker").join("agent.kdl");
    fs::hard_link(&declaration, temp.path().join("alias.kdl")).unwrap();
    let rejected = raw_snapshot(&catalog, &temp.path().join("capture"));
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("hard-linked"));
}

#[test]
fn raw_preimage_treats_the_live_envelope_and_pty_root_as_bytes() {
    let temp = tempfile::tempdir().unwrap();

    let catalog = temp.path().join("catalog");
    write_invalid_agent(&catalog, "worker");
    fs::write(catalog.join("catalog.kdl"), "catalog {").unwrap();
    let raw_capture_dir = temp.path().join("raw-capture");
    let raw_capture = raw_snapshot(&catalog, &raw_capture_dir);
    assert!(
        raw_capture.status.success(),
        "{}",
        String::from_utf8_lossy(&raw_capture.stderr)
    );
    let raw_capture: Value = serde_json::from_slice(&raw_capture.stdout).unwrap();
    assert_eq!(
        fs::read_to_string(raw_capture_dir.join("catalog.kdl")).unwrap(),
        "catalog {"
    );

    let desired_source = temp.path().join("desired-source");
    write_agent(&desired_source, "worker", false);
    fs::write(
        desired_source.join("catalog.kdl"),
        "catalog { pty-root \"/tmp/st2-catalog-transaction-other-pty\" }\n",
    )
    .unwrap();
    let prepared = temp.path().join("prepared");
    snapshot(&desired_source, &prepared);
    let applied = raw_apply(
        &catalog,
        &prepared,
        raw_capture["rootSha256"].as_str().unwrap(),
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read(catalog.join("catalog.kdl")).unwrap(),
        fs::read(prepared.join("catalog.kdl")).unwrap()
    );
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
}

#[test]
fn raw_preimage_resume_uses_the_durable_validated_stage() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_invalid_agent(&catalog, "worker");
    ensure_external_pty_config(&catalog);
    let raw_capture = raw_snapshot(&catalog, &temp.path().join("raw-capture"));
    assert!(raw_capture.status.success());
    let raw_capture: Value = serde_json::from_slice(&raw_capture.stdout).unwrap();

    let desired_source = temp.path().join("desired-source");
    write_agent(&desired_source, "worker", false);
    let prepared = temp.path().join("prepared");
    snapshot(&desired_source, &prepared);
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let interrupted = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            raw_capture["rootSha256"].as_str().unwrap(),
            "--raw-preimage",
            "--json",
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "marker-created")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());

    fs::remove_dir_all(&prepared).unwrap();
    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(recovered["schema"], "st2.catalog-raw-preimage-apply.v1");
    assert_eq!(recovered["recovered"], true);
    assert!(
        !fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl"))
            .unwrap()
            .contains("because=\"unsupported\"")
    );
}

#[test]
fn complete_template_library_survives_unused_apply_and_supports_a_later_reference() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    fs::create_dir_all(catalog.join("_templates/nested")).unwrap();
    fs::write(catalog.join("_templates/used.md"), "used").unwrap();
    fs::write(catalog.join("_templates/future.md"), "future-v1").unwrap();
    fs::write(catalog.join("_templates/nested/alias.md"), "alias").unwrap();

    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    assert_eq!(
        fs::read_to_string(prepared.join("_templates/future.md")).unwrap(),
        "future-v1"
    );
    assert_eq!(
        fs::read_to_string(prepared.join("_templates/nested/alias.md")).unwrap(),
        "alias"
    );

    fs::write(prepared.join("_templates/future.md"), "future-v2").unwrap();
    fs::write(prepared.join("_templates/unreferenced.md"), "keep").unwrap();
    let applied = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read_to_string(catalog.join("_templates/unreferenced.md")).unwrap(),
        "keep"
    );

    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let next = temp.path().join("next");
    let current = snapshot(&catalog, &next);
    fs::write(
        next.join("agents/host/worker/agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n  render {{ copy \"_templates/future.md\" \"future.md\" }}\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
    let later = apply(&catalog, &next, current["rootSha256"].as_str().unwrap());
    assert!(
        later.status.success(),
        "{}",
        String::from_utf8_lossy(&later.stderr)
    );
    assert_eq!(
        fs::read_to_string(catalog.join("_templates/future.md")).unwrap(),
        "future-v2"
    );
}

#[test]
fn template_library_rejects_malicious_nodes_and_every_explicit_bound() {
    let temp = tempfile::tempdir().unwrap();
    for case in [
        "symlink",
        "hardlink",
        "fifo",
        "depth",
        "files",
        "file-bytes",
        "total-bytes",
    ] {
        let catalog = temp.path().join(format!("catalog-{case}"));
        write_agent(&catalog, "worker", false);
        let prepared = temp.path().join(format!("prepared-{case}"));
        let before = snapshot(&catalog, &prepared);
        fs::create_dir(prepared.join("_templates")).unwrap();
        match case {
            "symlink" => {
                std::os::unix::fs::symlink(temp.path(), prepared.join("_templates/escape.md"))
                    .unwrap();
            }
            "hardlink" => {
                fs::write(prepared.join("_templates/original.md"), "same inode").unwrap();
                fs::hard_link(
                    prepared.join("_templates/original.md"),
                    prepared.join("_templates/alias.md"),
                )
                .unwrap();
            }
            "fifo" => {
                let fifo = prepared.join("_templates/fifo");
                let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            }
            "depth" => {
                let mut dir = prepared.join("_templates");
                for index in 0..9 {
                    dir.push(format!("d{index}"));
                    fs::create_dir(&dir).unwrap();
                }
                fs::write(dir.join("too-deep.md"), "deep").unwrap();
            }
            "files" => {
                for index in 0..257 {
                    fs::write(prepared.join(format!("_templates/{index}.md")), "").unwrap();
                }
            }
            "file-bytes" => {
                let file = fs::File::create(prepared.join("_templates/large.md")).unwrap();
                file.set_len(1024 * 1024 + 1).unwrap();
            }
            "total-bytes" => {
                for index in 0..33 {
                    let file =
                        fs::File::create(prepared.join(format!("_templates/large-{index}.md")))
                            .unwrap();
                    file.set_len(1024 * 1024).unwrap();
                }
            }
            _ => unreachable!(),
        }
        let output = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
        assert!(
            !output.status.success(),
            "template case {case} unexpectedly succeeded"
        );
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    }
}

#[test]
fn apply_is_cas_guarded_idempotent_and_preserves_orphan_state() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let dir = agent_dir(&catalog, "worker");
    fs::create_dir_all(dir.join("resources/inbox")).unwrap();
    fs::write(dir.join("resources/inbox/live.md"), "keep").unwrap();
    fs::write(dir.join("status"), "busy").unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::remove_file(prepared.join("agents/host/worker/agent.kdl")).unwrap();
    fs::remove_dir_all(prepared.join("agents/host/worker")).unwrap();
    fs::remove_dir(prepared.join("agents/host")).unwrap();
    fs::remove_dir(prepared.join("agents")).unwrap();

    let stale = apply(&catalog, &prepared, &"0".repeat(64));
    assert!(!stale.status.success());
    assert!(dir.join("agent.kdl").exists());
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());

    let applied = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "apply stderr: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(applied["schema"], "st2.catalog-apply.v1");
    assert_eq!(applied["status"], "applied");
    assert!(!dir.join("agent.kdl").exists());
    assert_eq!(
        fs::read_to_string(dir.join("resources/inbox/live.md")).unwrap(),
        "keep"
    );
    assert_eq!(fs::read_to_string(dir.join("status")).unwrap(), "busy");

    let after = snapshot(&catalog, &temp.path().join("after"));
    assert_eq!(after["rootSha256"], applied["afterSha256"]);
    let unchanged = apply(&catalog, &prepared, after["rootSha256"].as_str().unwrap());
    assert!(unchanged.status.success());
    let unchanged: Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged["status"], "unchanged");
}

#[test]
fn workspace_facts_are_empty_in_prepared_admitted_against_live_and_never_applied() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let workspace = catalog.join("agents/host/worker/.workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("live.txt"), "preserve").unwrap();
    fs::write(
        catalog.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let task_workspace = catalog.join("agents/host/tasker/.workspace");
    fs::create_dir_all(&task_workspace).unwrap();
    fs::write(task_workspace.join("task-live.txt"), "task preserve").unwrap();
    fs::write(
        catalog.join("agents/host/tasker/agent.kdl"),
        "agent \"tasker\" {\n  host \"host\"\n  pty \"work\" {\n    cwd \".workspace\"\n    argv \"true\"\n  }\n}\n",
    )
    .unwrap();

    // A declared workspace fact is runtime-only: the agent owns its contents, a raw preimage
    // captures none of them, and the transaction republishes the fact as an empty directory.
    // A prepared plane that carries no directory for it is therefore admitted, and the live
    // contents survive.
    let absent = temp.path().join("prepared-absent-fact");
    let current = snapshot(&catalog, &absent);
    fs::remove_dir(absent.join("agents/host/worker/.workspace")).unwrap();
    let admitted = apply(&catalog, &absent, current["rootSha256"].as_str().unwrap());
    assert!(
        admitted.status.success(),
        "prepared plane without a declared workspace fact was rejected: {}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    assert!(workspace.is_dir());
    assert!(task_workspace.is_dir());
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).unwrap(),
        "preserve"
    );

    // Runtime bytes still may not ride into the declaration plane through the fact.
    let invalid = temp.path().join("invalid-content");
    let current = snapshot(&catalog, &invalid);
    let fact = invalid.join("agents/host/worker/.workspace");
    let secret = temp.path().join("workspace-secret");
    fs::write(&secret, "must never be opened or copied").unwrap();
    std::os::unix::fs::symlink(&secret, fact.join("forbidden-link")).unwrap();
    let rejected = apply(&catalog, &invalid, current["rootSha256"].as_str().unwrap());
    assert!(
        !rejected.status.success(),
        "prepared workspace content unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("prepared workspace fact must be empty"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).unwrap(),
        "preserve"
    );

    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    let prepared_workspace = prepared.join("agents/host/worker/.workspace");
    assert!(prepared_workspace.is_dir());
    assert!(prepared_workspace.read_dir().unwrap().next().is_none());
    let prepared_task_workspace = prepared.join("agents/host/tasker/.workspace");
    assert!(prepared_task_workspace.is_dir());
    assert!(prepared_task_workspace.read_dir().unwrap().next().is_none());

    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  role \"updated\"\n  workspace \"./.workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let applied = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).unwrap(),
        "preserve"
    );
    assert_eq!(
        fs::read_to_string(task_workspace.join("task-live.txt")).unwrap(),
        "task preserve"
    );
    let unchanged = apply(
        &catalog,
        &prepared,
        serde_json::from_slice::<Value>(&applied.stdout).unwrap()["afterSha256"]
            .as_str()
            .unwrap(),
    );
    assert!(
        unchanged.status.success(),
        "{}",
        String::from_utf8_lossy(&unchanged.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&unchanged.stdout).unwrap()["status"],
        "unchanged"
    );

    let next_workspace = catalog.join("agents/host/renamed/.workspace");
    let next = temp.path().join("next");
    let current = snapshot(&catalog, &next);
    fs::create_dir_all(&next_workspace).unwrap();
    fs::write(next_workspace.join("next.txt"), "also preserve").unwrap();
    fs::remove_dir(next.join("agents/host/worker/.workspace")).unwrap();
    fs::remove_file(next.join("agents/host/worker/agent.kdl")).unwrap();
    fs::remove_dir(next.join("agents/host/worker")).unwrap();
    fs::create_dir_all(next.join("agents/host/renamed/.workspace")).unwrap();
    fs::write(
        next.join("agents/host/renamed/agent.kdl"),
        "agent \"renamed\" {\n  host \"host\"\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let changed = apply(&catalog, &next, current["rootSha256"].as_str().unwrap());
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).unwrap(),
        "preserve"
    );
    assert_eq!(
        fs::read_to_string(next_workspace.join("next.txt")).unwrap(),
        "also preserve"
    );
    let after = temp.path().join("after-workspace-move");
    snapshot(&catalog, &after);
    assert!(!after.join("agents/host/worker/.workspace").exists());
    assert!(
        after
            .join("agents/host/renamed/.workspace")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn environment_expanded_relative_workspace_is_projected_and_admitted_consistently() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let workspace = catalog.join("agents/host/worker/.workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("live.txt"), "preserve").unwrap();
    fs::write(
        catalog.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  workspace \".workspace\"\n  argv \"true\"\n}\n",
    )
    .unwrap();

    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  role \"updated\"\n  workspace \"$ST2_TEST_WORKSPACE\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let digest = st2()
        .args([
            "catalog",
            "digest",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_WORKSPACE", ".workspace")
        .output()
        .unwrap();
    assert!(digest.status.success());
    let digest: Value = serde_json::from_slice(&digest.stdout).unwrap();
    let input_sha256 = digest["rootSha256"].as_str().unwrap().to_string();
    let applied = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_WORKSPACE", ".workspace")
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).unwrap(),
        "preserve"
    );
}

#[test]
fn workspace_fact_symlink_ancestry_fails_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let old_workspace = temp.path().join("old-workspace");
    fs::create_dir(&old_workspace).unwrap();
    fs::create_dir_all(target.join("agents/host/worker")).unwrap();
    fs::write(
        target.join("agents/host/worker/agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n}}\n",
            old_workspace.display()
        ),
    )
    .unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&target, &prepared);

    let external = temp.path().join("external");
    fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, target.join("agents/host/worker/.workspace")).unwrap();
    let workspace = target.join("agents/host/worker/.workspace");

    fs::create_dir_all(prepared.join("agents/host/worker/.workspace")).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
    let output = apply(&target, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(!output.status.success());
    assert!(
        fs::read_to_string(target.join("agents/host/worker/agent.kdl"))
            .unwrap()
            .contains(old_workspace.to_str().unwrap())
    );
    assert!(!target.join(".st2/catalog-apply-incomplete").exists());
}

#[test]
fn prepared_state_symlinks_and_pty_root_changes_fail_before_a_marker() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let original = snapshot(&catalog, &temp.path().join("original"));
    let expected = original["rootSha256"].as_str().unwrap();

    for case in ["state", "workspace", "symlink", "fifo", "pty-root"] {
        let prepared = temp.path().join(format!("prepared-{case}"));
        snapshot(&catalog, &prepared);
        match case {
            "state" => {
                fs::create_dir_all(prepared.join("agents/host/worker/resources/inbox")).unwrap();
                fs::write(
                    prepared.join("agents/host/worker/resources/inbox/message.md"),
                    "forbidden",
                )
                .unwrap();
            }
            "workspace" => {
                fs::create_dir_all(prepared.join("workspaces/repo")).unwrap();
                fs::write(prepared.join("workspaces/repo/file"), "not declarations").unwrap();
            }
            "symlink" => {
                std::os::unix::fs::symlink("/tmp", prepared.join("agents/host/worker/assets-link"))
                    .unwrap();
            }
            "fifo" => {
                let fifo = prepared.join("agents/host/worker/fifo");
                let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            }
            "pty-root" => {
                fs::write(
                    prepared.join("catalog.kdl"),
                    "catalog { pty-root \"/tmp/other-pty\" }\n",
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let result = apply(&catalog, &prepared, expected);
        assert!(
            !result.status.success(),
            "case {case} unexpectedly succeeded"
        );
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    }
}

#[test]
fn apply_v1_requires_a_declared_pty_root_outside_the_catalog() {
    let temp = tempfile::tempdir().unwrap();
    for case in ["default", "relative", "catalog-variable"] {
        let catalog = temp.path().join(format!("catalog-{case}"));
        write_agent(&catalog, "worker", false);
        match case {
            "default" => {}
            "relative" => fs::write(
                catalog.join("catalog.kdl"),
                "catalog { pty-root \"registry\" }\n",
            )
            .unwrap(),
            "catalog-variable" => fs::write(
                catalog.join("catalog.kdl"),
                "catalog { pty-root \"$CATALOG/../catalog-catalog-variable/pty\" }\n",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let prepared = temp.path().join(format!("prepared-{case}"));
        let captured = st2()
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
        assert!(captured.status.success());
        let captured: Value = serde_json::from_slice(&captured.stdout).unwrap();
        let rejected = apply(
            &catalog,
            &prepared,
            captured["rootSha256"].as_str().unwrap(),
        );
        assert!(
            !rejected.status.success(),
            "{case} unexpectedly admitted: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("requires pty-root outside")
                || String::from_utf8_lossy(&rejected.stderr)
                    .contains("requires an explicit external pty-root")
        );
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
    }
}

#[test]
fn retained_capture_refuses_a_destination_inside_its_source_before_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join("agent.kdl"), agent("worker", false)).unwrap();
    let digest = st2()
        .args(["agent", "digest", "--bundle"])
        .arg(&bundle)
        .env("TMPDIR", &bundle)
        .output()
        .unwrap();
    assert!(!digest.status.success());
    assert!(
        String::from_utf8_lossy(&digest.stderr).contains("is contained by source"),
        "{}",
        String::from_utf8_lossy(&digest.stderr)
    );
    assert_eq!(bundle.read_dir().unwrap().count(), 1);

    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let result = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("TMPDIR", &prepared)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("is contained by source"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
}

#[test]
fn concurrent_control_creation_fsyncs_both_the_creator_and_racing_observer_paths() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let release = temp.path().join("release");
    let mut children = Vec::new();
    for index in 0..2 {
        let ready = temp.path().join(format!("ready-{index}"));
        let branch = temp.path().join(format!("branch-{index}"));
        let output = temp.path().join(format!("snapshot-{index}"));
        let mut command = st2();
        command
            .args([
                "catalog",
                "snapshot",
                "--catalog",
                catalog.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--json",
            ])
            .env("ST2_TEST_CATALOG_CONTROL_READY", &ready)
            .env("ST2_TEST_CATALOG_CONTROL_RELEASE", &release)
            .env("ST2_TEST_CATALOG_CONTROL_BRANCH", &branch)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        children.push((ready, branch, command.spawn().unwrap()));
    }
    wait_for(&children[0].0);
    wait_for(&children[1].0);
    fs::write(&release, "").unwrap();

    let mut branches = Vec::new();
    for (_, branch, child) in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        branches.push(fs::read_to_string(branch).unwrap());
    }
    branches.sort();
    assert_eq!(branches, ["created", "raced"]);
    assert!(catalog.join(".st2/catalog-authoring.lock").is_file());

    // A third contender can first observe the directory after mkdir but before the creator's
    // parent fsync. The observer must independently fsync the catalog before using the control dir.
    let observed_catalog = temp.path().join("observed-catalog");
    write_agent(&observed_catalog, "worker", false);
    let created_ready = temp.path().join("created-ready");
    let created_release = temp.path().join("created-release");
    let creator_branch = temp.path().join("creator-branch");
    let creator = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            observed_catalog.to_str().unwrap(),
            "--output",
            temp.path().join("creator-snapshot").to_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_CATALOG_CONTROL_CREATED_READY", &created_ready)
        .env("ST2_TEST_CATALOG_CONTROL_CREATED_RELEASE", &created_release)
        .env("ST2_TEST_CATALOG_CONTROL_BRANCH", &creator_branch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&created_ready);

    let observer_branch = temp.path().join("observer-branch");
    let observer = st2()
        .args([
            "catalog",
            "snapshot",
            "--catalog",
            observed_catalog.to_str().unwrap(),
            "--output",
            temp.path().join("observer-snapshot").to_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_CATALOG_CONTROL_BRANCH", &observer_branch)
        .output()
        .unwrap();
    assert!(
        observer.status.success(),
        "{}",
        String::from_utf8_lossy(&observer.stderr)
    );
    assert_eq!(fs::read_to_string(observer_branch).unwrap(), "observed");

    fs::write(&created_release, "").unwrap();
    let creator = creator.wait_with_output().unwrap();
    assert!(
        creator.status.success(),
        "{}",
        String::from_utf8_lossy(&creator.stderr)
    );
    assert_eq!(fs::read_to_string(creator_branch).unwrap(), "created");
}

#[test]
fn crashes_recover_from_the_durable_stage_without_rechecking_partial_live_state() {
    for point in [
        "marker-created",
        "leaf-staged",
        "mid-write",
        "before-verify",
        "before-clear",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        write_agent(&catalog, "worker", false);
        let prepared = temp.path().join("prepared");
        let before = snapshot(&catalog, &prepared);
        fs::write(
            prepared.join("agents/host/worker/agent.kdl"),
            agent("worker", true),
        )
        .unwrap();
        if point == "leaf-staged" {
            fs::write(
                agent_dir(&catalog, "worker").join(".agent.kdl.presentation-orphan"),
                "stale",
            )
            .unwrap();
            fs::create_dir_all(catalog.join("_templates")).unwrap();
            fs::write(
                catalog.join("_templates/.agent.kdl.publish-orphan"),
                "stale",
            )
            .unwrap();
        }
        let input_sha256 = prepared_root_sha256(&catalog, &prepared);

        let crashed = st2()
            .args([
                "catalog",
                "apply",
                "--catalog",
                catalog.to_str().unwrap(),
                "--prepared",
                prepared.to_str().unwrap(),
                "--input-sha256",
                &input_sha256,
                "--expect-sha256",
                before["rootSha256"].as_str().unwrap(),
                "--json",
            ])
            .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", point)
            .output()
            .unwrap();
        assert!(
            !crashed.status.success(),
            "crash point {point} did not abort"
        );
        assert!(catalog.join(".st2/catalog-apply-incomplete").is_file());

        let competing = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
        assert!(!competing.status.success());
        assert!(
            String::from_utf8_lossy(&competing.stderr)
                .contains("recover only with `catalog apply --resume`")
        );
        fs::remove_dir_all(&prepared).unwrap();
        let recovered = resume(&catalog);
        assert!(
            recovered.status.success(),
            "recovery {point}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
        assert_eq!(recovered["status"], "applied");
        assert_eq!(recovered["recovered"], true);
        assert!(recovered["prepared"].is_null());
        assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
        assert_eq!(
            fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
            agent("worker", true)
        );
        assert_no_writer_temporaries(&catalog);
        let verified = snapshot(&catalog, &temp.path().join("verified"));
        assert_eq!(verified["rootSha256"], recovered["afterSha256"]);
    }
}

#[test]
fn a_crash_before_new_identity_publication_resumes_from_control_plane_staging() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    write_agent(&prepared, "new", false);
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);

    let crashed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "identity-staged")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    assert!(!agent_dir(&catalog, "new").exists());
    assert!(catalog.join(".st2/catalog-apply-incomplete").is_file());
    fs::remove_dir_all(&prepared).unwrap();

    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(agent_dir(&catalog, "new").join("agent.kdl").is_file());
    assert_no_writer_temporaries(&catalog);
}

#[test]
fn apply_post_commit_generation_failure_is_fenced_and_recovered() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let failed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_GENERATION_FAIL_AFTER_COMMIT", "1")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        fs::read_to_string(prepared.join("agents/host/worker/agent.kdl")).unwrap()
    );
    assert!(catalog.join(".st2/catalog-apply-incomplete").is_file());
    assert!(catalog.join(".st2/catalog-generation-incomplete").is_file());
    assert!(!catalog.join(".st2/catalog-generation").exists());
    let shared = st2()
        .args(["agents", "--catalog", catalog.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(!shared.status.success());

    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(catalog.join(".st2/catalog-generation")).unwrap(),
        "2\n"
    );
    assert!(!catalog.join(".st2/catalog-generation-incomplete").exists());
    assert!(!catalog.join(".st2/catalog-apply-incomplete").exists());
}

#[test]
fn control_directory_swap_cannot_redirect_apply_leaf_or_identity_staging() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/old/agent.kdl"),
        agent("old", true),
    )
    .unwrap();
    write_agent(&prepared, "new", false);
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let apply = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
            "--json",
        ])
        .env("ST2_TEST_CATALOG_LOCK_HELD_READY", &ready)
        .env("ST2_TEST_CATALOG_LOCK_HELD_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&ready);
    let retained = temp.path().join("retained-control");
    fs::rename(catalog.join(".st2"), &retained).unwrap();
    let outside = temp.path().join("outside-control");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, catalog.join(".st2")).unwrap();
    fs::write(&release, "").unwrap();
    let apply = apply.wait_with_output().unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "old").join("agent.kdl")).unwrap(),
        fs::read_to_string(prepared.join("agents/host/old/agent.kdl")).unwrap()
    );
    assert!(agent_dir(&catalog, "new").join("agent.kdl").is_file());
    assert!(outside.read_dir().unwrap().next().is_none());
    assert!(retained.join("catalog-generation").is_file());
    assert!(!retained.join("catalog-apply-incomplete").exists());
}

#[test]
fn cross_device_leaf_publication_fails_closed_and_remains_source_free_resumable() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let failed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_EXDEV_AT", "leaf-staged")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("must share one filesystem"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(catalog.join(".st2/catalog-apply-incomplete").is_file());
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        agent("worker", false)
    );
    fs::remove_dir_all(&prepared).unwrap();
    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        agent("worker", true)
    );
}

#[test]
fn cross_device_identity_publication_fails_closed_and_remains_source_free_resumable() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    write_agent(&prepared, "new", false);
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let failed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_EXDEV_AT", "identity-staged")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("must share one filesystem"));
    assert!(catalog.join(".st2/catalog-apply-incomplete").is_file());
    assert!(!agent_dir(&catalog, "new").exists());
    fs::remove_dir_all(&prepared).unwrap();
    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(agent_dir(&catalog, "new").join("agent.kdl").is_file());
}

#[test]
fn recovery_does_not_need_a_partially_broken_old_render_graph() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir_all(catalog.join("_templates")).unwrap();
    fs::write(catalog.join("_templates/old.md"), "old").unwrap();
    fs::write(
        agent_dir(&catalog, "worker").join("agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n  render {{ copy \"_templates/old.md\" \"prompt.md\" }}\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(prepared.join("_templates/new.md"), "new").unwrap();
    fs::remove_file(prepared.join("_templates/old.md")).unwrap();
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"host\"\n  workspace \"{}\"\n  argv \"true\"\n  render {{ copy \"_templates/new.md\" \"prompt.md\" }}\n}}\n",
            workspace.display()
        ),
    )
    .unwrap();
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);

    let crashed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "mid-delete")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    assert!(!catalog.join("_templates/old.md").exists());

    fs::remove_dir_all(&prepared).unwrap();
    let recovered = resume(&catalog);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(catalog.join("_templates/new.md")).unwrap(),
        "new"
    );
    assert!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl"))
            .unwrap()
            .contains("_templates/new.md")
    );
}

#[test]
fn mismatched_recovery_and_malformed_markers_remain_fenced_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let input_sha256 = prepared_root_sha256(&catalog, &prepared);
    let crashed = st2()
        .args([
            "catalog",
            "apply",
            "--catalog",
            catalog.to_str().unwrap(),
            "--prepared",
            prepared.to_str().unwrap(),
            "--input-sha256",
            &input_sha256,
            "--expect-sha256",
            before["rootSha256"].as_str().unwrap(),
        ])
        .env("ST2_TEST_CATALOG_APPLY_CRASH_AT", "marker-created")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    let marker = catalog.join(".st2/catalog-apply-incomplete");
    assert!(marker.is_file());
    let marker_json: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(marker_json["schema"], "st2.catalog-apply-incomplete.v1");
    assert_eq!(marker_json["expectedRootSha256"], before["rootSha256"]);
    assert!(
        marker_json["stageName"]
            .as_str()
            .unwrap()
            .starts_with("catalog-apply-stage-")
    );
    assert!(marker_json["preparedRootSha256"].as_str().is_some());
    assert!(marker_json["originalProfileModules"].is_array());
    assert!(marker_json["originalPaths"].is_array());

    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", false),
    )
    .unwrap();
    let mismatched = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(!mismatched.status.success());
    assert!(marker.is_file());
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        agent("worker", false)
    );

    let live_workspace = agent_dir(&catalog, "worker").join(".workspace");
    fs::create_dir_all(&live_workspace).unwrap();
    let live_file = live_workspace.join("live.txt");
    fs::write(&live_file, "must survive forged recovery").unwrap();
    let mut forged = marker_json.clone();
    let original_paths = forged["originalPaths"].as_array_mut().unwrap();
    original_paths.push(Value::String(
        "agents/host/worker/.workspace/live.txt".to_owned(),
    ));
    original_paths.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    fs::write(&marker, serde_json::to_vec(&forged).unwrap()).unwrap();
    let forged_recovery = resume(&catalog);
    assert!(!forged_recovery.status.success());
    assert!(
        String::from_utf8_lossy(&forged_recovery.stderr).contains("workspace or state-plane path"),
        "{}",
        String::from_utf8_lossy(&forged_recovery.stderr)
    );
    assert_eq!(
        fs::read_to_string(&live_file).unwrap(),
        "must survive forged recovery"
    );
    assert!(marker.is_file());

    fs::write(&marker, "{broken").unwrap();
    let malformed = resume(&catalog);
    assert!(!malformed.status.success());
    assert_eq!(fs::read_to_string(&marker).unwrap(), "{broken");
}

#[test]
fn marker_time_state_routes_existing_orphans_but_never_flat_falls_back_for_new_agents() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    write_agent(&catalog, "sender", false);
    let old = agent_dir(&catalog, "old");
    fs::create_dir_all(old.join("resources/inbox")).unwrap();
    write_agent(&catalog, "mix.sup", false);
    fs::create_dir_all(agent_dir(&catalog, "mix.sup").join("resources/inbox")).unwrap();
    write_agent(&catalog, "remote.worker", false);
    fs::create_dir_all(agent_dir(&catalog, "remote.worker").join("resources/inbox")).unwrap();
    write_agent_for_host(&catalog, "remote", "worker", false);
    fs::create_dir_all(catalog.join("agents/remote/worker/resources/inbox")).unwrap();
    let trap = agent_dir(&catalog, "trap");
    fs::create_dir_all(&trap).unwrap();
    let external = temp.path().join("external-state");
    fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, trap.join("resources")).unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    write_agent(&prepared, "new", false);
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "marker-created",
        &ready,
        &release,
    );
    wait_for(&ready);

    let existing = send(&catalog, "host.old", "during apply");
    assert!(
        existing.status.success(),
        "{}",
        String::from_utf8_lossy(&existing.stderr)
    );
    assert!(
        old.join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    let hierarchical = send(&catalog, "mix.sup", "hierarchical");
    assert!(
        hierarchical.status.success(),
        "{}",
        String::from_utf8_lossy(&hierarchical.stderr)
    );
    assert!(
        agent_dir(&catalog, "mix.sup")
            .join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    let local_dotted = send(&catalog, "remote.worker", "local dotted address");
    assert!(
        local_dotted.status.success(),
        "{}",
        String::from_utf8_lossy(&local_dotted.stderr)
    );
    assert!(
        agent_dir(&catalog, "remote.worker")
            .join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    assert!(
        catalog
            .join("agents/remote/worker/resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
    let trapped = send(&catalog, "trap", "must not escape");
    assert!(!trapped.status.success());
    assert!(external.read_dir().unwrap().next().is_none());

    let catalog_text = catalog.to_str().unwrap();
    let context = run_with_stdin(
        &[
            "context",
            "write",
            "host.old",
            "--catalog",
            catalog_text,
            "--host",
            "host",
        ],
        "working during apply",
    );
    assert!(
        context.status.success(),
        "{}",
        String::from_utf8_lossy(&context.stderr)
    );
    let message = st2()
        .args([
            "message",
            "send",
            "host.old",
            "-m",
            "state plane during apply",
            "--catalog",
            catalog_text,
            "--as",
            "host.old",
            "--host",
            "host",
        ])
        .output()
        .unwrap();
    assert!(
        message.status.success(),
        "{}",
        String::from_utf8_lossy(&message.stderr)
    );
    let status = st2()
        .args([
            "status",
            "host.old",
            "--set",
            "busy",
            "--catalog",
            catalog_text,
            "--host",
            "host",
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        fs::read_to_string(old.join("resources/context/now.md")).unwrap(),
        "working during apply"
    );
    assert!(
        old.join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    let presence = fs::read_to_string(old.join("status")).unwrap();
    assert!(presence.starts_with("busy\nv1 "));
    assert_eq!(presence.lines().count(), 2);

    let phantom = send(&catalog, "host.new", "too early");
    assert!(!phantom.status.success());
    assert!(!catalog.join("host.new").exists());
    assert!(!agent_dir(&catalog, "new").exists());

    fs::write(&release, "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(agent_dir(&catalog, "new").join("agent.kdl").is_file());

    let dotted = temp.path().join("dotted-catalog");
    write_agent(&dotted, "sender", false);
    for path in [
        "agents/a/b.c/resources/inbox",
        "agents/a.b/c/resources/inbox",
        "agents/a.b/only/resources/inbox",
    ] {
        fs::create_dir_all(dotted.join(path)).unwrap();
    }
    write_test_marker(
        &dotted,
        &[
            "agents/a/b.c/agent.kdl",
            "agents/a.b/c/agent.kdl",
            "agents/a.b/only/agent.kdl",
            "agents/host/sender/agent.kdl",
        ],
    );
    let ambiguous_qualified = send(&dotted, "a.b.c", "ambiguous qualified");
    assert!(!ambiguous_qualified.status.success());
    let dotted_host = send(&dotted, "a.b.only", "dotted host");
    assert!(
        dotted_host.status.success(),
        "{}",
        String::from_utf8_lossy(&dotted_host.stderr)
    );
    assert!(
        dotted
            .join("agents/a.b/only/resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
}

#[test]
fn state_remains_addressable_after_its_spec_is_deleted_mid_apply() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    write_agent(&catalog, "sender", false);
    let old = agent_dir(&catalog, "old");
    fs::create_dir_all(old.join("resources/inbox")).unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::remove_file(prepared.join("agents/host/old/agent.kdl")).unwrap();
    fs::remove_dir_all(prepared.join("agents")).unwrap();
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "deleted-spec",
        &ready,
        &release,
    );
    wait_for(&ready);
    assert!(!old.join("agent.kdl").exists());

    let sent = send(&catalog, "host.old", "after delete");
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert!(
        old.join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    assert!(!catalog.join("host.old").exists());
    let filename = String::from_utf8(sent.stdout).unwrap();
    let thread = st2()
        .args([
            "message",
            "thread",
            filename.trim(),
            "--catalog",
            catalog.to_str().unwrap(),
            "--host",
            "host",
            "--tree",
        ])
        .output()
        .unwrap();
    assert!(
        thread.status.success(),
        "{}",
        String::from_utf8_lossy(&thread.stderr)
    );
    assert!(String::from_utf8_lossy(&thread.stdout).contains(filename.trim()));

    fs::write(&release, "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn marker_time_message_write_remains_bound_to_its_retained_agent_capability() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    write_agent(&catalog, "sender", false);
    fs::create_dir_all(agent_dir(&catalog, "old").join("resources/inbox")).unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/old/agent.kdl"),
        agent("old", true),
    )
    .unwrap();
    let apply_ready = temp.path().join("apply-ready");
    let apply_release = temp.path().join("apply-release");
    let apply_child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "marker-created",
        &apply_ready,
        &apply_release,
    );
    wait_for(&apply_ready);

    let message_ready = temp.path().join("message-ready");
    let message_release = temp.path().join("message-release");
    let message = st2()
        .args([
            "message",
            "send",
            "host.old",
            "--message",
            "capability-bound",
            "--as",
            "host.sender",
            "--host",
            "host",
            "--catalog",
            catalog.to_str().unwrap(),
        ])
        .env("ST2_TEST_MESSAGE_CAPABILITY_READY", &message_ready)
        .env("ST2_TEST_MESSAGE_CAPABILITY_RELEASE", &message_release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&message_ready);
    let retained_host = temp.path().join("retained-host");
    fs::rename(catalog.join("agents/host"), &retained_host).unwrap();
    let outside = temp.path().join("outside-host");
    fs::create_dir_all(outside.join("old/resources/inbox")).unwrap();
    std::os::unix::fs::symlink(&outside, catalog.join("agents/host")).unwrap();
    fs::write(&message_release, "").unwrap();
    let message = message.wait_with_output().unwrap();
    assert!(
        message.status.success(),
        "{}",
        String::from_utf8_lossy(&message.stderr)
    );
    assert!(
        retained_host
            .join("old/resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    assert!(
        outside
            .join("old/resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
    fs::remove_file(catalog.join("agents/host")).unwrap();
    fs::rename(&retained_host, catalog.join("agents/host")).unwrap();
    fs::write(&apply_release, "").unwrap();
    let applied = apply_child.wait_with_output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

#[test]
fn marker_time_status_write_remains_bound_to_its_retained_agent_capability() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/old/agent.kdl"),
        agent("old", true),
    )
    .unwrap();
    let apply_ready = temp.path().join("apply-ready");
    let apply_release = temp.path().join("apply-release");
    let apply_child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "marker-created",
        &apply_ready,
        &apply_release,
    );
    wait_for(&apply_ready);

    let state_ready = temp.path().join("state-ready");
    let state_release = temp.path().join("state-release");
    let state = st2()
        .args([
            "status",
            "host.old",
            "--set",
            "busy",
            "--host",
            "host",
            "--catalog",
            catalog.to_str().unwrap(),
        ])
        .env("ST2_TEST_MESSAGE_CAPABILITY_READY", &state_ready)
        .env("ST2_TEST_MESSAGE_CAPABILITY_RELEASE", &state_release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&state_ready);
    let retained_host = temp.path().join("retained-host");
    fs::rename(catalog.join("agents/host"), &retained_host).unwrap();
    let outside = temp.path().join("outside-host");
    fs::create_dir_all(outside.join("old")).unwrap();
    std::os::unix::fs::symlink(&outside, catalog.join("agents/host")).unwrap();
    fs::write(&state_release, "").unwrap();
    let state = state.wait_with_output().unwrap();
    assert!(
        state.status.success(),
        "{}",
        String::from_utf8_lossy(&state.stderr)
    );
    let presence = fs::read_to_string(retained_host.join("old/status")).unwrap();
    assert!(presence.starts_with("busy\nv1 "));
    assert_eq!(presence.lines().count(), 2);
    assert!(!outside.join("old/status").exists());
    fs::remove_file(catalog.join("agents/host")).unwrap();
    fs::rename(&retained_host, catalog.join("agents/host")).unwrap();
    fs::write(&apply_release, "").unwrap();
    let applied = apply_child.wait_with_output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

#[test]
fn marker_time_state_plane_writes_reject_a_swapped_state_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    fs::create_dir_all(agent_dir(&catalog, "old").join("resources")).unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/old/agent.kdl"),
        agent("old", true),
    )
    .unwrap();
    let apply_ready = temp.path().join("apply-ready");
    let apply_release = temp.path().join("apply-release");
    let apply_child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "marker-created",
        &apply_ready,
        &apply_release,
    );
    wait_for(&apply_ready);

    for (name, args, input) in [
        (
            "context",
            vec![
                "context",
                "write",
                "host.old",
                "--catalog",
                catalog.to_str().unwrap(),
                "--host",
                "host",
            ],
            "must-not-land",
        ),
        (
            "decisions",
            vec![
                "context",
                "append",
                "host.old",
                "--decision",
                "must-not-land",
                "--why",
                "must-not-land",
                "--catalog",
                catalog.to_str().unwrap(),
                "--host",
                "host",
            ],
            "",
        ),
    ] {
        let ready = temp.path().join(format!("{name}-ready"));
        let release = temp.path().join(format!("{name}-release"));
        let mut command = st2();
        command
            .args(args)
            .env("ST2_TEST_MESSAGE_CAPABILITY_READY", &ready)
            .env("ST2_TEST_MESSAGE_CAPABILITY_RELEASE", &release)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_empty() {
            command.stdin(Stdio::null());
        } else {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn_owned().unwrap();
        if !input.is_empty() {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        wait_for(&ready);
        let resources = agent_dir(&catalog, "old").join("resources");
        let retained = temp.path().join(format!("{name}-retained-resources"));
        fs::rename(&resources, &retained).unwrap();
        let outside = temp.path().join(format!("{name}-outside"));
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &resources).unwrap();
        fs::write(&release, "").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(outside.read_dir().unwrap().next().is_none());
        fs::remove_file(&resources).unwrap();
        fs::rename(retained, resources).unwrap();
    }

    fs::write(&apply_release, "").unwrap();
    let applied = apply_child.wait_with_output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

#[test]
fn completed_apply_between_address_fence_reads_cannot_accept_a_stale_recipient() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "old", false);
    fs::create_dir_all(agent_dir(&catalog, "old").join("resources/inbox")).unwrap();
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::remove_dir_all(prepared.join("agents")).unwrap();
    let ready = temp.path().join("fence-ready");
    let release = temp.path().join("fence-release");
    let message = st2()
        .args([
            "message",
            "send",
            "host.old",
            "--message",
            "must-not-land",
            "--as",
            "host.sender",
            "--host",
            "host",
            "--catalog",
            catalog.to_str().unwrap(),
        ])
        .env("ST2_TEST_ADDRESS_FENCE_READY", &ready)
        .env("ST2_TEST_ADDRESS_FENCE_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&ready);
    let applied = apply(&catalog, &prepared, before["rootSha256"].as_str().unwrap());
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    fs::write(&release, "").unwrap();
    let message = message.wait_with_output().unwrap();
    assert!(!message.status.success());
    assert!(
        agent_dir(&catalog, "old")
            .join("resources/inbox")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
    assert!(!catalog.join("host.old").exists());
}

#[test]
fn task_inventory_fails_closed_without_observing_runtime_during_a_partial_apply() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "a", false);
    write_agent(&catalog, "b", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(prepared.join("agents/host/a/agent.kdl"), agent("a", true)).unwrap();
    fs::write(prepared.join("agents/host/b/agent.kdl"), agent("b", true)).unwrap();
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "mid-write",
        &ready,
        &release,
    );
    wait_for(&ready);
    let changed = ["a", "b"]
        .iter()
        .filter(|identity| {
            fs::read_to_string(agent_dir(&catalog, identity).join("agent.kdl"))
                .unwrap()
                .contains("retired #true")
        })
        .count();
    assert_eq!(changed, 1, "fixture must expose a stable partial catalog");

    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let sentinel = temp.path().join("runtime-observed");
    let pty = bin.join("pty");
    fs::write(
        &pty,
        format!("#!/bin/sh\n: > {:?}\nprintf '[]\\n'\n", sentinel),
    )
    .unwrap();
    let mut permissions = fs::metadata(&pty).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt as _;
    permissions.set_mode(0o755);
    fs::set_permissions(&pty, permissions).unwrap();
    let tasks = st2()
        .args([
            "tasks",
            "--host",
            "host",
            "--json",
            "--catalog",
            catalog.to_str().unwrap(),
        ])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(!tasks.status.success());
    let inventory: Value = serde_json::from_slice(&tasks.stdout).unwrap();
    assert_eq!(inventory["complete"], false);
    assert!(inventory["tasks"].as_array().unwrap().is_empty());
    assert!(inventory["errors"].as_array().unwrap().iter().any(|error| {
        error
            .as_str()
            .is_some_and(|error| error.contains("apply is incomplete"))
    }));
    assert!(
        !sentinel.exists(),
        "runtime observer ran inside the fenced view"
    );

    fs::write(&release, "").unwrap();
    let applied = child.wait_with_output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

#[test]
fn agent_publish_serializes_behind_apply_and_rechecks_its_leaf_cas() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_agent(&catalog, "worker", false);
    let old = agent("worker", false);
    let prepared = temp.path().join("prepared");
    let before = snapshot(&catalog, &prepared);
    fs::write(
        prepared.join("agents/host/worker/agent.kdl"),
        agent("worker", true),
    )
    .unwrap();
    let publisher_spec = temp.path().join("publisher.kdl");
    fs::write(
        &publisher_spec,
        "agent \"worker\" {\n  host \"host\"\n  role \"publisher\"\n  argv \"true\"\n}\n",
    )
    .unwrap();
    let digest = st2()
        .args(["agent", "digest", "--spec"])
        .arg(&publisher_spec)
        .arg("--json")
        .output()
        .unwrap();
    assert!(digest.status.success());
    let digest: Value = serde_json::from_slice(&digest.stdout).unwrap();
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let apply_child = paused_apply(
        &catalog,
        &prepared,
        before["rootSha256"].as_str().unwrap(),
        "marker-created",
        &ready,
        &release,
    );
    wait_for(&ready);

    let mut publisher = st2()
        .args([
            "agent",
            "publish",
            "--catalog",
            catalog.to_str().unwrap(),
            "--spec",
            publisher_spec.to_str().unwrap(),
            "--input-sha256",
            digest["sha256"].as_str().unwrap(),
            "--expect-sha256",
            &format!("{:x}", Sha256::digest(old.as_bytes())),
            "--json",
        ])
        .env(
            "ST2_TEST_CATALOG_LOCK_ATTEMPT",
            temp.path().join("publisher-waiting"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&temp.path().join("publisher-waiting"));
    assert!(publisher.try_wait().unwrap().is_none());

    fs::write(&release, "").unwrap();
    let apply_output = apply_child.wait_with_output().unwrap();
    assert!(
        apply_output.status.success(),
        "{}",
        String::from_utf8_lossy(&apply_output.stderr)
    );
    let publisher = publisher.wait_with_output().unwrap();
    assert!(!publisher.status.success());
    assert!(
        String::from_utf8_lossy(&publisher.stderr).contains("precondition failed"),
        "{}",
        String::from_utf8_lossy(&publisher.stderr)
    );
    assert_eq!(
        fs::read_to_string(agent_dir(&catalog, "worker").join("agent.kdl")).unwrap(),
        agent("worker", true)
    );
}
