use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use st2::event::{self, EventReceiptStatus, RING_CAPACITY};
use st2::message;

static EVENT_FAIL_ENV: Mutex<()> = Mutex::new(());

fn declare_agent(root: &Path, desired: &str, streams: &str) -> PathBuf {
    let directory = root.join("agents/hetz/worker");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("agent.kdl"),
        format!(
            "agent \"worker\" {{\n  host \"hetz\"\n  desired-state {desired}\n  command \"agent\"\n{streams}}}\n"
        ),
    )
    .unwrap();
    event::publish_owner_binding_for_test(root, "hetz").unwrap();
    directory
}

fn emit(root: &Path, id: &str, key: Option<&str>, supersede: bool) -> event::EventReceipt {
    event::emit(
        root,
        "hetz",
        "hetz.worker",
        "gh-ci",
        id,
        key,
        Some(&format!("CI {id}")),
        &format!("{{\"id\":\"{id}\"}}"),
        supersede,
    )
    .unwrap()
}

#[test]
fn stable_event_identity_publishes_exactly_one_canonical_message() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");

    let first = emit(catalog.path(), "run-812", None, false);
    let replay = emit(catalog.path(), "run-812", None, false);

    assert_eq!(first.status, EventReceiptStatus::Created);
    assert_eq!(replay.status, EventReceiptStatus::Deduplicated);
    assert_eq!(first.filename, replay.filename);
    let inbox = message::list_inbox(&message::inbox_dir(&agent)).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].from.as_deref(), Some("hetz.worker/gh-ci"));
    assert_eq!(inbox[0].stream.as_deref(), Some("gh-ci"));
    assert_eq!(inbox[0].event_id.as_deref(), Some("run-812"));
    assert!(!agent.join("resources/sent").exists());
}

#[test]
fn public_emit_requires_a_declared_stream_even_for_reserved_resync() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "");

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "resync",
        "forged-resync",
        Some("goal"),
        Some("resource goal changed"),
        "forged carrier notice",
        true,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("does not declare stream 'resync'"),
        "{error:#}"
    );
    assert!(!message::inbox_dir(&agent).exists());
    assert!(!agent.join("resources/streams/resync").exists());
}

#[test]
fn completed_event_replay_rejects_changed_supersession_intent() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    emit(catalog.path(), "stable-intent", Some("pr-1"), false);

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "stable-intent",
        Some("pr-1"),
        Some("CI stable-intent"),
        "{\"id\":\"stable-intent\"}",
        true,
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("different supersession intent"),
        "{error:#}"
    );
}

#[test]
fn durable_successor_failure_retains_replayable_pending_receipt() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "durable-successor:durable") };
    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "durable-successor",
        None,
        None,
        "durable",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    assert!(
        error.to_string().contains("failure at durable"),
        "{error:#}"
    );
    assert!(
        message::list_inbox(&message::inbox_dir(&agent))
            .unwrap()
            .iter()
            .any(|message| { message.event_id.as_deref() == Some("durable-successor") })
    );

    let replay = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "durable-successor",
        None,
        None,
        "durable",
        false,
    )
    .unwrap();
    assert_eq!(replay.status, EventReceiptStatus::Deduplicated);
}

