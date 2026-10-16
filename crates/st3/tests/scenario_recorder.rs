//! On-demand scratch-only recording. No endpoint or state-directory override is accepted.
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io::{Read, Write}, path::{Path, PathBuf}, process::{Child, Command, Stdio}, time::{Duration, Instant}};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    format_version: u32,
    world: String,
    agent: String,
    host: String,
    person: String,
    workspace: String,
    id_map: BTreeMap<String, String>,
    turns: Vec<Turn>,
    reads: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn { prompt: String, answer: String, timestamp: String, command: Option<String> }

fn decode_script(bytes: &[u8]) -> Result<Script> {
    let schema: Value = serde_json::from_str(include_str!("../../../clients/typescript/st3-scenarios/scripts/recording-script.schema.json"))?;
    let value: Value = serde_json::from_slice(bytes)?;
    let validator = jsonschema::options().should_validate_formats(true).build(&schema)?;
    validator.validate(&value).map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(serde_json::from_value(value)?)
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn terminal_ready(frame: &Value, fence: &Value, turn: usize) -> bool {
    let marker = format!("scenario turn {turn} output complete");
    frame["id"] == "terminal" && frame["value"]["revision"] != *fence
        && frame["value"]["lines"].as_array().is_some_and(|lines|
            lines.iter().any(|line| line["text"].as_str().is_some_and(|text| text.contains(&marker))))
}

/// All destinations are beneath the freshly owned root; existing state or socket is foreign.
fn owned_destinations(root: &Path, state: &Path, endpoint: &Path) -> Result<()> {
    ensure!(state == root.join("state") && endpoint == root.join("run/st.sock"), "foreign destination");
    ensure!(!state.exists() && !endpoint.exists(), "pre-existing destination");
    Ok(())
}

struct Scratch { root: tempfile::TempDir, env: BTreeMap<String, String>, daemon: Option<Child> }
impl Scratch {
    fn new(script: &[u8], host: &str, person: &str) -> Result<Self> {
        let root = tempfile::Builder::new().prefix("st3-scenario-").tempdir()?;
        owned_destinations(root.path(), &root.path().join("state"), &root.path().join("run/st.sock"))?;
        for name in ["home", "run", "ws", "bin"] { fs::create_dir(root.path().join(name))?; }
        fs::write(root.path().join("script.json"), script)?;
        fs::write(root.path().join("bin/claude.py"), include_str!("scenario_recorder/claude.py"))?;
        let python = executable("python3")?;
        let wrapper = root.path().join("bin/claude");
        fs::write(&wrapper, format!("#!/bin/sh\nexec {} {} \"$@\"\n", shell_literal(&python.display().to_string()), shell_literal(&root.path().join("bin/claude.py").display().to_string())))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(wrapper, fs::Permissions::from_mode(0o755))?;
        let path = format!("{}:{}", root.path().join("bin").display(), std::env::var("PATH")?);
        let env = BTreeMap::from([
            ("PATH".into(), path.clone()), ("HOME".into(), root.path().join("home").display().to_string()),
            ("USER".into(), "example".into()), ("LOGNAME".into(), "example".into()),
            ("SHELL".into(), "/bin/bash".into()), ("LANG".into(), "C.UTF-8".into()),
            ("XDG_STATE_HOME".into(), root.path().join("home/.local/state").display().to_string()),
            ("XDG_CONFIG_HOME".into(), root.path().join("home/.config").display().to_string()),
            ("XDG_DATA_HOME".into(), root.path().join("home/.local/share").display().to_string()),
            ("XDG_RUNTIME_DIR".into(), root.path().join("run").display().to_string()),
            ("ST3_ENDPOINT".into(), root.path().join("run/st.sock").display().to_string()),
            ("ST3_DAEMON_WAIT".into(), "0".into()),
        ]);
        for name in [".profile", ".bash_profile", ".bashrc"] { fs::write(root.path().join("home").join(name), format!("export PATH={}\n", shell_literal(&path)))?; }
        fs::write(root.path().join("config.toml"), format!("node = {host:?}\nperson = {person:?}\nstate_dir = {:?}\npty_root = {:?}\nsocket = {:?}\nclient_gateway_socket = {:?}\n", root.path().join("state"), root.path().join("pty"), root.path().join("run/st.sock"), root.path().join("run/client.sock")))?;
        Ok(Self { root, env, daemon: None })
    }
    fn command(&self, binary: &Path) -> Command {
        let mut c = Command::new(binary);
        c.env_clear().envs(&self.env).current_dir(self.root.path().join("ws")); c
    }
    fn start(&mut self) -> Result<()> {
        let log = fs::File::create(self.root.path().join("daemon.log"))?;
        let mut cmd = self.command(Path::new(test_env!("CARGO_BIN_EXE_st3-fixture")));
        cmd.args(["up", "--config"]).arg(self.root.path().join("config.toml"));
        // Own the daemon process group, including non-PTY helper children.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        self.daemon = Some(cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?);
        Ok(())
    }
    fn cli(&self, args: &[&str]) -> Result<String> {
        let output = self.command(Path::new(test_env!("CARGO_BIN_EXE_st3-fixture"))).arg("--endpoint").arg(self.root.path().join("run/st.sock")).args(args).output()?;
        ensure!(output.status.success(), "CLI: {}", String::from_utf8_lossy(&output.stderr));
        Ok(String::from_utf8(output.stdout)?)
    }
    fn http(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let mut cmd = self.command(&executable("curl")?);
        cmd.args(["-q", "-sS", "--max-time", "10", "--unix-socket"]).arg(self.root.path().join("run/st.sock"))
            .args(["-X", method, "-w", "\n%{http_code}"]).arg(format!("http://localhost{path}"));
        if let Some(body) = body { cmd.args(["-H", "content-type: application/json", "--data"]).arg(body.to_string()); }
        let output = cmd.output()?;
        ensure!(output.status.success(), "HTTP transport unavailable");
        let text = String::from_utf8(output.stdout)?;
        let (body, status) = text.rsplit_once('\n').context("HTTP status missing")?;
        Ok((status.parse()?, serde_json::from_str(body)?))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only PTYs in our private registry can be stopped.
        if let Ok(pty) = executable("pty") {
            let output = self.command(&pty).env("PTY_ROOT", self.root.path().join("pty")).args(["list", "--json"]).output();
            if let Ok(output) = output && let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&output.stdout) {
                for row in rows { if let Some(name) = row["name"].as_str() { let _ = self.command(&pty).env("PTY_ROOT", self.root.path().join("pty")).args(["kill", name]).output(); } }
            }
        }
        if let Some(child) = &mut self.daemon {
            // PID belongs to the child we created; never accept a caller's process identifier.
            if child.try_wait().ok().flatten().is_none() {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM); }
                let deadline = Instant::now() + Duration::from_secs(10);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(20)); }
                if child.try_wait().ok().flatten().is_none() {
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
                    let _ = child.wait();
                }
            }
        }
    }
}
fn executable(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|p| p.join(name)).find(|p| p.is_file()).with_context(|| format!("{name} missing from PATH"))
}

