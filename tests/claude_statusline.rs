#![cfg(unix)]

//! The Claude status-line tee, exercised as Claude runs it: `hooks/claude-statusline.sh` as the
//! process, with the payload on stdin and the operator's renderer downstream.
//!
//! These assert the tee's exact stdout bytes rather than the record it happens to write, because
//! the failure HC-R18 names is invisible from the record's side. Claude's `statusLine` is a single
//! slot whose winning declaration replaces the others outright — measured against 2.1.250,
//! `.claude/settings.local.json` (the file st2 materializes) beats both `.claude/settings.json` and
//! `~/.claude/settings.json`, and the losing command never runs. So a tee that records perfectly
//! and forgets to chain leaves every managed agent with a blank status line, and a test that only
//! checked the record would be green throughout.
//!
//! Stdout is the renderer's channel and nothing else may reach it. The renderer's bytes, or no
//! bytes: a warning, a diagnostic, or the raw payload written there interleaves with the
//! operator's line, which is why the assertions are on exact bytes and not on substrings. The
//! degraded arms therefore assert stdout is EMPTY and take their positive evidence from stderr,
//! where the diagnostic goes and where Claude never renders.

use std::fs;
use std::io::Write as _;
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PAYLOAD: &str = include_str!("../crates/st-drivers/tests/fixtures/harness-context/claude-statusline-mid-session.json");

struct Seat {
    _tmp: tempfile::TempDir,
    catalog: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    agent_dir: PathBuf,
}

