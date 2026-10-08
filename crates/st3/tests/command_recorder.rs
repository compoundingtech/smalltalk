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
    Path::new(test_env!("CARGO_BIN_EXE_st3-fixture"))
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
        .env_remove("ST_STEP_RUN")
        .env_remove("ST_MISSION_RUN");
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

    fn receipts(&self) -> Vec<Value> {
        fs::read_dir(st3::recorder::receipt_path(&self.root.path().join("state")))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                assert_eq!(path.extension().unwrap(), "json");
                assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
            })
            .collect()
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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
    if st3::test_support::supervise_test() {
        return;
    }
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

#[test]
fn create_receipts_preserve_raw_output_and_even_nonzero_exit_status() {
    for (kind, resource, exit) in [("issue", "issues", "0"), ("pr", "pull", "7")] {
        let url = format!("https://github.com/owner/repo/{resource}/123");
        let fixture = fixture(&format!(
            "#!/bin/sh\nprintf '\\377\\000prefix\\r\\n'\ncat\nprintf '\\n{url}\\n'\nprintf 'diagnostic\\n' >&2\nexit \"$EXIT_WITH\"\n"
        ));
        let input = vec![b'x'; 200_000];
        let run = |mut command: Command| {
            command.args([kind, "create", "--title", "unchanged"])
                .env("ST_AGENT", "agent/example/builder")
                .env("ST3_SUBJECT", "exec/ignored")
                .env("ST_MISSION_RUN", "raw mission run / value")
                .env("EXIT_WITH", exit);
            run_with_input(command, &input)
        };
        let recorded = run(fixture.recorded("gh"));
        let direct = run(fixture.direct("gh"));
        assert_eq!(recorded.status, direct.status);
        assert_eq!(recorded.status.code(), Some(exit.parse().unwrap()));
        assert_eq!(recorded.stdout, direct.stdout);
        assert_eq!(recorded.stderr, direct.stderr);
        assert_eq!(fixture.records()[0]["receipt_url"], url);
        let receipts = fixture.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["schema"], "st3.recorder.receipt.v1");
        assert_eq!(receipts[0]["url"], url);
        assert_eq!(receipts[0]["actor"], "agent/example/builder");
        assert_eq!(receipts[0]["mission_run"], "raw mission run / value");
        assert_eq!(receipts[0]["exit_code"], exit.parse::<i32>().unwrap());
        chrono::DateTime::parse_from_rfc3339(receipts[0]["at"].as_str().unwrap()).unwrap();
    }
}

#[test]
fn noncreating_modes_never_credit_a_url_in_the_body() {
    for mode in ["--dry-run", "--dry-run=true", "--web", "-w", "--web=true", "--help", "-h"] {
        let fixture = fixture("#!/bin/sh\nprintf 'https://github.com/owner/repo/pull/123\\n'\n");
        let output = fixture.recorded("gh").args(["pr", "create", mode])
            .env("ST_AGENT", "agent/example/builder").output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"https://github.com/owner/repo/pull/123\n");
        assert!(fixture.receipts().is_empty(), "{mode}");
        assert!(fixture.records()[0].get("receipt_url").is_none(), "{mode}");
    }
}

#[test]
fn only_leading_gh_create_arguments_enable_receipts() {
    let fixture = fixture("#!/bin/sh\nprintf 'https://github.com/owner/repo/issues/123\\n'\n");
    for (program, arguments) in [
        ("git", vec!["issue", "create"]),
        ("gh", vec!["issue", "view"]),
        ("gh", vec!["pr", "create-other"]),
        ("gh", vec!["--repo", "owner/repo", "issue", "create"]),
    ] {
        let output = fixture.recorded(program).args(arguments).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"https://github.com/owner/repo/issues/123\n");
    }
    assert!(fixture.receipts().is_empty());
    for record in fixture.records() {
        assert!(record.get("receipt_url").is_none());
    }
}

