//! `st2 resource` operates on declared Agent Spec Resource bindings: `ls`/`read` project them,
//! `refresh` requests a demand observation without rewriting the declaration, and
//! `add`/`remove`/`rename` mutate one binding through mediated CAS publication.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static OBSERVE_ENV_LOCK: Mutex<()> = Mutex::new(());

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A declaration with `bindings` spliced in verbatim, so tests control exact bytes.
fn declaration(identity: &str, managed_by: &str, bindings: &str) -> String {
    format!(
        "// unrelated comment\nagent {identity:?} {{\n  host \"h\"\n  meta {{ managed-by {managed_by:?} }}\n{bindings}  command \"sleep 300\"\n}}\n"
    )
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args(args)
        .env_remove("ST_AGENT")
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(root: &Path, args: &[&str]) -> String {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "expected success from {args:?}\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    stdout(&output)
}

fn spec(root: &Path) -> String {
    fs::read_to_string(root.join("h/worker/agent.kdl")).unwrap()
}

#[test]
fn ls_projects_declared_bindings_and_reports_none_without_pointing_at_another_store() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", "catalog", ""),
    );

    let empty = ok(root, &["resource", "ls", "worker"]);
    assert!(
        empty.contains("0 resource"),
        "empty roster should report zero bindings, got: {empty}"
    );

    write(
        root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"notes\" reason=\"Durable notes.\" uri=\"agent-notes://h/worker\"\n\
             resource \"work\" reason=\"PR under preparation.\" uri=\"github-pr://github.com/o/r/pull/42\"\n",
        ),
    );

    let listed = ok(root, &["resource", "ls", "worker"]);
    assert!(listed.contains("2 resource"), "got: {listed}");
    assert!(listed.contains("notes"), "got: {listed}");
    assert!(
        listed.contains("agent-notes://h/worker"),
        "binding uri must appear: {listed}"
    );
    assert!(
        listed.contains("github-pr://github.com/o/r/pull/42"),
        "binding uri must appear: {listed}"
    );
}