#[test]
fn concurrent_replays_publish_exactly_one_event() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let root = Arc::new(catalog.path().to_path_buf());
    let barrier = Arc::new(Barrier::new(12));
    let threads = (0..12)
        .map(|_| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                emit(&root, "delivery-1", None, false)
            })
        })
        .collect::<Vec<_>>();
    let receipts = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.status == EventReceiptStatus::Created)
            .count(),
        1
    );
    assert!(
        receipts
            .windows(2)
            .all(|pair| pair[0].filename == pair[1].filename)
    );
    assert_eq!(
        message::list_inbox(&message::inbox_dir(&agent))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn conflicting_reuse_and_undeclared_or_suspended_ingress_fail_closed() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    emit(catalog.path(), "same", None, false);
    let conflict = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "same",
        None,
        Some("different"),
        "different",
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        conflict.contains("reused with different content"),
        "{conflict}"
    );
    let undeclared = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "other",
        "1",
        None,
        None,
        "x",
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        undeclared.contains("does not declare stream 'other'"),
        "{undeclared}"
    );

    fs::write(
        agent.join("agent.kdl"),
        "agent \"worker\" {\n  host \"hetz\"\n  desired-state \"suspended\" reason=\"hold\"\n  command \"agent\"\n  stream \"gh-ci\" {}\n}\n",
    )
    .unwrap();
    let suspended = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "2",
        None,
        None,
        "x",
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(suspended.contains("eyes are closed"), "{suspended}");
    assert_eq!(
        message::list_inbox(&message::inbox_dir(&agent))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn ambiguous_recipient_matching_a_bus_id_and_local_identity_fails_closed() {
    let catalog = tempfile::tempdir().unwrap();
    let canonical = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let ambiguous = catalog.path().join("agents/hetz/ambiguous");
    fs::create_dir_all(&ambiguous).unwrap();
    fs::write(
        ambiguous.join("agent.kdl"),
        "agent \"hetz.worker\" {\n  host \"hetz\"\n  desired-state \"running\"\n  command \"agent\"\n  stream \"gh-ci\" {}\n}\n",
    )
    .unwrap();
    event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "ambiguous",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err()
    .to_string();

    assert!(
        error.contains("recipient 'hetz.worker' is ambiguous"),
        "{error}"
    );
    assert!(!message::inbox_dir(&canonical).exists());
    assert!(!message::inbox_dir(&ambiguous).exists());
}

#[test]
fn exact_remote_bus_id_cannot_bypass_the_owner_host_lock_domain() {
    let catalog = tempfile::tempdir().unwrap();
    let remote = catalog.path().join("agents/berlin/worker");
    fs::create_dir_all(&remote).unwrap();
    fs::write(
        remote.join("agent.kdl"),
        "agent \"worker\" {\n  host \"berlin\"\n  desired-state \"running\"\n  command \"agent\"\n  stream \"gh-ci\" {}\n}\n",
    )
    .unwrap();
    event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "berlin.worker",
        "gh-ci",
        "remote-attempt",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("event publication must run on that host"),
        "{error:#}"
    );
    assert!(!message::inbox_dir(&remote).exists());
    assert!(!remote.join("resources/streams/gh-ci").exists());
}

#[test]
fn public_host_flag_cannot_override_detected_event_authority() {
    let catalog = tempfile::tempdir().unwrap();
    let remote = catalog.path().join("agents/berlin/worker");
    fs::create_dir_all(&remote).unwrap();
    fs::write(
        remote.join("agent.kdl"),
        "agent \"worker\" { host \"berlin\"; command \"agent\"; stream \"gh-ci\" {} }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            catalog.path().to_str().unwrap(),
            "event",
            "emit",
            "berlin.worker",
            "--stream",
            "gh-ci",
            "--event-id",
            "forged-host",
            "--message",
            "payload",
            "--host",
            "berlin",
        ])
        .env("ST2_TEST_EVENT_HOST", "hetz")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no active local stream owner binding"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!message::inbox_dir(&remote).exists());
}

#[test]
fn established_logical_host_alias_admits_owner_local_cli_ingress() {
    let catalog = tempfile::tempdir().unwrap();
    let owner = catalog.path().join("agents/berlin/worker");
    fs::create_dir_all(&owner).unwrap();
    fs::write(
        owner.join("agent.kdl"),
        "agent \"worker\" { host \"berlin\"; command \"agent\"; stream \"gh-ci\" {} }\n",
    )
    .unwrap();
    event::publish_owner_binding_for_test(catalog.path(), "berlin").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            catalog.path().to_str().unwrap(),
            "event",
            "emit",
            "berlin.worker",
            "--stream",
            "gh-ci",
            "--event-id",
            "alias-owned",
            "--message",
            "payload",
            "--host",
            "berlin",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        message::list_inbox(&message::inbox_dir(&owner))
            .unwrap()
            .len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn unobservable_declaration_entry_blocks_event_recipient_resolution() {
    use std::os::unix::fs::symlink;

    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    symlink(
        catalog.path().join("missing-agent.kdl"),
        catalog.path().join("concealed-agent.kdl"),
    )
    .unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "strict-discovery",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("catalog has errors"), "{error}");
    assert!(error.contains("unobservable declaration entry"), "{error}");
    assert!(!message::inbox_dir(&agent).exists());
}