impl Seat {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let agent_dir = catalog.join("agents/ExampleMac/cos");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.kdl"),
            r#"agent "cos" {
  host "ExampleMac"
  workspace "/tmp"
  env { ST_AGENT "ExampleMac.cos" }
  command "claude"
}"#,
        )
        .unwrap();

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(env!("CARGO_BIN_EXE_st2"), bin.join("st2")).unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();

        Self {
            _tmp: tmp,
            catalog,
            home,
            bin,
            agent_dir,
        }
    }

    /// A renderer that stamps a marker and reports what it received on stdin, so a test can tell
    /// "the renderer ran" from "st2 passed the payload through" from "both happened".
    fn renderer(&self, marker: &str) -> PathBuf {
        let path = self.home.join(format!("renderer-{marker}.sh"));
        fs::write(
            &path,
            format!(
                // `wc -c` reads stdin directly: a `$(cat)` here would strip the payload's
                // trailing newline and measure the shell rather than the tee.
                "#!/usr/bin/env bash\nprintf '{marker} %s' \"$(wc -c | tr -d ' ')\"\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn declare_renderer_file(&self, command: &str) {
        fs::write(
            self.home.join(".claude/statusline-renderer.json"),
            serde_json::json!({
                "schema": "dotfiles.claude-statusline-renderer.v1",
                "command": command,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn tee(&self, overrides: &[(&str, &str)]) -> Output {
        self.tee_as("ExampleMac.cos", overrides)
    }

    fn tee_as(&self, identity: &str, overrides: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/st-drivers/hooks/claude-statusline.sh");
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("bash");
        command
            .arg(script)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("CATALOG", &self.catalog)
            .env("ST_ROOT", &self.catalog)
            .env("ST_AGENT", identity)
            // The wrapper's token is deliberately absent: these seats are wrapperless, so the tee
            // falls back to Claude's own session id exactly as the hooks do.
            .env_remove("ST2_CLAUDE_SESSION")
            .env_remove("ST_CLAUDE_STATUSLINE_RENDERER")
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
        let raw = fs::read(self.agent_dir.join("harness-context")).ok()?;
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

fn which(program: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// A `PATH` holding exactly the directories the named programs live in.
fn which_dirs(programs: &[&str]) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = programs
        .iter()
        .filter_map(|program| which(program, &current))
        .filter_map(|found| found.parent().map(Path::to_path_buf))
        .collect();
    std::env::join_paths(dirs)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn the_tee_records_the_reading_and_hands_the_same_payload_to_the_env_renderer() {
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");

    let output = seat.tee(&[("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap())]);

    assert!(output.status.success());
    // The renderer's line, and ONLY the renderer's line: nothing of st2's beside it, and no
    // diagnostic on the channel the status line is drawn on.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );

    let record = seat.record().expect("the reading is recorded");
    assert_eq!(record["schema"], "st2.harness-context.v1");
    assert_eq!(record["harness"], "claude");
    assert_eq!(record["usedTokens"], 194_763);
    assert_eq!(record["windowTokens"], 1_000_000);
    assert_eq!(record["usedPercent"], 19.0);
    assert_eq!(record["model"], "claude-opus-5");
    assert_eq!(record["rateLimits"]["fiveHour"], 31.0);
    // The tee shares the incarnation the seat's hooks publish (HC-A03).
    assert_eq!(
        record["incarnation"],
        "claude-session-6c15eccc-0000-4000-8000-000000000000"
    );
}

#[test]
fn the_operator_file_supplies_the_renderer_when_no_variable_does() {
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
    let seat = Seat::new();
    let renderer = seat.renderer("ENVMARK");

    // An identity no catalog declares: the record cannot be resolved, let alone written. This is
    // the Rust fail-open path, not the script's `st2`-is-missing fallback — st2 runs, fails to
    // record, and must chain anyway.
    let output = seat.tee_as(
        "ExampleMac.undeclared",
        &[("ST_CLAUDE_STATUSLINE_RENDERER", renderer.to_str().unwrap())],
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ENVMARK {}", payload_len())
    );
    assert!(seat.record().is_none(), "nothing was recorded");
}

#[test]
fn a_recording_failure_with_no_renderer_still_renders_nothing() {
    let seat = Seat::new();

    let output = seat.tee_as("ExampleMac.undeclared", &[]);

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
    let seat = Seat::new();

    // A renderer name nothing on PATH resolves: `sh -c` reports it on stderr and exits 127. The
    // renderer spawned, so the tee must not write anything after it — it may already have drawn a
    // partial line, and appending the raw JSON would corrupt the row rather than restore it.
    let output = seat.tee(&[(
        "ST_CLAUDE_STATUSLINE_RENDERER",
        "st2-no-such-renderer-anywhere",
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
fn without_st2_on_path_the_script_drains_stdin_and_renders_nothing() {
    let seat = Seat::new();

    // The outermost fallback, and the one that must not depend on st2 being installable: a seat
    // whose `st2` vanished mid-upgrade. It degrades the same way every other arm does — a blank
    // status line, not a wall of JSON — and it DRAINS stdin to get there, because Claude writes
    // the payload into this process and an exit that never read it would earn an EPIPE every five
    // seconds. The PATH keeps the shell's own utilities and drops only `st2`, which is the shape
    // of that failure; a PATH with nothing on it would be testing the harness, not the tee.
    let path = which_dirs(&["bash", "cat"]);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/st-drivers/hooks/claude-statusline.sh");
    assert!(
        which("st2", &path).is_none(),
        "the fallback PATH must not carry an st2"
    );
    let mut child = Command::new("bash")
        .arg(script)
        .env("PATH", &path)
        .env("HOME", &seat.home)
        .env("CATALOG", &seat.catalog)
        .env("ST_AGENT", "ExampleMac.cos")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PAYLOAD.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    // The drain is structural rather than proven here: `exec cat >/dev/null` consumes stdin,
    // and this fixture fits inside the pipe buffer, so a bare `exit 0` would pass this assertion
    // too. What it does pin is the rendered output.
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "the script fallback rendered {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The bound one render must stay under with an unreachable collector configured.
///
/// Grounded in measurement, not guessed (2026-08-29, this worktree, debug build, against a
/// bound-but-never-accepting local port). With the tee building a pipeline, the recording-failure
/// path takes **5.009 s** — the logger provider's final `force_flush` waiting on the dead
/// collector — and `st2 driver claude-observe`, which stays instrumented by design, takes
/// **10.022 s**. With the tee building none, both paths return in **0.010–0.061 s**. Two seconds
/// is two orders of magnitude above the real cost and less than half the cheapest regression, so
/// it neither flakes under a loaded parallel suite nor lets a rebuilt pipeline through.
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
    let seat = Seat::new();
    let (collector, endpoint) = unreachable_collector();

    let started = std::time::Instant::now();
    let output = seat.tee_as(
        "ExampleMac.undeclared",
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)],
    );
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

/// The same, on the path that does write the record — so the guarantee covers a normal render and
/// not only the failing one.
#[test]
fn a_recording_render_is_also_free_of_the_collector() {
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

#[test]
fn the_rendered_registration_names_the_chaining_tee_and_carries_the_whole_slot() {
    let registration = st2::hooks::claude_settings_registration();
    let slot = &registration["statusLine"];

    // The slot is single-valued: what st2 writes here is the only command Claude runs, so it must
    // be the tee and not a bare recorder. Asserting the file name is what makes a future edit
    // that points the slot straight at a recorder fail here rather than in production.
    assert_eq!(slot["type"], "command");
    assert_eq!(slot["command"], "\"$ST_HOOKS/claude-statusline.sh\"");
    assert_eq!(slot["padding"], 0);
    assert_eq!(slot["refreshInterval"], 5);
}

#[test]
fn both_pre_compact_commands_are_registered_and_post_compact_joins_them() {
    let registration = st2::hooks::claude_settings_registration();
    let pre = registration["hooks"]["PreCompact"][0]["hooks"]
        .as_array()
        .expect("PreCompact carries a hook list");

    // The working-state stub and the compaction counter do different jobs on the same edge, so
    // the registration carries both rather than choosing.
    let commands: Vec<&str> = pre
        .iter()
        .map(|entry| entry["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        commands,
        [
            "\"$ST_HOOKS/claude-pre-compact.sh\"",
            "\"$ST_HOOKS/claude-observe.sh\" PreCompact",
        ]
    );
    assert_eq!(
        registration["hooks"]["PostCompact"][0]["hooks"][0]["command"],
        "\"$ST_HOOKS/claude-observe.sh\" PostCompact"
    );
}
