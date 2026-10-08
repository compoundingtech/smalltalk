//! CLI coverage for message-list filters and output modes.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use sha2::{Digest as _, Sha256};

fn json_digest(value: &serde_json::Value) -> String {
    bytes_digest(&serde_json::to_vec(value).unwrap())
}

fn bytes_digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_message(inbox: &Path, ts_ms: u64, suffix: &str, from: &str) {
    fs::create_dir_all(inbox).unwrap();
    fs::write(
        inbox.join(format!("{ts_ms:013}-{suffix}.md")),
        format!("---\nfrom: {from}\nsubject: test\n---\nbody\n"),
    )
    .unwrap();
}

fn write_agent(root: &Path, identity: &str) {
    let directory = root.join("h").join(identity);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("agent.kdl"),
        format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"h\"\n  type \"service\"\n  pty \"agent\" {{ command \"x\" }}\n}}\n"
        ),
    )
    .unwrap();
}

fn list_identity(root: &Path, identity: &str, extra: &[&str]) -> std::process::Output {
    Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "ls", identity, "--root"])
        .arg(root)
        .args(["--host", "h"])
        .args(extra)
        .output()
        .unwrap()
}

fn list(root: &Path, extra: &[&str]) -> std::process::Output {
    list_identity(root, "bob", extra)
}

fn send_message(
    root: &Path,
    from: &str,
    to: &str,
    body: &str,
    extra: &[&str],
) -> std::process::Output {
    Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", to, "--root"])
        .arg(root)
        .args(["--host", "h", "--as", from, "-m", body])
        .args(extra)
        .output()
        .unwrap()
}

fn sent(root: &Path, identity: &str, extra: &[&str]) -> std::process::Output {
    Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "sent", identity, "--root"])
        .arg(root)
        .args(["--host", "h", "--as", identity])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn send_rejects_empty_explicit_and_stdin_bodies_without_persisting_a_message() {
    for body_arg in [Some(""), None] {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "sender");
        write_agent(tmp.path(), "recipient");

        let mut command = Command::new(test_env!("CARGO_BIN_EXE_st2"));
        command
            .args(["message", "send", "recipient", "--root"])
            .arg(tmp.path())
            .args(["--host", "h", "--as", "sender"])
            .stdin(Stdio::null());
        if let Some(body) = body_arg {
            command.args(["-m", body]);
        }

        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("message body must not be empty"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!tmp.path().join("h/recipient/resources").exists());
        assert!(!tmp.path().join("h/sender/resources").exists());
    }
}

#[test]
fn event_metadata_is_exposed_by_list_and_read_json() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "bob");
    let inbox = tmp.path().join("h/bob/resources/inbox");
    fs::create_dir_all(&inbox).unwrap();
    let filename = "1785000000000-abcdef.md";
    fs::write(
        inbox.join(filename),
        "---\nfrom: h.bob/gh-ci\nsubject: CI result\nstream: gh-ci\nevent-id: run-812\nkey: main\n---\npayload\n",
    )
    .unwrap();

    let listed = list(tmp.path(), &["--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed[0]["stream"], "gh-ci");
    assert_eq!(listed[0]["eventId"], "run-812");
    assert_eq!(listed[0]["eventKey"], "main");

    let read = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "read", "bob", filename, "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--json"])
        .output()
        .unwrap();
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["stream"], "gh-ci");
    assert_eq!(read["eventId"], "run-812");
    assert_eq!(read["eventKey"], "main");
}

#[test]
fn uninitialized_sent_history_is_explicitly_unavailable_not_a_complete_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");

    let output = sent(tmp.path(), "h.sender", &["--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "coverage": { "_tag": "unavailable" },
            "messages": [],
        })
    );
    assert!(tmp.path().join("h/sender/resources/sent/.lock").is_file());

    let count = sent(tmp.path(), "h.sender", &["--count"]);
    assert!(!count.status.success());
    assert!(count.stdout.is_empty());
    assert!(String::from_utf8_lossy(&count.stderr).contains("coverage is unavailable"));
}