struct Capture { file: fs::File, start: Instant, ordinal: u64, root: String, workspace: String, capabilities: BTreeMap<String, String> }
impl Capture {
    fn scrub(&mut self, value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "stream_capability" | "capability") && let Some(secret) = value.as_str() {
                        let n = self.capabilities.len() + 1;
                        let token = self.capabilities.entry(secret.to_owned()).or_insert_with(|| format!("capability#{n}"));
                        *value = json!(token);
                    } else { self.scrub(value); }
                }
            }
            Value::Array(array) => { for value in array { self.scrub(value); } }
            Value::String(text) => {
                *text = text.replace(&format!("{}/ws", self.root), &self.workspace).replace(&self.root, &self.workspace);
                let escaped: String = self.root.chars().map(|c| if c.is_ascii_alphanumeric() {c} else {'-'}).collect();
                *text = text.replace(&escaped, "example-scratch");
                for (secret, token) in &self.capabilities { *text = text.replace(secret, token); }
            }
            _ => {}
        }
    }
    fn record(&mut self, kind: &str, socket: Option<&str>, mut value: Value) -> Result<()> {
        self.scrub(&mut value);
        let row = json!({"ordinal":self.ordinal,"socket_id":socket,"receive_ms":self.start.elapsed().as_millis(),"kind":kind,"value":value});
        serde_json::to_writer(&mut self.file, &row)?; self.file.write_all(b"\n")?; self.file.flush()?; self.ordinal += 1; Ok(())
    }
    fn exchange(&mut self, scratch: &Scratch, method: &str, path: &str, request: Option<&Value>) -> Result<Value> {
        let (status, body) = scratch.http(method, path, request)?;
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        self.record("http", None, json!({"method":method,"path":route,"query":query,"status":status,"request_body":request,"body":body}))?;
        ensure!(status == 200, "HTTP {status}: {body}"); Ok(body)
    }
}
async fn barrier(mut predicate: impl FnMut() -> bool, label: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !predicate() { ensure!(Instant::now() < deadline, "readiness timeout: {label}"); tokio::time::sleep(Duration::from_millis(25)).await; }
    Ok(())
}
async fn frame_until(stream: &mut st3_client::CollectionStream, capture: &mut Capture, mut predicate: impl FnMut(&Value) -> bool) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next()).await??.context("socket closed")?;
        capture.record("frame", Some("socket#1"), frame.clone())?;
        ensure!(frame["kind"] != "error", "subscription error: {frame}");
        if predicate(&frame) { return Ok(frame); }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "owns scratch daemon and real PTY; set ST3_SCENARIO_SCRIPT and ST3_SCENARIO_OUTPUT"]
