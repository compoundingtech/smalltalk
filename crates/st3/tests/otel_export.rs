//! Real CLI OTLP/HTTP-JSON export into the effect-utils otelite receiver.
//!
//! `ST3_OTELITE_BIN` supplies the collector; `ST3_OTEL_REQUIRE=1` forbids a local skip.
//! Like the st2 export test, `otelite run` owns the ephemeral receiver and flushes its
//! capture after the command exits, avoiding capture-mode stdin/readiness races.
//! Daemon startup export is not covered by PR 1: startup spans land in a later PR.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn st3() -> &'static Path {
    test_bin!("st3")
}

fn otelite(test: &str) -> Option<PathBuf> {
    let binary = std::env::var_os("ST3_OTELITE_BIN")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if binary.is_none() {
        assert!(
            std::env::var("ST3_OTEL_REQUIRE").as_deref() != Ok("1"),
            "{test}: ST3_OTELITE_BIN is required when ST3_OTEL_REQUIRE=1"
        );
        eprintln!("SKIP {test}: ST3_OTELITE_BIN is unset; cannot prove OTLP export");
    }
    binary
}

fn isolated_command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        // otelite's no-export child uses `env`; preserve only the executable search path.
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("OTEL_SERVICE_NAME", "oh-my-pi")
        .env("OTEL_TRACES_EXPORTER", "otlp")
        .env("OTEL_METRICS_EXPORTER", "otlp")
        .env("OTEL_LOGS_EXPORTER", "otlp")
        .current_dir(root)
        .stdin(Stdio::null());
    command
}

fn missing_daemon(command: &mut Command, root: &Path) {
    command
        .arg("--endpoint")
        .arg(root.join("missing.sock"))
        .args(["--daemon-wait", "0", "agents", "ls"]);
}

fn capture(collector: &Path, root: &Path, export: bool, success: bool) -> Output {
    let mut command = isolated_command(collector, root);
    command
        .args(["run", "--out"])
        .arg(root.join("capture"))
        .args(["--protocol", "http/json", "--"]);
    if !export {
        // otelite injects the endpoint into its child. Remove it while keeping the receiver live.
        command.args(["env", "-u", "OTEL_EXPORTER_OTLP_ENDPOINT"]);
    }
    command.arg(st3());
    if success {
        // `skill` is a fast offline success through run_cli and the telemetry gate.
        command.arg("skill");
    } else {
        missing_daemon(&mut command, root);
    }
    command.output().expect("run st3 under otelite")
}

fn string_attribute<'a>(record: &'a Value, name: &str) -> Option<&'a str> {
    record["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"].as_str() == Some(name))?["value"]["stringValue"]
        .as_str()
}

fn command_roots(traces: &str) -> Vec<(Value, Value)> {
    let mut roots = Vec::new();
    for line in traces.lines() {
        let request: Value = serde_json::from_str(line).expect("valid OTLP trace request");
        for batch in request["resourceSpans"].as_array().expect("resourceSpans") {
            for scope in batch["scopeSpans"].as_array().expect("scopeSpans") {
                for span in scope["spans"].as_array().expect("spans") {
                    if span["name"].as_str() == Some("st3.cli.command") {
                        roots.push((batch["resource"].clone(), span.clone()));
                    }
                }
            }
        }
    }
    roots
}

#[test]
fn cli_error_exports_root_span_and_process_identity() {
    let Some(collector) = otelite("cli_error_exports_root_span_and_process_identity") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = capture(&collector, root.path(), true, false);
    assert!(
        !output.status.success(),
        "missing daemon must fail: {output:?}"
    );
    let traces = std::fs::read_to_string(root.path().join("capture/traces.ndjson"))
        .expect("otelite writes the exported traces");
    let roots = command_roots(&traces);
    assert_eq!(
        roots.len(),
        1,
        "one CLI invocation must export exactly one command root:\n{traces}\n{output:?}"
    );
    let (resource, span) = &roots[0];
    assert_eq!(string_attribute(resource, "service.name"), Some("st-cli"));
    for name in ["service.version", "service.instance.id"] {
        assert!(
            string_attribute(resource, name).is_some_and(|value| !value.is_empty()),
            "missing {name}: {resource}"
        );
    }
    assert_eq!(string_attribute(span, "span.label"), Some("agents"));
    assert!(
        span["parentSpanId"]
            .as_str()
            .is_none_or(|id| { id.is_empty() || id == "0000000000000000" }),
        "CLI command must be a root: {span}"
    );
    assert_eq!(
        span["status"]["code"].as_u64(),
        Some(2),
        "ERROR status: {span}"
    );
}