#[cfg(unix)]
#[test]
fn symlinked_stream_state_ancestor_cannot_escape_the_agent_capability() {
    use std::os::unix::fs::symlink;

    let catalog = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    fs::create_dir_all(agent.join("resources")).unwrap();
    symlink(outside.path(), agent.join("resources/streams")).unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "escape-state",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err()
    .to_string();

    assert!(!error.is_empty());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn predictable_stream_state_temporary_symlink_is_never_followed() {
    use std::os::unix::fs::symlink;

    let catalog = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let state_dir = agent.join("resources/streams/gh-ci");
    fs::create_dir_all(&state_dir).unwrap();
    let victim = outside.path().join("victim");
    fs::write(&victim, "must remain unchanged").unwrap();
    for counter in 0..4096 {
        symlink(
            &victim,
            state_dir.join(format!(".state.tmp-{}-{counter}", std::process::id())),
        )
        .unwrap();
    }

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "temp-symlink",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("fresh stream state temporary"), "{error}");
    assert_eq!(fs::read_to_string(victim).unwrap(), "must remain unchanged");
    assert!(!state_dir.join("state.json").exists());
}

#[cfg(unix)]
#[test]
fn stream_state_symlink_and_fifo_fail_without_following_or_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::symlink;

    for entry in ["symlink", "fifo"] {
        let catalog = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
        let state_dir = agent.join("resources/streams/gh-ci");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("state.json");
        if entry == "symlink" {
            let victim = outside.path().join("victim");
            fs::write(&victim, "must not be read").unwrap();
            symlink(&victim, &state_path).unwrap();
        } else {
            let path = CString::new(state_path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }

        let error = event::emit(
            catalog.path(),
            "hetz",
            "hetz.worker",
            "gh-ci",
            entry,
            None,
            None,
            "payload",
            false,
        )
        .unwrap_err();

        assert!(!error.to_string().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_inbox_cannot_escape_the_agent_capability() {
    use std::os::unix::fs::symlink;

    let catalog = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    fs::create_dir_all(agent.join("resources")).unwrap();
    symlink(outside.path(), agent.join("resources/inbox")).unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "escape-inbox",
        None,
        None,
        "payload",
        false,
    )
    .unwrap_err()
    .to_string();

    assert!(!error.is_empty());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn supersede_collapses_only_the_matching_key_and_preserves_archive_receipts() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let pr_1_old = emit(catalog.path(), "pr1-fail", Some("pr-1"), true);
    let pr_2 = emit(catalog.path(), "pr2-fail", Some("pr-2"), true);
    let pr_1_new = emit(catalog.path(), "pr1-pass", Some("pr-1"), true);

    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    assert!(!inbox.join(&pr_1_old.filename).exists());
    assert!(archive.join(&pr_1_old.filename).exists());
    assert!(inbox.join(&pr_2.filename).exists());
    assert!(inbox.join(&pr_1_new.filename).exists());
    assert_eq!(
        pr_1_new.superseded.as_deref(),
        Some(pr_1_old.filename.as_str())
    );
    let replay = emit(catalog.path(), "pr1-fail", Some("pr-1"), true);
    assert_eq!(replay.status, EventReceiptStatus::Deduplicated);
    assert!(!inbox.join(&pr_1_old.filename).exists());
}

#[test]
fn supersede_skips_an_archived_head_and_retires_the_latest_unread_predecessor() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let older = emit(catalog.path(), "pr1-queued", Some("pr-1"), false);
    let archived_head = emit(catalog.path(), "pr1-running", Some("pr-1"), false);
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    message::archive_msg(&inbox, &archive, &archived_head.filename).unwrap();

    let successor = emit(catalog.path(), "pr1-pass", Some("pr-1"), true);

    assert_eq!(
        successor.superseded.as_deref(),
        Some(older.filename.as_str())
    );
    assert!(!inbox.join(&older.filename).exists());
    assert!(archive.join(&older.filename).exists());
    assert!(archive.join(&archived_head.filename).exists());
    assert!(inbox.join(&successor.filename).exists());
}

#[test]
fn supersede_skips_an_archive_shadowed_inbox_candidate() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let older = emit(catalog.path(), "pr1-queued", Some("pr-1"), false);
    let shadowed = emit(catalog.path(), "pr1-running", Some("pr-1"), false);
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    fs::create_dir_all(&archive).unwrap();
    fs::copy(
        inbox.join(&shadowed.filename),
        archive.join(&shadowed.filename),
    )
    .unwrap();

    let successor = emit(catalog.path(), "pr1-pass", Some("pr-1"), true);

    assert_eq!(
        successor.superseded.as_deref(),
        Some(older.filename.as_str())
    );
    assert!(inbox.join(&shadowed.filename).is_file());
    assert!(archive.join(&shadowed.filename).is_file());
    assert!(archive.join(&older.filename).is_file());
}

