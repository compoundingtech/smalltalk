#![cfg(unix)]

//! The current native Claude status-line hook must record and chain the exact stdin bytes.
//! These process proofs survive retirement of the root st2 status-line target.

use std::fs;
use std::io::Write as _;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PAYLOAD: &str = include_str!(
    "../../st-drivers/tests/fixtures/harness-context/claude-statusline-mid-session.json"
);
const IDENTITY: &str = "example/statusline-seat";

fn script() -> PathBuf {
    Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../st3-daemon/hooks/claude-statusline.sh")
}

struct Seat {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    paths: st_drivers::driver_paths::Paths,
}

impl Seat {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let paths = st_drivers::driver_paths::Paths {
            root: tmp.path().join("driver"),
            agent_dir: tmp.path().join("observations"),
            session_dir: tmp.path().join("sessions"),
        };
        for path in [&paths.root, &paths.agent_dir, &paths.session_dir] {
            fs::create_dir_all(path).unwrap();
        }
        st_drivers::harness_events::enable(&paths.agent_dir, "statusline-runtime").unwrap();
        let payload: serde_json::Value = serde_json::from_str(PAYLOAD).unwrap();
        let token = format!(
            "{}{}",
            st_drivers::harness_state::WRAPPERLESS_PREFIX,
            payload["session_id"].as_str().unwrap()
        );
        st_drivers::harness_state::claim(&paths.agent_dir, IDENTITY, "claude", &token).unwrap();
        Self {
            _tmp: tmp,
            home,
            paths,
        }
    }

    fn renderer(&self, marker: &str) -> PathBuf {
        let path = self.home.join(format!("renderer-{marker}.sh"));
        fs::write(
            &path,
            format!("#!/bin/sh\nprintf '{marker} %s' \"$(wc -c | tr -d ' ')\"\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn declare_renderer_file(&self, command: &str) {
        fs::write(
            self.home.join(".claude/statusline-renderer.json"),
            serde_json::json!({
                "schema": "dotfiles.claude-statusline-renderer.v1", "command": command,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn block_recording(&self) {
        let connection = rusqlite::Connection::open(
            self.paths
                .agent_dir
                .join(st_drivers::harness_events::DATABASE),
        )
        .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER refuse_context BEFORE INSERT ON snapshots
                 WHEN NEW.kind = 'harness-context'
                 BEGIN SELECT RAISE(ABORT, 'fixture context write refused'); END;",
            )
            .unwrap();
    }

    fn tee(&self, overrides: &[(&str, &str)]) -> Output {
        let mut command = Command::new("bash");
        command
            .arg(script())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("ST_AGENT", format!("agent/{IDENTITY}"))
            .env("ST3_BIN", test_env!("CARGO_BIN_EXE_st3-fixture"))
            .env("ST3_DRIVER_STATE_DIR", self.paths.root.join("driver-state"))
            .env("ST3_ENDPOINT", self.home.join("unused.sock"))
            .envs(self.paths.environment(IDENTITY))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in overrides {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(PAYLOAD.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn record(&self) -> Option<serde_json::Value> {
        assert!(st_drivers::harness_events::enabled(&self.paths.agent_dir));
        assert!(!self.paths.agent_dir.join("harness-context").exists());
        let raw =
            st_drivers::harness_events::read_snapshot(&self.paths.agent_dir, "harness-context")
                .expect("native observation outbox must remain readable")?;
        Some(serde_json::from_slice(&raw).unwrap())
    }
}

/// The rendered length the marker renderer reports for the payload it was handed.
fn payload_len() -> usize {
    PAYLOAD.len()
}

/// The diagnostic the tee writes when neither resolution path yields a renderer. Claude routes a
/// status-line command's stderr to its debug log and never to the rendered row, so this is the
/// only channel that can tell an operator why their line went blank — and it is what a degraded
/// test asserts *positively*, since an empty stdout on its own is also what a crashed tee leaves.
const NO_RENDERER_DIAGNOSTIC: &str = "no downstream renderer resolved";

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_tee_records_the_reading_and_hands_the_same_payload_to_the_env_renderer() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");

    let output = seat.tee(&[("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap())]);

    assert!(output.status.success());
    // Without push transport, only the downstream renderer writes the status line.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );

    let record = seat.record().expect("the reading is recorded");
    assert_eq!(record["schema"], "st.harness-context.v1");
    assert_eq!(record["harness"], "claude");
    assert_eq!(record["usedTokens"], 194_763);
    assert_eq!(record["windowTokens"], 1_000_000);
    assert_eq!(record["usedPercent"], 19.0);
    assert_eq!(record["model"], "claude-opus-5");
    assert_eq!(record["rateLimits"]["fiveHour"], 31.0);
    // Without a managed incarnation, the payload's session token supplies the incarnation.
    assert_eq!(
        record["incarnation"],
        "claude-session-6c15eccc-0000-4000-8000-000000000000"
    );
}

#[test]
fn the_operator_file_supplies_the_renderer_when_no_variable_does() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("FILEMARK");
    seat.declare_renderer_file(renderer.to_str().unwrap());

    let output = seat.tee(&[]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("FILEMARK {}", payload_len())
    );
    assert!(seat.record().is_some());
}

#[test]
fn the_variable_wins_over_the_file_and_the_file_is_never_also_run() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let winner = seat.renderer("ENVMARK");
    let loser = seat.renderer("FILEMARK");
    seat.declare_renderer_file(loser.to_str().unwrap());

    let output = seat.tee(&[("ST_CLAUDE_STATUSLINE_RENDERER", winner.to_str().unwrap())]);

    // Two sources in strict order, first hit wins — never merged and never both, so an operator
    // debugging their status line has one place to look for the answer.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
}

#[test]
fn with_no_renderer_the_tee_renders_nothing_rather_than_the_raw_payload() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();

    let output = seat.tee(&[]);

    assert!(output.status.success());
    // HC-R18 as amended: the degraded arm is SILENT, not transparent. The payload is machine
    // JSON, so echoing it paints `{"session_id":…,"transcript_path":…}` across the operator's
    // status line every five seconds — strictly worse for them than a blank row, and nothing they
    // can act on. This is the exact live regression the amendment fixes.
    assert!(
        output.stdout.is_empty(),
        "the degraded arm rendered {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    // The reason lands on stderr, which Claude never renders — so a blank line is diagnosable.
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(NO_RENDERER_DIAGNOSTIC),
        "stderr was {stderr:?}"
    );
    // Both resolution paths are named, because absent both is precisely the failure.
    assert!(
        stderr.contains("ST_CLAUDE_STATUSLINE_RENDERER"),
        "stderr was {stderr:?}"
    );
    assert!(
        stderr.contains(".claude/statusline-renderer.json"),
        "stderr was {stderr:?}"
    );
    // Silence is only the RENDER. The reading is recorded exactly as it would have been.
    assert!(seat.record().is_some(), "recording is unaffected");
}

#[test]
fn a_recording_failure_still_renders_the_status_line() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");

    // The native context write transaction aborts, leaving the snapshot absent.
    // The running hook must still chain to the renderer.
    seat.block_recording();
    let output = seat.tee(&[("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap())]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert!(seat.record().is_none(), "nothing was recorded");
}

#[test]
fn a_recording_failure_with_no_renderer_still_renders_nothing() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();

    seat.block_recording();
    let output = seat.tee(&[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    // Both failures are reported, and both on stderr: the tee never converts a failure into
    // stdout bytes, whichever one it hits.
    let stderr = stderr_of(&output);
    assert!(stderr.contains("recording failed"), "stderr was {stderr:?}");
    assert!(
        stderr.contains(NO_RENDERER_DIAGNOSTIC),
        "stderr was {stderr:?}"
    );
}

#[test]
fn a_renderer_that_exits_non_zero_leaves_stdout_empty() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();

    // A renderer name nothing on PATH resolves: `sh -c` reports it on stderr and exits 127. The
    // renderer spawned, so the tee must not write anything after it — it may already have drawn a
    // partial line, and appending the raw JSON would corrupt the row rather than restore it.
    let output = seat.tee(&[(
        "ST_CLAUDE_STATUSLINE_RENDERER",
        "st3-no-such-renderer-anywhere",
    )]);

    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a failed renderer rendered {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(seat.record().is_some(), "recording is unaffected");
}

#[test]
fn a_renderer_file_that_is_not_executable_leaves_stdout_empty() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();

    // The sibling of the non-zero exit, and the likelier operator mistake: the renderer file is
    // there and the path is right, but the mode is not. `sh -c` exits 126. A tee that fell back
    // to the payload on any renderer failure would spew JSON on a one-character permissions bug,
    // so this pins that no renderer failure can ever reach stdout.
    let renderer = seat.renderer("UNREACHABLE");
    fs::set_permissions(&renderer, fs::Permissions::from_mode(0o644)).unwrap();
    seat.declare_renderer_file(renderer.to_str().unwrap());

    let output = seat.tee(&[]);

    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a non-executable renderer rendered {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    // It resolved — this is not the no-renderer arm wearing a different hat.
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains(NO_RENDERER_DIAGNOSTIC),
        "the renderer should have resolved; stderr was {stderr:?}"
    );
    assert!(seat.record().is_some(), "recording is unaffected");
}

#[test]
fn without_the_seat_binary_the_script_drains_stdin_and_renders_the_fault() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    for binary in [None, Some("/no/such/st3-fixture")] {
        let mut command = Command::new("bash");
        command
            .arg(script())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &seat.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(binary) = binary {
            command.env("ST3_BIN", binary);
        }
        let mut child = command.spawn().unwrap();
        // More than a pipe buffer: returning before draining would make the write fail.
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&vec![b'x'; 256 * 1024])
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "st: hooks cannot run: this seat's st3 binary is missing (ST3_BIN={})\n",
                binary.unwrap_or("unset")
            )
        );
        assert!(output.stderr.is_empty());
        assert!(seat.record().is_none());
    }
}

// Preserve the old regression budget: a hook must not flush an unreachable collector.
const TEE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// A collector that accepts the connection and then never answers.
///
/// Bound and listening but never `accept`ed: the TCP handshake completes in the kernel backlog,
/// so an exporter's `connect` SUCCEEDS and it waits for a response that never comes — which is
/// what makes an exporter pay its full timeout. A closed port would be the weaker trap: it fails
/// fast, and a process that *did* build a pipeline would still look quick.
fn unreachable_collector() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    (listener, endpoint)
}

/// `DQ-C13`. Claude WAITS for the status-line command to exit and re-runs it every 5 seconds, so
/// a render that blocks on a collector round-trip is a stalled status line — and against an
/// unreachable collector, a permanently stalled one. The tee therefore builds no telemetry
/// pipeline at all rather than building one and hoping the flush is quick.
///
/// This drives the RECORDING-FAILURE path deliberately. The happy path emits no `tracing` event,
/// so it has nothing to flush and stays fast even with a pipeline built — a test that only drove
/// it would pass for the wrong reason, which is exactly what the first version of this test did.
/// The fail-open path warns, and that warning is what the log bridge would ship.
#[test]
fn the_tee_never_reaches_for_a_collector_even_when_it_has_something_to_report() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let (collector, endpoint) = unreachable_collector();

    let started = std::time::Instant::now();
    seat.block_recording();
    let output = seat.tee(&[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)]);
    let elapsed = started.elapsed();

    assert!(
        elapsed < TEE_BUDGET,
        "the tee took {elapsed:?} with an unreachable collector configured, \
         so it is building an OTel pipeline again"
    );
    // "No telemetry", not "no tee". With the degraded arm silent, an empty stdout is no longer
    // evidence the tee ran at all — a binary that crashed in 5ms would leave the same stdout and
    // pass the budget. The stderr diagnostic is the positive anchor: it is written on the way
    // OUT of a render that read stdin, tried to record, and resolved no renderer.
    assert!(output.stdout.is_empty());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("recording failed"),
        "the tee did not run; stderr was {stderr:?}"
    );
    assert!(
        stderr.contains(NO_RENDERER_DIAGNOSTIC),
        "stderr was {stderr:?}"
    );

    drop(collector);
}

