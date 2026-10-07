#![cfg(unix)]
//! A newcomer's first run: a fresh daemon and the first commands from the README, with nothing on
//! stdin. Key creation must add no step or prompt. Doctor leaves signature coverage unchecked;
//! signing and verification remain writer work rather than work triggered by this read.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Newcomer {
    root: tempfile::TempDir,
    daemon: Child,
}

/// `pty` from this process's PATH, which the daemon needs on its own.
fn pty() -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("pty"))
        .find(|path| path.is_file())
}

impl Newcomer {
    fn start(pty: &Path) -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in ["bin", "home", "run", "state", "ws"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::os::unix::fs::symlink(pty, root.path().join("bin/pty")).unwrap();
        // The daemon rebuilds PATH from a login shell; pin it to this one.
        for name in [
            ".profile",
            ".bash_profile",
            ".bashrc",
            ".zprofile",
            ".zshenv",
        ] {
            std::fs::write(
                root.path().join("home").join(name),
                format!("export PATH='{}'\n", Self::path(root.path())),
            )
            .unwrap();
        }
        let config = root.path().join("home/.config/st3/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            format!(
                "node = \"studio\"\nperson = \"person/ada\"\nstate_dir = \"{}\"\npty_root = \"{}\"\nsocket = \"{}\"\nclient_gateway_socket = \"{}\"\n",
                root.path().join("state").display(),
                root.path().join("pty").display(),
                root.path().join("run/st.sock").display(),
                root.path().join("run/client.sock").display(),
            ),
        )
        .unwrap();
        let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
        let daemon = Self::command(root.path())
            .arg("up")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let newcomer = Self { root, daemon };
        let deadline = Instant::now() + Duration::from_secs(60);
        while newcomer.run(&["doctor", "--json"]).is_none() {
            assert!(
                Instant::now() < deadline,
                "the daemon did not answer: {}",
                std::fs::read_to_string(newcomer.root.path().join("daemon.log"))
                    .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        newcomer
    }

    fn path(root: &Path) -> String {
        format!(
            "{}:{}",
            root.join("bin").display(),
            std::env::var("PATH").unwrap()
        )
    }

    fn command(root: &Path) -> Command {
        let mut command = st3::test_support::command(env!("CARGO_BIN_EXE_st3-fixture"));
        command
            .env_clear()
            .env("PATH", Self::path(root))
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_STATE_HOME", root.join("home/.local/state"))
            .env("XDG_CONFIG_HOME", root.join("home/.config"))
            .env("ST3_ENDPOINT", root.join("run/st.sock"))
            .env("ST3_DAEMON_WAIT", "0")
            .current_dir(root.join("ws"))
            .stdin(Stdio::null());
        command
    }

    /// Run one command; its stdout when it succeeds, else its stderr.
    fn try_run(&self, args: &[&str]) -> Result<String, String> {
        let output = Self::command(self.root.path()).args(args).output().unwrap();
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn run(&self, args: &[&str]) -> Option<String> {
        self.try_run(args).ok()
    }

    fn signatures(&self) -> Value {
        let report: Value =
            serde_json::from_str(&self.run(&["doctor", "--json"]).unwrap()).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "claim-signatures")
            .cloned()
            .expect("doctor reports claim signatures")
    }
}

impl Drop for Newcomer {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn a_newcomer_gets_keys_without_a_prompt_and_private_writer_oracle_verifies_claims() {
    if st3::test_support::supervise_test() {
        return;
    }
    let Some(pty) = pty() else {
        assert!(
            std::env::var_os("CI_RUN_ID").is_none(),
            "the getting-started test needs pty on PATH in CI"
        );
        eprintln!("skipped: the getting-started test needs pty on PATH");
        return;
    };
    let mut newcomer = Newcomer::start(&pty);
    for args in [
        &["now"][..],
        &["agents", "ls"],
        &["work", "ls"],
        &[
            "conversations",
            "send",
            "person/ada",
            "--from",
            "person/ada",
            "--body",
            "hello",
        ],
        &["conversations", "ls", "person/ada"],
    ] {
        let output = newcomer
            .try_run(args)
            .unwrap_or_else(|error| {
                panic!(
                    "st {} failed with nothing on stdin: {error}",
                    args.join(" ")
                )
            })
            .to_lowercase();
        assert!(
            !output.contains("key") && !output.contains("sign"),
            "st {} talks about keys: {output}",
            args.join(" ")
        );
    }
    // Locking down is one command, and every rule starts in audit.
    let rules = newcomer.try_run(&["rules", "ls"]).unwrap();
    assert!(rules.starts_with("no rules"), "{rules}");
    let lockdown = newcomer.try_run(&["rules", "lockdown"]).unwrap();
    assert!(lockdown.contains("st rules audit"), "{lockdown}");
    let rules = newcomer.try_run(&["rules", "ls"]).unwrap();
    assert_eq!(
        rules
            .lines()
            .filter(|line| line.contains("\taudit\t"))
            .count(),
        3,
        "{rules}"
    );
    newcomer
        .try_run(&["rules", "mode", "agents-create-no-missions", "enforce"])
        .unwrap();
    let rules = newcomer.try_run(&["rules", "ls"]).unwrap();
    assert!(
        rules.contains("agents-create-no-missions\tenforce"),
        "{rules}"
    );
    let audits = newcomer.try_run(&["rules", "audit"]).unwrap();
    assert!(audits.starts_with("no write"), "{audits}");
    let check = newcomer.signatures();
    assert_eq!(check["status"], "unknown", "{check}");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("evidence incomplete"),
        "{check}"
    );
    // The keys are private files in the state directory, made without asking.
    let keys = newcomer.root.path().join("state/keys");
    assert!(keys.join("node.key").exists());
    use std::os::unix::fs::PermissionsExt as _;
    for entry in std::fs::read_dir(&keys).unwrap() {
        let mode = entry.unwrap().metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
    // Preserve the full signing oracle on a frozen private copy. A diagnostic GET must
    // not seal or judge the daemon's claims to make this assertion pass.
    newcomer.daemon.kill().unwrap();
    newcomer.daemon.wait().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let database = scratch.path().join("claims.sqlite3");
    std::fs::copy(newcomer.root.path().join("state/claims.sqlite3"), &database).unwrap();
    let source_wal = newcomer.root.path().join("state/claims.sqlite3-wal");
    if source_wal.exists() {
        std::fs::copy(source_wal, scratch.path().join("claims.sqlite3-wal")).unwrap();
    }
    let writer = st3::store::Store::open(&database, "studio").unwrap();
    writer
        .set_node_key(std::sync::Arc::new(
            st3::fleet::join::standalone_node_key(&newcomer.root.path().join("state")).unwrap(),
        ))
        .unwrap();
    writer.use_key_directory(&keys).unwrap();
    writer.replication_snapshot().unwrap();
    writer.judge_claims(true).unwrap();
    let counts = writer.claim_verdict_counts().unwrap();
    assert!(
        counts.get("verified").copied().unwrap_or(0) > 0,
        "{counts:?}"
    );
    assert!(
        counts
            .iter()
            .all(|(verdict, count)| verdict == "verified" || *count == 0),
        "{counts:?}"
    );
}

#[test]
fn offline_strict_checks_computed_evidence_without_contacting_a_daemon() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    for directory in ["bin", "home", "run", "state", "ws", "scratch"] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    let input = root.path().join("private.sqlite3");
    let store = st3::store::Store::open(&input, "alder").unwrap();
    store
        .append_client_claim(&st3::model::ClaimInput {
            subject: "resource/example".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: std::collections::BTreeMap::from([(
                "kind".into(),
                serde_json::json!("custom.test.example"),
            )]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("offline-fixture".into()),
        })
        .unwrap();
    drop(store);
    let listener = std::os::unix::net::UnixListener::bind(root.path().join("run/st.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let run = |strict| {
        let mut command = Newcomer::command(root.path());
        command.args([
            "doctor",
            "--offline-audit",
            input.to_str().unwrap(),
            "--audit-scratch-dir",
            root.path().join("scratch").to_str().unwrap(),
            "--json",
        ]);
        if strict {
            command.arg("--strict");
        }
        command.output().unwrap()
    };
    let before = std::fs::read(&input).unwrap();
    let healthy = run(true);
    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    let report: Value = serde_json::from_slice(&healthy.stdout).unwrap();
    assert_eq!(report["status"], "warn");
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["status"] == "unknown")
    );
    assert!(
        !report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["status"] == "warn" || check["status"] == "fail")
    );
    assert_eq!(std::fs::read(&input).unwrap(), before);
    let connection = rusqlite::Connection::open(&input).unwrap();
    // An unsupported schema identifier prevents a computed digest audit from finishing.
    connection
        .execute_batch("ALTER TABLE operations ADD COLUMN \"unsupported name\" TEXT")
        .unwrap();
    drop(connection);
    let warning = run(true);
    assert_eq!(warning.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&warning.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["status"] == "warn")
    );
    assert!(
        !report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["status"] == "fail"),
        "{report}"
    );
    assert!(
        run(false).status.success(),
        "ordinary offline audit permits computed warnings"
    );
    let connection = rusqlite::Connection::open(&input).unwrap();
    connection
        .execute("UPDATE operations SET state='conflict'", [])
        .unwrap();
    drop(connection);
    let corrupted = run(true);
    assert_eq!(corrupted.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&corrupted.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "operation-projection" && check["status"] == "fail")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("scratch"))
            .unwrap()
            .count(),
        0
    );
}
