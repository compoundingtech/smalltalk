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

fn int_attribute(record: &Value, name: &str) -> Option<i64> {
    record["attributes"]
        .as_array()?
        .iter()
        .find_map(|attribute| {
            if attribute["key"] != name {
                return None;
            }
            let value = &attribute["value"]["intValue"];
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
}

#[cfg(target_os = "linux")]
fn double_attribute(record: &Value, name: &str) -> Option<f64> {
    record["attributes"].as_array()?.iter().find_map(|attribute| {
        if attribute["key"] != name {
            return None;
        }
        let value = &attribute["value"]["doubleValue"];
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

#[cfg(target_os = "linux")]
fn bool_attribute(record: &Value, name: &str) -> Option<bool> {
    record["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"].as_str() == Some(name))?["value"]["boolValue"]
        .as_bool()
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
    let logs = std::fs::read_to_string(root.path().join("capture/logs.ndjson")).unwrap_or_default();
    for line in logs
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
    {
        let record: Value = serde_json::from_str(line).expect("valid OTLP JSON log request");
        for resource in record["resourceLogs"].as_array().into_iter().flatten() {
            for scope in resource["scopeLogs"].as_array().into_iter().flatten() {
                for entry in scope["logRecords"].as_array().into_iter().flatten() {
                    let severity = entry["severityNumber"].as_u64().or_else(|| {
                        entry["severityNumber"]
                            .as_str()
                            .and_then(|value| value.parse().ok())
                    });
                    assert!(
                        severity.is_some_and(|value| value >= 13),
                        "CLI exported a below-WARN log record: {entry}"
                    );
                }
            }
        }
    }
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

fn timed_cli(root: &Path, endpoint: &str, success: bool) -> Duration {
    let mut command = isolated_command(st3(), root);
    command
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        // Must exceed the CLI budget so the exporter cannot mask unbounded teardown.
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "30000")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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
        if started.elapsed() >= Duration::from_secs(10) {
            child.kill().expect("stop stalled st3");
            let output = child.wait_with_output().unwrap();
            panic!("CLI stalled during telemetry teardown: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn cli_black_hole_collector_has_bounded_shutdown_then_backs_off() {
    // Error and success commands both export a root span, so both must honor the
    // hard flush deadline and then the negative cache instead of unbounded teardown.
    for success in [false, true] {
        // Keep each case's backlog separate. Accept only after the first process
        // exits, so TCP connects but no HTTP response can release its flush.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let first = timed_cli(root.path(), &endpoint, success);
        assert!(
            // The helper unit proof owns the 50 ms budget. Allow loaded-host
            // startup noise here, but never wait for the 30 s exporter timeout.
            first < Duration::from_secs(10),
            "success={success}: stalled flush took {first:?}, near the exporter timeout"
        );
        assert!(
            root.path().join("run/st3/otel-cli-backoff").is_file(),
            "success={success}: first stalled flush must establish the negative cache"
        );
        let mut connections = Vec::new();
        loop {
            match listener.accept() {
                Ok((connection, _)) => connections.push(connection),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("accept first-call collector connection: {error}"),
            }
        }
        assert!(
            !connections.is_empty(),
            "success={success}: first call must actually contact the black-hole collector"
        );
        timed_cli(root.path(), &endpoint, success);
        match listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            result => panic!("success={success}: backoff call contacted collector: {result:?}"),
        }
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

// The production daemon does not yet handle termination signals gracefully. Observe
// periodic exports before stopping it; these tests must not depend on shutdown flush.
#[cfg(target_os = "linux")]
struct ExportDaemon {
    collector: std::process::Child,
    socket: PathBuf,
    log: PathBuf,
}

#[cfg(target_os = "linux")]
impl ExportDaemon {
    fn start(collector: &Path, root: &Path) -> Self {
        Self::launch(Some(collector), root)
    }

    fn start_without_export(root: &Path) -> Self {
        Self::launch(None, root)
    }

    fn launch(collector: Option<&Path>, root: &Path) -> Self {
        use std::os::unix::process::CommandExt as _;
        let socket = root.join("run/api.sock");
        let log = root.join("daemon.log");
        let output = std::fs::File::create(&log).unwrap();
        let mut command = isolated_command(collector.unwrap_or_else(|| st3()), root);
        // Own a process group so failed startup also cannot orphan otelite's child.
        command.process_group(0);
        if collector.is_some() {
            command
                // SDK 0.30 reads all three intervals in milliseconds from the environment.
                .env("OTEL_BSP_SCHEDULE_DELAY", "100")
                .env("OTEL_BLRP_SCHEDULE_DELAY", "100")
                .env("OTEL_METRIC_EXPORT_INTERVAL", "250")
                .args(["run", "--out"])
                .arg(root.join("capture"))
                .args(["--protocol", "http/json", "--"])
                .arg(st3());
        }
        command.args(["up", "--node", "otel-test", "--state-dir"])
            .arg(root.join("daemon-state"))
            .arg("--socket")
            .arg(&socket)
            .arg("--client-gateway-socket")
            .arg(root.join("run/client.sock"))
            // Health requests never launch a PTY. Like DaemonConfig test fixtures,
            // supply its name explicitly instead of requiring login-PATH discovery.
            .args(["--pty-binary", "pty"])
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        let mut daemon = Self {
            collector: command.spawn().expect("run real st3 daemon under otelite"),
            socket,
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if st3::startup::read(&daemon.socket)
                .is_some_and(|readiness| readiness.phase == "ready")
            {
                return daemon;
            }
            assert!(
                daemon.collector.try_wait().unwrap().is_none(),
                "daemon/collector exited before readiness: {}",
                daemon.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "daemon readiness timed out: {}",
                daemon.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn diagnostics(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn health(&self, traceparent: Option<&str>) {
        use std::io::{Read as _, Write as _};
        let mut socket = std::os::unix::net::UnixStream::connect(&self.socket).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let trace_header = traceparent
            .map(|value| format!("traceparent: {value}\r\n"))
            .unwrap_or_default();
        write!(
            socket,
            "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nx-st3-client: fractal\r\n{trace_header}\r\n"
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 "),
            "health request failed: {response}\n{}",
            self.diagnostics()
        );
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        traceparent: &str,
        key: Option<&str>,
    ) -> (Value, usize) {
        use http_body_util::BodyExt as _;
        // Exercise the local API boundary; the separate fabric gateway requires pairing.
        let socket = &self.socket;
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(
            hyper_util::rt::TokioIo::new(stream),
        ).await.unwrap();
        let connection = tokio::spawn(connection);
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("x-st3-client", "fractal")
            .header("traceparent", traceparent)
            .header("content-type", "application/json");
        if path.starts_with("/v1/client/") {
            request = request.header("x-st3-person", "person/ada");
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = body.map(|body| serde_json::to_vec(&body).unwrap()).unwrap_or_default();
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            sender.send_request(request.body(axum::body::Body::from(body)).unwrap()),
        ).await.unwrap().unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        connection.abort();
        assert_eq!(status, hyper::StatusCode::OK, "{}\n{}", String::from_utf8_lossy(&body), self.diagnostics());
        (serde_json::from_slice(&body).unwrap(), body.len())
    }

    async fn websocket(
        &self,
        path: &str,
        protocol: &str,
        traceparent: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio::net::UnixStream> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await.unwrap();
        let mut request = format!("ws://localhost{path}").into_client_request().unwrap();
        request.headers_mut().insert("sec-websocket-protocol", protocol.parse().unwrap());
        request.headers_mut().insert("traceparent", traceparent.parse().unwrap());
        request.headers_mut().insert("x-st3-client", "fractal".parse().unwrap());
        request.headers_mut().insert("x-st3-person", "person/ada".parse().unwrap());
        let (socket, response) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::client_async(request, stream),
        ).await.unwrap().unwrap();
        assert_eq!(response.status(), hyper::StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.headers()["sec-websocket-protocol"], protocol);
        socket
    }

    fn await_span(&mut self, root: &Path, matches: impl Fn(&Value) -> bool) -> Value {
        let request = self.await_export(&root.join("capture/traces.ndjson"), |request| {
            exported_spans(request).any(&matches)
        });
        exported_spans(&request).find(|span| matches(span)).unwrap().clone()
    }

    fn captured_spans(&self, root: &Path) -> Vec<Value> {
        std::fs::read_to_string(root.join("capture/traces.ndjson")).unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .flat_map(|line| {
                let request: Value = serde_json::from_str(line).unwrap();
                exported_spans(&request).cloned().collect::<Vec<_>>()
            })
            .collect()
    }

    fn await_export(&mut self, path: &Path, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let capture = std::fs::read_to_string(path).unwrap_or_default();
            // A reader may race a line append. Revisit the incomplete final line
            // on the next poll rather than treating it as malformed OTLP.
            for line in capture
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n'))
            {
                let request: Value = serde_json::from_str(line).expect("valid OTLP JSON request");
                if matches(&request) {
                    return request;
                }
            }
            assert!(
                self.collector.try_wait().unwrap().is_none(),
                "daemon/collector exited while awaiting export: {}",
                self.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "expected export missing from {}:\n{capture}\n{}",
                path.display(),
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ExportDaemon {
    fn drop(&mut self) {
        if let Some(readiness) = st3::startup::read(&self.socket)
            && let Ok(pid) = libc::pid_t::try_from(readiness.pid)
        {
            // otelite remains alive to drain the exports already observed.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.collector.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                if let Ok(pid) = libc::pid_t::try_from(self.collector.id()) {
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.collector.wait();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_request_span_continues_caller_trace() {
    let Some(collector) = otelite("daemon_request_span_continues_caller_trace") else {
        return;
    };
    const TRACE_ID: &str = "1234567890abcdef1234567890abcdef";
    const PARENT_ID: &str = "1234567890abcdef";
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    for sampled in [true, false] {
        let flags = if sampled { "01" } else { "00" };
        daemon.health(Some(&format!("00-{TRACE_ID}-{PARENT_ID}-{flags}")));
        daemon.await_export(&root.path().join("capture/traces.ndjson"), |request| {
            request["resourceSpans"].as_array().is_some_and(|batches| {
                batches.iter().any(|batch| {
                    string_attribute(&batch["resource"], "service.name") == Some("st-daemon")
                        && batch["scopeSpans"].as_array().is_some_and(|scopes| {
                            scopes.iter().any(|scope| {
                                scope["spans"].as_array().is_some_and(|spans| {
                                    spans.iter().any(|span| {
                                        span["traceId"].as_str() == Some(TRACE_ID)
                                            && span["parentSpanId"].as_str() == Some(PARENT_ID)
                                            && span["name"].as_str() == Some("GET /v1/health")
                                            && string_attribute(span, "http.route")
                                                == Some("/v1/health")
                                            && string_attribute(span, "st3.client.class")
                                                == Some("fractal")
                                            && span["attributes"].as_array().is_some_and(
                                                |attributes| {
                                                    attributes.iter().any(|attribute| {
                                                        attribute["key"] == "st.parent.sampled"
                                                            && attribute["value"]["boolValue"]
                                                                == sampled
                                                    })
                                                },
                                            )
                                            // One server span per request: phase durations
                                            // are attributes, child spans must not exist.
                                            && int_attribute(span, "st.handler.duration_ms")
                                                .is_some()
                                    })
                                })
                            })
                        })
                })
            })
        });
        // The single-span shape exports no admission/handler child spans.
        let request = daemon.await_export(&root.path().join("capture/traces.ndjson"), |r| {
            r["resourceSpans"].as_array().is_some_and(|batches| {
                batches.iter().any(|batch| {
                    string_attribute(&batch["resource"], "service.name") == Some("st-daemon")
                })
            })
        });
        let span_names: Vec<&str> = request["resourceSpans"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|batch| batch["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|scope| scope["spans"].as_array().into_iter().flatten())
            .filter_map(|span| span["name"].as_str())
            .collect();
        assert!(
            span_names.iter().all(|name| {
                !matches!(
                    *name,
                    "admission.queue"
                        | "admission.authenticate"
                        | "admission.snapshot"
                        | "handler.queue"
                        | "handler"
                )
            }),
            "child spans leaked into the export: {span_names:?}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_request_metric_recorded_without_trace_sampling() {
    let Some(collector) = otelite("daemon_request_metric_recorded_without_trace_sampling") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    daemon.health(None);
    // HTTP-JSON capture stores ExportMetricsServiceRequest verbatim: resourceMetrics
    // -> scopeMetrics -> metrics -> histogram -> dataPoints. SDK 0.30 serializes
    // histogram count as a JSON number; also accept the OTLP decimal-string form.
    daemon.await_export(&root.path().join("capture/metrics.ndjson"), |request| {
        request["resourceMetrics"]
            .as_array()
            .is_some_and(|batches| {
                batches.iter().any(|batch| {
                    string_attribute(&batch["resource"], "service.name") == Some("st-daemon")
                        && batch["scopeMetrics"].as_array().is_some_and(|scopes| {
                            scopes.iter().any(|scope| {
                                scope["metrics"].as_array().is_some_and(|metrics| {
                                    metrics.iter().any(|metric| {
                                        metric["name"].as_str()
                                            == Some("http.server.request.duration")
                                            && metric["histogram"]["dataPoints"]
                                                .as_array()
                                                .is_some_and(|points| {
                                                    points.iter().any(|point| {
                                                        string_attribute(point, "http.route")
                                                            == Some("/v1/health")
                                                            && string_attribute(
                                                                point,
                                                                "st3.client.class",
                                                            ) == Some("fractal")
                                                            && point["count"]
                                                                .as_u64()
                                                                .or_else(|| {
                                                                    point["count"]
                                                                        .as_str()
                                                                        .and_then(|count| {
                                                                            count
                                                                                .parse::<u64>()
                                                                                .ok()
                                                                        })
                                                                })
                                                                .is_some_and(|count| count > 0)
                                                    })
                                                })
                                    })
                                })
                            })
                        })
                })
            })
    });
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn daemon_collection_stream_without_export_delivers_first_frame() {
    let root = tempfile::tempdir().unwrap();
    // Direct launch uses env_clear, so there is no inherited OTLP endpoint or provider.
    let daemon = ExportDaemon::start_without_export(root.path());
    let client = st3_client::Client::unix_as(&daemon.socket, "person/ada");
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe("missions", "missions", 2, None, None).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(15), stream.next())
        .await.unwrap().unwrap().unwrap();
    assert_eq!(frame["kind"], "snapshot", "{frame}");
    assert_eq!(frame["id"], "missions", "{frame}");
    assert!(frame["items"].is_array(), "{frame}");
    stream.close().await;
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_normal_request_exports_no_log_stream() {
    let Some(collector) = otelite("daemon_normal_request_exports_no_log_stream") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    daemon.health(None);
    // The request's span reaching the collector proves the export path ran; the log
    // bridge's 100 ms batch delay (set above) flushes any queued log record long
    // before this returns, so the grace period below closes the race.
    daemon.await_export(&root.path().join("capture/traces.ndjson"), |request| {
        request["resourceSpans"].as_array().is_some_and(|batches| {
            batches.iter().any(|batch| {
                string_attribute(&batch["resource"], "service.name") == Some("st-daemon")
                    && batch["scopeSpans"].as_array().is_some_and(|scopes| {
                        scopes.iter().any(|scope| {
                            scope["spans"].as_array().is_some_and(|spans| {
                                spans
                                    .iter()
                                    .any(|span| span["name"].as_str() == Some("GET /v1/health"))
                            })
                        })
                    })
            })
        })
    });
    std::thread::sleep(Duration::from_millis(500));
    let logs = std::fs::read_to_string(root.path().join("capture/logs.ndjson")).unwrap_or_default();
    // resourceLogs -> scopeLogs -> logRecords; WARN starts at severityNumber 13.
    // A healthy request must export no below-WARN record: per-request framework
    // events are DEBUG and stay on stderr.
    for line in logs
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
    {
        let record: Value = serde_json::from_str(line).expect("valid OTLP JSON log request");
        let below_warn = record["resourceLogs"].as_array().is_some_and(|resources| {
            resources.iter().any(|resource| {
                resource["scopeLogs"].as_array().is_some_and(|scopes| {
                    scopes.iter().any(|scope| {
                        scope["logRecords"].as_array().is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                let severity = entry["severityNumber"].as_u64().or_else(|| {
                                    entry["severityNumber"]
                                        .as_str()
                                        .and_then(|value| value.parse().ok())
                                });
                                severity.is_some_and(|severity| severity < 13)
                            })
                        })
                    })
                })
            })
        });
        assert!(
            !below_warn,
            "a non-diagnostic (below-WARN) log record was exported:\n{line}"
        );
    }
}

#[cfg(target_os = "linux")]
fn exported_spans(request: &Value) -> impl Iterator<Item = &Value> {
    request["resourceSpans"].as_array().into_iter().flatten()
        .filter(|batch| string_attribute(&batch["resource"], "service.name") == Some("st-daemon"))
        .flat_map(|batch| batch["scopeSpans"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["spans"].as_array().into_iter().flatten())
}

#[cfg(target_os = "linux")]
fn assert_internal_root(span: &Value) {
    assert!(
        span["parentSpanId"].as_str().is_none_or(|id| id.is_empty() || id == "0000000000000000"),
        "stage span must be parentless: {span}"
    );
    assert!(
        span["kind"] == 1 || span["kind"] == "SPAN_KIND_INTERNAL",
        "stage span must be INTERNAL: {span}"
    );
    assert!(!span["traceId"].as_str().unwrap().is_empty());
    assert!(!span["spanId"].as_str().unwrap().is_empty());
}

#[cfg(target_os = "linux")]
fn assert_upgrade_link(stage: &Value, upgrade: &Value) {
    assert_internal_root(stage);
    assert_ne!(stage["traceId"], upgrade["traceId"], "the stage must not retain the server trace as parent");
    let links = stage["links"].as_array().expect("linked root");
    assert_eq!(links.len(), 1, "{stage}");
    assert_eq!(links[0]["traceId"], upgrade["traceId"]);
    assert_eq!(links[0]["spanId"], upgrade["spanId"]);
}

#[cfg(target_os = "linux")]
const STAGE_FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";

#[cfg(target_os = "linux")]
fn stage_replication_source(root: &Path) -> st3::store::Store {
    let state = root.join("daemon-state");
    std::fs::create_dir_all(&state).unwrap();
    let target = st3::store::Store::open(&state.join("claims.sqlite3"), "otel-test").unwrap();
    target.bind_fleet(STAGE_FLEET).unwrap();
    target.project_replication_backlog().unwrap();
    let source = st3::store::Store::open_memory("otel-source").unwrap();
    source.bind_fleet(STAGE_FLEET).unwrap();
    source
}

#[cfg(target_os = "linux")]
async fn receive_stage_claims(daemon: &ExportDaemon, source: &st3::store::Store, traceparent: &str) {
    let exchange = source.export_replication_exchange(
        STAGE_FLEET, &st3::model::ReplicationInventory::default(),
    ).unwrap();
    let (response, _) = daemon.request("POST", "/v1/internal/replication/receive",
        Some(serde_json::json!({"peer":"otel-source", "fleet_id":STAGE_FLEET, "exchange":exchange})),
        traceparent, None).await;
    assert!(response["value"]["receipt"]["received"].as_u64().is_some_and(|count| count > 0), "{response}");
}

#[cfg(target_os = "linux")]
fn seed_roster(root: &Path, subject: &str) {
    let state = root.join("daemon-state");
    std::fs::create_dir_all(&state).unwrap();
    let store = st3::store::Store::open(&state.join("claims.sqlite3"), "otel-test").unwrap();
    store.append_claim(&st3::model::ClaimInput {
        subject: subject.into(),
        kind: "runtime.observed".into(),
        actor: None,
        fields: serde_json::from_value(serde_json::json!({
            "status":"stopped", "runtime_id":"otel-observed", "host":"otel-test",
            "incarnation_id":"otel-session"
        })).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

#[cfg(target_os = "linux")]
async fn next_json_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
) -> (Value, usize) {
    use futures_util::{SinkExt as _, StreamExt as _};
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let message = socket.next().await.expect("open websocket").unwrap();
            match message {
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    return (serde_json::from_str(&text).unwrap(), text.len());
                }
                tokio_tungstenite::tungstenite::Message::Ping(_) => socket.flush().await.unwrap(),
                other => panic!("unexpected websocket frame: {other:?}"),
            }
        }
    }).await.expect("deterministic first/change frame")
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn daemon_request_id_and_roster_stages_match_response() {
    const TRACE: &str = "31af7651916cd43dd8448eb211c80319";
    const PARENT: &str = "b7ad6b7169203331";
    const TRACEPARENT: &str = "00-31af7651916cd43dd8448eb211c80319-b7ad6b7169203331-01";
    let Some(collector) = otelite("daemon_request_id_and_roster_stages_match_response") else { return };
    let root = tempfile::tempdir().unwrap();
    let actor = std::env::var("ST_AGENT").ok().filter(|actor| actor.starts_with("agent/"))
        .unwrap_or_else(|| "agent/otel-test.observed".into());
    seed_roster(root.path(), &actor);
    let mut daemon = ExportDaemon::start(&collector, root.path());
    const STARTUP_BARRIER: &str = "00-21af7651916cd43dd8448eb211c80319-b7ad6b7169203330-01";
    daemon.health(Some(STARTUP_BARRIER));
    daemon.await_span(root.path(), |span| span["traceId"] == "21af7651916cd43dd8448eb211c80319");
    // History rosters are published on demand; await that publication before measuring hits.
    daemon.request("GET", "/v1/client/agents?history=true&fresh=true&status=stopped",
        None, STARTUP_BARRIER, None).await;
    // Reads serve published cuts, including after a new claim. A fresh history read
    // waits for the refresher's publication; neither kind folds cards in the request.
    let mut responses = Vec::new();
    for attempt in 0..3 {
        if attempt != 0 {
            daemon.request("POST", "/v1/diagnostics/harness", Some(serde_json::json!({
                "actor":actor, "code":"otel-roster-advance", "reason":"stage fixture",
                "severity":"warning", "status":"healthy", "incarnation_id":"otel-session",
                "idempotency_key":format!("otel-roster-advance-{attempt}"),
            })), TRACEPARENT, None).await;
        }
        for (path, expected_path, expected_fresh) in [
            ("/v1/client/agents?history=true&status=stopped", "filtered_full", false),
            ("/v1/client/agents?history=true&fresh=true&status=stopped", "filtered_full", true),
            ("/v1/client/agents?history=true", "published_complete", false),
        ] {
            let (body, bytes) = daemon.request("GET", path, None, TRACEPARENT, None).await;
            responses.push((body, bytes, expected_path, expected_fresh));
        }
    }
    for (body, bytes, expected_path, expected_fresh) in responses {
        let request_id = body["request_id"].as_str().expect("response request_id");
        assert!(request_id.starts_with("request/"), "{body}");
        let rows = body["value"]["items"].as_array().expect("roster page").len();
        assert_eq!(rows, 1, "{body}");
        let span = daemon.await_span(root.path(), |span| {
            string_attribute(span, "st.request.id") == Some(request_id)
        });
        assert_eq!(span["name"], "GET /v1/client/agents");
        assert_eq!(span["traceId"], TRACE);
        assert_eq!(span["parentSpanId"], PARENT);
        let mode = string_attribute(&span, "st.roster.mode").expect("roster mode");
        assert_eq!(mode, "hit", "HTTP reads serve the published roster: {span}");
        assert!(body["snapshot"]["published_at"].is_string(), "{body}");
        assert_eq!(int_attribute(&span, "st.roster.cards"), Some(1), "{span}");
        assert_eq!(int_attribute(&span, "st.page.rows"), Some(i64::try_from(rows).unwrap()));
        assert_eq!(int_attribute(&span, "st.page.bytes"), Some(i64::try_from(bytes).unwrap()));
        // Filtered and ordinary complete pages report the publication actually served.
        assert_eq!(string_attribute(&span, "st.roster.path"), Some(expected_path), "{span}");
        let fresh = bool_attribute(&span, "st.roster.fresh_requested").expect("fresh_requested bool");
        assert_eq!(fresh, expected_fresh, "{span}");
        let requested = int_attribute(&span, "st.roster.requested_cut").expect("requested_cut integer");
        let published = int_attribute(&span, "st.roster.publication_cut").expect("publication_cut integer");
        assert_eq!(Some(published), body["snapshot"]["store_index"].as_i64(), "{span}\n{body}");
        if fresh {
            assert!(published >= requested, "fresh read served an older cut: {span}");
        }
    }
    let spans = daemon.captured_spans(root.path());
    // The refresher's first publication is a detached cold rebuild; it carries the closed
    // invalidation reason, bounded counts and synchronous phase wall/CPU time on the root.
    let rebuilds: Vec<_> = spans.iter().filter(|span| span["name"] == "st.roster.rebuild").collect();
    assert!(!rebuilds.is_empty(), "published roster requires a rebuild root: {spans:?}");
    for rebuild in &rebuilds {
        assert_internal_root(rebuild);
        assert_ne!(rebuild["traceId"], TRACE);
        let reason = string_attribute(rebuild, "st.roster.invalidation").expect("invalidation reason");
        assert!(matches!(reason, "cold" | "unknown_kind" | "structural_claim" | "missing_message"
            | "membership" | "local_frontier" | "queue_deadline" | "coverage_gap" | "safe_delta"
            | "historical_snapshot"), "unbounded invalidation reason: {rebuild}");
        for count in ["st.roster.changed", "st.roster.covered", "st.roster.refolded"] {
            assert!(int_attribute(rebuild, count).is_some_and(|value| value >= 0), "{count}: {rebuild}");
        }
    }
    let cold = rebuilds.iter().find(|span| string_attribute(span, "st.roster.invalidation") == Some("cold"))
        .unwrap_or_else(|| panic!("first publication must be a cold rebuild: {rebuilds:?}"));
    // The refresher builds publications under admission; the root names that holder.
    assert_eq!(string_attribute(cold, "st.roster.admission.holder_class"), Some("refresher"), "{cold}");
    assert!(int_attribute(cold, "st.roster.admission.id").is_some(), "{cold}");
    assert!(double_attribute(cold, "st.roster.admission.hold_ms").is_some_and(|ms| ms >= 0.0), "{cold}");
    // The cold card projection always folds status and presents cards.
    for phase in ["status_fold", "card_presentation"] {
        for unit in ["wall_ms", "cpu_ms"] {
            let key = format!("st.roster.phase.{phase}.{unit}");
            assert!(double_attribute(cold, &key).is_some_and(|ms| ms >= 0.0), "{key}: {cold}");
        }
    }
    assert!(spans.iter().all(|span| span["parentSpanId"].as_str()
        .is_none_or(|parent| !spans.iter().any(|server| server["traceId"] == TRACE
            && server["spanId"].as_str() == Some(parent)))), "request stage children leaked: {spans:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn collection_first_frame_links_upgrade_and_changes_emit_no_extra_spans() {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;
    const TRACE: &str = "41af7651916cd43dd8448eb211c80319";
    const TRACEPARENT: &str = "00-41af7651916cd43dd8448eb211c80319-b7ad6b7169203332-01";
    const BARRIER: &str = "00-51af7651916cd43dd8448eb211c80319-b7ad6b7169203333-01";
    let Some(collector) = otelite("collection_first_frame_links_upgrade_and_changes_emit_no_extra_spans") else { return };
    let root = tempfile::tempdir().unwrap();
    let source = stage_replication_source(root.path());
    let mut daemon = ExportDaemon::start(&collector, root.path());
    let mut socket = daemon.websocket("/v1/client/collections/stream",
        "st3.client.collections.v0", TRACEPARENT).await;
    let upgrade = daemon.await_span(root.path(), |span| {
        span["traceId"] == TRACE && span["name"] == "GET /v1/client/collections/stream"
    });
    assert_eq!(upgrade["parentSpanId"], "b7ad6b7169203332");
    assert_eq!(int_attribute(&upgrade, "http.response.status_code"), Some(101));
    // Cover every ordinary collection with its real initial bounded window.
    for collection in ["missions", "attention", "agents", "work", "glasses", "arrangements"] {
        socket.send(Message::Text(serde_json::json!({
            "kind":"subscribe", "id":collection, "collection":collection, "limit":2,
            "person": if collection == "arrangements" { Some("person/ada") } else { None }
        }).to_string().into())).await.unwrap();
        let (frame, bytes) = next_json_frame(&mut socket).await;
        assert_eq!(frame["kind"], "snapshot", "{frame}");
        assert_eq!(frame["id"], collection);
        let first = daemon.await_span(root.path(), |span| {
            span["name"] == "st.subscription.first_frame"
                && string_attribute(span, "st.subscription.id") == Some(collection)
        });
        assert_upgrade_link(&first, &upgrade);
        assert_eq!(string_attribute(&first, "st.collection"), Some(collection));
        assert!(string_attribute(&first, "span.label").is_some_and(|label| !label.is_empty()), "{first}");
        assert_eq!(int_attribute(&first, "st.page.rows"),
            Some(i64::try_from(frame["items"].as_array().unwrap().len()).unwrap()));
        assert_eq!(int_attribute(&first, "st.page.bytes"), Some(i64::try_from(bytes).unwrap()));
        if collection == "agents" {
            assert!(first["attributes"].as_array().unwrap().iter().any(|attribute| {
                matches!(attribute["key"].as_str(),
                    Some("st.projection.hit" | "st.projection.cold" | "st.projection.incremental" | "st.projection.shared"))
                    && attribute["value"]["boolValue"] == true
            }), "{first}");
        }
    }
    // Each acknowledged mutation is observed through an actual change frame: no sleep is
    // standing in for N rereads. Other held windows are irrelevant to glass claims.
    let mut revision = Value::Null;
    for change in 0..3 {
        let saved = source.append_claim(&st3::model::ClaimInput {
            subject: "glass/person/ada/019a0000-0000-7000-8000-000000000002".into(),
            kind: "glass.upserted".into(), actor: Some("person/ada".into()),
            fields: serde_json::from_value(serde_json::json!({
                "body":{"name":format!("Glass {change}"),"layout":{"tabs":[{"pane":"opaque:otel"}]}},
                "base_revision":revision,
            })).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        revision = serde_json::json!(saved.id);
        receive_stage_claims(&daemon, &source, TRACEPARENT).await;
        let (frame, _) = next_json_frame(&mut socket).await;
        assert_eq!(frame["kind"], "changes", "{frame}");
        assert_eq!(frame["id"], "glasses");
        assert!(frame["upserts"].as_array().is_some_and(|items| !items.is_empty()), "{frame}");
    }
    socket.close(None).await.unwrap();
    // The captured server marker closes the exporter FIFO after all observed frames.
    daemon.health(Some(BARRIER));
    daemon.await_span(root.path(), |span| span["traceId"] == "51af7651916cd43dd8448eb211c80319");
    let spans = daemon.captured_spans(root.path());
    let first: Vec<_> = spans.iter().filter(|span| span["name"] == "st.subscription.first_frame").collect();
    assert_eq!(first.len(), 6, "changes must not start more first-frame spans: {first:?}");
    for collection in ["missions", "attention", "agents", "work", "glasses", "arrangements"] {
        assert_eq!(first.iter().filter(|span| string_attribute(span, "st.subscription.id") == Some(collection)).count(), 1);
    }
    assert!(spans.iter().all(|span| !matches!(span["name"].as_str(),
        Some("st.subscription.reread" | "st.subscription.change" | "st.subscription.frame"))));
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn conversation_first_frames_link_collection_and_dedicated_upgrades() {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;
    let Some(collector) = otelite("conversation_first_frames_link_collection_and_dedicated_upgrades") else { return };
    let root = tempfile::tempdir().unwrap();
    seed_roster(root.path(), "agent/otel-test.observed");
    let mut daemon = ExportDaemon::start(&collector, root.path());
    for (path, protocol, traceparent, trace, id) in [
        ("/v1/client/collections/stream", "st3.client.collections.v0",
            "00-61af7651916cd43dd8448eb211c80319-b7ad6b7169203334-01",
            "61af7651916cd43dd8448eb211c80319", "conversation-held"),
        ("/v1/client/conversations/agent%2Fotel-test.observed/stream", "st3.client.conversation.v0",
            "00-71af7651916cd43dd8448eb211c80319-b7ad6b7169203335-01",
            "71af7651916cd43dd8448eb211c80319", "conversation"),
    ] {
        let mut socket = daemon.websocket(path, protocol, traceparent).await;
        if protocol == "st3.client.collections.v0" {
            socket.send(Message::Text(serde_json::json!({
                "kind":"subscribe", "id":id, "collection":"conversation",
                "conversation":"agent/otel-test.observed"
            }).to_string().into())).await.unwrap();
        }
        let (frame, bytes) = next_json_frame(&mut socket).await;
        let page = frame.get("value").unwrap_or(&frame);
        let rows = page["items"].as_array().unwrap_or_else(|| panic!("conversation first page: {frame}")).len();
        let upgrade = daemon.await_span(root.path(), |span| {
            span["traceId"] == trace && int_attribute(span, "http.response.status_code") == Some(101)
        });
        let first = daemon.await_span(root.path(), |span| {
            span["name"] == "st.subscription.first_frame"
                && string_attribute(span, "st.subscription.id") == Some(id)
        });
        assert_upgrade_link(&first, &upgrade);
        assert_eq!(string_attribute(&first, "st.collection"), Some("conversation"));
        assert!(string_attribute(&first, "span.label").is_some_and(|label| !label.is_empty()), "{first}");
        assert_eq!(int_attribute(&first, "st.page.rows"), Some(i64::try_from(rows).unwrap()));
        assert_eq!(int_attribute(&first, "st.page.bytes"), Some(i64::try_from(bytes).unwrap()));
        socket.close(None).await.unwrap();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn replication_projection_is_linked_root_only_for_new_data() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    const TRACE: &str = "81af7651916cd43dd8448eb211c80319";
    const NEW: &str = "00-81af7651916cd43dd8448eb211c80319-b7ad6b7169203336-01";
    const DUPLICATE: &str = "00-91af7651916cd43dd8448eb211c80319-b7ad6b7169203337-01";
    const HEARTBEAT: &str = "00-a1af7651916cd43dd8448eb211c80319-b7ad6b7169203338-01";
    let Some(collector) = otelite("replication_projection_is_linked_root_only_for_new_data") else { return };
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("daemon-state");
    std::fs::create_dir_all(&state).unwrap();
    let target = st3::store::Store::open(&state.join("claims.sqlite3"), "otel-test").unwrap();
    target.bind_fleet(FLEET).unwrap();
    target.project_replication_backlog().unwrap();
    let source = st3::store::Store::open_memory("otel-source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    source.append_claim(&st3::model::ClaimInput {
        subject: "resource/otel-stage-projection".into(),
        kind: "resource.observed".into(),
        actor: None,
        fields: serde_json::from_value(serde_json::json!({
            "kind":"vcs.pull-request", "facts":{"state":"open"}
        })).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let exchange = source.export_replication_exchange(FLEET, &target.replication_inventory().unwrap()).unwrap();
    assert!(!exchange.envelopes.is_empty(), "the real exchange must carry new data");
    let heartbeat = source.export_replication_summary(FLEET).unwrap();
    assert!(heartbeat.envelopes.is_empty());
    drop(target);
    let mut daemon = ExportDaemon::start(&collector, root.path());
    let payload = serde_json::json!({"peer":"otel-source", "fleet_id":FLEET, "exchange":exchange});
    let (received, _) = daemon.request("POST", "/v1/internal/replication/receive",
        Some(payload.clone()), NEW, None).await;
    assert!(received["value"]["receipt"]["received"].as_u64().is_some_and(|count| count > 0), "{received}");
    let receive = daemon.await_span(root.path(), |span| {
        span["traceId"] == TRACE && span["name"] == "POST /v1/internal/replication/receive"
    });
    let projection = daemon.await_span(root.path(), |span| span["name"] == "st.replication.projection");
    assert_upgrade_link(&projection, &receive);
    assert_eq!(string_attribute(&projection, "st.replication.peer"), Some("otel-source"));
    assert!(int_attribute(&projection, "st.replication.moved_envelopes").is_some_and(|count| count > 0), "{projection}");
    for (body, traceparent, trace) in [
        (payload, DUPLICATE, "91af7651916cd43dd8448eb211c80319"),
        (serde_json::json!({"peer":"otel-source", "fleet_id":FLEET, "exchange":heartbeat}),
            HEARTBEAT, "a1af7651916cd43dd8448eb211c80319"),
    ] {
        let (response, _) = daemon.request("POST", "/v1/internal/replication/receive",
            Some(body), traceparent, None).await;
        assert_eq!(response["value"]["receipt"]["received"], 0, "{response}");
        assert_eq!(response["value"]["receipt"]["signatures"], 0, "{response}");
        daemon.await_span(root.path(), |span| span["traceId"] == trace
            && span["name"] == "POST /v1/internal/replication/receive");
    }
    let spans = daemon.captured_spans(root.path());
    let projections: Vec<_> = spans.iter().filter(|span| span["name"] == "st.replication.projection").collect();
    assert_eq!(projections.len(), 1, "no projection root for duplicate/heartbeat: {projections:?}");
    assert_upgrade_link(projections[0], &receive);
}

/// Contention is forced on the real roster admission, not timed: the waiter future is
/// polled once and must be pending behind the holder before the holder is released.
/// Export is scoped to this thread's subscriber and in-memory provider, never global.
#[test]
fn roster_admission_waiter_links_its_holder() {
    use opentelemetry::trace::TracerProvider as _;
    use tracing::Instrument as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use futures_util::FutureExt as _;

    fn attribute<'a>(
        span: &'a opentelemetry_sdk::trace::SpanData,
        key: &str,
    ) -> Option<&'a opentelemetry::Value> {
        span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| &kv.value)
    }
    fn int(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<i64> {
        match attribute(span, key)? {
            opentelemetry::Value::I64(value) => Some(*value),
            _ => None,
        }
    }
    fn double(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<f64> {
        match attribute(span, key)? {
            opentelemetry::Value::F64(value) => Some(*value),
            _ => None,
        }
    }
    fn string(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
        match attribute(span, key)? {
            opentelemetry::Value::String(value) => Some(value.as_str().to_owned()),
            _ => None,
        }
    }

    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("st3-admission-proof")));
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let _export = st3::test_support::roster_export_scope();
    let store = st3::store::Store::open_memory("otel-admission").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let holder = st3::test_support::admit_fixture_roster(&store, "refresher")
            .instrument(tracing::info_span!("admission.holder"))
            .await;
        let mut waiter = std::pin::pin!(
            st3::test_support::admit_fixture_roster(&store, "http")
                .instrument(tracing::info_span!("admission.waiter"))
        );
        assert!(
            futures_util::poll!(waiter.as_mut()).is_pending(),
            "the waiter must queue behind the held admission"
        );
        drop(holder);
        drop(waiter.await);
        // Multi-gate try-admit publishes the same holder identity as FIFO admission.
        let try_span = tracing::info_span!("admission.try_holder");
        let holder = {
            let _entered = try_span.enter();
            st3::test_support::try_admit_fixture_roster(&store, "ws").unwrap()
        };
        assert!(st3::test_support::try_admit_fixture_roster(&store, "other").is_none());
        let mut waiter = std::pin::pin!(
            st3::test_support::admit_fixture_roster(&store, "http")
                .instrument(tracing::info_span!("admission.try_waiter"))
        );
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        drop(holder);
        drop(waiter.await);
        drop(try_span);
        // An incremental refresher has no existing span; timing still identifies its hold.
        let holder = st3::test_support::admit_fixture_roster(&store, "refresher").await;
        let mut waiter = std::pin::pin!(
            st3::test_support::admit_fixture_roster(&store, "ws")
                .instrument(tracing::info_span!("admission.unlinked_waiter"))
        );
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        drop(holder);
        drop(waiter.await);

        // W starts while A owns admission, but hands it to B before W's actual
        // enqueue. Metadata must be sampled with that enqueue, not at W's start.
        let holder = st3::test_support::admit_fixture_roster(&store, "other")
            .instrument(tracing::info_span!("admission.handoff_a"))
            .await;
        let successor = std::cell::RefCell::new(None);
        let mut waiter = std::pin::pin!(
            st3::test_support::admit_fixture_roster_after_handoff(&store, "ws", || {
                drop(holder);
                successor.replace(Some(
                    st3::test_support::admit_fixture_roster(&store, "http")
                        .instrument(tracing::info_span!(parent: None, "admission.handoff_b"))
                        .now_or_never()
                        .expect("B must acquire the admission just released by A"),
                ));
            }).instrument(tracing::info_span!("admission.handoff_waiter"))
        );
        assert!(futures_util::poll!(waiter.as_mut()).is_pending(), "W must wait behind B");
        drop(successor.borrow_mut().take());
        drop(waiter.await);

        // A released permit belongs to queued B even before B is repolled. Tokio
        // cannot expose B's identity in that reservation window. W and C must not
        // blame stale A; canceling reserved B must let W acquire, with C still FIFO.
        let holder = st3::test_support::admit_fixture_roster(&store, "refresher")
            .instrument(tracing::info_span!("admission.reservation_a"))
            .await;
        let mut successor = Box::pin(
            st3::test_support::admit_fixture_roster(&store, "http")
                .instrument(tracing::info_span!("admission.reserved_b"))
        );
        assert!(futures_util::poll!(successor.as_mut()).is_pending());
        let mut canceled = Box::pin(
            st3::test_support::admit_fixture_roster(&store, "other")
                .instrument(tracing::info_span!("admission.canceled"))
        );
        assert!(futures_util::poll!(canceled.as_mut()).is_pending());
        drop(canceled);
        drop(holder);
        let mut waiter = Box::pin(
            st3::test_support::admit_fixture_roster(&store, "ws")
                .instrument(tracing::info_span!("admission.unknown_waiter"))
        );
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        let mut tail = Box::pin(
            st3::test_support::admit_fixture_roster(&store, "other")
                .instrument(tracing::info_span!("admission.unknown_tail"))
        );
        assert!(futures_util::poll!(tail.as_mut()).is_pending());
        drop(successor);
        let holder = waiter.await;
        assert!(futures_util::poll!(tail.as_mut()).is_pending(), "C cannot bypass W");
        drop(holder);
        drop(tail.await);
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let named = |name: &str| {
        spans.iter().find(|span| span.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {spans:?}"))
    };
    let holder = named("admission.holder");
    let waiter = named("admission.waiter");

    let holder_id = int(holder, "st.roster.admission.id").expect("holder admission id");
    assert!(double(holder, "st.roster.admission.hold_ms").is_some_and(|ms| ms >= 0.0), "{holder:?}");
    // Every acquisition begins as a waiter, so its own class is `waiter_class`;
    // `holder_class` names only a blocking holder and is absent when uncontended.
    assert_eq!(string(holder, "st.roster.admission.waiter_class").as_deref(), Some("refresher"), "{holder:?}");
    assert_eq!(string(holder, "st.roster.admission.holder_class"), None, "{holder:?}");
    assert_eq!(int(holder, "st.roster.admission.holder_id"), None, "uncontended holder: {holder:?}");
    assert!(holder.links.iter().next().is_none(), "uncontended holder has no link: {holder:?}");

    let waiter_id = int(waiter, "st.roster.admission.id").expect("waiter admission id");
    assert_ne!(waiter_id, holder_id, "each acquisition has its own id");
    assert_eq!(int(waiter, "st.roster.admission.holder_id"), Some(holder_id), "{waiter:?}");
    assert_eq!(string(waiter, "st.roster.admission.waiter_class").as_deref(), Some("http"), "{waiter:?}");
    assert_eq!(string(waiter, "st.roster.admission.holder_class").as_deref(), Some("refresher"), "{waiter:?}");
    assert!(double(waiter, "st.roster.admission.wait_ms").is_some_and(|ms| ms >= 0.0), "{waiter:?}");
    assert!(double(waiter, "st.roster.admission.holder_age_ms").is_some_and(|ms| ms >= 0.0), "{waiter:?}");
    assert!(double(waiter, "st.roster.admission.hold_ms").is_some_and(|ms| ms >= 0.0), "{waiter:?}");

    let links: Vec<_> = waiter.links.iter().collect();
    assert_eq!(links.len(), 1, "one link from waiter to holder: {waiter:?}");
    assert_eq!(links[0].span_context, holder.span_context, "link targets the holder span");
    assert!(
        links[0].attributes.iter().any(|kv| kv.key.as_str() == "st.roster.admission.holder_id"
            && kv.value == opentelemetry::Value::I64(holder_id)),
        "link carries the holder id: {:?}",
        links[0]
    );
    let unlinked = named("admission.unlinked_waiter");
    assert_eq!(string(unlinked, "st.roster.admission.waiter_class").as_deref(), Some("ws"));
    assert_eq!(string(unlinked, "st.roster.admission.holder_class").as_deref(), Some("refresher"));
    assert!(int(unlinked, "st.roster.admission.holder_id").is_some());
    assert!(double(unlinked, "st.roster.admission.wait_ms").is_some_and(|ms| ms >= 0.0));
    assert!(double(unlinked, "st.roster.admission.holder_age_ms").is_some_and(|ms| ms >= 0.0));
    assert!(unlinked.links.iter().next().is_none(), "no synthetic span or invalid link: {unlinked:?}");

    let try_holder = named("admission.try_holder");
    let try_waiter = named("admission.try_waiter");
    let try_id = int(try_holder, "st.roster.admission.id").expect("try-admit holder id");
    assert_eq!(string(try_holder, "st.roster.admission.waiter_class").as_deref(), Some("ws"));
    assert_eq!(double(try_holder, "st.roster.admission.wait_ms"), Some(0.0));
    assert!(double(try_holder, "st.roster.admission.hold_ms").is_some_and(|ms| ms >= 0.0));
    assert_eq!(int(try_waiter, "st.roster.admission.holder_id"), Some(try_id));
    assert_eq!(string(try_waiter, "st.roster.admission.holder_class").as_deref(), Some("ws"));
    assert!(double(try_waiter, "st.roster.admission.holder_age_ms").is_some_and(|ms| ms >= 0.0));
    let links: Vec<_> = try_waiter.links.iter().collect();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].span_context, try_holder.span_context);

    let handoff_a = named("admission.handoff_a");
    let handoff_b = named("admission.handoff_b");
    let handoff_waiter = named("admission.handoff_waiter");
    let handoff_a_id = int(handoff_a, "st.roster.admission.id").expect("A admission id");
    let handoff_b_id = int(handoff_b, "st.roster.admission.id").expect("B admission id");
    assert_ne!(handoff_a_id, handoff_b_id);
    assert_eq!(int(handoff_waiter, "st.roster.admission.holder_id"), Some(handoff_b_id));
    assert_ne!(int(handoff_waiter, "st.roster.admission.holder_id"), Some(handoff_a_id));
    assert_eq!(string(handoff_waiter, "st.roster.admission.holder_class").as_deref(), Some("http"));
    assert_eq!(string(handoff_waiter, "st.roster.admission.waiter_class").as_deref(), Some("ws"));
    assert!(double(handoff_waiter, "st.roster.admission.holder_age_ms").is_some_and(|ms| ms >= 0.0));
    let links: Vec<_> = handoff_waiter.links.iter().collect();
    assert_eq!(links.len(), 1, "handoff waiter has exactly one blocker link");
    assert_eq!(links[0].span_context, handoff_b.span_context, "W links B, not stale A");
    assert_ne!(links[0].span_context, handoff_a.span_context);
    assert!(links[0].attributes.iter().any(|kv| kv.key.as_str() == "st.roster.admission.holder_id"
        && kv.value == opentelemetry::Value::I64(handoff_b_id)));
    for event in handoff_waiter.events.iter().filter(|event| event.name == "st.roster.admission.wait") {
        assert!(event.attributes.iter().any(|kv| kv.key.as_str() == "st.roster.admission.holder_id"
            && kv.value == opentelemetry::Value::I64(handoff_b_id)), "{event:?}");
        assert!(event.attributes.iter().any(|kv| kv.key.as_str() == "st.roster.admission.holder_class"
            && kv.value == opentelemetry::Value::String("http".into())), "{event:?}");
    }
    let unknown = named("admission.unknown_waiter");
    let unknown_tail = named("admission.unknown_tail");
    for span in [unknown, unknown_tail] {
        assert_eq!(string(span, "st.roster.admission.holder_class").as_deref(), Some("unknown"));
        assert_eq!(int(span, "st.roster.admission.holder_id"), None, "no invented blocker id");
        assert_eq!(double(span, "st.roster.admission.holder_age_ms"), None, "no invented blocker age");
        assert!(span.links.iter().next().is_none(), "no guessed blocker context: {span:?}");
        assert!(double(span, "st.roster.admission.wait_ms").is_some_and(|ms| ms >= 0.0));
        assert!(double(span, "st.roster.admission.hold_ms").is_some_and(|ms| ms >= 0.0));
    }
    assert_eq!(string(unknown, "st.roster.admission.waiter_class").as_deref(), Some("ws"));
    assert_eq!(string(unknown_tail, "st.roster.admission.waiter_class").as_deref(), Some("other"));
    // Correlation uses process-local numbers and closed classes only.
    for span in [holder, waiter, unlinked, handoff_a, handoff_b, handoff_waiter, unknown, unknown_tail] {
        for kv in span.attributes.iter().filter(|kv| kv.key.as_str().starts_with("st.roster.")) {
            assert!(
                !matches!(&kv.value, opentelemetry::Value::String(value)
                    if !matches!(value.as_str(), "http" | "ws" | "refresher" | "other" | "unknown")),
                "unbounded roster string attribute: {kv:?}"
            );
        }
    }
}