#[test]
fn ls_json_and_read_expose_every_declared_field() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"work\" reason=\"PR under preparation.\" uri=\"github-pr://github.com/o/r/pull/42\" inactive-reason=\"Superseded by #43.\" selector=#\"{\"topics\":[\"ci.failure\"]}\"#\n",
        ),
    );

    let listed = ok(root, &["resource", "ls", "worker", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let row = &rows.as_array().expect("json array")[0];
    assert_eq!(row["name"], "work");
    assert_eq!(row["uri"], "github-pr://github.com/o/r/pull/42");
    assert_eq!(row["reason"], "PR under preparation.");
    // `inactive_reason`, not `inactiveReason`: this is the descriptor `st2 agents --json`
    // already emits, and INVARIANTS.md pins that surface to preserve its field names. The
    // snake_case is inconsistent with sibling roster fields but predates this change.
    assert_eq!(row["inactive_reason"], "Superseded by #43.");
    assert_eq!(
        row["selector"],
        serde_json::json!({"topics": ["ci.failure"]})
    );

    let read = ok(root, &["resource", "read", "worker", "work"]);
    assert!(
        read.contains("github-pr://github.com/o/r/pull/42"),
        "got: {read}"
    );
    assert!(read.contains("PR under preparation."), "got: {read}");
    assert!(read.contains("Superseded by #43."), "got: {read}");
    assert!(read.contains(r#"{"topics":["ci.failure"]}"#), "got: {read}");
}

#[test]
fn add_publishes_one_binding_and_is_idempotent_on_identical_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", "catalog", ""),
    );

    let added = ok(
        root,
        &[
            "resource",
            "add",
            "work",
            "--agent",
            "worker",
            "--uri",
            "github-pr://github.com/o/r/pull/42",
            "--reason",
            "PR under preparation.",
            "--selector-json",
            r####"{"literal":"a\"#b\"##c","topics":["ci.failure"]}"####,
            "--json",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&added).unwrap();
    assert_eq!(receipt["result"], "changed");
    assert_eq!(
        receipt["selector"],
        serde_json::json!({
            "literal": "a\"#b\"##c",
            "topics": ["ci.failure"]
        })
    );

    let after = spec(root);
    assert!(after.contains("resource \"work\""), "got:\n{after}");
    assert!(
        after
            .contains(r####"selector=###"{"literal":"a\"#b\"##c","topics":["ci.failure"]}"###"####),
        "selector must use the smallest safe raw-string fence:\n{after}"
    );
    assert!(
        after.contains("// unrelated comment"),
        "unrelated bytes must survive:\n{after}"
    );
    assert!(
        after.contains("command \"sleep 300\""),
        "unrelated bytes must survive:\n{after}"
    );

    let repeated = ok(
        root,
        &[
            "resource",
            "add",
            "work",
            "--agent",
            "worker",
            "--uri",
            "github-pr://github.com/o/r/pull/42",
            "--reason",
            "PR under preparation.",
            "--selector-json",
            r####"{"literal":"a\"#b\"##c","topics":["ci.failure"]}"####,
            "--json",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&repeated).unwrap();
    assert_eq!(
        receipt["result"], "unchanged",
        "re-adding identical bytes must not republish"
    );
    assert_eq!(spec(root), after, "unchanged add must not rewrite the file");
}

#[test]
fn add_preserves_the_uri_byte_for_byte_without_normalization() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", "catalog", ""),
    );

    // Percent-encoding, mixed case, and a trailing slash must all survive verbatim.
    let exact = "vendor+Thing://Authority/exact%20identity/";
    ok(
        root,
        &[
            "resource",
            "add",
            "odd",
            "--agent",
            "worker",
            "--uri",
            exact,
            "--reason",
            "Exactness probe.",
        ],
    );

    assert!(
        spec(root).contains(exact),
        "uri must round-trip unchanged, got:\n{}",
        spec(root)
    );
    assert!(
        ok(root, &["resource", "ls", "worker"]).contains(exact),
        "uri must project unchanged"
    );
}

#[test]
fn remove_is_idempotent_and_rename_refuses_absent_and_colliding_names() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"notes\" reason=\"Durable notes.\" uri=\"agent-notes://h/worker\"\n\
             resource \"work\" reason=\"PR under preparation.\" uri=\"github-pr://github.com/o/r/pull/42\" selector=#\"{\"topics\":[\"ci.failure\"]}\"#\n",
        ),
    );

    let renamed = ok(
        root,
        &[
            "resource",
            "rename",
            "work",
            "current-work",
            "--agent",
            "worker",
            "--json",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&renamed).unwrap();
    assert_eq!(receipt["result"], "changed");
    assert!(spec(root).contains("resource \"current-work\""));
    assert!(!spec(root).contains("resource \"work\""));
    assert!(
        spec(root).contains(r##"selector=#"{"topics":["ci.failure"]}"#"##),
        "rename must preserve selector configuration"
    );

    let absent = run(
        root,
        &["resource", "rename", "work", "other", "--agent", "worker"],
    );
    assert!(
        !absent.status.success(),
        "renaming an absent binding must fail"
    );

    let collision = run(
        root,
        &[
            "resource",
            "rename",
            "notes",
            "current-work",
            "--agent",
            "worker",
        ],
    );
    assert!(
        !collision.status.success(),
        "renaming onto an existing name must fail; names are unique per agent"
    );

    let removed = ok(
        root,
        &[
            "resource",
            "remove",
            "current-work",
            "--agent",
            "worker",
            "--json",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&removed).unwrap();
    assert_eq!(receipt["result"], "changed");

    let again = ok(
        root,
        &[
            "resource",
            "remove",
            "current-work",
            "--agent",
            "worker",
            "--json",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(
        receipt["result"], "unchanged",
        "removing an absent binding is an idempotent success"
    );
}

#[test]
fn mutation_refuses_an_invalid_uri_an_empty_reason_and_a_nix_managed_declaration() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", "catalog", ""),
    );

    // #345 widened the envelope: a binding may name a catalog-relative carrier path as well as
    // an absolute URI, so `not-absolute` is now a *valid* relative carrier. What stays refused is
    // a path that escapes the catalog.
    for escaping in ["/etc/passwd", "../escape"] {
        let refused = run(
            root,
            &[
                "resource", "add", "bad", "--agent", "worker", "--uri", escaping, "--reason",
                "Probe.",
            ],
        );
        assert!(
            !refused.status.success(),
            "a catalog-relative uri that escapes the catalog must be refused: {escaping}"
        );
    }

    // `declaration` is reserved by resync (#345) and may not be taken as a binding name.
    let reserved = run(
        root,
        &[
            "resource",
            "add",
            "declaration",
            "--agent",
            "worker",
            "--uri",
            "https://example.test/x",
            "--reason",
            "Probe.",
        ],
    );
    assert!(
        !reserved.status.success(),
        "the resync-reserved binding name must be refused"
    );

    let blank = run(
        root,
        &[
            "resource",
            "add",
            "bad",
            "--agent",
            "worker",
            "--uri",
            "https://example.test/x",
            "--reason",
            "",
        ],
    );
    assert!(!blank.status.success(), "an empty reason must be refused");
    assert!(
        !spec(root).contains("resource \"bad\""),
        "a refused mutation must not write"
    );

    write(root, "h/nixed/agent.kdl", &declaration("nixed", "nix", ""));
    let nixed = run(
        root,
        &[
            "resource",
            "add",
            "work",
            "--agent",
            "nixed",
            "--uri",
            "https://example.test/x",
            "--reason",
            "Probe.",
        ],
    );
    assert!(
        !nixed.status.success(),
        "a Nix-owned declaration must refuse a runtime binding edit"
    );
}

#[test]
fn the_retired_link_record_plane_is_gone() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/worker/agent.kdl",
        &declaration("worker", "catalog", ""),
    );

    // `add` took a bare URL under the retired plane. It now requires a name plus --uri/--reason,
    // so the old invocation must not silently succeed and write a link record.
    let legacy = run(root, &["resource", "add", "https://example.test/output"]);
    assert!(
        !legacy.status.success(),
        "the retired link-record invocation must not be accepted"
    );
    assert!(
        !root.join("h/worker/resources/links").exists(),
        "no link-record store may be created"
    );
}

