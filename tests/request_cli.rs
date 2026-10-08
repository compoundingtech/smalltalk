use std::fs;
use std::path::Path;
use std::process::Command;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn declare_agent(root: &Path, identity: &str) {
    declare_agent_on_host(root, identity, "h");
}

fn declare_agent_on_host(root: &Path, identity: &str, host: &str) {
    write(
        root,
        &format!("{host}/{identity}/agent.kdl"),
        &format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{host}\"\n  type \"service\"\n  pty \"agent\" {{ command \"x\" }}\n}}\n"
        ),
    );
}

fn declare_principal(root: &Path, identity: &str) {
    write(
        root,
        &format!("principals/h/{identity}/principal.kdl"),
        &format!("principal \"{identity}\" host=\"h\"\n"),
    );
}

fn request(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .arg("request")
        .args(args)
        .args(["--root", root.to_str().unwrap(), "--host", "h"])
        .output()
        .unwrap()
}

#[test]
fn stable_request_key_atomically_deduplicates_one_canonical_agent_message() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");

    let args = [
        "send",
        "h.worker",
        "--as",
        "h.example-ci",
        "--idempotency-key",
        "escalate:repo#7:abc",
        "--tag",
        "kind=example-ci.escalation",
        "--tag",
        "schema=1",
        "-m",
        r#"{"candidate":"abc","allowedOutcomes":["needs-human"]}"#,
        "--json",
    ];

    let first = request(tmp.path(), &args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["status"], "published");
    assert_eq!(first["deduplicated"], false);
    assert_eq!(first["idempotencyKey"], "escalate:repo#7:abc");

    let second = request(tmp.path(), &args);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["status"], "published");
    assert_eq!(second["deduplicated"], true);
    assert_eq!(second["filename"], first["filename"]);

    let inbox = tmp.path().join("h/worker/resources/inbox");
    let messages = st2::message::list_dir(&inbox).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from.as_deref(), Some("h.example-ci"));
    assert_eq!(messages[0].in_reply_to, None);
    assert!(messages[0].body.contains("\"candidate\":\"abc\""));
    assert!(!tmp.path().join("h.example-ci/inbox").exists());

    let conflict = request(
        tmp.path(),
        &[
            "send",
            "h.worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "escalate:repo#7:abc",
            "-m",
            r#"{"candidate":"different"}"#,
            "--json",
        ],
    );
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr)
            .contains("idempotency key reused with different request")
    );
    assert_eq!(st2::message::list_dir(&inbox).unwrap().len(), 1);
}

#[test]
fn request_api_rejects_agent_impersonation_and_unknown_flat_principals() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");

    for actor in ["h.worker", "h.not-declared"] {
        let output = request(
            tmp.path(),
            &[
                "send",
                "h.worker",
                "--as",
                actor,
                "--idempotency-key",
                "one",
                "-m",
                "{}",
                "--json",
            ],
        );
        assert!(!output.status.success(), "actor {actor} must be rejected");
        assert!(String::from_utf8_lossy(&output.stderr).contains("declared service principal"));
    }

    assert!(!tmp.path().join("h.not-declared/inbox").exists());
    assert!(
        st2::message::list_dir(&tmp.path().join("h/worker/resources/inbox"))
            .unwrap()
            .is_empty()
    );

    let collision = tempfile::tempdir().unwrap();
    declare_agent(collision.path(), "example-ci");
    declare_agent(collision.path(), "worker");
    declare_principal(collision.path(), "example-ci");
    let output = request(
        collision.path(),
        &[
            "send",
            "h.worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "one",
            "-m",
            "{}",
            "--json",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("collides with an Agent Spec"));
}

#[test]
fn bare_request_recipient_does_not_resolve_an_agent_on_another_host() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent_on_host(tmp.path(), "worker", "remote");
    declare_principal(tmp.path(), "example-ci");

    let output = request(
        tmp.path(),
        &[
            "send",
            "worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "one",
            "-m",
            "{}",
            "--json",
        ],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no agent 'worker' found"));
    assert!(!tmp.path().join("remote/worker/resources/inbox").exists());
}