#[test]
fn a_receipt_requires_an_exact_bounded_final_url_line() {
    for text in [
        "",
        "https://github.com/owner/repo/issues/123\nlater\n",
        "https://github.com/owner/repo/issues/123\n\n",
        " https://github.com/owner/repo/issues/123\n",
        "https://github.com/owner/repo/issues/123 \n",
        "https://github.com/owner/repo/issues/123?x=1\n",
        "https://example.com/owner/repo/issues/123\n",
        "https://github.com/owner/repo/issues/no\n",
        "https://github.com/owner/repo/issues/0\n",
        "https://github.com/owner/repo/discussions/123\n",
        "\x1b[32mhttps://github.com/owner/repo/issues/123\x1b[0m\n",
    ] {
        let fixture = fixture("#!/bin/sh\ncat\n");
        let mut command = fixture.recorded("gh");
        command.args(["issue", "create"]);
        let output = run_with_input(command, text.as_bytes());
        assert_eq!(output.stdout, text.as_bytes());
        assert!(output.status.success());
        assert!(fixture.receipts().is_empty(), "{text:?}");
        assert!(fixture.records()[0].get("receipt_url").is_none(), "{text:?}");
    }
    let fixture = fixture("#!/bin/sh\ncat\n");
    let text = format!("{}https://github.com/owner/repo/issues/123\n", "x".repeat(1024));
    let mut command = fixture.recorded("gh");
    command.args(["issue", "create"]);
    assert_eq!(run_with_input(command, text.as_bytes()).stdout, text.as_bytes());
    assert!(fixture.receipts().is_empty());
}

#[test]
fn a_regular_stdout_file_and_unterminated_url_are_noninteractive() {
    let url = "https://github.com/owner/repo/pull/42";
    let fixture = fixture(&format!("#!/bin/sh\nprintf '{url}'\n"));
    let destination = fixture.root.path().join("stdout");
    let status = fixture.recorded("gh")
        .args(["pr", "create"])
        .env("ST3_SUBJECT", "exec/example/subject")
        .stdout(fs::File::create(&destination).unwrap())
        .status().unwrap();
    assert!(status.success());
    assert_eq!(fs::read(&destination).unwrap(), url.as_bytes());
    let receipts = fixture.receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["url"], url);
    assert_eq!(receipts[0]["actor"], "exec/example/subject");
    assert!(receipts[0]["mission_run"].is_null());
}

#[test]
fn legacy_or_missing_markers_disable_receipt_capture() {
    for missing in [false, true] {
        let fixture = fixture("#!/bin/sh\nprintf 'https://github.com/owner/repo/issues/123\\n'\n");
        let marker_path = fixture.recorder.join("st3-recorder.json");
        if missing {
            fs::remove_file(&marker_path).unwrap();
        } else {
            let mut marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
            marker.as_object_mut().unwrap().remove("receipts");
            fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        }
        let output = fixture.recorded("gh").args(["issue", "create"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"https://github.com/owner/repo/issues/123\n");
        assert!(fixture.receipts().is_empty());
        let records = fixture.records();
        if missing {
            assert!(records.is_empty());
        } else {
            assert_eq!(records.len(), 1);
            assert!(records[0].get("receipt_url").is_none());
        }
    }
}

#[test]
fn captured_stdout_propagates_a_closed_downstream_pipe_to_the_real_program() {
    let fixture = fixture("#!/bin/sh\nwhile :; do echo line; done\n");
    let mut statuses = Vec::new();
    for mut command in [fixture.recorded("gh"), fixture.direct("gh")] {
        let mut child = command.args(["issue", "create"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut bytes = [0; 64];
        stdout.read_exact(&mut bytes).unwrap();
        drop(stdout);
        statuses.push(wait_within(&mut child, Duration::from_secs(10)));
    }
    assert_eq!(statuses[1].signal(), Some(libc::SIGPIPE));
    assert_eq!(statuses[0], statuses[1]);
    assert_eq!(fixture.records()[0]["signal"], libc::SIGPIPE);
    assert!(fixture.receipts().is_empty());
}

#[test]
fn captured_stdout_relays_signals_and_publishes_a_signaled_child_receipt() {
    let fixture = fixture(
        "#!/bin/sh\ntrap 'printf \"https://github.com/owner/repo/issues/123\\n\"; trap - TERM; kill -TERM $$' TERM\necho ready\nwhile :; do sleep 0.05; done\n",
    );
    let mut child = fixture.recorded("gh").args(["issue", "create"])
        .stdout(Stdio::piped()).spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    assert_eq!(read_line(&mut stdout), "ready");
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) }, 0);
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert_eq!(read_line(&mut stdout), "https://github.com/owner/repo/issues/123");
    let receipts = fixture.receipts();
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0]["exit_code"].is_null());
}

