//! The recorder runs as `git` or `gh` from a recorder directory. Each test compares a recorded
//! call with the same call made without the recorder, or checks the one line it appends.

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const ECHO: &str = r#"#!/bin/sh
printf 'argc=%s\n' "$#"
for argument in "$@"; do printf '[%s]\n' "$argument"; done
printf 'stdin='
cat
printf '\ncwd=%s\ncustom=%s\n' "$(pwd)" "$CUSTOM_VALUE"
printf 'to stderr\n' >&2
exit "${EXIT_WITH:-0}"
"#;

struct Fixture {
    root: tempfile::TempDir,
    recorder: PathBuf,
    log: PathBuf,
    real: PathBuf,
}

fn st3() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_st3-fixture"))
}

fn write_program(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn install(state: &Path, real: &Path) -> st3::recorder::Installation {
    st3::recorder::install(state, "test-host", st3(), &[real.as_os_str()]).unwrap()
}

fn fixture(source: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    fs::create_dir(&real).unwrap();
    write_program(&real.join("git"), source);
    write_program(&real.join("gh"), source);
    let installation = install(&root.path().join("state"), &real);
    assert_eq!(installation.programs, ["git", "gh"]);
    Fixture {
        root,
        recorder: installation.directory,
        log: installation.log,
        real,
    }
}

fn path_of(entries: &[&Path]) -> OsString {
    let base = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        entries
            .iter()
            .map(|entry| entry.to_path_buf())
            .chain(std::env::split_paths(&base)),
    )
    .unwrap()
}

fn command(program: &str, path: OsString) -> Command {
    let mut command = Command::new(program);
    command
        .env("PATH", path)
        .env_remove("ST_AGENT")
        .env_remove("ST3_SUBJECT")
        .env_remove("ST_STEP_RUN");
    command
}

impl Fixture {
    fn recorded(&self, program: &str) -> Command {
        command(program, path_of(&[&self.recorder, &self.real]))
    }

    fn direct(&self, program: &str) -> Command {
        command(program, path_of(&[&self.real]))
    }

    fn records(&self) -> Vec<Value> {
        records(&self.log)
    }
}

fn records(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("each log line is one JSON record"))
        .collect()
}

fn run_with_input(mut command: Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    output
}