#[test]
fn initial_supersession_authenticates_predecessor_immediately_before_archive() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    let inbox = message::inbox_dir(&agent);
    fs::write(inbox.join(&predecessor.filename), "forged predecessor").unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "initial-passed",
        Some("pr-1"),
        Some("initial passed"),
        "initial passed",
        true,
    )
    .unwrap_err();

    assert!(error.to_string().contains("different bytes"), "{error:#}");
    assert!(inbox.join(&predecessor.filename).is_file());
    assert!(
        !message::archive_dir(&agent)
            .join(&predecessor.filename)
            .exists()
    );
    assert!(
        message::list_inbox(&inbox)
            .unwrap()
            .iter()
            .any(|message| { message.event_id.as_deref() == Some("initial-passed") })
    );
}

#[test]
fn predecessor_replacement_after_validation_cannot_forge_archive_receipt() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "race-running", Some("pr-1"), false);
    let inbox = message::inbox_dir(&agent);
    let predecessor_path = inbox.join(&predecessor.filename);
    let authentic = fs::read(&predecessor_path).unwrap();
    let ready = catalog.path().join("archive-ready");
    let release = catalog.path().join("archive-release");
    unsafe {
        std::env::set_var("ST2_TEST_EVENT_ARCHIVE_READY", &ready);
        std::env::set_var("ST2_TEST_EVENT_ARCHIVE_RELEASE", &release);
    }
    let root = catalog.path().to_path_buf();
    let publish = std::thread::spawn(move || {
        event::emit(
            &root,
            "hetz",
            "hetz.worker",
            "gh-ci",
            "race-passed",
            Some("pr-1"),
            None,
            "passed",
            true,
        )
    });
    for _ in 0..500 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        ready.exists(),
        "publisher never reached validated archive checkpoint"
    );
    let displaced = inbox.join("displaced-authentic-predecessor");
    fs::rename(&predecessor_path, &displaced).unwrap();
    fs::write(&predecessor_path, "forged after validation").unwrap();
    fs::write(&release, b"release").unwrap();
    let receipt = publish.join().unwrap().unwrap();
    unsafe {
        std::env::remove_var("ST2_TEST_EVENT_ARCHIVE_READY");
        std::env::remove_var("ST2_TEST_EVENT_ARCHIVE_RELEASE");
    }

    assert_eq!(receipt.status, EventReceiptStatus::Created);
    assert_eq!(
        fs::read(message::archive_dir(&agent).join(&predecessor.filename)).unwrap(),
        authentic
    );
    assert_eq!(
        fs::read(&predecessor_path).unwrap(),
        b"forged after validation",
        "conditional unlink must preserve a replacement directory entry"
    );
    fs::remove_file(displaced).unwrap();
}

