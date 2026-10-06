//! The machine CLI inventory is a real process contract, independent of a daemon.
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 1_048_576;

#[cfg(unix)]
fn stop_tree(child: &mut Child) {
    // The CLI and any provider it starts belong to the same isolated process group.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn stop_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn bounded_output(mut command: Command, limit: Duration) -> Result<Output, String> {
    use std::io::Read as _;
    use std::sync::mpsc;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let (sender, receiver) = mpsc::channel();
    for (name, pipe) in [
        (
            "stdout",
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (
            "stderr",
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ),
    ] {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut limited = pipe.take((MAX_OUTPUT_BYTES + 1) as u64);
            let result = limited
                .read_to_end(&mut bytes)
                .map(|_| bytes)
                .map_err(|error| error.to_string());
            let _ = sender.send((name, result));
        });
    }
    drop(sender);
    let deadline = Instant::now() + limit;
    let (mut stdout, mut stderr, mut status) = (None, None, None);
    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    stop_tree(&mut child);
                    return Err(error.to_string());
                }
            };
        }
        if let Ok((name, result)) = receiver.recv_timeout(Duration::from_millis(10)) {
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    stop_tree(&mut child);
                    return Err(error);
                }
            };
            if bytes.len() > MAX_OUTPUT_BYTES {
                stop_tree(&mut child);
                return Err(format!("{name} exceeds {MAX_OUTPUT_BYTES} bytes"));
            }
            match name {
                "stdout" => stdout = Some(bytes),
                "stderr" => stderr = Some(bytes),
                _ => unreachable!(),
            }
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            return Ok(Output {
                status: status.unwrap(),
                stdout: stdout.unwrap(),
                stderr: stderr.unwrap(),
            });
        }
        if Instant::now() >= deadline {
            stop_tree(&mut child);
            return Err("child or inherited output pipe exceeded the process deadline".into());
        }
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_st3"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("ST_AGENT", "agent/test/cli-inventory")
        .env("ST3_ENDPOINT", root.join("absent-daemon.sock"))
        .env("ST3_DAEMON_WAIT", "0")
        .stdin(Stdio::null());
    command
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    let mut child = command(root);
    child.args(arguments);
    bounded_output(child, Duration::from_secs(3)).unwrap()
}

#[test]
fn standalone_inventory_exits_with_live_json_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let output = run(root.path(), &["--read-contract-inventory-json"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "st3.cli-inventory.v1");
    assert_eq!(value["modes"], serde_json::json!(["human", "json"]));
    assert!(value["aliases"].is_object());
    let commands = value["commands"].as_array().unwrap();
    assert!(commands.len() > 100, "{commands:?}");
    assert!(
        commands
            .iter()
            .any(|row| row.as_str() == Some("st claude-channel install-policy"))
    );
    assert!(value.get("source_head").is_none());
    assert!(value.get("source_tree").is_none());
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[test]
fn mixed_inventory_requests_exit_before_an_action() {
    let root = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--read-contract-inventory-json", "documents", "put"],
        vec!["documents", "put", "--read-contract-inventory-json"],
        vec!["--read-contract-inventory-json=true"],
    ] {
        let output = run(root.path(), &arguments);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(
            output.stderr,
            b"st: --read-contract-inventory-json must be used alone\n"
        );
    }
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[cfg(unix)]
#[test]
fn literal_inventory_names_after_terminator_reach_ordinary_dispatch() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("payload"), b"invented document\n").unwrap();
    for literal in [
        "--read-contract-inventory-json",
        "--read-contract-inventory-json=true",
    ] {
        symlink("payload", root.path().join(literal)).unwrap();
        let output = run(
            root.path(),
            &[
                "documents",
                "put",
                "--as",
                "doc/test/inventory",
                "--",
                literal,
            ],
        );
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(
            output.stderr,
            b"st: document input cannot be a symbolic link\n"
        );
        assert_eq!(
            std::fs::read_link(root.path().join(literal)).unwrap(),
            Path::new("payload")
        );
    }
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[cfg(unix)]
#[test]
fn literal_inventory_argument_after_terminator_reaches_a_benign_provider() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;

    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("fake-provider");
    let marker = root.path().join("received-argv");
    std::fs::write(
        &script,
        b"#!/bin/sh\nmarker=$1\nshift\nprintf '%s\\n' \"$@\" > \"$marker\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let literal = "--read-contract-inventory-json=true";
    let mut provider = command(root.path());
    provider.process_group(0);
    let mut child = provider
        .args([
            "driver",
            "exec",
            "--subject",
            "test/cli-inventory",
            "--",
            script.to_str().unwrap(),
            marker.to_str().unwrap(),
            literal,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            stop_tree(&mut child);
            panic!("the fake provider never received its argv");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The missing daemon cannot acknowledge runtime.observed; stop only this isolated process.
    stop_tree(&mut child);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("{literal}\n")
    );
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[cfg(unix)]
#[test]
fn process_capture_rejects_oversize_and_child_held_pipes() {
    let root = tempfile::tempdir().unwrap();
    let large = root.path().join("large-output");
    std::fs::write(&large, vec![b'x'; MAX_OUTPUT_BYTES + 1]).unwrap();
    let mut output = Command::new("/bin/cat");
    output.arg(large);
    assert!(
        bounded_output(output, Duration::from_secs(2))
            .unwrap_err()
            .contains("exceeds"),
        "oversize stdout must be refused"
    );

    let mut held = Command::new("/bin/sh");
    held.args(["-c", "/bin/sleep 5 & exit 0"]);
    assert!(
        bounded_output(held, Duration::from_millis(250))
            .unwrap_err()
            .contains("deadline"),
        "a descendant holding stdout open must be killed with the process group"
    );
}