#[test]
fn malformed_or_misplaced_principal_declarations_fail_before_publication() {
    for declaration in [
        "principal \"different\" host=\"h\"\n",
        "principal \"example-ci\"\n",
        "principal \"example-ci\" host=\"h\"\nprincipal \"second\" host=\"h\"\n",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        declare_agent(tmp.path(), "worker");
        write(
            tmp.path(),
            "principals/h/example-ci/principal.kdl",
            declaration,
        );
        let output = request(
            tmp.path(),
            &[
                "send",
                "h.worker",
                "--as",
                "h.example-ci",
                "--idempotency-key",
                "one",
                "-m",
                "{}",
                "--json",
            ],
        );
        assert!(!output.status.success());
        assert!(
            st2::message::list_dir(&tmp.path().join("h/worker/resources/inbox"))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn request_json_and_typed_tags_fail_closed_at_the_cli_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");

    for tail in [
        vec!["-m", "not-json"],
        vec!["--tag", "missing-equals", "-m", "{}"],
        vec!["--tag", "kind=one", "--tag", "kind=two", "-m", "{}"],
    ] {
        let mut args = vec![
            "send",
            "h.worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "one",
        ];
        args.extend(tail);
        args.push("--json");
        let output = request(tmp.path(), &args);
        assert!(!output.status.success());
    }
    assert!(
        st2::message::list_dir(&tmp.path().join("h/worker/resources/inbox"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn request_read_and_reply_bind_the_json_envelope_to_native_message_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");
    let inbox = tmp.path().join("h/worker/resources/inbox");
    fs::create_dir_all(&inbox).unwrap();
    let filename = "1784649988123-abc23z.md";
    let envelope = r#"{"version":1,"idempotencyKey":"forged","from":"h.example-ci","to":"h.worker","replyTo":"h.example-ci","tags":{},"body":{}}"#;
    fs::write(
        inbox.join(filename),
        st2::message::render_message(
            "h.attacker",
            Some("request forged"),
            None,
            &["st2-request".to_string()],
            envelope,
        ),
    )
    .unwrap();

    for args in [
        vec!["read", filename, "--as", "h.worker", "--json"],
        vec!["reply", filename, "--as", "h.worker", "-m", "{}", "--json"],
    ] {
        let output = request(tmp.path(), &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("frontmatter sender"));
    }
    assert!(
        st2::message::list_dir(&tmp.path().join("principals/h/example-ci/resources/inbox"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn typed_reply_routes_to_the_principal_and_status_is_a_tagged_json_union() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");

    let sent = request(
        tmp.path(),
        &[
            "send",
            "h.worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "repair:42",
            "--tag",
            "kind=example-ci.escalation",
            "-m",
            r#"{"candidate":"abc"}"#,
            "--json",
        ],
    );
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let sent: serde_json::Value = serde_json::from_slice(&sent.stdout).unwrap();
    let filename = sent["filename"].as_str().unwrap();

    let read = request(
        tmp.path(),
        &["read", filename, "--as", "h.worker", "--json"],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["status"], "request");
    assert_eq!(read["idempotencyKey"], "repair:42");
    assert_eq!(read["from"], "h.example-ci");
    assert_eq!(
        read["tags"],
        serde_json::json!({"kind": "example-ci.escalation"})
    );
    assert_eq!(read["body"], serde_json::json!({"candidate": "abc"}));

    let pending = request(
        tmp.path(),
        &[
            "status",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "repair:42",
            "--json",
        ],
    );
    assert!(pending.status.success());
    let pending: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert_eq!(
        pending,
        serde_json::json!({
            "status": "pending",
            "idempotencyKey": "repair:42",
            "requestFilename": filename,
        })
    );

    let reply_args = [
        "reply",
        filename,
        "--as",
        "h.worker",
        "--tag",
        "outcome=needs-human",
        "-m",
        r#"{"outcome":"needs-human"}"#,
        "--json",
    ];
    let first_reply = request(tmp.path(), &reply_args);
    assert!(
        first_reply.status.success(),
        "{}",
        String::from_utf8_lossy(&first_reply.stderr)
    );
    let first_reply: serde_json::Value = serde_json::from_slice(&first_reply.stdout).unwrap();
    assert_eq!(first_reply["deduplicated"], false);
    assert_eq!(first_reply["idempotencyKey"], "repair:42");

    let second_reply = request(tmp.path(), &reply_args);
    assert!(second_reply.status.success());
    let second_reply: serde_json::Value = serde_json::from_slice(&second_reply.stdout).unwrap();
    assert_eq!(second_reply["deduplicated"], true);
    assert_eq!(second_reply["filename"], first_reply["filename"]);

    let principal_inbox = tmp.path().join("principals/h/example-ci/resources/inbox");
    assert_eq!(st2::message::list_dir(&principal_inbox).unwrap().len(), 1);

    let replied = request(
        tmp.path(),
        &[
            "status",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "repair:42",
            "--json",
        ],
    );
    assert!(
        replied.status.success(),
        "{}",
        String::from_utf8_lossy(&replied.stderr)
    );
    let replied: serde_json::Value = serde_json::from_slice(&replied.stdout).unwrap();
    assert_eq!(replied["status"], "replied");
    assert_eq!(replied["idempotencyKey"], "repair:42");
    assert_eq!(replied["requestFilename"], filename);
    assert_eq!(replied["from"], "h.worker");
    assert_eq!(
        replied["tags"],
        serde_json::json!({"outcome": "needs-human"})
    );
    assert_eq!(
        replied["body"],
        serde_json::json!({"outcome": "needs-human"})
    );
}

#[test]
fn request_status_propagates_non_not_found_message_directory_errors() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");

    let sent = request(
        tmp.path(),
        &[
            "send",
            "h.worker",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "repair:42",
            "-m",
            "{}",
            "--json",
        ],
    );
    assert!(sent.status.success());

    let inbox = tmp.path().join("principals/h/example-ci/resources/inbox");
    fs::create_dir_all(inbox.parent().unwrap()).unwrap();
    fs::write(&inbox, "not a directory").unwrap();
    assert!(inbox.is_file());

    let status = request(
        tmp.path(),
        &[
            "status",
            "--as",
            "h.example-ci",
            "--idempotency-key",
            "repair:42",
            "--json",
        ],
    );

    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("Not a directory"));
}

#[test]
fn concurrent_replays_publish_exactly_one_request() {
    let tmp = tempfile::tempdir().unwrap();
    declare_agent(tmp.path(), "worker");
    declare_principal(tmp.path(), "example-ci");
    let root = tmp.path().to_path_buf();

    let threads: Vec<_> = (0..12)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                request(
                    &root,
                    &[
                        "send",
                        "h.worker",
                        "--as",
                        "h.example-ci",
                        "--idempotency-key",
                        "same",
                        "-m",
                        "{}",
                        "--json",
                    ],
                )
            })
        })
        .collect();

    let mut filenames = Vec::new();
    for thread in threads {
        let output = thread.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        filenames.push(output["filename"].as_str().unwrap().to_string());
    }
    filenames.sort();
    filenames.dedup();
    assert_eq!(filenames.len(), 1);
    assert_eq!(
        st2::message::list_dir(&root.join("h/worker/resources/inbox"))
            .unwrap()
            .len(),
        1
    );
}