#[test]
fn catalog_authoring_and_emit_linearize_without_deadlock() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let admission = st2::CatalogLock::shared(catalog.path()).unwrap();
    let root = catalog.path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    let author = std::thread::spawn(move || {
        let result = st2::agent_author::remove_stream(&root, "hetz.worker", "hetz", None, "gh-ci");
        done_tx.send(result).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());

    let receipt = emit(catalog.path(), "before-remove", None, false);
    assert_eq!(receipt.status, EventReceiptStatus::Created);
    drop(admission);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    author.join().unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "after-remove",
        None,
        None,
        "after",
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("does not declare stream"),
        "{error:#}"
    );
}

#[test]
fn desired_state_authoring_and_emit_linearize_without_deadlock() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let admission = st2::CatalogLock::shared(catalog.path()).unwrap();
    let root = catalog.path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    let author = std::thread::spawn(move || {
        let result = st2::agent_author::set_desired_state(
            &root,
            "hetz.worker",
            "hetz",
            None,
            st2::agent_author::DesiredStateValue::Suspended,
            Some("maintenance"),
            None,
        );
        done_tx.send(result).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());

    assert_eq!(
        emit(catalog.path(), "before-suspend", None, false).status,
        EventReceiptStatus::Created
    );
    drop(admission);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    author.join().unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "after-suspend",
        None,
        None,
        "after",
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("is suspended"), "{error:#}");
}

#[test]
fn invalid_archive_receipt_blocks_supersession_before_successor_publication() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "pr1-running", Some("pr-1"), false);
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    fs::create_dir_all(archive.join(&predecessor.filename)).unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "pr1-pass",
        Some("pr-1"),
        Some("CI pr1-pass"),
        "{\"id\":\"pr1-pass\"}",
        true,
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("not a real regular file"),
        "{error:#}"
    );
    let unread = message::list_inbox(&inbox).unwrap();
    assert_eq!(unread.len(), 1);
    assert!(
        unread
            .iter()
            .all(|message| message.event_id.as_deref() != Some("pr1-pass"))
    );
    assert!(inbox.join(&predecessor.filename).exists());

    fs::remove_dir(archive.join(&predecessor.filename)).unwrap();
    let replay = emit(catalog.path(), "pr1-pass", Some("pr-1"), true);
    assert_eq!(replay.status, EventReceiptStatus::Created);
    assert_eq!(
        replay.superseded.as_deref(),
        Some(predecessor.filename.as_str())
    );
    assert_eq!(message::list_inbox(&inbox).unwrap().len(), 1);
    assert!(!inbox.join(&predecessor.filename).exists());
    assert!(archive.join(&predecessor.filename).is_file());
}

#[test]
fn a_different_event_reconciles_both_pending_crash_windows() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let inbox = message::inbox_dir(&agent);

    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "reserved-a:pending") };
    let before_materialization = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "reserved-a",
        None,
        Some("reserved A"),
        "reserved A",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    assert!(
        before_materialization
            .to_string()
            .contains("injected event failure at pending")
    );
    assert!(message::list_inbox(&inbox).unwrap().is_empty());

    let after_abandoned = emit(catalog.path(), "after-abandoned", None, false);
    assert_eq!(after_abandoned.status, EventReceiptStatus::Created);

    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "materialized-a:materialized") };
    let after_materialization = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "materialized-a",
        Some("pr-1"),
        Some("materialized A"),
        "materialized A",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    assert!(
        after_materialization
            .to_string()
            .contains("injected event failure at materialized")
    );
    let materialized_filename = message::list_inbox(&inbox)
        .unwrap()
        .into_iter()
        .find(|message| message.event_id.as_deref() == Some("materialized-a"))
        .unwrap()
        .filename;

    let after_materialized = emit(catalog.path(), "after-materialized", None, false);
    assert_eq!(after_materialized.status, EventReceiptStatus::Created);
    let replay = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "materialized-a",
        Some("pr-1"),
        Some("materialized A"),
        "materialized A",
        false,
    )
    .unwrap();
    assert_eq!(replay.status, EventReceiptStatus::Deduplicated);
    assert_eq!(replay.filename, materialized_filename);
    assert_eq!(
        message::list_inbox(&inbox)
            .unwrap()
            .into_iter()
            .filter(|message| message.event_id.as_deref() == Some("materialized-a"))
            .count(),
        1
    );
}