async fn record_scenario() -> Result<()> {
    ensure!(std::env::var_os("ST3_SCENARIO_ENDPOINT").is_none() && std::env::var_os("ST3_SCENARIO_STATE_DIR").is_none(), "foreign daemon/state override refused");
    let bytes = fs::read(std::env::var("ST3_SCENARIO_SCRIPT")?)?;
    let script = decode_script(&bytes)?;
    ensure!(script.format_version == 1 && !script.turns.is_empty(), "unsupported or empty script");
    ensure!(script.agent.starts_with("agent/example/") && script.host.starts_with("host/") && script.person.starts_with("person/") && script.workspace.starts_with("~/src/"), "script must use public cast identities");
    let mut scratch = Scratch::new(&bytes, script.host.trim_start_matches("host/"), &script.person)?;
    let output = fs::OpenOptions::new().write(true).create_new(true).open(std::env::var("ST3_SCENARIO_OUTPUT")?)?;
    let started_at = chrono::Utc::now().to_rfc3339();
    let mut capture = Capture { file:output, start:Instant::now(), ordinal:0, root:scratch.root.path().display().to_string(), workspace:script.workspace.clone(), capabilities:BTreeMap::new() };
    let version = scratch.command(Path::new(test_env!("CARGO_BIN_EXE_st3-fixture"))).arg("--version").output()?;
    ensure!(version.status.success(), "daemon version unavailable");
    use sha2::Digest;
    let mut binary = fs::File::open(test_env!("CARGO_BIN_EXE_st3-fixture"))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = binary.read(&mut buffer)?;
        if count == 0 { break; }
        digest.update(&buffer[..count]);
    }
    let daemon_digest = hex::encode(digest.finalize());
    capture.record("header", None, json!({"format_version":1,"script_format_version":1,"world":script.world,"daemon_build":String::from_utf8(version.stdout)?.trim(),"daemon_sha256":daemon_digest,"started_at":started_at}))?;
    scratch.start()?;
    barrier(|| scratch.http("GET", "/v1/health", None).is_ok(), "daemon health").await?;
    let seat = scratch.root.path().join("seats.kdl");
    fs::write(&seat, format!("version 2\nagent \"builder\" {{ identity {:?}; workspace {:?}; restart \"never\"; harness \"claude\" {{}}; }}\n", script.agent.trim_start_matches("agent/"), scratch.root.path().join("ws")))?;
    scratch.cli(&["apply", seat.to_str().context("seat path")?, "--as", &script.person, "--no-gate-check"])?;
    barrier(|| scratch.root.path().join("ready").exists(), "native harness ready").await?;
    let terminal = format!("terminal/{}", script.agent);
    let route = format!("/v1/client/terminals/{}/screen", urlencoding::encode(&script.agent));
    barrier(|| scratch.http("GET", &route, None).is_ok_and(|(status, body)| status == 200 && !body["value"]["runtime_incarnation"].is_null()), "projected terminal").await?;
    let screen = capture.exchange(&scratch, "GET", &route, None)?;
    let action = json!({"api_version":"st3.client.v0","id":"action/example-attach","type":"terminal.attach","idempotency_key":"scenario-example-attach","fence":{"snapshot_id":screen["snapshot"]["id"],"subject_revisions":{},"runtime_incarnation":screen["value"]["runtime_incarnation"]},"parameters":{"target_id":terminal}});
    capture.record("attach", None, action.clone())?;
    let attached = capture.exchange(&scratch, "POST", "/v1/client/actions", Some(&action))?;
    let attachment = &attached["value"]["terminal_attachment"];
    let capability = attachment["stream_capability"].as_str().context("attach capability")?;
    let incarnation = attachment["runtime_incarnation"].as_str().context("attach incarnation")?;
    let client = st3_client::Client::unix(scratch.root.path().join("run/st.sock"));
    let mut stream = client.collection_stream().await?;
    capture.record("command", Some("socket#1"), json!({"kind":"subscribe","id":"conversation","collection":"conversation","conversation":script.agent}))?;
    stream.subscribe_conversation("conversation", &script.agent).await?;
    frame_until(&mut stream, &mut capture, |f| f["id"] == "conversation").await?;
    capture.record("command", Some("socket#1"), json!({"kind":"subscribe","id":"terminal","collection":"terminal","terminal":terminal,"incarnation":incarnation,"capability":capability}))?;
    stream.subscribe_terminal("terminal", &terminal, Some(incarnation), capability).await?;
    let initial_terminal = frame_until(&mut stream, &mut capture, |f| f["id"] == "terminal").await?;
    let mut terminal_revision = initial_terminal["value"]["revision"].clone();
    capture.record("baseline", None, json!({"last_pre_timeline_ordinal":capture.ordinal - 1}))?;
    for (index, turn) in script.turns.iter().enumerate() {
        let fence = terminal_revision.clone();
        capture.record("harness", None, json!({"turn":index,"prompt":turn.prompt,"answer":turn.answer,"timestamp":turn.timestamp,"command":turn.command}))?;
        fs::write(scratch.root.path().join(format!("step-{index}")), "")?;
        barrier(|| scratch.root.path().join(format!("done-{index}")).exists(), "native turn complete").await?;
        let output = if turn.command.is_some() {
            let text = String::from_utf8(fs::read(scratch.root.path().join(format!("output-{index}")))?)?;
            capture.record("pty_output", None, json!({"turn":index,"terminal":terminal,"text":text}))?;
            Some(text)
        } else { None };
        let mut conversation_seen = false;
        let mut terminal_seen = output.is_none();
        frame_until(&mut stream, &mut capture, |f| {
            conversation_seen |= f["id"] == "conversation" && f.to_string().contains(&turn.answer);
            terminal_seen |= terminal_ready(f, &fence, index);
            if f["id"] == "terminal" && !f["value"]["revision"].is_null() { terminal_revision = f["value"]["revision"].clone(); }
            conversation_seen && terminal_seen
        }).await?;
    }
    for route in &script.reads { capture.exchange(&scratch, "GET", route, None)?; }
    Ok(())
}

