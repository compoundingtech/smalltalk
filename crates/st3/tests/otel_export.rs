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

fn capture(collector: &Path, root: &Path, export: bool) -> Output {
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
    if export {
        // ERROR retention makes this proof independent of the default 1% success sampling.
        missing_daemon(&mut command, root);
    } else {
        // Unlike --version, skill goes through run_cli and the telemetry initialization gate.
        command.arg("skill");
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

#[test]
fn cli_error_exports_root_span_and_process_identity() {
    let Some(collector) = otelite("cli_error_exports_root_span_and_process_identity") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = capture(&collector, root.path(), true);
    assert!(
        !output.status.success(),
        "missing daemon must fail: {output:?}"
    );
    let traces = std::fs::read_to_string(root.path().join("capture/traces.ndjson"))
        .expect("otelite writes the exported traces");
    let requests: Vec<Value> = traces
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid OTLP trace request"))
        .collect();
    let mut roots = Vec::new();
    for request in &requests {
        for batch in request["resourceSpans"].as_array().expect("resourceSpans") {
            for scope in batch["scopeSpans"].as_array().expect("scopeSpans") {
                for span in scope["spans"].as_array().expect("spans") {
                    if span["name"].as_str() == Some("st3.cli.command") {
                        roots.push((&batch["resource"], span));
                    }
                }
            }
        }
    }
    assert_eq!(
        roots.len(),
        1,
        "one CLI invocation must export exactly one command root:\n{traces}\n{output:?}"
    );
    let (resource, span) = roots[0];
    assert_eq!(string_attribute(resource, "service.name"), Some("st3-cli"));
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
fn cli_without_endpoint_succeeds_without_export() {
    let Some(collector) = otelite("cli_without_endpoint_succeeds_without_export") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = capture(&collector, root.path(), false);
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

#[test]
fn cli_unreachable_collector_exits_in_under_one_second() {
    let Some(_collector) = otelite("cli_unreachable_collector_exits_in_under_one_second") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    // Keep the bound listener alive without accepting. TCP connects, but HTTP cannot complete.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut command = isolated_command(Path::new(env!("CARGO_BIN_EXE_st3")), root.path());
    command
        .env(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        // The exporter timeout must exceed the CLI budget so it cannot mask unbounded teardown.
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    missing_daemon(&mut command, root.path());
    let started = Instant::now();
    let mut child = command.spawn().expect("run st3 with stalled collector");
    loop {
        if let Some(status) = child.try_wait().expect("observe st3 exit") {
            let elapsed = started.elapsed();
            let output = child.wait_with_output().unwrap();
            assert_eq!(status.code(), Some(5), "missing-daemon exit: {output:?}");
            assert!(elapsed < Duration::from_secs(1), "CLI took {elapsed:?}");
            break;
        }
        if started.elapsed() >= Duration::from_secs(1) {
            child.kill().expect("stop stalled st3");
            let output = child.wait_with_output().unwrap();
            panic!("CLI exceeded its one-second wall-time bound: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