#[test]
fn pending_reconciliation_rejects_corrupt_and_forged_reserved_files() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "reserved:pending") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "reserved",
        Some("pr-1"),
        Some("reserved"),
        "reserved",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };

    let state_path = agent.join("resources/streams/gh-ci/state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let filename = state["pending"]["filename"].as_str().unwrap().to_owned();
    let inbox = message::inbox_dir(&agent);
    fs::create_dir_all(&inbox).unwrap();
    let forged = event::render_event(
        "hetz.worker/gh-ci",
        Some("forged"),
        "gh-ci",
        "different",
        Some("pr-1"),
        "forged",
    );
    fs::write(inbox.join(&filename), &forged).unwrap();

    let corrupt = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "next",
        None,
        None,
        "next",
        false,
    )
    .unwrap_err();
    assert!(
        corrupt.to_string().contains("different bytes"),
        "{corrupt:#}"
    );

    state["pending"]["renderedSha256"] =
        serde_json::Value::String(format!("{:x}", Sha256::digest(forged.as_bytes())));
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let forged_identity = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "next",
        None,
        None,
        "next",
        false,
    )
    .unwrap_err();
    assert!(
        forged_identity
            .to_string()
            .contains("different event identity"),
        "{forged_identity:#}"
    );
    assert!(
        message::list_inbox(&inbox)
            .unwrap()
            .iter()
            .all(|message| message.event_id.as_deref() != Some("next"))
    );
}

#[cfg(unix)]
#[test]
fn pending_reconciliation_rejects_a_fifo_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "reserved-fifo:pending") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "reserved-fifo",
        None,
        None,
        "reserved",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };

    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(agent.join("resources/streams/gh-ci/state.json")).unwrap(),
    )
    .unwrap();
    let filename = state["pending"]["filename"].as_str().unwrap();
    let inbox = message::inbox_dir(&agent);
    fs::create_dir_all(&inbox).unwrap();
    let fifo = CString::new(inbox.join(filename).as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let started = std::time::Instant::now();
    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "next",
        None,
        None,
        "next",
        false,
    )
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        error.to_string().contains("not a real regular file"),
        "{error:#}"
    );
}

#[test]
fn a_different_event_completes_pending_successor_compaction() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "passed:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "passed",
        Some("pr-1"),
        Some("passed"),
        "passed",
        true,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let inbox = message::inbox_dir(&agent);
    assert!(inbox.join(&predecessor.filename).is_file());

    let next = emit(catalog.path(), "unrelated", Some("pr-2"), false);

    assert_eq!(next.status, EventReceiptStatus::Created);
    assert!(!inbox.join(&predecessor.filename).exists());
    assert!(
        message::archive_dir(&agent)
            .join(&predecessor.filename)
            .is_file()
    );
    let unread = message::list_inbox(&inbox).unwrap();
    assert_eq!(unread.len(), 2);
    assert!(
        unread
            .iter()
            .any(|message| message.event_id.as_deref() == Some("passed"))
    );
    assert!(
        unread
            .iter()
            .any(|message| message.event_id.as_deref() == Some("unrelated"))
    );
}