fn wait_within(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the command did not finish within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_line(reader: &mut impl std::io::Read) -> String {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    while reader.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    String::from_utf8(line).unwrap()
}

#[test]
fn a_recorded_call_is_byte_for_byte_the_real_call() {
    let fixture = fixture(ECHO);
    let workspace = fixture.root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let arguments = [
        "commit",
        "-m",
        "two words",
        "",
        "ünïcode",
        "line\nbreak",
        "--flag=value",
    ];
    let input = (0..20_000)
        .map(|line| format!("input line {line}\n"))
        .collect::<String>();
    for exit in ["0", "1", "42"] {
        let run = |mut command: Command| {
            command
                .args(arguments)
                .current_dir(&workspace)
                .env("CUSTOM_VALUE", "kept")
                .env("EXIT_WITH", exit);
            run_with_input(command, input.as_bytes())
        };
        let recorded = run(fixture.recorded("git"));
        let direct = run(fixture.direct("git"));
        assert_eq!(recorded.status.code(), Some(exit.parse().unwrap()));
        assert_eq!(recorded.status, direct.status);
        assert_eq!(recorded.stdout, direct.stdout);
        assert_eq!(recorded.stderr, direct.stderr);
    }
    assert_eq!(fixture.records().len(), 3);
}

#[test]
fn each_call_appends_one_record_with_its_context() {
    let fixture = fixture(ECHO);
    let workspace = fixture.root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let status = fixture
        .recorded("git")
        .args([
            "push",
            "https://x-access-token:secret@forge.example/org/repo.git",
        ])
        .current_dir(&workspace)
        .env("ST_AGENT", "agent/example/builder")
        .env("ST3_SUBJECT", "agent/example/builder")
        .env("ST_STEP_RUN", "step-run/example/build")
        .env("EXIT_WITH", "3")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
    fixture
        .recorded("gh")
        .args(["pr", "view"])
        .env("ST3_SUBJECT", "exec/example/check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    fixture
        .recorded("git")
        .arg("status")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    let records = fixture.records();
    assert_eq!(records.len(), 3);
    let push = &records[0];
    assert_eq!(push["schema"], "st3.recorder.command.v1");
    assert_eq!(push["host"], "test-host");
    assert_eq!(push["actor"], "agent/example/builder");
    assert_eq!(push["subject"], "agent/example/builder");
    assert_eq!(push["step_run"], "step-run/example/build");
    assert_eq!(
        Path::new(push["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        workspace.canonicalize().unwrap()
    );
    assert_eq!(push["program"], "git");
    assert_eq!(
        push["args"],
        serde_json::json!(["push", "https://***@forge.example/org/repo.git"])
    );
    assert_eq!(push["real"], fixture.real.join("git").to_str().unwrap());
    assert_eq!(push["exit_code"], 3);
    assert!(push["signal"].is_null());
    assert!(push["duration_ms"].as_f64().unwrap() >= 0.0);
    chrono::DateTime::parse_from_rfc3339(push["time"].as_str().unwrap()).unwrap();

    assert_eq!(records[1]["program"], "gh");
    assert_eq!(records[1]["actor"], "exec/example/check");
    assert!(records[1]["step_run"].is_null());
    assert_eq!(records[2]["actor"], "daemon");
    assert!(records[2]["subject"].is_null());
}

#[test]
fn the_recorder_leaves_unread_input_for_the_next_command() {
    let fixture = fixture("#!/bin/sh\nexit 0\n");
    let output = run_with_input(
        {
            let mut command = command("sh", path_of(&[&fixture.recorder, &fixture.real]));
            command.args(["-c", "git status; cat"]);
            command
        },
        b"left for cat\n",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"left for cat\n");
}

#[test]
fn a_real_program_ended_by_a_signal_ends_the_recorder_by_that_signal() {
    let fixture = fixture("#!/bin/sh\nkill -TERM $$\nsleep 5\n");
    let recorded = fixture.recorded("git").status().unwrap();
    let direct = fixture.direct("git").status().unwrap();
    assert_eq!(direct.signal(), Some(libc::SIGTERM));
    assert_eq!(recorded.signal(), Some(libc::SIGTERM));
    let records = fixture.records();
    assert_eq!(records[0]["signal"], libc::SIGTERM);
    assert!(records[0]["exit_code"].is_null());
}

#[test]
fn a_signal_sent_to_the_recorder_reaches_the_real_program() {
    let fixture = fixture(
        "#!/bin/sh\ntrap 'echo stopped; exit 7' TERM\necho ready\nwhile :; do sleep 0.05; done\n",
    );
    let mut child = fixture
        .recorded("git")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    assert_eq!(read_line(&mut stdout), "ready");
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert_eq!(read_line(&mut stdout), "stopped");
    assert_eq!(status.code(), Some(7));
    assert_eq!(fixture.records()[0]["exit_code"], 7);
}

#[test]
fn a_closed_output_pipe_ends_the_recorder_as_it_ends_the_real_program() {
    let fixture = fixture("#!/bin/sh\nwhile :; do echo line; done\n");
    let mut statuses = Vec::new();
    for mut command in [fixture.recorded("git"), fixture.direct("git")] {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut buffer = [0_u8; 64];
        stdout.read_exact(&mut buffer).unwrap();
        drop(stdout);
        statuses.push(wait_within(&mut child, Duration::from_secs(10)));
    }
    assert_eq!(statuses[1].signal(), Some(libc::SIGPIPE));
    assert_eq!(statuses[0].signal(), Some(libc::SIGPIPE));
    assert_eq!(fixture.records()[0]["signal"], libc::SIGPIPE);
}

#[test]
fn a_log_that_cannot_be_written_changes_nothing() {
    let fixture = fixture(ECHO);
    let state = fixture.root.path().join("state");
    assert!(st3::recorder::health(&state).0);
    fs::remove_file(&fixture.log).unwrap();
    fs::create_dir(&fixture.log).unwrap();
    let (recording, message) = st3::recorder::health(&state);
    assert!(!recording);
    assert!(message.contains("cannot be appended"), "{message}");
    let run = |mut command: Command| {
        command.args(["log", "-1"]).env("EXIT_WITH", "5");
        run_with_input(command, b"input")
    };
    let recorded = run(fixture.recorded("git"));
    let direct = run(fixture.direct("git"));
    assert_eq!(recorded.status.code(), Some(5));
    assert_eq!(recorded.status, direct.status);
    assert_eq!(recorded.stdout, direct.stdout);
    assert_eq!(recorded.stderr, direct.stderr);
}

#[test]
fn a_log_fifo_without_a_reader_does_not_hold_the_command() {
    let fixture = fixture("#!/bin/sh\necho done\n");
    fs::remove_file(&fixture.log).unwrap();
    let fifo = std::ffi::CString::new(fixture.log.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(!st3::recorder::health(&fixture.root.path().join("state")).0);
    let mut child = fixture
        .recorded("git")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert!(status.success());
    assert_eq!(read_line(&mut child.stdout.take().unwrap()), "done");
}

#[test]
fn a_recorder_never_runs_itself_or_another_recorder() {
    let fixture = fixture("#!/bin/sh\necho real\n");
    let second = install(&fixture.root.path().join("second-state"), &fixture.real);
    let unmarked = fixture.root.path().join("unmarked");
    fs::create_dir(&unmarked).unwrap();
    std::os::unix::fs::symlink(st3(), unmarked.join("git")).unwrap();
    let spelled_again = PathBuf::from(format!("{}/", fixture.recorder.display()));

    let output = command(
        "git",
        path_of(&[
            &fixture.recorder,
            &spelled_again,
            &second.directory,
            &unmarked,
            &fixture.real,
        ]),
    )
    .stdin(Stdio::null())
    .output()
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"real\n");
    assert_eq!(fixture.records().len(), 1);
    assert!(records(&second.log).is_empty());

    let mut child = command(
        "git",
        std::env::join_paths([&fixture.recorder, &second.directory, &unmarked]).unwrap(),
    )
    .stdin(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let status = wait_within(&mut child, Duration::from_secs(10));
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(127));
    assert!(stderr.contains("command not found"), "{stderr}");
    let records = fixture.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["exit_code"], 127);
    assert!(records[1]["real"].is_null());
}

#[test]
fn concurrent_calls_append_whole_lines() {
    let fixture = fixture("#!/bin/sh\nexit 0\n");
    let children = (0..16)
        .map(|index| {
            fixture
                .recorded("git")
                .arg(index.to_string())
                .arg("x".repeat(3000))
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let mut indexes = fixture
        .records()
        .iter()
        .map(|record| record["args"][0].as_str().unwrap().parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    indexes.sort_unstable();
    assert_eq!(indexes, (0..16).collect::<Vec<_>>());
}

/// The terminal sends its interrupt to the whole foreground process group. The recorder outlives
/// it long enough to record the interrupted call, then ends by the same signal.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_interrupt_ends_the_call_and_is_still_recorded() {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::process::CommandExt as _;

    // A shell that takes the interrupt while it starts a child acts on it when that child
    // exits, so the child sleeps briefly.
    let fixture = fixture("#!/bin/sh\necho ready\nwhile :; do sleep 0.05; done\n");
    let (mut master, slave) = unsafe {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null()
            ),
            0
        );
        // Children that other tests start at the same time must not hold this terminal open.
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave))
    };
    let mut command = fixture.recorded("git");
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    unsafe {
        command.pre_exec(|| {
            // A shell starts a background job with SIGINT ignored; a terminal session does not.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(slave);
    let mut reader = master.try_clone().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buffer = [0_u8; 256];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..count]);
            if seen.windows(5).any(|window| window == b"ready") {
                let _ = sender.send(());
            }
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the real program starts");
    master.write_all(b"\x03").unwrap();
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert_eq!(status.signal(), Some(libc::SIGINT));
    let records = fixture.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["signal"], libc::SIGINT);
}

/// The recorder handles signals while it waits, but the real program starts with the caller's
/// ignored and blocked signals, as it would without the recorder.
#[cfg(target_os = "linux")]
#[test]
fn the_real_program_keeps_the_callers_ignored_and_blocked_signals() {
    use std::os::unix::process::CommandExt as _;

    // GNU grep reads its own status without changing its signals first, as a shell would, and
    // it ignores the `git` name it runs under.
    let fixture = fixture("");
    let grep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("grep"))
        .find(|candidate| candidate.is_file())
        .expect("grep is on PATH");
    fs::remove_file(fixture.real.join("git")).unwrap();
    std::os::unix::fs::symlink(grep, fixture.real.join("git")).unwrap();
    let run = |mut command: Command| {
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                let mut blocked = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGUSR1);
                libc::sigprocmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
                Ok(())
            });
        }
        command
            .args(["^Sig", "/proc/self/status"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let recorded = run(fixture.recorded("git"));
    let direct = run(fixture.direct("git"));
    let signals = |output: &Output| {
        String::from_utf8(output.stdout.clone())
            .unwrap()
            .lines()
            .filter(|line| {
                ["SigBlk:", "SigIgn:", "SigCgt:"]
                    .iter()
                    .any(|name| line.starts_with(name))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let text = String::from_utf8(direct.stdout.clone()).unwrap();
    let mask = |name: &str| {
        let line = text.lines().find(|line| line.starts_with(name)).unwrap();
        u64::from_str_radix(line.split_whitespace().nth(1).unwrap(), 16).unwrap()
    };
    let bit = |signal: libc::c_int| 1_u64 << (signal - 1);
    assert_ne!(mask("SigIgn:") & bit(libc::SIGHUP), 0);
    assert_ne!(mask("SigIgn:") & bit(libc::SIGINT), 0);
    assert_eq!(mask("SigIgn:") & bit(libc::SIGTERM), 0);
    assert_ne!(mask("SigBlk:") & bit(libc::SIGUSR1), 0);
    assert_eq!(signals(&recorded), signals(&direct));
}

/// Killing the recorder outright ends the real program, as killing the real program would have.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_recorder_takes_the_real_program_with_it() {
    let fixture = fixture("#!/bin/sh\necho $$\nexec sleep 30\n");
    let mut child = fixture
        .recorded("git")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let real = read_line(&mut child.stdout.take().unwrap())
        .parse::<u32>()
        .unwrap();
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stat = fs::read_to_string(format!("/proc/{real}/stat")).unwrap_or_default();
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        if stat.is_empty() || state == Some("Z") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the real program outlived its recorder"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