#[test]
fn send_persists_canonical_sender_history_independent_of_every_recipient_box() {
    let tmp = tempfile::tempdir().unwrap();
    for identity in ["sender", "recipient", "unrelated"] {
        write_agent(tmp.path(), identity);
    }
    assert!(!tmp.path().join("h/sender/resources").exists());

    let output = send_message(
        tmp.path(),
        "sender",
        "recipient",
        "durable body",
        &["--subject", "indexed"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let filename = String::from_utf8(output.stdout).unwrap().trim().to_string();
    let delivered =
        st2::message::read_msg(&tmp.path().join("h/recipient/resources/inbox"), &filename).unwrap();
    assert_eq!(delivered.from.as_deref(), Some("h.sender"));

    fs::remove_file(
        tmp.path()
            .join("h/recipient/resources/inbox")
            .join(&filename),
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("h/unrelated/resources")).unwrap();
    fs::write(
        tmp.path().join("h/unrelated/resources/inbox"),
        "not a directory",
    )
    .unwrap();

    let output = sent(
        tmp.path(),
        "sender",
        &["--to", "h.recipient", "--include-body", "--json"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["coverage"]["_tag"], "since");
    assert!(value["coverage"]["since"].as_u64().is_some());
    assert_eq!(value["messages"].as_array().unwrap().len(), 1);
    assert_eq!(value["messages"][0]["filename"], filename);
    assert_eq!(value["messages"][0]["to"], "h.recipient");
    assert_eq!(value["messages"][0]["body"], "durable body\n");
    assert!(value["messages"][0].get("from").is_none());
}

#[test]
fn replies_are_indexed_with_the_canonical_recipient_and_thread_relation() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");

    let original = send_message(tmp.path(), "recipient", "sender", "question", &[]);
    assert!(original.status.success());
    let original = String::from_utf8(original.stdout)
        .unwrap()
        .trim()
        .to_string();
    let reply = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "reply", &original, "--root"])
        .arg(tmp.path())
        .args([
            "--host",
            "h",
            "--as",
            "sender",
            "--idempotency-key",
            "answer-once",
            "-m",
            "answer",
        ])
        .output()
        .unwrap();
    assert!(
        reply.status.success(),
        "{}",
        String::from_utf8_lossy(&reply.stderr)
    );

    let output = sent(tmp.path(), "sender", &["--include-body", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["messages"].as_array().unwrap().len(), 1);
    assert_eq!(value["messages"][0]["to"], "h.recipient");
    assert_eq!(value["messages"][0]["inReplyTo"], original);
    assert_eq!(value["messages"][0]["idempotencyKey"], "answer-once");
}

#[test]
fn keyed_reply_retry_reads_an_archived_source_and_returns_the_committed_reply() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");

    let original = send_message(tmp.path(), "recipient", "sender", "question", &[]);
    assert!(original.status.success());
    let original = String::from_utf8(original.stdout)
        .unwrap()
        .trim()
        .to_string();
    let first = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "reply", &original, "--root"])
        .arg(tmp.path())
        .args([
            "--host",
            "h",
            "--as",
            "sender",
            "--idempotency-key",
            "answer-once",
            "-m",
            "answer",
        ])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "active-cleanup")
        .output()
        .unwrap();
    assert!(
        !first.status.success(),
        "first reply must simulate lost output"
    );
    let recipient_inbox = tmp.path().join("h/recipient/resources/inbox");
    let reply_filename = fs::read_dir(&recipient_inbox)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let archived = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "archive", &original, "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--as", "sender"])
        .output()
        .unwrap();
    assert!(archived.status.success());
    let retry = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "reply", &original, "--root"])
        .arg(tmp.path())
        .args([
            "--host",
            "h",
            "--as",
            "sender",
            "--idempotency-key",
            "answer-once",
            "-m",
            "answer",
        ])
        .output()
        .unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&retry.stdout).trim(),
        reply_filename
    );
    assert_eq!(fs::read_dir(recipient_inbox).unwrap().count(), 1);
    assert_eq!(
        String::from_utf8_lossy(&sent(tmp.path(), "sender", &["--count"]).stdout).trim(),
        "1"
    );
}