#[test]
fn push_with_an_unavailable_daemon_still_records_and_renders() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");
    let output = seat.tee(&[
        ("ST3_MAILBOX_TRANSPORT", "push"),
        ("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap()),
    ]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert!(seat.record().is_some());
}

#[test]
fn push_with_an_unresponsive_daemon_keeps_the_render_budget() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");
    let endpoint = seat.home.join("hung-daemon.sock");
    // A connection succeeds, but no server reads or answers it. The live label read must
    // finish inside its existing deadline so the renderer can still draw this reading.
    let listener = UnixListener::bind(&endpoint).unwrap();
    listener.set_nonblocking(true).unwrap();
    let started = std::time::Instant::now();
    let output = seat.tee(&[
        ("ST3_MAILBOX_TRANSPORT", "push"),
        ("ST3_ENDPOINT", endpoint.to_str().unwrap()),
        ("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap()),
    ]);
    let elapsed = started.elapsed();

    let _request = listener
        .accept()
        .expect("the push-mode label read must have connected to the hung daemon");
    assert!(elapsed < TEE_BUDGET, "the tee took {elapsed:?}");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert!(seat.record().is_some());
}

#[test]
fn the_managed_incarnation_wins_over_the_payload_session() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");
    let incarnation = "claude-managed-example";
    st_drivers::harness_state::claim(&seat.paths.agent_dir, IDENTITY, "claude", incarnation)
        .unwrap();
    let output = seat.tee(&[
        ("ST_CLAUDE_SESSION", incarnation),
        ("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap()),
    ]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert_eq!(seat.record().unwrap()["incarnation"], incarnation);
}

/// The same, on the path that does write the record — so the guarantee covers a normal render and
/// not only the failing one.
#[test]
fn a_recording_render_is_also_free_of_the_collector() {
    if st3::test_support::supervise_test() {
        return;
    }
    let seat = Seat::new();
    let (collector, endpoint) = unreachable_collector();
    // A real renderer, so this drives the whole production path — read, record, chain — and its
    // marker line is what proves the render completed inside the budget rather than aborting.
    let renderer = seat.renderer("ENVMARK");

    let started = std::time::Instant::now();
    let output = seat.tee(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
        ("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap()),
    ]);
    let elapsed = started.elapsed();

    assert!(elapsed < TEE_BUDGET, "the tee took {elapsed:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert!(seat.record().is_some(), "the reading is still recorded");

    drop(collector);
}