#[test]
fn refuses_foreign_or_preexisting_destinations() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state"); let endpoint = root.path().join("run/st.sock");
    assert!(owned_destinations(root.path(), &state, &endpoint).is_ok());
    fs::create_dir(&state).unwrap();
    assert!(owned_destinations(root.path(), &state, &endpoint).is_err());
    assert!(owned_destinations(root.path(), Path::new("/foreign/state"), &endpoint).is_err());
    fs::remove_dir(&state).unwrap(); fs::create_dir(root.path().join("run")).unwrap(); fs::write(&endpoint, "foreign").unwrap();
    assert!(owned_destinations(root.path(), &state, &endpoint).is_err());
}
fn test_capture(root: &Path) -> Capture {
    Capture { file:fs::File::create(root.join("output")).unwrap(), start:Instant::now(), ordinal:0, root:root.display().to_string(), workspace:"~/src/atlas".into(), capabilities:BTreeMap::new() }
}
#[test]
fn capabilities_never_reach_output() {
    let root = tempfile::tempdir().unwrap(); let mut capture = test_capture(root.path());
    capture.record("http", None, json!({"value":{"terminal_attachment":{"stream_capability":"live-scratch-secret"}}})).unwrap();
    capture.record("command", Some("socket#1"), json!({"capability":"live-scratch-secret"})).unwrap();
    let text = fs::read_to_string(root.path().join("output")).unwrap();
    assert!(!text.contains("live-scratch-secret")); assert_eq!(text.matches("capability#1").count(), 2);
}
#[test]
fn temporary_paths_never_reach_output() {
    let root = tempfile::tempdir().unwrap(); let mut capture = test_capture(root.path());
    capture.record("frame", Some("socket#1"), json!({"cwd":root.path().join("ws"),"nested":[root.path().join("home/transcript.jsonl")]})).unwrap();
    let text = fs::read_to_string(root.path().join("output")).unwrap();
    assert!(!text.contains(&root.path().display().to_string())); assert!(text.contains("~/src/atlas"));
}