#[test]
fn pending_supersession_authenticates_its_predecessor_before_archive() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "passed:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "passed",
        Some("pr-1"),
        Some("passed"),
        "passed",
        true,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let inbox = message::inbox_dir(&agent);
    fs::write(inbox.join(&predecessor.filename), "forged predecessor").unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "unrelated",
        None,
        None,
        "unrelated",
        false,
    )
    .unwrap_err();

    assert!(error.to_string().contains("different bytes"), "{error:#}");
    assert!(inbox.join(&predecessor.filename).is_file());
    assert!(
        !message::archive_dir(&agent)
            .join(&predecessor.filename)
            .exists()
    );
    assert!(
        message::list_inbox(&inbox)
            .unwrap()
            .iter()
            .all(|message| message.event_id.as_deref() != Some("unrelated"))
    );
}

#[test]
fn pending_supersession_accepts_an_authenticated_archive_only_predecessor() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "passed:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "passed",
        Some("pr-1"),
        Some("passed"),
        "passed",
        true,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    message::archive_msg(&inbox, &archive, &predecessor.filename).unwrap();

    let next = emit(catalog.path(), "unrelated", Some("pr-2"), false);

    assert_eq!(next.status, EventReceiptStatus::Created);
    assert!(archive.join(&predecessor.filename).is_file());
    assert!(
        message::list_inbox(&inbox)
            .unwrap()
            .iter()
            .any(|message| { message.event_id.as_deref() == Some("passed") })
    );
}

#[test]
fn pending_supersession_recovery_unlinks_a_durably_archived_predecessor() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "passed:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "passed",
        Some("pr-1"),
        Some("passed"),
        "passed",
        true,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    fs::create_dir_all(&archive).unwrap();
    fs::hard_link(
        inbox.join(&predecessor.filename),
        archive.join(&predecessor.filename),
    )
    .unwrap();

    let next = emit(catalog.path(), "unrelated", Some("pr-2"), false);

    assert_eq!(next.status, EventReceiptStatus::Created);
    assert!(archive.join(&predecessor.filename).is_file());
    assert!(!inbox.join(&predecessor.filename).exists());
}

#[test]
fn archived_pending_event_still_authenticates_a_same_name_inbox_entry() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "reserved:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "reserved",
        Some("pr-1"),
        Some("reserved"),
        "reserved",
        false,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(agent.join("resources/streams/gh-ci/state.json")).unwrap(),
    )
    .unwrap();
    let filename = state["pending"]["filename"].as_str().unwrap();
    let inbox = message::inbox_dir(&agent);
    let archive = message::archive_dir(&agent);
    message::archive_msg(&inbox, &archive, filename).unwrap();
    fs::write(inbox.join(filename), b"forged same-name inbox entry").unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "next",
        None,
        None,
        "next",
        false,
    )
    .unwrap_err();

    assert!(error.to_string().contains("different bytes"), "{error:#}");
    assert_eq!(
        fs::read(inbox.join(filename)).unwrap(),
        b"forged same-name inbox entry"
    );
}

#[test]
fn pending_supersession_fails_closed_when_predecessor_has_no_receipt() {
    let _fail_env = EVENT_FAIL_ENV.lock().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let predecessor = emit(catalog.path(), "running", Some("pr-1"), false);
    unsafe { std::env::set_var("ST2_TEST_EVENT_FAIL_AT", "passed:materialized") };
    let _ = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "passed",
        Some("pr-1"),
        Some("passed"),
        "passed",
        true,
    )
    .unwrap_err();
    unsafe { std::env::remove_var("ST2_TEST_EVENT_FAIL_AT") };
    let inbox = message::inbox_dir(&agent);
    fs::remove_file(inbox.join(&predecessor.filename)).unwrap();

    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "unrelated",
        None,
        None,
        "unrelated",
        false,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("has no inbox file or archive receipt"),
        "{error:#}"
    );
    assert!(
        message::list_inbox(&inbox)
            .unwrap()
            .iter()
            .all(|message| { message.event_id.as_deref() != Some("unrelated") })
    );
}

