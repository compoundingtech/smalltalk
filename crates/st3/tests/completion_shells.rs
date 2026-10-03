#![cfg(unix)]
//! `st completions <shell>` installs a stub that asks st for candidates on every TAB. These tests
//! serve an in-process daemon with known terminals and drive the stub in real bash, fish, and zsh
//! (docs/st3/cli-completion/spec.md, R01-R08). The shells are test inputs: the Nix check and the
//! dev shell provide them, and a missing shell fails the test rather than skipping it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::model::ClaimInput;
use st3::store::Store;

use crate::terminal_attach::{SilentDaemon, serve_unix, state};

/// More running terminals than one 200-item page, so completion must follow the cursor.
const RUNNING: usize = 205;

fn observe(store: &Store, subject: &str, status: &str) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: serde_json::from_value::<BTreeMap<String, Value>>(json!({
                "runtime_id": subject.trim_start_matches("agent/").replace('/', "."),
                "incarnation_id": "4242:2026-10-03T08:00:00.000Z",
                "status": status,
                "terminal": true,
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn fleet_subject(index: usize) -> String {
    format!("agent/fleet/worker-{index:03}")
}

/// A daemon on `ROOT/st3.sock` with `RUNNING` fleet terminals, one solo terminal, and one
/// stopped terminal.
async fn daemon(root: &Path) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let state = state(root);
    for index in 0..RUNNING {
        observe(&state.store, &fleet_subject(index), "running");
    }
    observe(&state.store, "agent/example/solo-worker", "running");
    observe(&state.store, "agent/example/stopped-worker", "stopped");
    let socket = root.join("st3.sock");
    let server = serve_unix(state, &socket).await;
    (socket, server)
}

fn st3() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("st3").to_path_buf()
}

/// An operator environment: no seat identity, config and state under `root`, daemon at `socket`.
fn environment(command: &mut Command, root: &Path, socket: &Path) {
    command
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("COMPLETE")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("ST3_ENDPOINT", socket)
        // A debug daemon in this process can miss the 300 ms TAB deadline on a loaded host; the
        // silent-daemon test sets its own bound.
        .env("ST3_COMPLETION_DEADLINE_MS", "10000");
}

/// One completion request through the fish protocol, the one with values and descriptions.
fn complete(root: &Path, socket: &Path, line: &[&str]) -> (Output, Duration) {
    complete_with(root, socket, line, |_| {})
}

fn complete_with(
    root: &Path,
    socket: &Path,
    line: &[&str],
    adjust: impl FnOnce(&mut Command),
) -> (Output, Duration) {
    let mut command = Command::new(st3());
    environment(&mut command, root, socket);
    adjust(&mut command);
    command
        .env("COMPLETE", "fish")
        .arg("--")
        .arg("st")
        .args(line);
    let started = Instant::now();
    let output = command.output().unwrap();
    (output, started.elapsed())
}

fn lines(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn stub(root: &Path, shell: &str) -> PathBuf {
    let output = Command::new(st3())
        .args(["completions", shell])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = root.join(format!("stub.{shell}"));
    std::fs::write(&path, &output.stdout).unwrap();
    path
}

/// The shell from PATH; a missing shell is a broken test environment, not a skip.
fn shell(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("{name} is not on PATH; the completion tests drive it"))
}

fn run_shell(root: &Path, socket: &Path, shell: &Path, args: &[&str]) -> String {
    let mut command = Command::new(shell);
    environment(&mut command, root, socket);
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{} failed: {}",
        shell.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_offers_every_running_terminal_with_a_description() {
    let root = tempfile::tempdir().unwrap();
    let (socket, _server) = daemon(root.path()).await;
    let socket_for_task = socket.clone();
    let root_path = root.path().to_owned();
    let (output, elapsed) = tokio::task::spawn_blocking(move || {
        complete(
            &root_path,
            &socket_for_task,
            &["terminals", "attach", "agent/"],
        )
    })
    .await
    .unwrap();
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidates = lines(&output);
    let subjects: Vec<&str> = candidates
        .iter()
        .filter_map(|line| line.split_once('\t').map(|(subject, _)| subject))
        .filter(|subject| subject.starts_with("agent/"))
        .collect();
    assert_eq!(
        subjects.len(),
        RUNNING + 1,
        "every running terminal across pages (took {elapsed:?})"
    );
    assert!(subjects.contains(&"agent/example/solo-worker"));
    assert!(subjects.contains(&fleet_subject(RUNNING - 1).as_str()));
    assert!(!subjects.contains(&"agent/example/stopped-worker"));
    let solo = candidates
        .iter()
        .find(|line| line.starts_with("agent/example/solo-worker\t"))
        .unwrap();
    assert!(solo.contains("\trunning · "), "{solo}");
    assert!(solo.contains(" · up "), "{solo}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_is_silent_and_bounded_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing.sock");
    let silent_socket = root.path().join("silent.sock");
    let silent = SilentDaemon::unix(&silent_socket);
    let root_path = root.path().to_owned();
    let results = tokio::task::spawn_blocking(move || {
        [missing, silent_socket].map(|socket| {
            complete_with(
                &root_path,
                &socket,
                &["terminals", "attach", "agent/"],
                |command| {
                    command.env_remove("ST3_COMPLETION_DEADLINE_MS");
                },
            )
        })
    })
    .await
    .unwrap();
    silent.stop();
    for (output, elapsed) in results {
        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            lines(&output)
                .iter()
                .all(|line| !line.starts_with("agent/")),
            "no entity candidates: {:?}",
            lines(&output)
        );
        // The real 300 ms deadline plus process start; generous so a loaded host does not flake.
        assert!(
            elapsed < Duration::from_secs(3),
            "completion took {elapsed:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_shells_complete_live_terminals_through_the_installed_stub() {
    let root = tempfile::tempdir().unwrap();
    let (socket, _server) = daemon(root.path()).await;
    let root_path = root.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let root = root_path.as_path();

        let fish_stub = stub(root, "fish");
        let fish = run_shell(
            root,
            &socket,
            &shell("fish"),
            &[
                "--no-config",
                "-c",
                &format!(
                    "source {}; complete -C 'st terminals attach agent/example/'",
                    fish_stub.display()
                ),
            ],
        );
        assert!(
            fish.lines().any(|line| line.starts_with("agent/example/solo-worker\trunning · ")),
            "fish: {fish}"
        );
        assert!(!fish.contains("stopped-worker"), "fish: {fish}");

        let bash_stub = stub(root, "bash");
        let bash = run_shell(
            root,
            &socket,
            &shell("bash"),
            &[
                "--norc",
                "-c",
                &format!(
                    "source {}; COMP_WORDS=(st terminals attach agent/example/so); COMP_CWORD=3; \
                     COMP_TYPE=9; _clap_complete_st st agent/example/so attach; \
                     printf '%s\\n' \"${{COMPREPLY[@]}}\"",
                    bash_stub.display()
                ),
            ],
        );
        assert_eq!(bash.trim(), "agent/example/solo-worker", "bash: {bash}");

        // zsh completes inside its line editor, so drive an interactive zsh through zpty: TAB
        // must insert the only matching subject. The loop waits for that, up to 30 s.
        let zsh_stub = stub(root, "zsh");
        let script = root.join("drive.zsh");
        std::fs::write(
            &script,
            format!(
                r#"zmodload zsh/zpty
zpty z 'zsh -f -i'
zpty -w z 'PS1="ready> "; autoload -Uz compinit; compinit -u -D; source {stub}; print -r -- "stub-$((1+1))"'
zpty -r z out '*stub-2*'
zpty -w -n z $'st terminals attach agent/example/so\t'
typeset seen=
for attempt in {{1..600}}; do
  while zpty -r -t z chunk; do seen+=$chunk; done
  [[ $seen == *solo-worker* ]] && {{ print -r -- completed; exit 0 }}
  sleep 0.05
done
print -r -- "no completion: ${{(q)seen}}"
exit 1
"#,
                stub = zsh_stub.display()
            ),
        )
        .unwrap();
        let mut command = Command::new("timeout");
        environment(&mut command, root, &socket);
        let zsh_binary = shell("zsh");
        let output = command
            .arg("60")
            .arg(&zsh_binary)
            .arg("-f")
            .arg(&script)
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && printed.contains("completed"),
            "zsh did not complete the subject: {printed}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    })
    .await
    .unwrap();
}