#[test]
fn keyed_retry_recovers_every_crash_boundary_without_false_sent_or_duplicates() {
    for crash_after in [
        "coverage",
        "pending",
        "active",
        "recipient",
        "row",
        "node",
        "head",
        "pending-cleanup",
        "active-cleanup",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "sender");
        write_agent(tmp.path(), "recipient");

        let failed = Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .args(["message", "send", "recipient", "--root"])
            .arg(tmp.path())
            .args([
                "--host",
                "h",
                "--as",
                "sender",
                "--idempotency-key",
                "retry-key",
                "-m",
                "one semantic message",
            ])
            .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", crash_after)
            .output()
            .unwrap();
        assert!(
            !failed.status.success(),
            "{crash_after} must inject failure"
        );

        let interrupted = sent(tmp.path(), "sender", &["--json"]);
        assert!(interrupted.status.success());
        let interrupted: serde_json::Value = serde_json::from_slice(&interrupted.stdout).unwrap();
        assert_ne!(interrupted["coverage"]["_tag"], "unavailable");
        if matches!(
            crash_after,
            "pending" | "active" | "recipient" | "row" | "node" | "head" | "pending-cleanup"
        ) {
            assert_eq!(interrupted["coverage"]["_tag"], "partial");
        }
        if crash_after == "recipient" {
            assert!(interrupted["messages"].as_array().unwrap().is_empty());
            assert_eq!(
                fs::read_dir(tmp.path().join("h/recipient/resources/inbox"))
                    .unwrap()
                    .count(),
                1,
                "the partial view must not hide recipient-only delivery"
            );
        }

        let first_retry = send_message(
            tmp.path(),
            "sender",
            "recipient",
            "one semantic message",
            &["--idempotency-key", "retry-key"],
        );
        assert!(
            first_retry.status.success(),
            "{crash_after}: {}",
            String::from_utf8_lossy(&first_retry.stderr)
        );
        let second_retry = send_message(
            tmp.path(),
            "sender",
            "recipient",
            "one semantic message",
            &["--idempotency-key", "retry-key"],
        );
        assert!(second_retry.status.success());
        assert_eq!(first_retry.stdout, second_retry.stdout);
        assert_eq!(
            fs::read_dir(tmp.path().join("h/recipient/resources/inbox"))
                .unwrap()
                .count(),
            1,
            "{crash_after} created duplicate recipient messages"
        );

        let recovered = sent(tmp.path(), "sender", &["--json"]);
        let recovered: serde_json::Value = serde_json::from_slice(&recovered.stdout).unwrap();
        assert_eq!(recovered["coverage"]["_tag"], "since");
        assert_eq!(recovered["messages"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn sent_ledger_fails_closed_when_head_nodes_or_rows_are_lost_substituted_or_invalid() {
    let prepare = || {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "sender");
        write_agent(tmp.path(), "recipient");
        let output = send_message(tmp.path(), "sender", "recipient", "durable", &[]);
        assert!(output.status.success());
        tmp
    };

    for mutate in [
        "missing-directory",
        "missing-row",
        "extra-row",
        "unexpected-row-entry",
        "payload-filename",
        "corrupt-row",
        "unreadable-row",
        "row-version",
    ] {
        let tmp = prepare();
        let messages = tmp.path().join("h/sender/resources/sent/messages");
        let row = fs::read_dir(&messages)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        match mutate {
            "missing-directory" => fs::remove_dir_all(&messages).unwrap(),
            "missing-row" => fs::remove_file(&row).unwrap(),
            "extra-row" => {
                let extra = messages.join("1700000000000-aaaaaa.md.json");
                let value = fs::read_to_string(&row).unwrap().replace(
                    serde_json::from_slice::<serde_json::Value>(&fs::read(&row).unwrap()).unwrap()
                        ["filename"]
                        .as_str()
                        .unwrap(),
                    "1700000000000-aaaaaa.md",
                );
                fs::write(extra, value).unwrap();
            }
            "unexpected-row-entry" => fs::write(messages.join("unexpected"), "junk").unwrap(),
            "payload-filename" => {
                fs::rename(&row, messages.join("1700000000000-aaaaaa.md.json")).unwrap();
            }
            "corrupt-row" => fs::write(&row, "not json").unwrap(),
            "unreadable-row" => {
                fs::remove_file(&row).unwrap();
                fs::create_dir(&row).unwrap();
            }
            "row-version" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&row).unwrap()).unwrap();
                value["version"] = 2.into();
                fs::write(&row, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let output = sent(tmp.path(), "sender", &["--json"]);
        assert!(!output.status.success(), "{mutate} must fail closed");
        assert!(output.stdout.is_empty());
    }

    for mutate in [
        "missing-node",
        "extra-node",
        "unexpected-node-entry",
        "corrupt-node",
        "unreadable-node",
        "node-version",
        "node-filename",
        "predecessor",
        "ordinal",
        "genesis",
    ] {
        let tmp = prepare();
        let commits = tmp.path().join("h/sender/resources/sent/commits");
        let node = fs::read_dir(&commits)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        match mutate {
            "missing-node" => fs::remove_file(&node).unwrap(),
            "extra-node" => {
                fs::copy(&node, commits.join("extra.json")).unwrap();
            }
            "unexpected-node-entry" => fs::write(commits.join("unexpected"), "junk").unwrap(),
            "corrupt-node" => fs::write(&node, "not json").unwrap(),
            "unreadable-node" => {
                fs::remove_file(&node).unwrap();
                fs::create_dir(&node).unwrap();
            }
            "node-version" | "node-filename" | "predecessor" | "ordinal" | "genesis" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&node).unwrap()).unwrap();
                match mutate {
                    "node-version" => value["version"] = 2.into(),
                    "node-filename" => value["filename"] = "../index".into(),
                    "predecessor" => value["previous"] = "substituted".into(),
                    "ordinal" => value["ordinal"] = 2.into(),
                    "genesis" => value["previous"] = "missing-genesis".into(),
                    _ => unreachable!(),
                }
                let digest = json_digest(&value);
                fs::remove_file(&node).unwrap();
                fs::write(
                    commits.join(format!("{digest}.json")),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
                let head = tmp.path().join("h/sender/resources/sent/index.json");
                let mut head_value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&head).unwrap()).unwrap();
                head_value["tip"] = digest.into();
                fs::write(head, serde_json::to_vec(&head_value).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let output = sent(tmp.path(), "sender", &["--json"]);
        assert!(!output.status.success(), "{mutate} must fail closed");
        assert!(output.stdout.is_empty());
    }

    for mutate in [
        "missing-index",
        "corrupt-index",
        "index-version",
        "unreadable-index",
        "tip-format",
        "count-mismatch",
        "rollback-head",
    ] {
        let tmp = prepare();
        let index = tmp.path().join("h/sender/resources/sent/index.json");
        match mutate {
            "missing-index" => fs::remove_file(&index).unwrap(),
            "corrupt-index" => fs::write(&index, "not json").unwrap(),
            "index-version" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
                value["version"] = 2.into();
                fs::write(&index, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "unreadable-index" => {
                fs::remove_file(&index).unwrap();
                fs::create_dir(&index).unwrap();
            }
            "tip-format" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
                value["tip"] = "../outside".into();
                fs::write(&index, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "count-mismatch" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
                value["count"] = 2.into();
                fs::write(&index, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "rollback-head" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
                value["count"] = 0.into();
                value["tip"] = serde_json::Value::Null;
                fs::write(&index, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let output = sent(tmp.path(), "sender", &["--json"]);
        assert!(!output.status.success(), "{mutate} must fail closed");
        assert!(output.stdout.is_empty());
        if mutate == "tip-format" {
            let retry = send_message(tmp.path(), "sender", "recipient", "next", &[]);
            assert!(
                !retry.status.success(),
                "invalid tip must fail before node lookup"
            );
        }
    }

    for mutate in [
        "missing-key",
        "extra-key",
        "corrupt-key",
        "key-filename",
        "unexpected-key-entry",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "sender");
        write_agent(tmp.path(), "recipient");
        let output = send_message(
            tmp.path(),
            "sender",
            "recipient",
            "durable",
            &["--idempotency-key", "stable"],
        );
        assert!(output.status.success());
        let keys = tmp.path().join("h/sender/resources/sent/keys");
        let key = fs::read_dir(&keys).unwrap().next().unwrap().unwrap().path();
        match mutate {
            "missing-key" => fs::remove_file(&key).unwrap(),
            "extra-key" => {
                fs::copy(&key, keys.join("extra.json")).unwrap();
            }
            "corrupt-key" => fs::write(&key, "not json").unwrap(),
            "key-filename" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&key).unwrap()).unwrap();
                value["filename"] = "../index".into();
                fs::write(&key, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "unexpected-key-entry" => fs::write(keys.join("unexpected"), "junk").unwrap(),
            _ => unreachable!(),
        }
        let output = sent(tmp.path(), "sender", &["--json"]);
        assert!(!output.status.success(), "{mutate} must fail closed");
        assert!(output.stdout.is_empty());
        if mutate == "key-filename" {
            let retry = send_message(
                tmp.path(),
                "sender",
                "recipient",
                "durable",
                &["--idempotency-key", "stable"],
            );
            assert!(
                !retry.status.success(),
                "key filename must fail before row lookup"
            );
        }
    }

    let tmp = prepare();
    for directory in ["pending", "messages", "commits", "keys"] {
        let path = tmp.path().join("h/sender/resources/sent").join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".message.tmp-999-0"), "interrupted atomic write").unwrap();
    }
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        output.status.success(),
        "atomic-write temporary siblings stay invisible"
    );

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    let interrupted = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", "recipient", "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--as", "sender", "-m", "original pending"])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "pending")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let pending = tmp.path().join("h/sender/resources/sent/pending");
    let pending_record = fs::read_dir(&pending)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&pending_record).unwrap()).unwrap();
    value["body"] = "substituted pending\n".into();
    value["renderedMessage"] = "---\nfrom: h.sender\n---\nsubstituted pending\n".into();
    fs::write(&pending_record, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "substituted pending intent must fail closed"
    );
    assert!(output.stdout.is_empty());
    let retry = send_message(tmp.path(), "sender", "recipient", "next", &[]);
    assert!(
        !retry.status.success(),
        "next sender operation must reject substituted pending"
    );

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    let interrupted = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", "recipient", "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--as", "sender", "-m", "original row"])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "row")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let messages = tmp.path().join("h/sender/resources/sent/messages");
    let row = fs::read_dir(&messages)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&row).unwrap()).unwrap();
    value["body"] = "substituted row\n".into();
    value["renderedMessage"] = "---\nfrom: h.sender\n---\nsubstituted row\n".into();
    fs::write(row, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "active-owned substituted row must fail closed"
    );
    assert!(output.stdout.is_empty());

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    let interrupted = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", "recipient", "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--as", "sender", "-m", "pending"])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "active")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let pending = tmp.path().join("h/sender/resources/sent/pending");
    fs::remove_file(
        fs::read_dir(pending)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "missing pending intent must fail closed"
    );
    assert!(output.stdout.is_empty());

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    let interrupted = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", "recipient", "--root"])
        .arg(tmp.path())
        .args(["--host", "h", "--as", "sender", "-m", "committed"])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "pending-cleanup")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let active = tmp.path().join("h/sender/resources/sent/active.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&active).unwrap()).unwrap();
    value["recordDigest"] = "substituted".into();
    fs::write(active, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "substituted active digest must fail closed"
    );
    assert!(output.stdout.is_empty());

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    let interrupted = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", "recipient", "--root"])
        .arg(tmp.path())
        .args([
            "--host",
            "h",
            "--as",
            "sender",
            "-m",
            "committed",
            "--idempotency-key",
            "committed-key",
        ])
        .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", "head")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let sent_root = tmp.path().join("h/sender/resources/sent");
    fs::remove_file(sent_root.join("active.json")).unwrap();
    let key = fs::read_dir(sent_root.join("keys"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(key).unwrap();

    // This test used to require failure here. That was wrong. The head, current tip, and immutable
    // row prove that the send completed. The message specification requires cleanup recovery.
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        output.status.success(),
        "an exact committed head-tip pending row is cleanup, not an incomplete send: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["coverage"]["_tag"], "since");
    assert_eq!(view["messages"].as_array().unwrap().len(), 1);
    let committed_filename = view["messages"][0]["filename"].as_str().unwrap();
    assert_eq!(fs::read_dir(sent_root.join("pending")).unwrap().count(), 1);
    assert!(
        !sent_root
            .join("keys")
            .read_dir()
            .unwrap()
            .any(|entry| entry.is_ok()),
        "the shared-lock read must not repair the missing key receipt"
    );

    let retry = send_message(
        tmp.path(),
        "sender",
        "recipient",
        "committed",
        &["--idempotency-key", "committed-key"],
    );
    assert!(
        retry.status.success(),
        "the next exclusive sender operation must finish cleanup: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&retry.stdout).trim(),
        committed_filename
    );
    assert_eq!(fs::read_dir(sent_root.join("pending")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(sent_root.join("keys")).unwrap().count(), 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &sent(tmp.path(), "sender", &["--json"]).stdout
        )
        .unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "cleanup recovery must not commit the same row twice"
    );

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    assert!(
        send_message(tmp.path(), "sender", "recipient", "committed", &[])
            .status
            .success()
    );
    let sent_root = tmp.path().join("h/sender/resources/sent");
    let row_path = fs::read_dir(sent_root.join("messages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let row = fs::read_to_string(row_path).unwrap();
    let substituted = row.replace("committed", "substituted committed body");
    assert_ne!(substituted, row);
    let substituted = substituted.into_bytes();
    fs::create_dir_all(sent_root.join("pending")).unwrap();
    fs::write(
        sent_root
            .join("pending")
            .join(format!("{}.json", bytes_digest(&substituted))),
        substituted,
    )
    .unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "a current-tip filename with different bytes must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("committed pending intent differs from head tip"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    assert!(
        send_message(tmp.path(), "sender", "recipient", "older", &[])
            .status
            .success()
    );
    assert!(
        send_message(tmp.path(), "sender", "recipient", "newer", &[])
            .status
            .success()
    );
    let messages = tmp.path().join("h/sender/resources/sent/messages");
    let mut rows = fs::read_dir(&messages)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    rows.sort();
    let pending = tmp.path().join("h/sender/resources/sent/pending");
    let older = fs::read(&rows[0]).unwrap();
    fs::write(
        pending.join(format!("{}.json", bytes_digest(&older))),
        older,
    )
    .unwrap();
    let output = sent(tmp.path(), "sender", &["--json"]);
    assert!(
        !output.status.success(),
        "older committed row cannot become pending again"
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("committed pending intent is missing its active marker")
    );
    let retry = send_message(tmp.path(), "sender", "recipient", "next", &[]);
    assert!(
        !retry.status.success(),
        "recovery must not recommit an older row"
    );
    assert!(
        String::from_utf8_lossy(&retry.stderr)
            .contains("committed pending intent is missing its active marker")
    );
}

#[test]
fn idempotency_key_scope_is_independent_across_senders_and_recipients() {
    let tmp = tempfile::tempdir().unwrap();
    for identity in ["sender-a", "sender-b", "recipient-a", "recipient-b"] {
        write_agent(tmp.path(), identity);
    }

    for (sender, recipient) in [
        ("sender-a", "recipient-a"),
        ("sender-a", "recipient-b"),
        ("sender-b", "recipient-a"),
    ] {
        let output = send_message(
            tmp.path(),
            sender,
            recipient,
            "same scoped operation",
            &["--idempotency-key", "shared"],
        );
        assert!(output.status.success(), "{sender} -> {recipient}");
    }
    assert_eq!(
        String::from_utf8_lossy(&sent(tmp.path(), "sender-a", &["--count"]).stdout).trim(),
        "2"
    );
    assert_eq!(
        String::from_utf8_lossy(&sent(tmp.path(), "sender-b", &["--count"]).stdout).trim(),
        "1"
    );
}

#[test]
fn unkeyed_retry_after_committed_response_loss_publishes_again() {
    for crash_after in ["head", "pending-cleanup", "active-cleanup"] {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "sender");
        write_agent(tmp.path(), "recipient");

        let failed = Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .args(["message", "send", "recipient", "--root"])
            .arg(tmp.path())
            .args(["--host", "h", "--as", "sender", "-m", "at least once"])
            .env("ST2_TEST_MESSAGE_SEND_FAIL_AFTER", crash_after)
            .output()
            .unwrap();
        assert!(!failed.status.success());
        let retry = send_message(tmp.path(), "sender", "recipient", "at least once", &[]);
        assert!(retry.status.success(), "{crash_after}");
        assert_eq!(
            String::from_utf8_lossy(&sent(tmp.path(), "sender", &["--count"]).stdout).trim(),
            "2",
            "{crash_after} must preserve unkeyed response ambiguity"
        );
    }
}

#[test]
fn idempotency_keys_reject_changed_content_while_unkeyed_identical_sends_stay_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");

    let first = send_message(tmp.path(), "sender", "recipient", "same", &[]);
    let second = send_message(tmp.path(), "sender", "recipient", "same", &[]);
    assert!(first.status.success());
    assert!(second.status.success());
    assert_ne!(first.stdout, second.stdout);

    let keyed = send_message(
        tmp.path(),
        "sender",
        "recipient",
        "original",
        &["--idempotency-key", "stable"],
    );
    assert!(keyed.status.success());
    let conflict = send_message(
        tmp.path(),
        "sender",
        "recipient",
        "different",
        &["--idempotency-key", "stable"],
    );
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("reused with different content"));

    let count = sent(tmp.path(), "sender", &["--to", "h.recipient", "--count"]);
    assert!(count.status.success());
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "3");
    let after = sent(
        tmp.path(),
        "sender",
        &["--since", &u64::MAX.to_string(), "--count"],
    );
    assert!(after.status.success());
    assert_eq!(String::from_utf8_lossy(&after.stdout).trim(), "0");
}