#[test]
fn committed_script_matches_shared_schema() {
    let bytes = include_bytes!("../../../clients/typescript/st3-scenarios/recordings/fleet-mid-refactor-proof.script.json");
    let script = decode_script(bytes).unwrap();
    assert!(script.id_map.contains_key(&script.agent));
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    value["unexpected"] = json!(true);
    assert!(decode_script(&serde_json::to_vec(&value).unwrap()).is_err());
    value.as_object_mut().unwrap().remove("unexpected");
    value.as_object_mut().unwrap().remove("id_map");
    assert!(decode_script(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn each_command_requires_its_own_projected_output() {
    let fence = json!("screen-1");
    let screen = |revision, marker| json!({"id":"terminal","value":{"revision":revision,"lines":[{"text":marker}]}});
    assert!(!terminal_ready(&screen("screen-2", "scenario turn 0 output complete"), &fence, 1));
    assert!(!terminal_ready(&screen("screen-1", "scenario turn 1 output complete"), &fence, 1));
    assert!(!terminal_ready(&screen("screen-3", "intermediate output"), &fence, 1));
    assert!(terminal_ready(&screen("screen-4", "scenario turn 1 output complete"), &fence, 1));
}

#[test]
fn generated_shell_literals_preserve_apostrophes_without_execution() {
    let root = tempfile::Builder::new().prefix("recorder-'quoted-").tempdir().unwrap();
    let planted = root.path().join("must-not-exist");
    let value = format!("{}:'; touch {}; #", root.path().display(), planted.display());
    let output = Command::new("/bin/sh").args(["-c", &format!("value={}; printf '%s' \"$value\"", shell_literal(&value))]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
    assert!(!planted.exists());
}

#[test]
fn scratch_commands_have_only_owned_environment_and_curl_ignores_config() {
    let scratch = Scratch::new(b"{}", "meadow", "person/ada").unwrap();
    let output = scratch.command(&executable("env").unwrap()).output().unwrap();
    let actual: BTreeMap<String, String> = String::from_utf8(output.stdout).unwrap().lines()
        .map(|line| { let (key, value) = line.split_once('=').unwrap(); (key.into(), value.into()) }).collect();
    assert_eq!(actual, scratch.env);
    let planted = scratch.root.path().join("curlrc-loaded");
    fs::write(scratch.root.path().join("home/.curlrc"), format!("output = {:?}\nurl = \"file:///etc/hostname\"\n", planted)).unwrap();
    assert!(scratch.http("GET", "/v1/health", None).is_err());
    assert!(!planted.exists());
}
