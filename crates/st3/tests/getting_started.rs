#![cfg(unix)]
//! A newcomer's first run: a fresh daemon and the first commands from the README, with nothing on
//! stdin. Signing must add no step and no prompt, and every claim the newcomer's daemon writes
//! must be signed and verify.

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
        format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap())
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
fn a_newcomer_gets_signed_claims_with_no_new_step_or_prompt() {
    let Some(pty) = pty() else {
        assert!(
            std::env::var_os("CI_RUN_ID").is_none(),
            "the getting-started test needs pty on PATH in CI"
        );
        eprintln!("skipped: the getting-started test needs pty on PATH");
        return;
    };
    let newcomer = Newcomer::start(&pty);
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
    assert_eq!(check["status"], "pass", "{check}");
    let message = check["message"].as_str().unwrap();
    let verified: u64 = message
        .split(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap();
    assert!(verified > 0, "nothing verified: {message}");
    assert!(
        message.contains(" 0 unsigned")
            && message.contains(" 0 waiting")
            && message.ends_with(" 0 invalid"),
        "every claim the newcomer's daemon wrote is signed and verifies: {message}"
    );
    // The keys are private files in the state directory, made without asking.
    let keys = newcomer.root.path().join("state/keys");
    assert!(keys.join("node.key").exists());
    use std::os::unix::fs::PermissionsExt as _;
    for entry in std::fs::read_dir(&keys).unwrap() {
        let mode = entry.unwrap().metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