#[test]
fn external_eval_capability_bypasses_ordinary_sent_history_at_either_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "sender");
    write_agent(tmp.path(), "recipient");
    fs::create_dir_all(tmp.path().join("requester/inbox")).unwrap();

    for (from, to) in [("sender", "requester"), ("requester", "recipient")] {
        let output = Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .args(["message", "send", to, "--root"])
            .arg(tmp.path())
            .args(["--host", "h", "--as", from, "-m", "eval traffic"])
            .env("ST2_EVAL_REQUESTER", "requester")
            .output()
            .unwrap();
        assert!(output.status.success(), "{from} -> {to}");
    }

    let history = sent(tmp.path(), "sender", &["--json"]);
    assert!(history.status.success());
    let history: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history["coverage"]["_tag"], "unavailable");
    assert!(history["messages"].as_array().unwrap().is_empty());
}

#[test]
fn since_is_strict_and_composes_with_other_list_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("bob/inbox");
    write_message(&inbox, 1_700_000_000_000, "aaaaaa", "alice");
    write_message(&inbox, 1_700_000_000_001, "bbbbbb", "alice");
    write_message(&inbox, 1_700_000_000_002, "cccccc", "carol");

    let out = list(tmp.path(), &["--since", "1700000000000", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["ts"], 1_700_000_000_001_u64);
    assert_eq!(rows[1]["ts"], 1_700_000_000_002_u64);

    // Equality is excluded, and --since composes with the existing sender/count filters.
    let out = list(
        tmp.path(),
        &["--since", "1700000000001", "--from", "alice", "--count"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn archived_filename_wins_over_a_restored_raw_inbox_copy_in_all_list_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("bob/inbox");
    let archive = tmp.path().join("bob/archive");
    let filename = "1700000000000-aaaaaa.md";
    write_message(&inbox, 1_700_000_000_000, "aaaaaa", "alice");
    fs::create_dir_all(&archive).unwrap();
    fs::copy(inbox.join(filename), archive.join(filename)).unwrap();

    let out = list(tmp.path(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("# 0 messages in bob inbox"));
    assert!(
        !inbox.join(filename).exists(),
        "listing must clean the inbox file shadowed by the archive receipt"
    );

    let out = list(tmp.path(), &["--count"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");

    let out = list(tmp.path(), &["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[]");

    // The receipt remains visible when the operator explicitly lists the archive.
    let out = list(tmp.path(), &["--archive", "--count"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
}

#[test]
fn an_unknown_catalog_identity_fails_before_every_output_mode_reads_a_box() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "alice");

    // A populated flat box must not turn an unknown catalog identity into a valid result.
    write_message(
        &tmp.path().join("missing/inbox"),
        1_700_000_000_000,
        "aaaaaa",
        "alice",
    );
    write_message(
        &tmp.path().join("missing/archive"),
        1_700_000_000_001,
        "bbbbbb",
        "alice",
    );

    for extra in [
        vec![],
        vec!["--json"],
        vec!["--count"],
        vec!["--archive"],
        vec!["--archive", "--json"],
        vec!["--archive", "--count"],
    ] {
        let out = list_identity(tmp.path(), "missing", &extra);
        assert!(!out.status.success(), "mode {extra:?} must fail");
        assert!(
            out.stdout.is_empty(),
            "mode {extra:?} must fail before rendering output"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("no agent 'missing' found in catalog"));
        assert!(stderr.contains(&tmp.path().display().to_string()));
    }
}

#[test]
fn known_empty_native_and_catalog_less_flat_boxes_remain_valid() {
    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path(), "alice");
    for extra in [
        vec![],
        vec!["--json"],
        vec!["--count"],
        vec!["--archive", "--count"],
    ] {
        let out = list_identity(catalog.path(), "alice", &extra);
        assert!(
            out.status.success(),
            "known native mode {extra:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let flat = tempfile::tempdir().unwrap();
    write_message(
        &flat.path().join("bob/inbox"),
        1_700_000_000_000,
        "aaaaaa",
        "alice",
    );
    let out = list(flat.path(), &["--count"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
}

#[test]
fn send_routes_only_by_stable_identity_in_a_catalog_and_preserves_catalogless_bus() {
    let send = |root: &Path, recipient: &str, root_flag: &str, external: Option<&str>| {
        let mut command = Command::new(test_env!("CARGO_BIN_EXE_st2"));
        command
            .args(["message", "send", recipient, root_flag])
            .arg(root)
            .args(["--host", "h", "--as", "h.sender"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(identity) = external {
            command.env("ST2_EVAL_REQUESTER", identity);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"work\n").unwrap();
        child.wait_with_output().unwrap()
    };

    let catalog = tempfile::tempdir().unwrap();
    write_agent(catalog.path(), "worker");
    write_agent(catalog.path(), "sender");
    let declaration = catalog.path().join("h/worker/agent.kdl");
    fs::write(
        &declaration,
        fs::read_to_string(&declaration).unwrap().replace(
            "  type \"service\"\n",
            "  type \"service\"\n  name \"Shared Worker\"\n",
        ),
    )
    .unwrap();

    let display = send(catalog.path(), "Shared Worker", "--catalog", None);
    assert!(!display.status.success());
    assert!(
        String::from_utf8_lossy(&display.stderr)
            .contains("no agent 'Shared Worker' found in catalog")
    );
    assert!(!catalog.path().join("Shared Worker").exists());

    let stable = send(catalog.path(), "h.worker", "--catalog", None);
    assert!(
        stable.status.success(),
        "{}",
        String::from_utf8_lossy(&stable.stderr)
    );
    assert_eq!(
        fs::read_dir(catalog.path().join("h/worker/resources/inbox"))
            .unwrap()
            .count(),
        1
    );

    fs::create_dir_all(catalog.path().join("requester/inbox")).unwrap();
    assert!(
        !send(catalog.path(), "requester", "--catalog", None)
            .status
            .success()
    );
    assert!(
        !send(catalog.path(), "requester", "--catalog", Some("other"))
            .status
            .success()
    );
    let external = send(catalog.path(), "requester", "--catalog", Some("requester"));
    assert!(
        external.status.success(),
        "{}",
        String::from_utf8_lossy(&external.stderr)
    );
    assert_eq!(
        fs::read_dir(catalog.path().join("requester/inbox"))
            .unwrap()
            .count(),
        1
    );

    let flat = tempfile::tempdir().unwrap();
    let raw = send(flat.path(), "requester", "--root", None);
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    assert_eq!(
        fs::read_dir(flat.path().join("requester/inbox"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn orphan_mode_explicitly_reads_raw_flat_inbox_and_archive() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "alice");
    write_message(
        &tmp.path().join("missing/inbox"),
        1_700_000_000_000,
        "aaaaaa",
        "alice",
    );
    write_message(
        &tmp.path().join("missing/archive"),
        1_700_000_000_001,
        "bbbbbb",
        "alice",
    );

    let inbox = list_identity(tmp.path(), "missing", &["--orphan", "--json"]);
    assert!(
        inbox.status.success(),
        "{}",
        String::from_utf8_lossy(&inbox.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&inbox.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let archive = list_identity(tmp.path(), "missing", &["--orphan", "--archive", "--count"]);
    assert!(
        archive.status.success(),
        "{}",
        String::from_utf8_lossy(&archive.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&archive.stdout).trim(), "1");
}

#[test]
fn malformed_catalog_declarations_disable_implicit_flat_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let declaration = tmp.path().join("h/broken/agent.kdl");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::write(&declaration, "agent \"broken\" { this is not valid").unwrap();
    write_message(
        &tmp.path().join("missing/inbox"),
        1_700_000_000_000,
        "aaaaaa",
        "alice",
    );

    for extra in [
        vec![],
        vec!["--json"],
        vec!["--count"],
        vec!["--archive"],
        vec!["--archive", "--json"],
        vec!["--archive", "--count"],
    ] {
        let out = list_identity(tmp.path(), "missing", &extra);
        assert!(!out.status.success(), "mode {extra:?} must fail");
        assert!(
            out.stdout.is_empty(),
            "mode {extra:?} must fail before rendering output"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("no agent 'missing' found in catalog")
        );
    }
}
