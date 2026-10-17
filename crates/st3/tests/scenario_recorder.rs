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
    #[serde(default)]
    agents: Vec<RecordedAgent>,
    #[serde(default)]
    historical_turns: usize,
    #[serde(default)]
    empty_baseline: bool,
    #[serde(default)]
    reconnect_after: Vec<usize>,
    anchor: Option<String>,
    #[serde(default)]
    walk_pages: bool,
}

fn shift_native_turn_times(script: &mut Script, captured_at: chrono::DateTime<chrono::Utc>) -> Result<bool> {
    let Some(anchor) = script.anchor.as_deref().filter(|_| script.format_version == 2) else { return Ok(false); };
    let anchor = chrono::DateTime::parse_from_rfc3339(anchor).context("invalid script anchor")?;
    let shift = captured_at.signed_duration_since(anchor);
    for turn in script.turns.iter_mut().take(script.historical_turns) {
        let authored = chrono::DateTime::parse_from_rfc3339(&turn.timestamp).context("invalid native turn timestamp")?;
        turn.timestamp = authored.checked_add_signed(shift).context("native turn timestamp overflow")?
            .with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    }
    Ok(true)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    prompt: String, answer: String, timestamp: String, command: Option<String>,
    agent: Option<String>, tool: Option<Tool>,
    #[serde(default)]
    revisions: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedAgent { agent: String, host: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool { id: String, name: String, input: Value, result: Value }

/// Native items keep their actual revision/final fields; only content identity is fenced here.
fn native_content_id<'a>(value: &'a Value, text: &str, after: u64) -> Option<&'a str> {
    match value {
        Value::Object(object) => {
            if value["type"] == "content" && value["role"] == "assistant" && value["body"]["text"] == text
                && value["sequence"].as_u64().is_some_and(|sequence| sequence > after) {
                return value["id"].as_str();
            }
            object.values().find_map(|child| native_content_id(child, text, after))
        }
        Value::Array(items) => items.iter().find_map(|child| native_content_id(child, text, after)),
        _ => None,
    }
}

impl Script {
    fn cast(&self) -> Result<Vec<&str>> {
        ensure!(matches!(self.format_version, 1 | 2), "unsupported script version");
        ensure!(self.agent.starts_with("agent/example/") && self.host.starts_with("host/") && self.person.starts_with("person/") && self.workspace.starts_with("~/src/"), "script must use public cast identities");
        ensure!(!self.turns.is_empty() && self.historical_turns <= self.turns.len(), "invalid historical turn count");
        ensure!(!self.empty_baseline || self.historical_turns == 0, "empty baseline cannot contain historical turns");
        let mut cast = vec![self.agent.as_str()];
        for seat in &self.agents {
            ensure!(seat.host == self.host, "additional recorded agent {} belongs to another host", seat.agent);
            ensure!(seat.agent.starts_with("agent/example/"), "additional agent must use public cast identity");
            if seat.agent == self.agent { continue; }
            ensure!(!cast.contains(&seat.agent.as_str()), "duplicate recorded agent");
            cast.push(&seat.agent);
        }
        for turn in &self.turns {
            let agent = turn.agent.as_deref().unwrap_or(&self.agent);
            ensure!(cast.contains(&agent), "turn names an unrecorded agent");
            ensure!(turn.command.is_none() || agent == self.agent, "only primary agent may execute terminal commands");
        }
        ensure!(self.reconnect_after.iter().all(|index| *index < self.turns.len()), "reconnect turn out of range");
        Ok(cast)
    }
}

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