#[test]
fn cli_success_exports_root_span() {
    let Some(collector) = otelite("cli_success_exports_root_span") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = capture(&collector, root.path(), true, true);
    assert!(output.status.success(), "skill must succeed: {output:?}");
    let traces = std::fs::read_to_string(root.path().join("capture/traces.ndjson"))
        .expect("otelite writes the exported traces");
    let roots = command_roots(&traces);
    assert_eq!(
        roots.len(),
        1,
        "one successful CLI invocation must export exactly one command root:\n{traces}\n{output:?}"
    );
    let (resource, span) = &roots[0];
    assert_eq!(string_attribute(resource, "service.name"), Some("st-cli"));
    assert_eq!(string_attribute(span, "span.label"), Some("skill"));
    assert!(
        span["parentSpanId"]
            .as_str()
            .is_none_or(|id| { id.is_empty() || id == "0000000000000000" }),
        "CLI command must be a root: {span}"
    );
    // Instrumentation records status only on error; success stays UNSET (0 or absent).
    assert_ne!(
        span["status"]["code"].as_u64(),
        Some(2),
        "successful command must not carry ERROR status: {span}"
    );
}

#[test]
fn cli_without_endpoint_succeeds_without_export() {
    let Some(collector) = otelite("cli_without_endpoint_succeeds_without_export") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = capture(&collector, root.path(), false, true);
    assert!(output.status.success(), "skill must succeed: {output:?}");
    for signal in ["traces.ndjson", "metrics.ndjson", "logs.ndjson"] {
        let path = root.path().join("capture").join(signal);
        match std::fs::read(&path) {
            Ok(bytes) => assert!(bytes.is_empty(), "unexpected {signal} export: {bytes:?}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("read {}: {error}", path.display()),
        }
    }
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

fn timed_cli(root: &Path, endpoint: &str, disabled: bool, success: bool) -> Duration {
    let mut command = isolated_command(st3(), root);
    command
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        // Must exceed the CLI budget so the exporter cannot mask unbounded teardown.
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "10000")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if disabled {
        command.env("ST3_CLI_OTEL", "off");
    }
    if success {
        command.arg("skill");
    } else {
        missing_daemon(&mut command, root);
    }
    let started = Instant::now();
    let mut child = command.spawn().expect("run st3 with stalled collector");
    loop {
        if let Some(status) = child.try_wait().expect("observe st3 exit") {
            let elapsed = started.elapsed();
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                status.code(),
                Some(if success { 0 } else { 5 }),
                "{output:?}"
            );
            return elapsed;
        }
        if started.elapsed() >= Duration::from_secs(5) {
            child.kill().expect("stop stalled st3");
            let output = child.wait_with_output().unwrap();
            panic!("CLI stalled during telemetry teardown: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn cli_black_hole_collector_adds_at_most_50ms_then_backs_off() {
    // Bound but never accept: TCP connects while HTTP cannot complete.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    // Error and success commands both export a root span, so both must honor the
    // hard flush deadline and then the negative cache instead of unbounded teardown.
    for success in [false, true] {
        let mut disabled = Vec::new();
        let mut first = Vec::new();
        let mut second = Vec::new();
        for _ in 0..9 {
            let root = tempfile::tempdir().unwrap();
            // Warm the exact binary before collecting paired measurements.
            timed_cli(root.path(), &endpoint, true, success);
            disabled.push(timed_cli(root.path(), &endpoint, true, success));
            first.push(timed_cli(root.path(), &endpoint, false, success));
            assert!(
                root.path().join("run/st3/otel-cli-backoff").is_file(),
                "success={success}: first stalled flush must establish the negative cache"
            );
            second.push(timed_cli(root.path(), &endpoint, false, success));
        }
        let baseline = median(disabled);
        let first_delta = median(first).saturating_sub(baseline);
        let second_delta = median(second).saturating_sub(baseline);
        // Nine-run medians reject isolated shared-host scheduling outliers. The flush
        // deadline contributes 50 ms; allow 15 ms more for startup/provider
        // initialization, atomic cache writes, and scheduler noise.
        assert!(
            first_delta <= Duration::from_millis(65),
            "success={success}: first delta {first_delta:?}"
        );
        assert!(
            second_delta <= Duration::from_millis(15),
            "success={success}: backoff delta {second_delta:?}"
        );
    }
}

#[test]
fn otel_sdk_disabled_and_st3_cli_off_disable_export() {
    let Some(collector) = otelite("otel_sdk_disabled_and_st3_cli_off_disable_export") else {
        return;
    };
    for (switch, value) in [("OTEL_SDK_DISABLED", "TrUe"), ("ST3_CLI_OTEL", "off")] {
        let root = tempfile::tempdir().unwrap();
        let mut command = isolated_command(&collector, root.path());
        command
            .args(["run", "--out"])
            .arg(root.path().join("capture"))
            .args(["--protocol", "http/json", "--", "env"])
            .arg(format!("{switch}={value}"))
            .arg(st3());
        missing_daemon(&mut command, root.path());
        let output = command
            .output()
            .expect("run disabled CLI with live otelite");
        assert_eq!(output.status.code(), Some(5), "{switch}: {output:?}");
        for signal in ["traces.ndjson", "metrics.ndjson", "logs.ndjson"] {
            let path = root.path().join("capture").join(signal);
            match std::fs::read(&path) {
                Ok(bytes) => assert!(bytes.is_empty(), "{switch}: unexpected {signal}"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("read {}: {error}", path.display()),
            }
        }
        assert!(!root.path().join("run/st3/otel-cli-backoff").exists());
    }
}