/// A hand-authored binding may carry a trailing `//` comment explaining it. Removing the binding
/// removes that explanation with it, and updating one keeps the separator before it.
/// Regression: `remove` previously refused with `unsafe-source-shape`, and `add` glued the
/// rendered node onto the comment.
#[test]
fn a_binding_with_a_trailing_line_comment_is_removable_and_updatable() {
    let commented = "// unrelated comment\nagent \"worker\" {\n  host \"h\"\n  meta { managed-by \"catalog\" }\n  resource \"work\" uri=\"github-pr://github.com/o/r/pull/42\" reason=\"PR under preparation.\" // why it is here\n  resource \"notes\" uri=\"agent-notes://h/worker\" reason=\"Durable notes.\"\n  command \"sleep 300\"\n}\n";

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/worker/agent.kdl", commented);

    ok(
        root,
        &[
            "resource",
            "add",
            "work",
            "--agent",
            "worker",
            "--uri",
            "https://example.test/x",
            "--reason",
            "Changed.",
        ],
    );
    let updated = spec(root);
    assert!(
        updated.contains(r#"reason="Changed." // why it is here"#),
        "the blank before a trailing comment must survive an update:\n{updated}"
    );

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/worker/agent.kdl", commented);

    ok(root, &["resource", "remove", "work", "--agent", "worker"]);
    let after = spec(root);
    assert!(!after.contains("resource \"work\""), "got:\n{after}");
    assert!(
        !after.contains("why it is here"),
        "the binding's own trailing comment goes with it:\n{after}"
    );
    assert!(
        after.contains("resource \"notes\""),
        "the sibling binding must survive:\n{after}"
    );
    assert!(
        after.contains("// unrelated comment") && after.contains("command \"sleep 300\""),
        "unrelated bytes must survive:\n{after}"
    );
}

#[test]
fn refresh_binding_validation_and_generation_share_one_catalog_snapshot() {
    let _guard = OBSERVE_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let state = temporary.path().join("state");
    write(
        &root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"work\" reason=\"Old binding.\" uri=\"https://example.test/old\"\n",
        ),
    );
    ok(
        &root,
        &[
            "resource",
            "add",
            "anchor",
            "--agent",
            "worker",
            "--uri",
            "https://example.test/anchor",
            "--reason",
            "Initialize the catalog generation.",
        ],
    );
    let old_generation = st2::resource_observe::catalog_generation(&root)
        .unwrap()
        .expect("mediated edit initialized catalog generation");

    let previous_state = std::env::var_os("XDG_STATE_HOME");
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };
    st2::event::publish_owner_binding_for_test(&root, "h").unwrap();
    let scope = st2::park::SupervisorScope::current(&root, "h").unwrap();
    let request_dir = scope.park_dir().parent().unwrap().join("observe-requests");
    match previous_state {
        Some(value) => unsafe { std::env::set_var("XDG_STATE_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
    }

    let snapshot_ready = temporary.path().join("snapshot-ready");
    let snapshot_release = temporary.path().join("snapshot-release");
    let replacement_attempt = temporary.path().join("replacement-attempt");
    let refresh = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args([
            "resource", "refresh", "worker", "work", "--wait", "0", "--host", "h",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("ST2_TEST_RESOURCE_REFRESH_SNAPSHOT_READY", &snapshot_ready)
        .env(
            "ST2_TEST_RESOURCE_REFRESH_SNAPSHOT_RELEASE",
            &snapshot_release,
        )
        .env_remove("ST_AGENT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&snapshot_ready);

    let mut replacement = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args([
            "resource",
            "add",
            "work",
            "--agent",
            "worker",
            "--uri",
            "https://example.test/replacement",
            "--reason",
            "Replacement binding.",
        ])
        .env("ST2_TEST_CATALOG_LOCK_ATTEMPT", &replacement_attempt)
        .env_remove("ST_AGENT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&replacement_attempt);
    assert!(
        replacement.try_wait().unwrap().is_none(),
        "replacement crossed the shared snapshot lock before generation capture"
    );
    assert!(
        spec(&root).contains("https://example.test/old"),
        "replacement landed while refresh held its snapshot"
    );

    fs::write(&snapshot_release, b"release").unwrap();
    let refresh = refresh.wait_with_output().unwrap();
    assert!(
        !refresh.status.success(),
        "zero client wait should leave the request queued"
    );
    let replacement = replacement.wait_with_output().unwrap();
    assert!(
        replacement.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&replacement),
        stderr(&replacement)
    );

    let request = wait_for_refresh_request(&request_dir);
    assert_eq!(
        request["expectedCatalogGeneration"],
        serde_json::json!(old_generation),
        "request generation must come from the validated old binding snapshot"
    );
    let replacement_generation = st2::resource_observe::catalog_generation(&root)
        .unwrap()
        .unwrap();
    assert!(
        replacement_generation > old_generation,
        "replacement did not advance catalog generation"
    );
    assert!(spec(&root).contains("https://example.test/replacement"));
}

#[test]
fn refresh_cli_reports_exact_receipts_and_wait_expiry_keeps_the_request() {
    let _guard = OBSERVE_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("catalog");
    let state = temporary.path().join("state");
    write(
        &root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"work\" reason=\"Observed.\" uri=\"dev.example://work\"\n",
        ),
    );

    let previous_state = std::env::var_os("XDG_STATE_HOME");
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };
    st2::event::publish_owner_binding_for_test(&root, "h").unwrap();
    let scope = st2::park::SupervisorScope::current(&root, "h").unwrap();
    let scope_root = scope.park_dir().parent().unwrap().to_path_buf();
    match previous_state {
        Some(value) => unsafe { std::env::set_var("XDG_STATE_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
    }
    let request_dir = scope_root.join("observe-requests");
    let receipt_dir = scope_root.join("observe-receipts");

    let unchanged = spawn_refresh(&root, &state, 2);
    let request = wait_for_refresh_request(&request_dir);
    publish_refresh_receipt(&receipt_dir, &request, "settledUnchanged", None);
    let unchanged = unchanged.wait_with_output().unwrap();
    assert!(
        unchanged.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&unchanged),
        stderr(&unchanged)
    );
    let unchanged_json: serde_json::Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged_json["status"], "settledUnchanged");
    assert_eq!(unchanged_json["recipient"], "h.worker");
    clear_refresh_records(&request_dir, &receipt_dir);

    let failed = spawn_refresh(&root, &state, 2);
    let request = wait_for_refresh_request(&request_dir);
    publish_refresh_receipt(
        &receipt_dir,
        &request,
        "settledFailed",
        Some("provider refused"),
    );
    let failed = failed.wait_with_output().unwrap();
    assert!(!failed.status.success());
    let failed_json: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed_json["status"], "settledFailed");
    assert_eq!(failed_json["diagnostic"], "provider refused");
    clear_refresh_records(&request_dir, &receipt_dir);

    let timed_out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args([
            "resource", "refresh", "worker", "work", "--wait", "0", "--json", "--host", "h",
        ])
        .env("XDG_STATE_HOME", &state)
        .env_remove("ST_AGENT")
        .output()
        .unwrap();
    assert!(!timed_out.status.success());
    let timeout_json: serde_json::Value = serde_json::from_slice(&timed_out.stdout).unwrap();
    assert_eq!(timeout_json["status"], "timeout");
    assert_eq!(timeout_json["queued"], true);
    assert!(
        fs::read_dir(&request_dir)
            .unwrap()
            .flatten()
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")),
        "the client wait bound dropped its queued request"
    );
    clear_refresh_records(&request_dir, &receipt_dir);

    let final_read_ready = temporary.path().join("observe-final-read-ready");
    let final_read_release = temporary.path().join("observe-final-read-release");
    let final_read = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args([
            "resource", "refresh", "worker", "work", "--wait", "1", "--json", "--host", "h",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("ST2_TEST_OBSERVE_WAIT_TIMEOUT_READY", &final_read_ready)
        .env("ST2_TEST_OBSERVE_WAIT_TIMEOUT_RELEASE", &final_read_release)
        .env_remove("ST_AGENT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = wait_for_refresh_request(&request_dir);
    wait_for_file(&final_read_ready);
    publish_refresh_receipt(&receipt_dir, &request, "settledUnchanged", None);
    fs::write(&final_read_release, b"release").unwrap();
    let final_read = final_read.wait_with_output().unwrap();
    assert!(
        final_read.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&final_read),
        stderr(&final_read)
    );
    let final_read_json: serde_json::Value = serde_json::from_slice(&final_read.stdout).unwrap();
    assert_eq!(final_read_json["status"], "settledUnchanged");
    clear_refresh_records(&request_dir, &receipt_dir);

    let no_supervisor_root = temporary.path().join("no-supervisor-catalog");
    write(
        &no_supervisor_root,
        "h/worker/agent.kdl",
        &declaration(
            "worker",
            "catalog",
            "  resource \"work\" reason=\"Observed.\" uri=\"dev.example://work\"\n",
        ),
    );
    let no_supervisor_state = temporary.path().join("no-supervisor-state");
    let no_supervisor = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", no_supervisor_root.to_str().unwrap()])
        .args([
            "resource", "refresh", "worker", "work", "--wait", "0", "--host", "h",
        ])
        .env("XDG_STATE_HOME", no_supervisor_state)
        .env_remove("ST_AGENT")
        .output()
        .unwrap();
    assert!(!no_supervisor.status.success());
    assert!(stderr(&no_supervisor).contains("no live Resource Profile supervisor"));
}

fn spawn_refresh(root: &Path, state: &Path, wait: u64) -> std::process::Child {
    let wait = wait.to_string();
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args([
            "resource",
            "refresh",
            "worker",
            "work",
            "--wait",
            wait.as_str(),
            "--json",
            "--host",
            "h",
        ])
        .env("XDG_STATE_HOME", state)
        .env_remove("ST_AGENT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_refresh_request(dir: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(request) = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|entry| {
                (entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .then(|| fs::read(entry.path()).ok())
                .flatten()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            })
        {
            return request;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for refresh request"
        );
        std::thread::yield_now();
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::yield_now();
    }
}

fn publish_refresh_receipt(
    dir: &Path,
    request: &serde_json::Value,
    status: &str,
    diagnostic: Option<&str>,
) {
    fs::create_dir_all(dir).unwrap();
    let request_id = request["requestId"].as_str().unwrap();
    let mut receipt = serde_json::json!({
        "schema": "st2.resource-observe-receipt.v1",
        "requestId": request_id,
        "recipient": request["recipient"],
        "binding": request["binding"],
        "status": status,
        "authority": {
            "owner": {"incarnation": "incarnation", "claim": "claim"},
            "bindingId": "binding",
            "registration": "registration"
        },
        "demandWatermark": 1,
        "updatedAt": "2026-08-31T00:00:00Z"
    });
    if let Some(diagnostic) = diagnostic {
        receipt["diagnostic"] = serde_json::Value::String(diagnostic.to_owned());
    }
    let temporary = dir.join(format!(".{request_id}.tmp"));
    let final_path = dir.join(format!("{request_id}.json"));
    fs::write(&temporary, serde_json::to_vec(&receipt).unwrap()).unwrap();
    fs::rename(temporary, final_path).unwrap();
}

fn clear_refresh_records(request_dir: &Path, receipt_dir: &Path) {
    for dir in [request_dir, receipt_dir] {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            fs::remove_file(entry.path()).unwrap();
        }
    }
}