struct Capture { file: fs::File, start: Instant, ordinal: u64, root: String, workspace: String, capabilities: BTreeMap<String, String>, native_sequences: BTreeMap<String, u64> }
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
        let receive_ms = self.start.elapsed().as_millis();
        self.scrub(&mut value);
        if kind == "frame" && value["kind"] == "conversation"
            && let (Some(id), Some(items)) = (value["id"].as_str(), value["items"].as_array())
            && let Some(sequence) = items.iter().filter(|item| item["type"] == "content" && item["role"] == "assistant")
                .filter_map(|item| item["sequence"].as_u64()).max() {
            let previous = self.native_sequences.entry(id.to_owned()).or_default();
            *previous = (*previous).max(sequence);
        }
        let row = json!({"ordinal":self.ordinal,"socket_id":socket,"receive_ms":receive_ms,"kind":kind,"value":value});
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
async fn frame_until(stream: &mut st3_client::CollectionStream, capture: &mut Capture, socket: &str, mut predicate: impl FnMut(&Value) -> bool) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next()).await??.context("socket closed")?;
        capture.record("frame", Some(socket), frame.clone())?;
        ensure!(frame["kind"] != "error", "subscription error: {frame}");
        if frame["id"] == "terminal" && frame["kind"] == "screen" {
            ensure!(bounded_viewport(&frame["value"]), "terminal viewport exceeds 80x24 capture bounds");
        }
        if predicate(&frame) { return Ok(frame); }
    }
}

fn bounded_viewport(screen: &Value) -> bool {
    screen["rows"].as_u64().is_some_and(|rows| (1..=24).contains(&rows))
        && screen["columns"].as_u64().is_some_and(|columns| (1..=80).contains(&columns))
}

fn harness_record(script: &Script, index: usize, turn: &Turn) -> Value {
    let mut record = json!({"turn":index,"prompt":turn.prompt,"answer":turn.answer,"timestamp":turn.timestamp,"command":turn.command});
    if script.format_version == 2 {
        record["agent"] = json!(turn.agent.as_deref().unwrap_or(&script.agent));
        record["tool"] = turn.tool.as_ref().map_or(Value::Null, |tool| json!({"id":tool.id,"name":tool.name,"input":tool.input,"result":tool.result}));
        record["revisions"] = json!(turn.revisions);
    }
    record
}

fn conversation_id(index: usize) -> String {
    if index == 0 { "conversation".into() } else { format!("conversation#{}", index + 1) }
}

async fn subscribe_catalogs(stream: &mut st3_client::CollectionStream, capture: &mut Capture, socket: &str) -> Result<()> {
    for collection in ["agents", "terminals"] {
        capture.record("command", Some(socket), json!({"kind":"subscribe","id":collection,"collection":collection,"limit":100,"actor":null,"status":null}))?;
        stream.subscribe(collection, collection, 100, None, None).await?;
        frame_until(stream, capture, socket, |frame| frame["id"] == collection).await?;
    }
    Ok(())
}

async fn subscribe_cast(stream: &mut st3_client::CollectionStream, capture: &mut Capture, socket: &str, cast: &[&str]) -> Result<()> {
    for (index, agent) in cast.iter().enumerate() {
        let id = conversation_id(index);
        capture.record("command", Some(socket), json!({"kind":"subscribe","id":id,"collection":"conversation","conversation":agent}))?;
        stream.subscribe_conversation(&id, agent).await?;
        frame_until(stream, capture, socket, |frame| frame["id"] == id).await?;
    }
    Ok(())
}