#[test]
fn keyless_supersede_replaces_the_stream_wide_head() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let old = emit(catalog.path(), "old", Some("pr-1"), true);
    let new = emit(catalog.path(), "new", None, true);
    assert_eq!(new.superseded.as_deref(), Some(old.filename.as_str()));
    assert!(!message::inbox_dir(&agent).join(old.filename).exists());
}

#[test]
fn crash_replay_honors_an_archive_receipt_and_never_restores_the_inbox_copy() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let filename = "1784649988123-proof1.md";
    let rendered = event::render_event(
        "hetz.worker/gh-ci",
        Some("CI archived"),
        "gh-ci",
        "archived",
        None,
        "payload",
    );
    let archive = message::archive_dir(&agent);
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join(filename), &rendered).unwrap();
    let state_dir = agent.join("resources/streams/gh-ci");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "stream": "gh-ci",
            "recipient": "hetz.worker",
            "pending": {
                "eventId": "archived",
                "filename": filename,
                "key": null,
                "renderedSha256": format!("{:x}", Sha256::digest(rendered.as_bytes())),
                "supersede": false
            },
            "recent": []
        }))
        .unwrap(),
    )
    .unwrap();

    let receipt = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "archived",
        None,
        Some("CI archived"),
        "payload",
        false,
    )
    .unwrap();
    assert_eq!(receipt.status, EventReceiptStatus::Deduplicated);
    assert!(!message::inbox_dir(&agent).join(filename).exists());
    assert!(archive.join(filename).exists());
}

#[test]
fn subject_frontmatter_injection_is_refused_before_any_write() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let error = event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "gh-ci",
        "safe-id",
        None,
        Some("safe\nevent-id: forged"),
        "body",
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("event subject"), "{error}");
    assert!(!message::inbox_dir(&agent).exists());
    assert!(!agent.join("resources/streams").exists());
}

#[test]
fn stream_state_is_bounded_and_forgets_only_beyond_its_honest_horizon() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let first = emit(catalog.path(), "event-0", None, false);
    for index in 1..=RING_CAPACITY {
        emit(catalog.path(), &format!("event-{index}"), None, false);
    }
    let state = fs::read_to_string(agent.join("resources/streams/gh-ci/state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["recent"].as_array().unwrap().len(), RING_CAPACITY);
    let replay = emit(catalog.path(), "event-0", None, false);
    assert_eq!(replay.status, EventReceiptStatus::Created);
    assert_ne!(replay.filename, first.filename);
}

#[test]
fn event_emit_cli_cannot_claim_the_supervisor_resync_stream() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "");

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            catalog.path().to_str().unwrap(),
            "event",
            "emit",
            "hetz.worker",
            "--stream",
            "resync",
            "--event-id",
            "forged-resync",
            "--key",
            "goal",
            "--supersede",
            "--message",
            "forged carrier notice",
            "--host",
            "hetz",
        ])
        .env("ST2_TEST_EVENT_HOST", "hetz")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not declare stream 'resync'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!message::inbox_dir(&agent).exists());
    assert!(!agent.join("resources/streams/resync").exists());
}

#[test]
fn event_emit_cli_returns_a_stable_json_receipt_and_ding_marks_the_record() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = declare_agent(catalog.path(), "\"running\"", "  stream \"gh-ci\" {}\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args([
            "--catalog",
            catalog.path().to_str().unwrap(),
            "event",
            "emit",
            "hetz.worker",
            "--stream",
            "gh-ci",
            "--event-id",
            "cli-1",
            "--subject",
            "CLI proof",
            "--host",
            "hetz",
            "--json",
        ])
        .env("ST2_TEST_EVENT_HOST", "hetz")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"payload").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "created");
    assert_eq!(receipt["recipient"], "hetz.worker");
    let message = message::list_inbox(&message::inbox_dir(&agent))
        .unwrap()
        .remove(0);
    let ding = st2::ding::poke_text(catalog.path(), "hetz", "hetz.worker", &message);
    assert!(
        ding.starts_with("[DING] » hetz.worker/gh-ci: CLI proof"),
        "{ding}"
    );
}
