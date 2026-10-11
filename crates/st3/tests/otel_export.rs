//! Real CLI OTLP/HTTP-JSON export into the effect-utils otelite receiver.
//!
//! `ST3_OTELITE_BIN` supplies the collector; `ST3_OTEL_REQUIRE=1` forbids a local skip.
//! Like the st2 export test, `otelite run` owns the ephemeral receiver and flushes its
//! capture after the command exits, avoiding capture-mode stdin/readiness races.

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

// OTLP histogram counts arrive as a JSON number or an OTLP decimal string.
#[cfg(target_os = "linux")]
fn histogram_count(point: &Value) -> Option<u64> {
    point["count"]
        .as_u64()
        .or_else(|| point["count"].as_str().and_then(|count| count.parse().ok()))
}

// scopeMetrics -> metric -> histogram, gauge or sum data points.
#[cfg(target_os = "linux")]
fn metric_points<'a>(request: &'a Value, name: &str) -> Vec<&'a Value> {
    service_metric_points(request, "st-daemon", name)
}

#[cfg(target_os = "linux")]
fn service_metric_points<'a>(request: &'a Value, service: &str, name: &str) -> Vec<&'a Value> {
    request["resourceMetrics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|batch| string_attribute(&batch["resource"], "service.name") == Some(service))
        .flat_map(|batch| batch["scopeMetrics"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["metrics"].as_array().into_iter().flatten())
        .filter(|metric| metric["name"].as_str() == Some(name))
        .flat_map(|metric| {
            metric["histogram"]["dataPoints"]
                .as_array()
                .or_else(|| metric["gauge"]["dataPoints"].as_array())
                .or_else(|| metric["sum"]["dataPoints"].as_array())
                .into_iter()
                .flatten()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn daemon_spans<'a>(request: &'a Value, name: &str) -> Vec<&'a Value> {
    service_spans(request, "st-daemon", name)
}

#[cfg(target_os = "linux")]
fn service_spans<'a>(request: &'a Value, service: &str, name: &str) -> Vec<&'a Value> {
    request["resourceSpans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|batch| string_attribute(&batch["resource"], "service.name") == Some(service))
        .flat_map(|batch| batch["scopeSpans"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["spans"].as_array().into_iter().flatten())
        .filter(|span| span["name"].as_str() == Some(name))
        .collect()
}

// `st.writer.wait_ms` is a double-valued total in milliseconds.
#[cfg(target_os = "linux")]
fn double_attribute(record: &Value, name: &str) -> Option<f64> {
    record["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"].as_str() == Some(name))?["value"]["doubleValue"]
        .as_f64()
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

    /// A real write: POST /v1/claims, so the batch writer commits and ACKs
    /// inside this request's SERVER span.
    fn write_claim(&self, traceparent: &str) {
        use std::io::{Read as _, Write as _};
        let mut socket = std::os::unix::net::UnixStream::connect(&self.socket).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let body = serde_json::to_string(&serde_json::json!({
            "subject": "custom/otel/storage",
            "kind": "custom.otel.storage-written",
            "fields": {},
            "idempotency_key": "otel-export-storage-proof",
        }))
        .unwrap();
        write!(
            socket,
            "POST /v1/claims HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nx-st3-client: fractal\r\ntraceparent: {traceparent}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 "),
            "claim write failed: {response}\n{}",
            self.diagnostics()
        );
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
fn cli_request_and_daemon_server_share_one_trace() {
    let Some(collector) = otelite("cli_request_and_daemon_server_share_one_trace") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    let mut command = isolated_command(&collector, root.path());
    command
        .args(["run", "--out"])
        .arg(root.path().join("cli-capture"))
        .args(["--protocol", "http/json", "--"])
        .arg(st3())
        .arg("--endpoint")
        .arg(&daemon.socket)
        .args(["--daemon-wait", "0", "agents", "ls"]);
    let output = command.output().expect("run CLI against exporting test daemon");
    assert!(
        output.status.success(),
        "CLI request failed: {output:?}\n{}",
        daemon.diagnostics(),
    );
    let traces = std::fs::read_to_string(root.path().join("cli-capture/traces.ndjson"))
        .expect("CLI exports to otelite");
    let roots = command_roots(&traces);
    assert_eq!(roots.len(), 1, "exactly one CLI command root: {traces}");
    let (resource, cli) = &roots[0];
    assert_eq!(string_attribute(resource, "service.name"), Some("st-cli"));
    let trace_id = cli["traceId"].as_str().expect("CLI trace id");
    let cli_span_id = cli["spanId"].as_str().expect("CLI span id");
    daemon.await_export(&root.path().join("capture/traces.ndjson"), |request| {
        request["resourceSpans"].as_array().is_some_and(|batches| {
            batches.iter().any(|batch| {
                string_attribute(&batch["resource"], "service.name") == Some("st-daemon")
                    && batch["scopeSpans"].as_array().is_some_and(|scopes| {
                        scopes.iter().any(|scope| {
                            scope["spans"].as_array().is_some_and(|spans| {
                                spans.iter().any(|server| {
                                    server["traceId"].as_str() == Some(trace_id)
                                        && server["parentSpanId"].as_str() == Some(cli_span_id)
                                        && string_attribute(server, "http.request.method") == Some("GET")
                                        && int_attribute(server, "http.response.status_code") == Some(200)
                                        && (server["kind"].as_str() == Some("SPAN_KIND_SERVER")
                                            || server["kind"].as_u64() == Some(2))
                                        && server["attributes"].as_array().is_some_and(|attributes| {
                                            !attributes.iter().any(|attribute| {
                                                attribute["key"] == "st.parent.sampled"
                                                    && attribute["value"]["boolValue"] == true
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

// A real write proves the storage instruments export: the batch writer commits
// the claim, the WAL gauge observes the file the write produced, and the
// request's own SERVER span carries the writer totals the handler accumulated.
#[cfg(target_os = "linux")]
#[test]
fn daemon_write_request_exports_storage_metrics() {
    let Some(collector) = otelite("daemon_write_request_exports_storage_metrics") else {
        return;
    };
    const TRACE_ID: &str = "fedcba0987654321fedcba0987654321";
    const PARENT_ID: &str = "fedcba0987654321";
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    daemon.write_claim(&format!("00-{TRACE_ID}-{PARENT_ID}-01"));
    // The COMMIT's frames are in the daemon's WAL before the response returns, so the file
    // on disk is the same WAL the gauge must report.
    let wal = root.path().join("daemon-state").join("claims.sqlite3-wal");
    let wal_bytes = std::fs::metadata(&wal).map(|metadata| metadata.len()).unwrap_or(0);
    assert!(
        wal_bytes > 0,
        "the written claim must fill {}: {} bytes",
        wal.display(),
        wal_bytes
    );
    daemon.await_export(&root.path().join("capture/metrics.ndjson"), |request| {
        metric_points(request, "db.client.operation.duration")
            .into_iter()
            .any(|point| {
                string_attribute(point, "db.system.name") == Some("sqlite")
                    && string_attribute(point, "db.operation.name") == Some("write.batched")
                    && histogram_count(point).is_some_and(|count| count > 0)
            })
    });
    daemon.await_export(&root.path().join("capture/metrics.ndjson"), |request| {
        metric_points(request, "st.db.writer.commit.duration")
            .into_iter()
            .any(|point| {
                string_attribute(point, "db.system.name") == Some("sqlite")
                    && histogram_count(point).is_some_and(|count| count > 0)
            })
    });
    // Observable gauge over the main database's -wal file, which the write above filled.
    daemon.await_export(&root.path().join("capture/metrics.ndjson"), |request| {
        metric_points(request, "st.db.wal.size")
            .into_iter()
            .any(|point| {
                string_attribute(point, "db.system.name") == Some("sqlite")
                    && point["asInt"]
                        .as_f64()
                        .or_else(|| point["asDouble"].as_f64())
                        .is_some_and(|size| size > 0.0)
            })
    });
    daemon.await_export(&root.path().join("capture/traces.ndjson"), |request| {
        request["resourceSpans"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|batch| batch["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|scope| scope["spans"].as_array().into_iter().flatten())
            .any(|span| {
                span["traceId"].as_str() == Some(TRACE_ID)
                    && span["parentSpanId"].as_str() == Some(PARENT_ID)
                    && span["name"].as_str() == Some("POST /v1/claims")
                    && string_attribute(span, "http.route") == Some("/v1/claims")
                    && int_attribute(span, "st.writer.ops").is_some_and(|ops| ops >= 1)
                    && double_attribute(span, "st.writer.wait_ms")
                        .is_none_or(|wait| wait >= 1.0)
            })
    });
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_write_exports_independent_reconcile_pass_and_metrics() {
    let Some(collector) =
        otelite("daemon_write_exports_independent_reconcile_pass_and_metrics")
    else {
        return;
    };
    const TRACE_ID: &str = "abcdef0123456789abcdef0123456789";
    const PARENT_ID: &str = "abcdef0123456789";
    const WAKE_CAUSES: &[&str] = &[
        "api",
        "replication_receive",
        "deadline",
        "timer_restart",
        "timer_step_timeout",
        "timer_resume_verification",
        "timer_gate_timeout",
        "timer_gate_recheck",
        "timer_gate",
        "timer_gate_poll",
        "timer_llm_gate",
        "file_watch",
        "reconciler",
        "recorder_receipts",
        "startup",
        "continuation",
        "other",
    ];
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    let traces_path = root.path().join("capture/traces.ndjson");
    let metrics_path = root.path().join("capture/metrics.ndjson");
    daemon.write_claim(&format!("00-{TRACE_ID}-{PARENT_ID}-01"));
    // Prove the write accepted the remote context; a reconcile span accidentally
    // inheriting this request must fail the root checks below.
    daemon.await_export(&traces_path, |request| {
        daemon_spans(request, "POST /v1/claims")
            .into_iter()
            .any(|span| {
                span["traceId"].as_str() == Some(TRACE_ID)
                    && span["parentSpanId"].as_str() == Some(PARENT_ID)
                    && int_attribute(span, "http.response.status_code") == Some(200)
            })
    });
    daemon.await_export(&traces_path, |request| {
        daemon_spans(request, "st.reconcile_pass")
            .into_iter()
            .any(|span| string_attribute(span, "st.reconcile.wake_cause") == Some("api"))
    });
    let durations = daemon.await_export(&metrics_path, |request| {
        metric_points(request, "st.reconcile.pass.duration")
            .into_iter()
            .any(|point| {
                string_attribute(point, "task") == Some("pass")
                    && histogram_count(point).is_some_and(|count| count > 0)
            })
    });
    for metric in durations["resourceMetrics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|batch| string_attribute(&batch["resource"], "service.name") == Some("st-daemon"))
        .flat_map(|batch| batch["scopeMetrics"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["metrics"].as_array().into_iter().flatten())
        .filter(|metric| metric["name"].as_str() == Some("st.reconcile.pass.duration"))
    {
        assert_eq!(metric["unit"].as_str(), Some("s"), "duration unit: {metric}");
    }
    let wakes = daemon.await_export(&metrics_path, |request| {
        metric_points(request, "st.reconcile.wakes")
            .into_iter()
            .any(|point| {
                string_attribute(point, "cause") == Some("api")
                    && point["asInt"]
                        .as_u64()
                        .or_else(|| point["asInt"].as_str().and_then(|value| value.parse().ok()))
                        .is_some_and(|count| count > 0)
            })
    });
    for point in metric_points(&wakes, "st.reconcile.wakes") {
        let cause = string_attribute(point, "cause").expect("wake counter cause attribute");
        assert!(WAKE_CAUSES.contains(&cause), "unbounded wake counter cause: {point}");
    }
    daemon.await_export(&metrics_path, |request| {
        metric_points(request, "st.fifo.depth")
            .into_iter()
            .any(|point| {
                string_attribute(point, "queue").is_some_and(|queue| ["writer", "conversation", "terminal_emulation"].contains(&queue))
                    && point["asInt"]
                        .as_u64()
                        .or_else(|| point["asInt"].as_str().and_then(|value| value.parse().ok()))
                        .is_some()
            })
    });
    // Stop and drain the collector before checking every pass, including passes
    // exported in other batches or at shutdown, not just the first API match.
    drop(daemon);
    let traces = std::fs::read_to_string(&traces_path).expect("otelite writes reconcile traces");
    let mut pass_count = 0;
    let mut api_count = 0;
    for line in traces.lines() {
        let request: Value = serde_json::from_str(line).expect("valid OTLP trace request");
        for span in daemon_spans(&request, "st.reconcile_pass") {
            pass_count += 1;
            assert!(
                span["parentSpanId"].as_str().is_none_or(str::is_empty),
                "reconcile pass must be a root, not a request child: {span}"
            );
            let trace_id = span["traceId"].as_str().expect("reconcile pass trace id");
            assert_ne!(trace_id, TRACE_ID, "reconcile pass inherited the request trace: {span}");
            let cause = string_attribute(span, "st.reconcile.wake_cause")
                .expect("reconcile pass wake cause attribute");
            assert!(WAKE_CAUSES.contains(&cause), "unbounded reconcile wake cause: {span}");
            if cause == "api" {
                api_count += 1;
            }
        }
    }
    assert!(pass_count > 0, "no reconcile pass exported:\n{traces}");
    assert!(api_count > 0, "write must export an API-woken reconcile pass:\n{traces}");
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_startup_exports_completed_root_phases_and_duration() {
    let Some(collector) = otelite("daemon_startup_exports_completed_root_phases_and_duration") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut daemon = ExportDaemon::start(&collector, root.path());
    let traces_path = root.path().join("capture/traces.ndjson");
    let metrics_path = root.path().join("capture/metrics.ndjson");
    // Observe the completed root while the daemon is still serving: no flush at readiness.
    daemon.await_export(&traces_path, |request| {
        daemon_spans(request, "st.startup").into_iter().any(|span| {
            string_attribute(span, "st.startup.outcome") == Some("serving")
        })
    });
    let durations = daemon.await_export(&metrics_path, |request| {
        ["total", "node_identity", "open_store"].iter().all(|phase| {
            metric_points(request, "st.startup.duration").into_iter().any(|point| {
                string_attribute(point, "phase") == Some(*phase)
                    && histogram_count(point) == Some(1)
            })
        })
    });
    for point in metric_points(&durations, "st.startup.duration") {
        assert_eq!(histogram_count(point), Some(1), "one duration per phase: {point}");
        let boundaries = point["explicitBounds"].as_array().expect("explicit startup boundaries");
        assert_eq!(boundaries.first().and_then(Value::as_f64), Some(0.01));
        assert_eq!(boundaries.last().and_then(Value::as_f64), Some(1800.0));
    }
    drop(daemon);
    let traces = std::fs::read_to_string(&traces_path).unwrap();
    let requests: Vec<Value> = traces.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let roots: Vec<_> = requests.iter().flat_map(|request| daemon_spans(request, "st.startup")).collect();
    assert_eq!(roots.len(), 1, "one root per start: {traces}");
    let startup = roots[0];
    assert_eq!(string_attribute(startup, "st.startup.outcome"), Some("serving"));
    assert!(startup["parentSpanId"].as_str().is_none_or(|id| id.is_empty() || id == "0000000000000000"));
    assert_ne!(startup["status"]["code"].as_u64(), Some(2));
    let phases: Vec<_> = requests.iter().flat_map(|request| daemon_spans(request, "st.startup.phase")).collect();
    assert_eq!(phases.len(), 11, "one aggregate span per startup phase: {traces}");
    for phase in &phases {
        assert_eq!(phase["traceId"], startup["traceId"]);
        assert_eq!(phase["parentSpanId"], startup["spanId"]);
        assert!(string_attribute(phase, "span.label").is_some());
    }
    let identity = phases.iter().find(|phase| {
        string_attribute(phase, "st.startup.phase") == Some("node_identity")
    }).expect("node identity phase");
    let timestamp = |span: &Value, key: &str| {
        span[key].as_str().and_then(|value| value.parse::<u64>().ok())
            .or_else(|| span[key].as_u64()).expect("OTLP timestamp")
    };
    assert!(timestamp(startup, "startTimeUnixNano") <= timestamp(identity, "startTimeUnixNano"));
    let hooks = phases.iter().find(|phase| {
        string_attribute(phase, "st.startup.phase") == Some("install_hooks")
    }).unwrap();
    assert!(timestamp(identity, "endTimeUnixNano") <= timestamp(hooks, "startTimeUnixNano"));
    for name in ["open_store", "project_replication_backlog"] {
        assert!(phases.iter().any(|phase| string_attribute(phase, "st.startup.phase") == Some(name)));
    }
    let projection = phases.iter().find(|phase| {
        string_attribute(phase, "st.startup.phase") == Some("project_replication_backlog")
    }).unwrap();
    assert!(int_attribute(projection, "st.startup.claims_processed").is_some());
    assert!(projection["attributes"].as_array().unwrap().iter().any(|attribute| {
        attribute["key"] == "st.startup.full_replay" && attribute["value"]["boolValue"].is_boolean()
    }));
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_startup_identity_failure_exports_failed_error_root() {
    assert_startup_failure("node_identity");
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_startup_store_failure_exports_failed_error_root() {
    assert_startup_failure("open_store");
}

#[cfg(target_os = "linux")]
fn assert_startup_failure(failed_phase: &str) {
    let Some(collector) = otelite("daemon_startup_failure_exports_failed_error_root") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("daemon-state");
    std::fs::create_dir_all(&state).unwrap();
    if failed_phase == "node_identity" {
        // Recovery cannot inspect a directory as a database, even when run as root.
        std::fs::create_dir(state.join("claims.sqlite3")).unwrap();
    } else {
        // Identity inspection accepts this readable database; store schema admission does not.
        let database = rusqlite::Connection::open(state.join("claims.sqlite3")).unwrap();
        database.execute_batch("PRAGMA user_version = 999;").unwrap();
    }
    let mut command = isolated_command(&collector, root.path());
    let output = command
        .args(["run", "--out"])
        .arg(root.path().join("capture"))
        .args(["--protocol", "http/json", "--"])
        .arg(st3())
        .args(["up", "--node", "otel-test", "--state-dir"])
        .arg(state)
        .arg("--socket")
        .arg(root.path().join("run/api.sock"))
        .arg("--client-gateway-socket")
        .arg(root.path().join("run/client.sock"))
        .args(["--pty-binary", "pty"])
        .output().unwrap();
    assert!(!output.status.success(), "{failed_phase} must fail: {output:?}");
    let traces = std::fs::read_to_string(root.path().join("capture/traces.ndjson")).unwrap();
    let requests: Vec<Value> = traces.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let roots: Vec<_> = requests.iter().flat_map(|request| daemon_spans(request, "st.startup")).collect();
    assert_eq!(roots.len(), 1, "one failed startup root: {traces}\n{output:?}");
    assert_eq!(string_attribute(roots[0], "st.startup.outcome"), Some("failed"));
    assert_eq!(roots[0]["status"]["code"].as_u64(), Some(2));
    let phases: Vec<_> = requests.iter().flat_map(|request| daemon_spans(request, "st.startup.phase")).collect();
    let failed = phases.iter().find(|phase| {
        string_attribute(phase, "st.startup.phase") == Some(failed_phase)
    }).expect("failed startup phase");
    assert_eq!(failed["status"]["code"].as_u64(), Some(2));
    assert_eq!(failed["traceId"], roots[0]["traceId"]);
    assert_eq!(failed["parentSpanId"], roots[0]["spanId"]);
    assert_eq!(string_attribute(failed, "span.label"), Some(failed_phase));
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

// Run the gate proof in its own process, so SDK globals cannot leak between tests.
#[cfg(target_os = "linux")]
#[test]
fn storage_stage_gates_export_detached_roots() {
    const CHILD: &str = "ST3_OTEL_STORAGE_STAGE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use opentelemetry::trace::TraceContextExt as _;
        let mut telemetry = st3::otel::Telemetry::init(st3::otel::Unit::Daemon, None);
        assert!(telemetry.enabled());
        for wait in [0.0, 0.999, 1.0, 1.001] {
            let server = tracing::info_span!(
                "writer gate", otel.kind = "server",
                "st.writer.wait_ms" = tracing::field::Empty,
                "st.writer.ops" = tracing::field::Empty,
            );
            st3::otel::record_writer_wait(&server, (wait, 2));
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let store = st3::store::Store::open(&path, "otel-test").unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        // Real WAL frames with no reader pin deterministically take the TRUNCATE branch.
        connection.execute_batch(
            "CREATE TABLE otel_checkpoint_proof(value INTEGER); INSERT INTO otel_checkpoint_proof VALUES (1);"
        ).unwrap();
        let started = std::time::SystemTime::now();
        let truncated = store.checkpoint_idle_wal_report(&connection).unwrap();
        assert!(truncated.truncate_ms.is_some() && truncated.recycled);
        st3::otel::record_wal_checkpoint(started, Some(&truncated), false);
        let ordinary = store.checkpoint_idle_wal_report(&connection).unwrap();
        assert!(ordinary.truncate_ms.is_none());
        st3::otel::record_wal_checkpoint(std::time::SystemTime::now(), Some(&ordinary), false);
        let ambient = tracing::info_span!("ambient checkpoint");
        let ambient_context = ambient.context();
        let _entered = ambient.enter();
        let _attached = ambient_context.attach();
        // Repeat both gates under a tracing span and an attached OTel context.
        st3::otel::record_wal_checkpoint(std::time::SystemTime::now(), Some(&ordinary), false);
        st3::otel::record_wal_checkpoint(std::time::SystemTime::now(), Some(&truncated), false);
        st3::otel::record_wal_checkpoint(std::time::SystemTime::now(), Some(&ordinary), true);
        st3::otel::record_wal_checkpoint(std::time::SystemTime::now(), None, true);
        let current = opentelemetry::Context::current();
        assert!(current.span().span_context().is_valid());
        drop(_attached);
        drop(_entered);
        drop(ambient);
        telemetry.shutdown();
        return;
    }
    let Some(collector) = otelite("storage_stage_gates_export_detached_roots") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = isolated_command(&collector, root.path())
        .env(CHILD, "1")
        .args(["run", "--out"]).arg(root.path().join("capture"))
        .args(["--protocol", "http/json", "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["otel_export::storage_stage_gates_export_detached_roots", "--exact", "--nocapture"])
        .output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let traces = std::fs::read_to_string(root.path().join("capture/traces.ndjson")).unwrap();
    let spans: Vec<Value> = traces.lines().flat_map(|line| {
        let request: Value = serde_json::from_str(line).unwrap();
        request["resourceSpans"].as_array().unwrap().iter()
            .flat_map(|batch| batch["scopeSpans"].as_array().unwrap())
            .flat_map(|scope| scope["spans"].as_array().unwrap())
            .cloned().collect::<Vec<_>>()
    }).collect();
    let writers: Vec<_> = spans.iter().filter(|span| span["name"] == "writer gate").collect();
    assert_eq!(writers.len(), 4, "{traces}");
    let waits: Vec<_> = writers.iter().filter_map(|span| double_attribute(span, "st.writer.wait_ms")).collect();
    assert_eq!(waits.len(), 2, "{traces}");
    assert!(waits.contains(&1.0) && waits.contains(&1.001), "{traces}");
    assert!(writers.iter().all(|span| int_attribute(span, "st.writer.ops") == Some(2)));
    let checkpoints: Vec<_> = spans.iter().filter(|span| span["name"] == "st.db.checkpoint").collect();
    assert_eq!(checkpoints.len(), 4, "ordinary passes must emit none: {traces}");
    assert_eq!(checkpoints.iter().filter(|span| span["status"]["code"] == 2).count(), 2, "{traces}");
    assert_eq!(checkpoints.iter().filter(|span| string_attribute(span, "db.operation.name") == Some("checkpoint.truncate")).count(), 2);
    let ambient = spans.iter().find(|span| span["name"] == "ambient checkpoint").unwrap();
    for span in checkpoints {
        assert!(span["parentSpanId"].as_str().is_none_or(|id| id.is_empty() || id == "0000000000000000"), "{span}");
        assert_ne!(span["traceId"], ambient["traceId"], "{span}");
        assert!(span["links"].as_array().is_none_or(Vec::is_empty), "{span}");
        assert_eq!(string_attribute(span, "db.system.name"), Some("sqlite"));
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
    // The refresher publishes rosters independently. HTTP reads serve a published
    // cut, including after a new claim, and never rebuild on the request's behalf.
    let mut responses = Vec::new();
    for attempt in 0..3 {
        if attempt != 0 {
            daemon.request("POST", "/v1/diagnostics/harness", Some(serde_json::json!({
                "actor":actor, "code":"otel-roster-advance", "reason":"stage fixture",
                "severity":"warning", "status":"healthy", "incarnation_id":"otel-session",
                "idempotency_key":format!("otel-roster-advance-{attempt}"),
            })), TRACEPARENT, None).await;
        }
        let (body, bytes) = daemon.request("GET",
            "/v1/client/agents?history=true&status=stopped", None, TRACEPARENT, None).await;
        responses.push((body, bytes));
    }
    for (body, bytes) in responses {
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
    }
    let spans = daemon.captured_spans(root.path());
    for rebuild in spans.iter().filter(|span| span["name"] == "st.roster.rebuild") {
        assert_internal_root(rebuild);
        assert_ne!(rebuild["traceId"], TRACE);
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

#[cfg(target_os = "linux")]
const REPLICATION_SERVICE: &str = "st-replication-worker";
#[cfg(target_os = "linux")]
const REPLICATION_PEERS: [&str; 2] = ["amber", "cobalt"];

#[cfg(target_os = "linux")]
fn capture_requests(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split_inclusive('\n')
        // The running collector may still be appending the final line.
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).expect("valid OTLP JSON request"))
        .collect()
}

#[cfg(target_os = "linux")]
fn replication_point_count(point: &Value, metric: &str) -> Option<u64> {
    if metric == "st.replication.round.duration" {
        histogram_count(point)
    } else {
        point["asInt"]
            .as_u64()
            .or_else(|| point["asInt"].as_str().and_then(|value| value.parse().ok()))
    }
}

#[cfg(target_os = "linux")]
fn replication_exports_ready(traces: &[Value], metrics: &[Value]) -> bool {
    let rounds: Vec<_> = traces
        .iter()
        .flat_map(|request| service_spans(request, REPLICATION_SERVICE, "st.replication.round"))
        .collect();
    // Prove both movement and a subsequent idle exchange, not just worker startup.
    ["moved", "in_sync"].into_iter().all(|outcome| {
        rounds.iter().any(|span| {
            string_attribute(span, "st.replication.outcome") == Some(outcome)
        })
    }) && ["st.replication.rounds", "st.replication.round.duration"]
        .into_iter()
        .all(|name| {
            let points: Vec<_> = metrics
                .iter()
                .flat_map(|request| service_metric_points(request, REPLICATION_SERVICE, name))
                .collect();
            let peers_exported = REPLICATION_PEERS.into_iter().all(|peer| {
                points.iter().any(|point| {
                    string_attribute(point, "peer") == Some(peer)
                        && string_attribute(point, "outcome")
                            .is_some_and(|outcome| ["moved", "in_sync"].contains(&outcome))
                        && replication_point_count(point, name).is_some_and(|count| count > 0)
                })
            });
            peers_exported
                && ["moved", "in_sync"].into_iter().all(|outcome| {
                    points.iter().any(|point| {
                        string_attribute(point, "outcome") == Some(outcome)
                            && replication_point_count(point, name).is_some_and(|count| count > 0)
                    })
                })
        })
}

/// The collector launches this exact ignored test in the existing integration binary.
/// Node owns two isolated foreground daemons and real replication-worker processes;
/// no services are installed, and its Drop kills and waits for every child.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "launched by replication_worker_exports_rounds_after_graph_convergence under otelite"]
async fn replication_export_scenario() {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;

    use super::fleet::{Node, wait_for_notes};

    let root = PathBuf::from(
        std::env::var_os("ST3_OTEL_REPLICATION_SCENARIO_ROOT")
            .expect("replication scenario must run under its otelite parent"),
    );
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .expect("otelite supplies an ephemeral local endpoint");
    let secret = root.join("fleet.secret");
    std::fs::write(&secret, hex::encode([42_u8; 32])).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut amber = Node::new(&root, REPLICATION_PEERS[0]);
    let mut cobalt = Node::new(&root, REPLICATION_PEERS[1]);
    let fleet_id = "8f14e45f-ceea-467a-9a2b-5c3d6e7f8091";
    amber.legacy_config(fleet_id, &secret, &[(REPLICATION_PEERS[1], cobalt.port)]);
    cobalt.legacy_config(fleet_id, &secret, &[(REPLICATION_PEERS[0], amber.port)]);
    for node in [&mut amber, &mut cobalt] {
        // Node clears inherited environment; opt these processes into this receiver only.
        node.env.extend(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
                ("OTEL_TRACES_EXPORTER", "otlp"),
                ("OTEL_METRICS_EXPORTER", "otlp"),
                ("OTEL_LOGS_EXPORTER", "none"),
                ("OTEL_BSP_SCHEDULE_DELAY", "100"),
                ("OTEL_METRIC_EXPORT_INTERVAL", "250"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        );
        // legacy_config causes start() to launch the actual replication worker too.
        node.start().await;
    }
    amber.note("otel-amber").await;
    cobalt.note("otel-cobalt").await;
    let expected = BTreeSet::from([
        "custom/fleet-test/otel-amber".to_owned(),
        "custom/fleet-test/otel-cobalt".to_owned(),
    ]);
    for node in [&amber, &cobalt] {
        wait_for_notes(node, &expected, 30, &[&amber, &cobalt]).await;
    }

    // Workers are killed on Drop, so prove periodic export while they are still live.
    // Shutdown flushing is deliberately not part of this test's correctness.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let traces = capture_requests(&root.join("capture/traces.ndjson"));
        let metrics = capture_requests(&root.join("capture/metrics.ndjson"));
        if replication_exports_ready(&traces, &metrics) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "replication periodic exports missing:\ntraces: {traces:?}\nmetrics: {metrics:?}\n{}\n{}",
            amber.logs(),
            cobalt.logs()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(target_os = "linux")]
#[test]
fn replication_worker_exports_rounds_after_graph_convergence() {
    let Some(collector) = otelite("replication_worker_exports_rounds_after_graph_convergence") else {
        return;
    };
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let output = isolated_command(&collector, root.path())
        .env("ST3_OTEL_REPLICATION_SCENARIO_ROOT", root.path())
        // The scenario resolves fixture binaries through test_env!; keep nextest archive paths.
        .envs(std::env::vars_os().filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                ["NEXTEST_BIN_EXE_", "CARGO_BIN_EXE_", "CI_TEST_"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            })
        }))
        .args(["run", "--out"])
        .arg(root.path().join("capture"))
        .args(["--protocol", "http/json", "--"])
        .arg(std::env::current_exe().expect("existing integration test binary"))
        .args([
            "--exact",
            "otel_export::replication_export_scenario",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .expect("run two-node replication scenario under otelite");
    assert!(
        output.status.success(),
        "two-node graph replication/export scenario failed: {output:?}"
    );
    let traces = capture_requests(&root.path().join("capture/traces.ndjson"));
    let metrics = capture_requests(&root.path().join("capture/metrics.ndjson"));
    assert!(
        replication_exports_ready(&traces, &metrics),
        "collector must retain worker spans and metrics: {output:?}"
    );
    for span in traces
        .iter()
        .flat_map(|request| service_spans(request, REPLICATION_SERVICE, "st.replication.round"))
    {
        assert!(
            span["parentSpanId"]
                .as_str()
                .is_none_or(|id| id.is_empty() || id == "0000000000000000"),
            "replication exchanges must be detached roots: {span}"
        );
        assert!(
            string_attribute(span, "st.replication.peer")
                .is_some_and(|peer| REPLICATION_PEERS.contains(&peer)),
            "unexpected replication peer: {span}"
        );
        let outcome = string_attribute(span, "st.replication.outcome")
            .expect("replication round outcome attribute");
        assert!(
            ["moved", "in_sync", "failed", "cancelled"].contains(&outcome),
            "closed replication round outcomes: {span}"
        );
        if outcome == "failed" {
            assert_eq!(span["status"]["code"].as_u64(), Some(2), "{span}");
        } else {
            assert_ne!(span["status"]["code"].as_u64(), Some(2), "{span}");
        }
    }
    for name in ["st.replication.rounds", "st.replication.round.duration"] {
        for point in metrics
            .iter()
            .flat_map(|request| service_metric_points(request, REPLICATION_SERVICE, name))
        {
            let attributes = point["attributes"].as_array().expect("round metric labels");
            let labels: std::collections::BTreeSet<_> = attributes
                .iter()
                .map(|attribute| attribute["key"].as_str().expect("metric label key"))
                .collect();
            assert_eq!(
                labels,
                std::collections::BTreeSet::from(["peer", "outcome"]),
                "only closed peer/outcome labels on {name}: {point}"
            );
            assert_eq!(attributes.len(), 2, "no duplicate labels: {point}");
            assert!(
                string_attribute(point, "peer")
                    .is_some_and(|peer| REPLICATION_PEERS.contains(&peer)),
                "bounded invented peers: {point}"
            );
            assert!(
                string_attribute(point, "outcome")
                    .is_some_and(|outcome| ["moved", "in_sync", "failed", "cancelled"].contains(&outcome)),
                "closed metric outcomes: {point}"
            );
            assert!(
                replication_point_count(point, name).is_some_and(|count| count > 0),
                "a completed round contributes a point: {point}"
            );
        }
    }
    for metric in metrics
        .iter()
        .flat_map(|request| request["resourceMetrics"].as_array().into_iter().flatten())
        .filter(|batch| {
            string_attribute(&batch["resource"], "service.name") == Some(REPLICATION_SERVICE)
        })
        .flat_map(|batch| batch["scopeMetrics"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["metrics"].as_array().into_iter().flatten())
        .filter(|metric| metric["name"].as_str() == Some("st.replication.round.duration"))
    {
        assert_eq!(metric["unit"].as_str(), Some("s"), "duration unit: {metric}");
    }
}