#[test]
fn a_descendant_retaining_stdout_does_not_delay_the_receipt() {
    let fixture = fixture("#!/bin/sh\nsleep 30 &\necho $! >&2\nprintf 'https://github.com/owner/repo/issues/123\\n'\n");
    let mut child = fixture.recorded("gh").args(["issue", "create"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let descendant = read_line(&mut child.stderr.take().unwrap()).parse::<i32>().unwrap();
    let status = wait_within(&mut child, Duration::from_secs(3));
    assert_eq!(unsafe { libc::kill(descendant, libc::SIGTERM) }, 0);
    assert!(status.success());
    let mut stdout = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
    assert_eq!(stdout, b"https://github.com/owner/repo/issues/123\n");
    assert_eq!(fixture.receipts()[0]["url"], "https://github.com/owner/repo/issues/123");
}

#[test]
fn receipt_publication_does_not_depend_on_a_writable_command_log() {
    let fixture = fixture("#!/bin/sh\nprintf 'https://github.com/owner/repo/issues/123\\n'\nexit 9\n");
    fs::remove_file(&fixture.log).unwrap();
    fs::create_dir(&fixture.log).unwrap();
    let output = fixture.recorded("gh").args(["issue", "create"]).output().unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert_eq!(output.stdout, b"https://github.com/owner/repo/issues/123\n");
    assert_eq!(fixture.receipts()[0]["exit_code"], 9);
}

#[test]
fn terminal_stdout_stays_interactive_and_does_not_publish_a_receipt() {
    use std::os::fd::FromRawFd as _;

    let fixture = fixture(
        "#!/bin/sh\nif test -t 1; then echo interactive; else echo piped; fi\nprintf 'https://github.com/owner/repo/issues/123\\n'\n",
    );
    let (mut master, slave) = unsafe {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            libc::openpty(
                &mut master, &mut slave, std::ptr::null_mut(),
                std::ptr::null(), std::ptr::null(),
            ),
            0,
        );
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave))
    };
    let mut child = fixture.recorded("gh").args(["issue", "create"])
        .stdout(slave).spawn().unwrap();
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert!(status.success());
    assert_eq!(read_line(&mut master).trim_end_matches('\r'), "interactive");
    assert_eq!(
        read_line(&mut master).trim_end_matches('\r'),
        "https://github.com/owner/repo/issues/123",
    );
    assert!(fixture.receipts().is_empty());
    assert!(fixture.records()[0].get("receipt_url").is_none());
}

#[test]
fn a_receipt_spool_that_cannot_be_written_changes_nothing() {
    let fixture = fixture("#!/bin/sh\nprintf 'https://github.com/owner/repo/issues/123\\n'\nexit 17\n");
    let spool = st3::recorder::receipt_path(&fixture.root.path().join("state"));
    fs::remove_dir(&spool).unwrap();
    fs::write(&spool, "not a directory").unwrap();
    let recorded = fixture.recorded("gh").args(["issue", "create"]).output().unwrap();
    let direct = fixture.direct("gh").args(["issue", "create"]).output().unwrap();
    assert_eq!(recorded.status, direct.status);
    assert_eq!(recorded.status.code(), Some(17));
    assert_eq!(recorded.stdout, direct.stdout);
    assert_eq!(recorded.stderr, direct.stderr);
    assert_eq!(fixture.records()[0]["receipt_url"], "https://github.com/owner/repo/issues/123");
}

#[cfg(target_os = "linux")]
#[test]
fn a_terminating_signal_still_finishes_capture_under_stdout_backpressure() {
    // Use the default action: dash defers a shell trap until its blocked printf completes,
    // so a trapped fixture would wait on its consumer even without the recorder.
    let fixture = fixture(&format!(
        "#!/bin/sh\necho $$\nwhile :; do printf '%s' '{}'; done\n",
        "x".repeat(16_384),
    ));
    let mut child = fixture.recorded("gh").args(["issue", "create"])
        .stdout(Stdio::piped()).spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let real_pid = read_line(&mut stdout).parse::<i32>().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting = fs::read_to_string(format!("/proc/{real_pid}/wchan")).unwrap();
        if waiting.contains("pipe_write") {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the real program did not reach output backpressure");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) }, 0);
    let status = wait_within(&mut child, Duration::from_secs(10));
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert_eq!(fixture.records()[0]["signal"], libc::SIGTERM);
    assert!(fixture.receipts().is_empty());
}