async fn subscribe_terminal(stream: &mut st3_client::CollectionStream, capture: &mut Capture, socket: &str, terminal: &str, incarnation: &str, capability: &str) -> Result<Value> {
    capture.record("command", Some(socket), json!({"kind":"subscribe","id":"terminal","collection":"terminal","terminal":terminal,"incarnation":incarnation,"capability":capability}))?;
    stream.subscribe_terminal("terminal", terminal, Some(incarnation), capability).await?;
    let frame = frame_until(stream, capture, socket, |frame| frame["id"] == "terminal").await?;
    Ok(frame["value"]["revision"].clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "owns scratch daemon and real PTY; set ST3_SCENARIO_SCRIPT and ST3_SCENARIO_OUTPUT"]
async fn record_scenario() -> Result<()> {
    ensure!(std::env::var_os("ST3_SCENARIO_ENDPOINT").is_none() && std::env::var_os("ST3_SCENARIO_STATE_DIR").is_none(), "foreign daemon/state override refused");
    let captured_at = chrono::Utc::now();
    let mut bytes = fs::read(std::env::var("ST3_SCENARIO_SCRIPT")?)?;
    let mut script = decode_script(&bytes)?;
    if shift_native_turn_times(&mut script, captured_at)? {
        // Only the owned harness copy is shifted; authored source and receipt offsets are untouched.
        let mut native: Value = serde_json::from_slice(&bytes)?;
        let turns = native["turns"].as_array_mut().context("native turns missing")?;
        ensure!(turns.len() == script.turns.len(), "native turn count changed");
        for (value, turn) in turns.iter_mut().zip(&script.turns) {
            value["timestamp"] = json!(turn.timestamp);
        }
        bytes = serde_json::to_vec(&native)?;
    }
    let cast = script.cast()?;
    let mut scratch = Scratch::new(&bytes, script.host.trim_start_matches("host/"), &script.person)?;
    let output = fs::OpenOptions::new().write(true).create_new(true).open(std::env::var("ST3_SCENARIO_OUTPUT")?)?;
    let started_at = captured_at.to_rfc3339();
    let mut capture = Capture { file:output, start:Instant::now(), ordinal:0, root:scratch.root.path().display().to_string(), workspace:script.workspace.clone(), capabilities:BTreeMap::new(), native_sequences:BTreeMap::new() };
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
    capture.record("header", None, json!({"format_version":1,"script_format_version":script.format_version,"world":script.world,"daemon_build":String::from_utf8(version.stdout)?.trim(),"daemon_sha256":daemon_digest,"started_at":started_at}))?;
    scratch.start()?;
    barrier(|| scratch.http("GET", "/v1/health", None).is_ok(), "daemon health").await?;
    let client = st3_client::Client::unix(scratch.root.path().join("run/st.sock"));
    let mut socket_number = 1;
    let mut socket = format!("socket#{socket_number}");
    let mut stream = client.collection_stream().await?;
    if script.empty_baseline {
        subscribe_catalogs(&mut stream, &mut capture, &socket).await?;
        // No conversation exists yet: the real empty collection snapshots are the baseline.
        capture.record("baseline", None, json!({"last_pre_timeline_ordinal":capture.ordinal.checked_sub(1).context("missing baseline frames")?}))?;
        for collection in ["agents", "terminals"] {
            capture.record("command", Some(&socket), json!({"kind":"unsubscribe","id":collection}))?;
            stream.unsubscribe(collection).await?;
        }
    }
    let seat = scratch.root.path().join("seats.kdl");
    let mut seats = String::from("version 2\n");
    for (index, agent) in cast.iter().enumerate() {
        seats.push_str(&format!("agent \"scenario-{index}\" {{ identity {:?}; workspace {:?}; restart \"never\"; harness \"claude\" {{}}; }}\n", agent.trim_start_matches("agent/"), scratch.root.path().join("ws")));
    }
    fs::write(&seat, seats)?;
    scratch.cli(&["apply", seat.to_str().context("seat path")?, "--as", &script.person, "--no-gate-check"])?;
    for index in 0..cast.len() {
        barrier(|| scratch.root.path().join(format!("ready-{index}")).exists(), "native harness ready per agent").await?;
    }
    let terminal = format!("terminal/{}", script.agent);
    let route = format!("/v1/client/terminals/{}/screen", urlencoding::encode(&script.agent));
    barrier(|| scratch.http("GET", &route, None).is_ok_and(|(status, body)| status == 200 && !body["value"]["runtime_incarnation"].is_null()), "projected terminal").await?;
    let screen = capture.exchange(&scratch, "GET", &route, None)?;
    ensure!(bounded_viewport(&screen["value"]), "terminal viewport exceeds 80x24 capture bounds");
    let action = json!({"api_version":"st3.client.v0","id":"action/example-attach","type":"terminal.attach","idempotency_key":"scenario-example-attach","fence":{"snapshot_id":screen["snapshot"]["id"],"subject_revisions":{},"runtime_incarnation":screen["value"]["runtime_incarnation"]},"parameters":{"target_id":terminal}});
    capture.record("attach", None, action.clone())?;
    let attached = capture.exchange(&scratch, "POST", "/v1/client/actions", Some(&action))?;
    let attachment = &attached["value"]["terminal_attachment"];
    let capability = attachment["stream_capability"].as_str().context("attach capability")?;
    let incarnation = attachment["runtime_incarnation"].as_str().context("attach incarnation")?;
    subscribe_cast(&mut stream, &mut capture, &socket, &cast).await?;
    let mut terminal_revision = subscribe_terminal(&mut stream, &mut capture, &socket, &terminal, incarnation, capability).await?;
    if !script.empty_baseline && script.historical_turns == 0 {
        capture.record("baseline", None, json!({"last_pre_timeline_ordinal":capture.ordinal.checked_sub(1).context("missing baseline frames")?}))?;
    }
    for (index, turn) in script.turns.iter().enumerate() {
        let fence = terminal_revision.clone();
        let agent = turn.agent.as_deref().unwrap_or(&script.agent);
        let cast_index = cast.iter().position(|seat| *seat == agent).context("turn agent absent")?;
        let subscription = conversation_id(cast_index);
        let native_fence = capture.native_sequences.get(&subscription).copied().unwrap_or_default();
        let mut harness = harness_record(&script, index, turn);
        if script.format_version == 2 {
            let timestamp = if index < script.historical_turns { turn.timestamp.clone() }
                else { chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true) };
            fs::write(scratch.root.path().join(format!("timestamp-{index}")), &timestamp)?;
            harness["timestamp"] = json!(timestamp);
        }
        capture.record("harness", None, harness)?;
        fs::write(scratch.root.path().join(format!("step-{index}")), "")?;
        let mut terminal_seen = turn.command.is_none();
        let mut content_id: Option<String> = None;
        for (revision, text) in turn.revisions.iter().enumerate() {
            barrier(|| scratch.root.path().join(format!("revision-{index}-{revision}")).exists(), "native content revision written").await?;
            let frame = frame_until(&mut stream, &mut capture, &socket, |frame| {
                terminal_seen |= terminal_ready(frame, &fence, index);
                if frame["id"] == "terminal" && !frame["value"]["revision"].is_null() { terminal_revision = frame["value"]["revision"].clone(); }
                frame["id"] == subscription && native_content_id(frame, text, native_fence).is_some()
            }).await?;
            let id = native_content_id(&frame, text, native_fence).context("native content identity missing")?;
            ensure!(content_id.as_deref().is_none_or(|previous| previous == id), "native content revision changed identity");
            content_id = Some(id.to_owned());
            fs::write(scratch.root.path().join(format!("ack-{index}-{revision}")), "")?;
        }
        barrier(|| scratch.root.path().join(format!("done-{index}")).exists(), "native turn complete").await?;
        if turn.command.is_some() {
            let text = String::from_utf8(fs::read(scratch.root.path().join(format!("output-{index}")))?)?;
            capture.record("pty_output", None, json!({"turn":index,"terminal":terminal,"text":text}))?;
        }
        let mut conversation_seen = false;
        frame_until(&mut stream, &mut capture, &socket, |f| {
            conversation_seen |= f["id"] == subscription && native_content_id(f, &turn.answer, native_fence)
                .is_some_and(|id| content_id.as_deref().is_none_or(|previous| previous == id));
            terminal_seen |= terminal_ready(f, &fence, index);
            if f["id"] == "terminal" && !f["value"]["revision"].is_null() { terminal_revision = f["value"]["revision"].clone(); }
            conversation_seen && terminal_seen
        }).await?;
        let reaches_baseline = script.historical_turns > 0 && index + 1 == script.historical_turns;
        if script.reconnect_after.contains(&index) || reaches_baseline {
            capture.record("command", Some(&socket), json!({"kind":"disconnect"}))?;
            drop(stream);
            socket_number += 1;
            socket = format!("socket#{socket_number}");
            stream = client.collection_stream().await?;
            subscribe_cast(&mut stream, &mut capture, &socket, &cast).await?;
            terminal_revision = subscribe_terminal(&mut stream, &mut capture, &socket, &terminal, incarnation, capability).await?;
        }
        if reaches_baseline {
            capture.record("baseline", None, json!({"last_pre_timeline_ordinal":capture.ordinal.checked_sub(1).context("missing baseline frames")?}))?;
        }
    }
    for route in &script.reads {
        let mut page = capture.exchange(&scratch, "GET", route, None)?;
        let mut cursors = std::collections::BTreeSet::new();
        while script.walk_pages && page["value"]["kind"] == "timeline-page" && page["value"]["page"]["has_more"] == true {
            let cursor = page["value"]["page"]["next_cursor"].as_str().context("older page cursor missing")?;
            ensure!(cursors.insert(cursor.to_owned()), "older page cursor cycle");
            let separator = if route.contains('?') { '&' } else { '?' };
            let next = format!("{route}{separator}cursor={}", urlencoding::encode(cursor));
            page = capture.exchange(&scratch, "GET", &next, None)?;
        }
    }
    capture.record("command", Some(&socket), json!({"kind":"disconnect"}))?;
    drop(stream);
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
    Capture { file:fs::File::create(root.join("output")).unwrap(), start:Instant::now(), ordinal:0, root:root.display().to_string(), workspace:"~/src/atlas".into(), capabilities:BTreeMap::new(), native_sequences:BTreeMap::new() }
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

#[test]
fn additive_scripts_preserve_primary_defaults_and_validate_cast() {
    let bytes = include_bytes!("../../../clients/typescript/st3-scenarios/recordings/fleet-mid-refactor-proof.script.json");
    let mut script: Script = serde_json::from_slice(bytes).unwrap();
    assert_eq!(script.cast().unwrap(), vec![script.agent.as_str()]);
    assert_eq!(script.historical_turns, 0);
    assert!(!script.empty_baseline);
    assert!(script.reconnect_after.is_empty());
    script.format_version = 2;
    script.agents.push(RecordedAgent { agent: "agent/example/reviewer".into(), host: script.host.clone() });
    script.turns[0].agent = Some("agent/example/reviewer".into());
    script.turns[0].command = None;
    script.turns[0].tool = Some(Tool { id: "tool/example".into(), name: "Read".into(), input: json!({"path":"src/main.rs"}), result: json!(["native", "result"]) });
    script.historical_turns = script.turns.len();
    script.reconnect_after = vec![0];
    assert_eq!(script.cast().unwrap().len(), 2);
    script.turns[0].command = Some("printf forbidden".into());
    assert!(script.cast().is_err());
    script.turns[0].command = None;
    script.agents[0].host = "host/other".into();
    assert!(script.cast().is_err());
}

#[test]
fn script_boundaries_reject_unrecorded_agents_and_invalid_indices() {
    let bytes = include_bytes!("../../../clients/typescript/st3-scenarios/recordings/fleet-mid-refactor-proof.script.json");
    let mut script: Script = serde_json::from_slice(bytes).unwrap();
    script.historical_turns = script.turns.len() + 1;
    assert!(script.cast().is_err());
    script.historical_turns = 0;
    script.reconnect_after = vec![script.turns.len()];
    assert!(script.cast().is_err());
    script.reconnect_after.clear();
    script.turns[0].agent = Some("agent/example/not-recorded".into());
    assert!(script.cast().is_err());
    script.turns[0].agent = None;
    script.empty_baseline = true;
    assert!(script.cast().is_ok());
    script.historical_turns = 1;
    assert!(script.cast().is_err());
    assert_eq!(conversation_id(0), "conversation");
    assert_eq!(conversation_id(1), "conversation#2");
}

#[test]
fn additive_turns_decode_real_json_tool_payloads() {
    let turn: Turn = serde_json::from_value(json!({
        "prompt":"Inspect", "answer":"Inspected", "timestamp":"2026-10-13T00:00:00Z",
        "agent":"agent/example/reviewer",
        "tool":{"id":"tool/inspect","name":"Inspect","input":[1,null,true],"result":{"ok":true}}
    })).unwrap();
    let tool = turn.tool.unwrap();
    assert_eq!(tool.input, json!([1,null,true]));
    assert_eq!(tool.result, json!({"ok":true}));
    assert!(turn.command.is_none());
}

#[test]
fn native_content_fence_preserves_identity_without_inventing_revision_numbers() {
    let partial = json!({"id":"conversation","value":{"items":[{"id":"timeline-entry/native-5","sequence":5,"revision":1,"final":true,"type":"content","role":"assistant","body":{"text":"Working"}}]}});
    let mut completed = partial.clone();
    completed["value"]["items"][0]["body"]["text"] = json!("Complete");
    assert_eq!(native_content_id(&partial, "Working", 0), Some("timeline-entry/native-5"));
    assert_eq!(native_content_id(&completed, "Complete", 0), Some("timeline-entry/native-5"));
    assert!(native_content_id(&partial, "Complete", 0).is_none());
    assert!(native_content_id(&partial, "Working", 5).is_none());
    let repeated = json!({"items":[partial["value"]["items"][0], {"id":"timeline-entry/native-9","sequence":9,"type":"content","role":"assistant","body":{"text":"Working"}}]});
    assert_eq!(native_content_id(&repeated, "Working", 5), Some("timeline-entry/native-9"));
    assert_eq!(completed["value"]["items"][0]["revision"], 1);
    let turn: Turn = serde_json::from_value(json!({"prompt":"Work","answer":"Complete","timestamp":"2026-10-13T00:00:00Z","revisions":["Working"]})).unwrap();
    assert_eq!(turn.revisions, vec!["Working"]);
}

#[test]
fn primary_proof_harness_shape_and_viewport_bounds_are_preserved() {
    let bytes = include_bytes!("../../../clients/typescript/st3-scenarios/recordings/fleet-mid-refactor-proof.script.json");
    let mut script: Script = serde_json::from_slice(bytes).unwrap();
    let proof = harness_record(&script, 0, &script.turns[0]);
    assert_eq!(proof.as_object().unwrap().len(), 5);
    assert!(proof.get("agent").is_none());
    assert!(proof.get("tool").is_none());
    assert!(proof.get("revisions").is_none());
    script.format_version = 2;
    let narrative = harness_record(&script, 0, &script.turns[0]);
    assert_eq!(narrative["agent"], script.agent);
    assert_eq!(narrative["revisions"], json!([]));
    assert!(bounded_viewport(&json!({"rows":24,"columns":80})));
    assert!(!bounded_viewport(&json!({"rows":25,"columns":80})));
    assert!(!bounded_viewport(&json!({"rows":24,"columns":81})));
    assert!(!bounded_viewport(&json!({"rows":0,"columns":80})));
}

#[test]
fn native_anchor_shift_preserves_historical_span_and_leaves_proof_unchanged() {
    let bytes = include_bytes!("../../../clients/typescript/st3-scenarios/recordings/fleet-mid-refactor-proof.script.json");
    let mut script: Script = serde_json::from_slice(bytes).unwrap();
    let captured_at = chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
    let original = script.turns[0].timestamp.clone();
    assert!(!shift_native_turn_times(&mut script, captured_at).unwrap());
    assert_eq!(script.turns[0].timestamp, original);
    script.format_version = 2;
    script.historical_turns = 1;
    script.anchor = Some("2030-01-01T12:00:00Z".into());
    script.turns[0].timestamp = "2030-01-01T11:20:00Z".into();
    assert!(shift_native_turn_times(&mut script, captured_at).unwrap());
    assert_eq!(script.turns[0].timestamp, "2026-10-13T11:20:00.000Z");
    assert_eq!(script.turns[1].timestamp, serde_json::from_slice::<Script>(bytes).unwrap().turns[1].timestamp);
    script.anchor = Some("invalid".into());
    assert!(shift_native_turn_times(&mut script, captured_at).is_err());
}
