#![cfg(unix)]
//! Fleet join end to end: real `st3 up` and `st3 replication-worker` processes on one machine,
//! each node with its own home, XDG directories, state, sockets, and ports, driven with the
//! `st3` CLI. Nothing here touches the real fleet's state, services, ports, or Fabric names: every
//! node runs in the foreground under a temporary root, and no test installs a service.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::client::Client;
use st3::model::ClaimInput;

const ST3: &str = env!("CARGO_BIN_EXE_st3-fixture");
const PERSON: &str = "person/fleet-tester";
const NOTE: &str = "custom.fleet-test.note";

/// A port for a node's listener. It comes from below every ephemeral range (Linux hands out
/// 32768-60999 and macOS 49152-65535 to outgoing connections), so it is still free when a
/// stopped node starts again.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    const LOW: u16 = 20_000;
    const SPAN: u16 = 12_000;
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let mut seed = [0_u8; 2];
    getrandom::fill(&mut seed).unwrap();
    let _ = NEXT.compare_exchange(
        0,
        u16::from_le_bytes(seed) % SPAN + 1,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    loop {
        let port = LOW + NEXT.fetch_add(1, Ordering::AcqRel) % SPAN;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// One isolated node: its own directories, daemon, and replication worker.
struct Node {
    name: String,
    root: PathBuf,
    /// The st3 executable this node runs; an older release for compatibility tests.
    binary: PathBuf,
    port: u16,
    env: Vec<(String, String)>,
    daemon: Option<Child>,
    worker: Option<Child>,
    /// Every argument list this test passed to an st3 process on this node.
    arguments: std::sync::Mutex<Vec<Vec<String>>>,
}

impl Node {
    fn new(parent: &Path, name: &str) -> Self {
        Self::named(parent, name, name)
    }

    /// A node in directory `directory` that joins under `name`: another machine using a name.
    fn named(parent: &Path, directory: &str, name: &str) -> Self {
        let root = parent.join(directory);
        for directory in ["home", "config/st3", "state", "data", "run", "bin"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::write(
            root.join("config/st3/config.toml"),
            format!("node = \"{name}\"\nperson = \"{PERSON}\"\n"),
        )
        .unwrap();
        let pty = root.join("bin/pty");
        fs::write(&pty, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&pty, fs::Permissions::from_mode(0o755)).unwrap();
        // A release without --pty-binary finds pty through the login shell's PATH.
        let profile = format!("export PATH=\"{}:$PATH\"\n", root.join("bin").display());
        for file in [".profile", ".bash_profile", ".zprofile"] {
            fs::write(root.join("home").join(file), &profile).unwrap();
        }
        Self {
            name: name.into(),
            binary: PathBuf::from(ST3),
            root,
            port: free_port(),
            env: Vec::new(),
            daemon: None,
            worker: None,
            arguments: Default::default(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state/st3")
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/st3.sock")
    }

    fn client(&self) -> Client {
        Client::unix(self.socket())
    }

    fn command(&self, arguments: &[&str]) -> Command {
        self.arguments.lock().unwrap().push(
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        );
        let mut command = st3::test_support::command(&self.binary);
        command
            .args(arguments)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("ST3_WORKER_INTERVAL_MS", "300")
            .env("ST3_DAEMON_WAIT", "0");
        // The Nix check account has no passwd login shell. Preserve the shell supplied by
        // preCheck so the daemon can capture its login environment after env_clear().
        if let Some(shell) = std::env::var_os("SHELL") {
            command.env("SHELL", shell);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    fn st(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    fn st_ok(&self, arguments: &[&str]) -> String {
        let output = self.st(arguments);
        assert!(
            output.status.success(),
            "st3 {} on {} failed:\n{}{}\n{}",
            arguments.join(" "),
            self.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.logs()
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn st_json(&self, arguments: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(arguments);
        serde_json::from_str(&self.st_ok(&all)).unwrap()
    }

    fn logs(&self) -> String {
        [
            "daemon.log",
            "daemon.stderr.log",
            "worker.log",
            "worker.stderr.log",
        ]
        .iter()
        .map(|log| {
            let text = fs::read_to_string(self.root.join(log)).unwrap_or_default();
            let tail = text.lines().rev().take(20).collect::<Vec<_>>();
            format!(
                "--- {} {log}\n{}",
                self.name,
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
    }

    async fn start(&mut self) {
        let pty = self.root.join("bin/pty");
        let log = |name: &str| fs::File::create(self.root.join(name)).unwrap();
        let up: Vec<&str> = if self.binary == Path::new(ST3) {
            vec!["up", "--pty-binary", pty.to_str().unwrap()]
        } else {
            vec!["up"]
        };
        let daemon = self
            .command(&up)
            .stdin(Stdio::null())
            .stdout(log("daemon.log"))
            .stderr(log("daemon.stderr.log"))
            .spawn()
            .unwrap();
        self.daemon = Some(daemon);
        let client = self.client();
        // The pinned compatibility build may be cold while other CI lanes compile.
        // Give startup room for that load. Bound each health probe as well: a request
        // stalled behind startup must not consume the whole startup deadline.
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            let health_error = match tokio::time::timeout(
                Duration::from_secs(3),
                client.get::<Value>("/v1/health"),
            )
            .await
            {
                Ok(Ok(_)) => break,
                Ok(Err(error)) => error.to_string(),
                Err(_) => "health request timed out after 3 seconds".into(),
            };
            if let Some(status) = self.daemon.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "{} daemon exited with {status} (last health probe: {health_error}):\n{}",
                    self.name,
                    self.logs()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting until {} daemon starts (last health probe: {health_error}):\n{}",
                self.name,
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let legacy = fs::read_to_string(self.root.join("config/st3/config.toml"))
            .is_ok_and(|config| config.contains("fleet_id"));
        if legacy || self.state_dir().join("fleet/fleet.toml").exists() {
            self.start_worker();
        }
    }

    fn start_worker(&mut self) {
        assert!(self.worker.is_none());
        self.worker = Some(
            self.command(&["replication-worker"])
                .stdin(Stdio::null())
                .stdout(fs::File::create(self.root.join("worker.log")).unwrap())
                .stderr(fs::File::create(self.root.join("worker.stderr.log")).unwrap())
                .spawn()
                .unwrap(),
        );
    }

    fn stop_worker(&mut self) {
        if let Some(mut child) = self.worker.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn stop(&mut self) {
        for child in [self.worker.take(), self.daemon.take()]
            .into_iter()
            .flatten()
        {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(self.socket());
    }

    /// Send `signal` (such as `STOP` or `CONT`) to this node's daemon and worker.
    fn signal(&self, signal: &str) {
        for child in [&self.daemon, &self.worker].into_iter().flatten() {
            let status = Command::new("kill")
                .args([format!("-{signal}"), child.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success(), "kill -{signal} {}", child.id());
        }
    }

    async fn restart(&mut self) {
        self.stop();
        self.start().await;
    }

    /// Configure this node as a config-peer fleet node, the way the running fleet is today.
    fn legacy_config(&self, fleet_id: &str, secret: &Path, peers: &[(&str, u16)]) {
        let mut config = format!(
            "node = \"{}\"\nperson = \"{PERSON}\"\nfleet_id = \"{fleet_id}\"\nshared_secret_file = \"{}\"\npeer_listen = \"127.0.0.1:{}\"\n",
            self.name,
            secret.display(),
            self.port
        );
        for (name, port) in peers {
            config.push_str(&format!(
                "\n[[peers]]\nname = \"{name}\"\nurl = \"http://127.0.0.1:{port}\"\n"
            ));
        }
        fs::write(self.root.join("config/st3/config.toml"), config).unwrap();
    }

    fn migrate(&self, arguments: &[&str]) {
        let mut all = vec![
            "fleet",
            "migrate",
            "--no-service",
            "--transports",
            "loopback",
            "--advertise-loopback",
        ];
        all.extend(arguments);
        self.st_ok(&all);
    }

    /// Found a fleet on this node, listening on loopback only.
    fn create(&self) {
        let port = self.port.to_string();
        self.st_ok(&[
            "fleet",
            "create",
            "--no-service",
            "--name",
            &self.name,
            "--port",
            &port,
            "--transports",
            "loopback",
            "--advertise-loopback",
        ]);
    }

    /// Redeem `code` on this stopped node.
    fn join(&self, code: &str, extra: &[&str]) -> Output {
        let port = self.port.to_string();
        let mut arguments = vec![
            "fleet",
            "join",
            code,
            "--no-service",
            "--name",
            &self.name,
            "--port",
            &port,
            "--transports",
            "loopback",
            "--advertise-loopback",
        ];
        arguments.extend(extra);
        self.st(&arguments)
    }

    fn invite(&self, name: &str, extra: &[&str]) -> String {
        let mut arguments = vec![
            "fleet",
            "invite",
            name,
            "--code-only",
            "--via",
            "loopback",
            "--as",
            PERSON,
        ];
        arguments.extend(extra);
        self.st_ok(&arguments).trim().to_owned()
    }

    /// Wait until this member announces a loopback endpoint, so its invites can carry it, and its
    /// worker accepts connections. After a restart the announcement is the previous run's, so
    /// only the open port says the new worker listens.
    async fn wait_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let announced = match self
                .client()
                .get::<Value>("/v1/internal/fleet/status")
                .await
            {
                Ok(status) => status["view"]["members"].as_array().is_some_and(|members| {
                    members.iter().any(|member| {
                        member["name"] == self.name.as_str()
                            && member["endpoints"]
                                .as_array()
                                .is_some_and(|endpoints| !endpoints.is_empty())
                    })
                }),
                Err(_) => false,
            };
            let open = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
                .await
                .is_ok();
            if announced && open {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} did not announce and open its endpoint (announced {announced}, open {open})\n{}",
                self.name,
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn note(&self, text: &str) -> String {
        self.note_as(text, PERSON).await
    }

    async fn note_as(&self, text: &str, actor: &str) -> String {
        let claim: Value = self
            .client()
            .post(
                "/v1/claims",
                &ClaimInput {
                    subject: format!("custom/fleet-test/{text}"),
                    kind: NOTE.into(),
                    actor: Some(actor.into()),
                    fields: [("text".to_owned(), Value::String(text.into()))]
                        .into_iter()
                        .collect(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                },
            )
            .await
            .unwrap();
        claim["id"].as_str().unwrap().to_owned()
    }

    /// Every claim on this node, oldest first.
    async fn claims(&self) -> Vec<Value> {
        let mut claims = Vec::new();
        let mut after = 0_u64;
        loop {
            let page: Value = self
                .client()
                .get(&format!("/v1/claims?after_index={after}&limit=500"))
                .await
                .unwrap();
            let items = page["claims"].as_array().cloned().unwrap_or_default();
            claims.extend(items);
            match page["next_cursor"].as_u64() {
                Some(next) if next > after => after = next,
                _ => return claims,
            }
        }
    }

    async fn notes(&self) -> BTreeSet<String> {
        self.claims()
            .await
            .into_iter()
            .filter(|claim| claim["kind"] == NOTE)
            .filter_map(|claim| claim["subject"].as_str().map(str::to_owned))
            .collect()
    }

    fn secret(&self) -> Option<Vec<u8>> {
        fs::read_to_string(self.state_dir().join("fleet/secret"))
            .ok()
            .and_then(|text| hex::decode(text.trim()).ok())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn wait_until<F, Fut>(what: &str, seconds: u64, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting until {what}");
}

async fn wait_for_notes(node: &Node, expected: &BTreeSet<String>, seconds: u64, nodes: &[&Node]) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let notes = node.notes().await;
        if expected.is_subset(&notes) {
            return;
        }
        if Instant::now() > deadline {
            let logs = nodes
                .iter()
                .map(|node| node.logs())
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "{} has {} of {} notes; missing e.g. {:?}\n{logs}",
                node.name,
                expected.intersection(&notes).count(),
                expected.len(),
                expected.difference(&notes).next()
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// A founds the fleet and runs; the returned node is listening and announced.
async fn anchor(root: &Path, name: &str) -> Node {
    let mut node = Node::new(root, name);
    node.create();
    node.start().await;
    node.wait_listening().await;
    node
}

/// `name` joins through `sponsor` and runs.
async fn joined(root: &Path, sponsor: &Node, name: &str, extra: &[&str]) -> Node {
    let mut node = Node::new(root, name);
    let code = sponsor.invite(name, &[]);
    let output = node.join(&code, extra);
    assert!(
        output.status.success(),
        "{name} failed to join:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    node.start().await;
    node
}

/// Re-publication and parent revisions must preserve the weekly occurrence, even after its
/// child finishes. Put the next real weekly tick close enough to exercise it in this daemon.
#[tokio::test(flavor = "multi_thread")]
async fn scheduled_occurrence_survives_parent_reapply_revision_and_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut node = Node::new(root.path(), "orchard");
    node.start().await;
    let child_file = node.root.join("child.kdl");
    let mut revisions = Vec::new();
    for label in ["apple", "pear"] {
        let source = format!(
            r#"version 2
mission "harvest" state="ready" {{
  completion {{ when "all-steps-exhausted" }}
  goal "Harvest {label}."
  step "done" {{ agentless }}
}}"#
        );
        revisions.push(
            st3::parse_intent(&source, "orchard").unwrap().missions["harvest"]
                .revision
                .clone(),
        );
        fs::write(&child_file, source).unwrap();
        node.st_ok(&[
            "missions",
            "publish",
            child_file.to_str().unwrap(),
            "--as",
            PERSON,
        ]);
    }
    let next = chrono::Utc::now() + chrono::Duration::seconds(30);
    let anchor =
        (next - chrono::Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let parent_file = node.root.join("parent.kdl");
    let parent = |revision: &str| {
        format!(
            r#"version 2
mission "orchard/weekly" state="ready" {{
  goal "Maintain the orchard."
  schedule "cycle" {{
    host "orchard"
    every "7d"
    anchor "{anchor}"
    catch-up "latest"
    work {{ mission "harvest@{revision}"; workspace "{}" }}
  }}
  step "retire" {{
    agentless
    gate "retire" type="human" {{ reviewer "{PERSON}"; question "Retire the orchard?" }}
  }}
}}"#,
            node.root.join("cycles").display()
        )
    };
    fs::write(&parent_file, parent(&revisions[0])).unwrap();
    node.st_ok(&[
        "missions",
        "publish",
        parent_file.to_str().unwrap(),
        "--as",
        PERSON,
    ]);
    node.st_ok(&[
        "missions",
        "start",
        "orchard/weekly",
        "--id",
        "orchard/weekly",
        "--workspace",
        node.root.to_str().unwrap(),
        "--as",
        PERSON,
    ]);
    wait_until("the first weekly child finishes", 15, || async {
        node.claims().await.iter().any(|claim| {
            claim["kind"] == "mission-run.state" && claim["body"]["fields"]["status"] == "completed"
        })
    })
    .await;

    for index in 0..4 {
        fs::write(&parent_file, parent(&revisions[index % 2])).unwrap();
        node.st_ok(&[
            "missions",
            "publish",
            parent_file.to_str().unwrap(),
            "--as",
            PERSON,
        ]);
        if index > 0 {
            node.st_ok(&[
                "work",
                "revise",
                "mission-run/orchard/weekly",
                parent_file.to_str().unwrap(),
                "--reason",
                "Select the next harvest revision",
                "--as",
                PERSON,
            ]);
        }
        // Let the daemon reach a quiet pass, including any incorrectly admitted duplicate.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let claims = node.claims().await;
        assert_eq!(
            claims
                .iter()
                .filter(|claim| claim["kind"] == "schedule.work-started")
                .count(),
            1,
            "parent publication/revision replayed the due weekly tick"
        );
    }
    node.restart().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        node.claims()
            .await
            .iter()
            .filter(|claim| claim["kind"] == "schedule.work-started")
            .count(),
        1,
        "restart replayed the due weekly tick"
    );
    wait_until("the next genuine weekly tick starts", 35, || async {
        node.claims()
            .await
            .iter()
            .filter(|claim| claim["kind"] == "schedule.work-started")
            .count()
            >= 2
    })
    .await;
    let claims = node.claims().await;
    assert_eq!(
        claims
            .iter()
            .filter(|claim| claim["kind"] == "schedule.work-started")
            .count(),
        2
    );
    let occurrences: Vec<_> = claims
        .iter()
        .filter(|claim| claim["kind"] == "schedule.occurrence-reached")
        .map(|claim| claim["body"]["fields"]["occurrence"].as_u64().unwrap())
        .collect();
    assert_eq!(occurrences, vec![0, 1]);
}

/// A stopped origin's earlier running claims must not fence a seat placed on another host.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_seat_is_reachable_after_a_cross_host_move() {
    if st3::test_support::supervise_test() {
        return;
    }
    cross_host_seat_move(SeatMove::Stopped).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_handoff_moves_a_running_seat_between_real_daemons() {
    if st3::test_support::supervise_test() {
        return;
    }
    cross_host_seat_move(SeatMove::Running).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_handoff_source_offline_retires_the_returning_real_daemon_runtime() {
    if st3::test_support::supervise_test() {
        return;
    }
    cross_host_seat_move(SeatMove::SourceOffline).await;
}

#[derive(Clone, Copy, PartialEq)]
enum SeatMove { Stopped, Running, SourceOffline }

async fn cross_host_seat_move(mode: SeatMove) {
    const SUBJECT: &str = "agent/move/worker";
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "amber").await;
    let b = joined(root.path(), &a, "cobalt", &[]).await;
    let pty = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("pty"))
        .find(|path| path.is_file())
        .expect("the development and CI environments provide the real pty binary");
    for node in [&a, &b] {
        let launcher = node.root.join("bin/pty");
        fs::remove_file(&launcher).unwrap();
        std::os::unix::fs::symlink(&pty, launcher).unwrap();
    }
    let source = root.path().join("seat.kdl");
    fs::write(
        &source,
        r#"version 2
agent "move/worker" {
    host "amber"
    restart "always"
    argv "sh" "-c" "echo moved-seat-ready; while :; do sleep 1; done"
}
"#,
    )
    .unwrap();
    a.st_ok(&["agents", "apply", source.to_str().unwrap(), "--as", PERSON]);
    let status =
        |node: &Node| node.st_json(&["subject", "show", SUBJECT])["status"]["subjects"][0].clone();
    wait_until("amber runs the seat on both replicas", 30, || async {
        [&a, &b].iter().all(|node| {
            let view = status(node);
            view["actual"]["status"] == "running"
                && view["actual"]["host"] == "amber"
                && view["reachability"] == "reachable"
        })
    })
    .await;
    let old_incarnation = status(&a)["actual"]["incarnation_id"].clone();
    if mode == SeatMove::Stopped {
        a.st_ok(&["agents", "stop", SUBJECT, "--as", PERSON]);
        wait_until("amber's stop reaches both replicas", 30, || async {
            [&a, &b]
                .iter()
                .all(|node| status(node)["actual"]["status"] == "stopped")
        })
        .await;
    }
    if mode == SeatMove::SourceOffline {
        a.stop();
        b.st_ok(&["agents", "start", SUBJECT, "--host", "cobalt", "--source-offline", "--as", PERSON]);
    } else {
        b.st_ok(&["agents", "start", SUBJECT, "--host", "cobalt", "--as", PERSON]);
    }
    wait_until("cobalt's running observation reaches both replicas", 30, || async {
        let replicas = if mode == SeatMove::SourceOffline { vec![&b] } else { vec![&a, &b] };
        for node in replicas {
            if !node.claims().await.iter().any(|claim| {
                claim["subject"] == SUBJECT
                    && claim["kind"] == "runtime.observed"
                    && claim["origin"] == "cobalt"
                    && claim["body"]["fields"]["status"] == "running"
                    && claim["body"]["fields"]["incarnation_id"] != old_incarnation
            }) {
                return false;
            }
        }
        true
    })
    .await;
    if mode == SeatMove::SourceOffline {
        let proof = b.claims().await.into_iter().find(|claim| {
            claim["kind"] == "agent.placement.source-offline" && claim["subject"] == SUBJECT
        }).expect("source-offline move records its exception");
        assert_eq!(proof["actor"], PERSON);
        let peek = b.st_ok(&["terminals", "peek", SUBJECT]);
        assert!(peek.contains("moved-seat-ready"), "destination is reachable while source is offline: {peek}");
        a.start().await;
        wait_until("returning source retires its old incarnation", 30, || async {
            b.claims().await.iter().any(|claim| {
                claim["subject"] == SUBJECT && claim["origin"] == "amber"
                    && claim["body"]["fields"]["reason"] == "placed-elsewhere"
                    && claim["body"]["fields"]["incarnation_id"] == old_incarnation
            })
        }).await;
    }
    wait_until("both replicas report the destination incarnation", 30, || async {
        [&a, &b].iter().all(|node| {
            let view = status(node);
            view["actual"]["host"] == "cobalt"
                && view["actual"]["status"] == "running"
                && view["actual"]["incarnation_id"] != old_incarnation
                && view["reachability"] == "reachable"
        })
    }).await;
    let runtime_claims = b.claims().await.into_iter()
        .filter(|claim| claim["subject"] == SUBJECT && claim["kind"] == "runtime.observed")
        .collect::<Vec<_>>();
    let source_stop = runtime_claims.iter().find(|claim| {
        claim["kind"] == "runtime.observed"
            && claim["origin"] == "amber"
            && claim["body"]["fields"]["status"] == "stopped"
            && claim["body"]["fields"]["reason"] == "placed-elsewhere"
    });
    let moved = [&a, &b].map(status);
    let peek = b.st(&["terminals", "peek", SUBJECT]);
    let screen = b.st(&["terminals", "screen", SUBJECT]);
    // Stop the actual terminal runtime before dropping the daemons, including on assertion failure.
    b.st_ok(&["agents", "stop", SUBJECT, "--as", PERSON]);
    wait_until("the moved process stops", 30, || async {
        status(&b)["actual"]["status"] == "stopped"
    })
    .await;
    if mode != SeatMove::Stopped {
        let stopped = source_stop.unwrap_or_else(|| {
            panic!("destination received no source placement-away stop: {runtime_claims:#?}")
        });
        assert_eq!(stopped["body"]["fields"]["incarnation_id"], old_incarnation);
        assert!(!stopped["body"]["evidence"].as_array().unwrap().is_empty());
    }
    for (node, view) in [&a, &b].into_iter().zip(moved) {
        assert_eq!(view["reachability"], "reachable", "{view}\n{}", node.logs());
        assert_ne!(view["actual"]["incarnation_id"], old_incarnation);
    }
    for output in [peek, screen] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("moved-seat-ready"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

fn authority_digest(node: &Node) -> String {
    node.st_json(&["replication", "status"])["authority_digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_populated_standalone_daemon_founds_a_fleet_with_verified_person_and_agent_claims() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut a = Node::new(root.path(), "studio");
    a.start().await;
    wait_until(
        "the initial standalone checkpoint is sealed",
        30,
        || async {
            a.claims()
                .await
                .iter()
                .any(|claim| claim["kind"] == "checkpoint.sealed")
        },
    )
    .await;
    let agent = "agent/garden/worker";
    let mut claims = vec![
        a.note("person-before-founding").await,
        a.note_as("agent-before-founding", agent).await,
    ];
    // No doctor, replication exchange or later checkpoint may seal the grants first.
    let pending: u64 = rusqlite::Connection::open_with_flags(
        a.state_dir().join("claims.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT COUNT(*) FROM claims WHERE kind='principal.key-granted'
         AND NOT EXISTS (SELECT 1 FROM replica_envelopes WHERE batch_id=claims.batch_id)",
        [],
        |row| row.get(0),
    )
    .unwrap();
    assert!(
        pending > 0,
        "the regression needs unsealed delegation history"
    );
    a.stop();
    a.create();
    a.start().await;
    claims.push(a.note("person-after-founding").await);
    claims.push(a.note_as("agent-after-founding", agent).await);
    a.restart().await;
    claims.push(a.note_as("agent-after-another-restart", agent).await);
    a.wait_listening().await;
    let b = joined(root.path(), &a, "beacon", &[]).await;
    b.st_ok(&["fleet", "wait", "--timeout", "90s"]);
    wait_until(
        "the peer receives every person and agent claim",
        60,
        || async {
            let received = b.claims().await;
            claims
                .iter()
                .all(|id| received.iter().any(|claim| claim["id"] == id.as_str()))
        },
    )
    .await;
    for node in [&a, &b] {
        let doctor = node.st_json(&["doctor"]);
        let signatures = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "claim-signatures")
            .unwrap();
        assert_eq!(signatures["status"], "pass", "{}: {signatures}", node.name);
        let connection = rusqlite::Connection::open_with_flags(
            node.state_dir().join("claims.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for id in &claims {
            let (verdict, reason): (String, Option<String>) = connection
                .query_row(
                    "SELECT verdict, reason FROM claim_verdicts WHERE claim_id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(verdict, "verified", "{} claim {id}: {reason:?}", node.name);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_and_join_sync_full_history() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut expected = BTreeSet::new();
    for index in 0..60 {
        a.note(&format!("a-{index}")).await;
        expected.insert(format!("custom/fleet-test/a-{index}"));
    }

    let b = joined(root.path(), &a, "b", &[]).await;
    // A newcomer's first sync ends by checking that it projects its sponsor's graph.
    let waited = b.st_ok(&["fleet", "wait", "--timeout", "90s"]);
    assert!(waited.contains("first sync verified"), "{waited}");
    let first = b.st_json(&["replication", "status"])["first_sync"].clone();
    assert_eq!(first["state"], "verified", "{first}");
    assert_eq!(first["healed"], false, "{first}");
    assert_eq!(first["graph_digest"], first["peer_graph_digest"], "{first}");
    wait_for_notes(&b, &expected, 60, &[&a, &b]).await;
    b.wait_listening().await;
    b.note("b-0").await;
    expected.insert("custom/fleet-test/b-0".into());
    wait_for_notes(&a, &expected, 60, &[&a, &b]).await;

    // C joins through B, not the anchor.
    let c = joined(root.path(), &b, "c", &[]).await;
    c.note("c-0").await;
    expected.insert("custom/fleet-test/c-0".into());
    for node in [&a, &b, &c] {
        wait_for_notes(node, &expected, 60, &[&a, &b, &c]).await;
    }
    wait_until("the three authority digests agree", 60, || async {
        let digests = [&a, &b, &c]
            .iter()
            .map(|node| authority_digest(node))
            .collect::<BTreeSet<_>>();
        digests.len() == 1
    })
    .await;

    for node in [&a, &b, &c] {
        let config = fs::read_to_string(node.root.join("config/st3/config.toml")).unwrap();
        assert!(
            !config.contains("[[peers]]"),
            "{} has config peers",
            node.name
        );
        for arguments in node.arguments.lock().unwrap().iter() {
            assert!(!arguments.iter().any(|argument| argument == "--peer"));
        }
        // Every admitted envelope of a keyed writer carries its signature: nothing is held.
        let status = node.st_json(&["replication", "status"]);
        assert_eq!(status["unsigned_envelopes"], 0, "{status}");
        assert_eq!(status["fenced_envelopes"], 0, "{status}");
        // Every member verifies every other member's signed claims through membership.
        let doctor = node.st_json(&["doctor"]);
        let signatures = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "claim-signatures")
            .cloned()
            .unwrap();
        assert_eq!(signatures["status"], "pass", "{}: {signatures}", node.name);
        let members = node.st_json(&["fleet", "status"])["view"]["members"].clone();
        for name in ["a", "b", "c"] {
            assert!(
                members
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|member| { member["name"] == name && member["state"] == "current" }),
                "{} does not see {name} as a member: {members}",
                node.name
            );
        }
    }
}

/// Members have no `[[peers]]`, so a client read to another host's owner state goes through the
/// relay that dials by the fleet view. Each direction must answer, and a host nobody can reach
/// must say it has no route rather than that it is temporarily unavailable.
#[tokio::test(flavor = "multi_thread")]
async fn members_read_each_others_owner_state_and_unreachable_owners_say_why() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    let waited = b.st_ok(&["fleet", "wait", "--timeout", "90s"]);
    assert!(waited.contains("first sync verified"), "{waited}");
    b.wait_listening().await;

    for (from, owner) in [(&a, "b"), (&b, "a")] {
        let client = Client::unix_as(from.socket(), PERSON).unwrap();
        let path = format!("/v1/hosts/{owner}/agent-workspace?identity=agent/fleet-test/probe");
        let mut last = String::new();
        let mut answered = None;
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            match client.get::<Value>(&path).await {
                Ok(value) => {
                    answered = Some(value);
                    break;
                }
                Err(error) => last = format!("{error:#}"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let value = answered.unwrap_or_else(|| {
            panic!(
                "{} could not read {owner}'s workspace through the relay: {last}\n{}",
                from.name,
                from.logs()
            )
        });
        assert_eq!(value["host_id"], format!("host/{owner}"), "{value}");
        assert!(value["workspace"].is_string(), "{value}");
    }

    let client = Client::unix_as(a.socket(), PERSON).unwrap();
    let error = client
        .get::<Value>("/v1/hosts/ghost/agent-workspace?identity=agent/fleet-test/probe")
        .await
        .unwrap_err();
    let (status, code, message, details) =
        st3::client::api_error_parts(&error).unwrap_or_else(|| panic!("{error:#}"));
    assert_eq!(status, 503, "{message}");
    assert_eq!(code, "remote-unavailable", "{message}");
    assert!(!message.contains("temporarily"), "{message}");
    assert_eq!(details["reason"], "no-route", "{details:?}");
    assert_eq!(details["owner_host_id"], "host/ghost", "{details:?}");
}

/// The `sync` object `st replication status` reports for `peer`, or null.
fn peer_sync(node: &Node, peer: &str) -> Value {
    node.st_json(&["replication", "status"])["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|status| status["peer"] == peer)
        .map(|status| status["sync"].clone())
        .unwrap_or(Value::Null)
}

/// Two members can hold the same envelopes and still project different graphs, as when one of
/// them lost claims it had admitted. No exchange fixes that, so neither may call the pair in
/// sync: replication status says diverged, doctor fails, and every client page carries it.
/// Then they heal: they find the claims one lacks and admit their envelopes again.
#[tokio::test(flavor = "multi_thread")]
async fn members_with_the_same_envelopes_but_different_claims_report_divergence_and_heal() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "a").await;
    let mut b = joined(root.path(), &a, "b", &[]).await;
    let mission = root.path().join("probe.kdl");
    fs::write(
        &mission,
        "version 2\nmission \"divergence-probe\" state=\"ready\" {\n  \
         goal \"Give both graphs a row that one member can lose.\"\n  \
         step \"only\" { agentless }\n}\n",
    )
    .unwrap();
    a.st_ok(&[
        "missions",
        "publish",
        mission.to_str().unwrap(),
        "--as",
        PERSON,
    ]);
    wait_until(
        "both members hold the mission and call the pair in sync",
        90,
        || async {
            a.st_json(&["replication", "status"])["graph_digest"]
                == b.st_json(&["replication", "status"])["graph_digest"]
                && [(&a, "b"), (&b, "a")].iter().all(|(node, peer)| {
                    node.st_ok(&["replication", "status"])
                        .contains("in sync: the same envelopes and the same graph")
                        && peer_sync(node, peer)["graph_compared_at_unix_ms"].is_number()
                })
        },
    )
    .await;

    let in_sync_tables = a.st_json(&["replication", "status"])["projection_digests"].clone();
    // Heals wait while the divergence is inspected.
    let hold = (
        "ST3_REPLICATION_HEAL_AFTER_MS".to_owned(),
        "3600000".to_owned(),
    );
    a.env.push(hold.clone());
    a.restart().await;
    b.env.push(hold);

    // B loses the mission's claims but keeps their envelopes, so both inventories still match.
    b.stop();
    {
        let store = rusqlite::Connection::open(b.state_dir().join("claims.sqlite3")).unwrap();
        st3::store::configure_projection_writer(&store).unwrap();
        let dropped = store
            .execute(
                "DELETE FROM mission_definitions WHERE mission_id LIKE '%divergence-probe%'",
                [],
            )
            .unwrap();
        assert_eq!(dropped, 1, "b projected the mission");
        store
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                 DELETE FROM claims WHERE subject LIKE '%divergence-probe%';",
            )
            .unwrap();
    }
    b.start().await;

    wait_until("both members report the divergence", 180, || async {
        peer_sync(&a, "b")["diverged"] == true && peer_sync(&b, "a")["diverged"] == true
    })
    .await;
    for (node, peer) in [(&a, "b"), (&b, "a")] {
        let status = node.st_ok(&["replication", "status"]);
        assert!(
            status.contains(&format!(
                "sync\tdiverged: {peer} holds the same envelopes but projects a different graph"
            )),
            "{status}"
        );
        assert!(!status.contains("in sync"), "{status}");
        let doctor = node.st(&["--json", "doctor"]);
        assert!(!doctor.status.success(), "{} doctor passed", node.name);
        let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "replication")
            .cloned()
            .unwrap();
        assert_eq!(check["status"], "fail", "{check}");
        assert!(
            check["message"]
                .as_str()
                .unwrap()
                .starts_with(&format!("graph diverged from {peer}")),
            "{check}"
        );
        let page = node.st_json(&["machines"]);
        assert_eq!(page["value"]["sync"]["state"], "diverged", "{page}");
        assert_eq!(
            page["value"]["sync"]["peers"][0]["host_id"],
            format!("host/{peer}")
        );
        assert!(page["value"]["sync"]["peers"][0]["diverged_since"].is_string());
        let machines = node.st_ok(&["machines"]);
        assert!(
            machines.starts_with(&format!(
                "DIVERGED  {peer} projects a different graph from the same envelopes"
            )),
            "{machines}"
        );
    }

    // Without the hold, the members heal soon after they find the graphs different.
    for node in [&mut a, &mut b] {
        node.env = vec![(
            "ST3_REPLICATION_HEAL_AFTER_MS".to_owned(),
            "1000".to_owned(),
        )];
        node.restart().await;
    }
    wait_until("the members heal", 120, || async {
        a.st_json(&["replication", "status"])["graph_digest"]
            == b.st_json(&["replication", "status"])["graph_digest"]
            && [(&a, "b"), (&b, "a")]
                .iter()
                .all(|(node, peer)| peer_sync(node, peer)["diverged"] != true)
    })
    .await;
    let restored_tables = b.st_json(&["replication", "status"])["projection_digests"].clone();
    for (table, digest) in in_sync_tables.as_object().unwrap() {
        // Restarts and healing append durable recovery claims and their retry operations.
        // Both modern peers must agree on their complete maps above; materialized business
        // tables must also recover the exact pre-fault contents.
        if !matches!(table.as_str(), "claim_sources" | "operations") {
            assert_eq!(
                &restored_tables[table], digest,
                "b restores the pre-fault shared table {table}"
            );
        }
    }
    let heals = [(&a, "b"), (&b, "a")]
        .iter()
        .filter_map(|(node, peer)| {
            let heal = peer_sync(node, peer)["heal"].clone();
            (!heal.is_null()).then_some((node.name.clone(), heal))
        })
        .collect::<Vec<_>>();
    assert!(
        heals.iter().any(|(_, heal)| heal["healed"] == true
            && heal["refetched"].as_u64().unwrap_or(0) + heal["pushed"].as_u64().unwrap_or(0) > 0),
        "{heals:?}"
    );
    let restored = b
        .claims()
        .await
        .into_iter()
        .filter(|claim| {
            claim["subject"]
                .as_str()
                .unwrap_or("")
                .contains("divergence-probe")
        })
        .count();
    assert!(restored > 0, "b admitted the mission's claims again");
    for node in [&a, &b] {
        let doctor = node.st(&["--json", "doctor"]);
        let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "replication")
            .cloned()
            .unwrap();
        assert_ne!(check["status"], "fail", "{check}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dial_out_member_is_caught_up_and_never_reported_down() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut laptop = joined(root.path(), &a, "laptop", &["--dial-out"]).await;
    let mut expected = BTreeSet::from(["custom/fleet-test/before".to_owned()]);
    a.note("before").await;
    wait_for_notes(&laptop, &expected, 60, &[&a, &laptop]).await;

    laptop.stop();
    for index in 0..600 {
        a.note(&format!("away-{index}")).await;
        expected.insert(format!("custom/fleet-test/away-{index}"));
    }
    // Three idle periods pass with the laptop gone.
    tokio::time::sleep(Duration::from_secs(2)).await;
    laptop.start().await;
    wait_for_notes(&laptop, &expected, 120, &[&a, &laptop]).await;

    let down = a
        .claims()
        .await
        .into_iter()
        .filter(|claim| {
            claim["subject"] == "host/laptop"
                && claim["kind"] == "transport.observed"
                && claim["body"]["fields"]["status"] == "down"
        })
        .count();
    assert_eq!(down, 0, "the dial-out laptop was reported down");
    let peers = a.st_json(&["replication", "status"])["peers"].clone();
    assert!(
        peers
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer["peer"] == "laptop" && peer["last_success_at_unix_ms"].is_number()),
        "{peers}"
    );
    let machines = a.st_json(&["machines"]);
    let laptop_machine = machines["value"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|machine| machine["host_id"] == "host/laptop")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(laptop_machine["state"], "reachable", "{machines}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_join_resumes_with_the_same_code() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut a = Node::new(root.path(), "a");
    let marker = root.path().join("drop-one-join-answer");
    a.env.push((
        "ST3_TEST_DROP_JOIN_ANSWER".into(),
        marker.display().to_string(),
    ));
    a.create();
    a.start().await;
    a.wait_listening().await;

    // The sponsor binds the invite, and the answer is lost.
    fs::write(&marker, b"").unwrap();
    let code = a.invite("b", &[]);
    let mut b = Node::new(root.path(), "b");
    let lost = b.join(&code, &[]);
    assert!(!lost.status.success());
    assert!(!marker.exists(), "the fault point fired");

    // Retry after the sponsor restarts: the same key redeems again.
    a.restart().await;
    a.wait_listening().await;
    let retried = b.join(&code, &[]);
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    // Once more, now that the secret is stored: it resumes without contacting the sponsor.
    assert!(b.join(&code, &[]).status.success());
    let admissions = a
        .claims()
        .await
        .into_iter()
        .filter(|claim| claim["subject"] == "host/b" && claim["kind"] == "fleet.member-admitted")
        .count();
    assert_eq!(admissions, 1, "a retry appended claims");
    b.start().await;
    a.note("hello").await;
    wait_for_notes(
        &b,
        &BTreeSet::from(["custom/fleet-test/hello".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;

    // Another machine with the same code is refused.
    let other = Node::named(root.path(), "b2", "b");
    let refused = other.join(&code, &[]);
    assert!(!refused.status.success());

    // A lost answer whose code then expires strands an admitted member that is never seen.
    fs::write(&marker, b"").unwrap();
    let short = a.invite("e", &["--expires", "10s"]);
    let e = Node::new(root.path(), "e");
    assert!(!e.join(&short, &[]).status.success());
    tokio::time::sleep(Duration::from_secs(11)).await;
    let expired = e.join(&short, &[]);
    assert!(!expired.status.success());
    let members = a.st_json(&["fleet", "status"])["view"]["members"].clone();
    assert!(
        members
            .as_array()
            .unwrap()
            .iter()
            .any(|member| member["name"] == "e" && member["state"] == "current"),
        "the stranded member is admitted: {members}"
    );
    let peers = a.st_json(&["replication", "status"])["peers"].clone();
    assert!(
        peers
            .as_array()
            .unwrap()
            .iter()
            .all(|peer| peer["peer"] != "e" || peer["last_success_at_unix_ms"].is_null()),
        "the stranded member was never seen: {peers}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_revoked_and_used_codes_are_refused() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let refusal = |output: Output| {
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(stderr.contains("refused this code"), "{stderr}");
        stderr
    };

    let expiring = a.invite("x", &["--expires", "10s"]);
    let revoked = a.invite("y", &[]);
    let used = a.invite("z", &[]);
    let listed = a.st_json(&["fleet", "invites"]);
    let revoked_id = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|invite| invite["name"] == "y")
        .unwrap()["invite"]
        .as_str()
        .unwrap()
        .to_owned();
    a.st_ok(&[
        "fleet",
        "invites",
        "revoke",
        &revoked_id,
        "--reason",
        "test",
        "--as",
        PERSON,
    ]);
    let z = Node::new(root.path(), "z");
    assert!(z.join(&used, &[]).status.success());
    tokio::time::sleep(Duration::from_secs(11)).await;

    let first = refusal(Node::new(root.path(), "x").join(&expiring, &[]));
    let second = refusal(Node::new(root.path(), "y").join(&revoked, &[]));
    let third = refusal(Node::named(root.path(), "z2", "z").join(&used, &[]));
    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leaked_code_is_visible_and_can_be_revoked() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let m = joined(root.path(), &a, "m", &[]).await;

    // A stranger redeems the code meant for the laptop first.
    let code = a.invite("laptop", &[]);
    let stranger = Node::named(root.path(), "stranger", "laptop");
    assert!(stranger.join(&code, &[]).status.success());
    let stranger_key = fs::read_to_string(stranger.state_dir().join("fleet/fleet.toml")).unwrap();
    assert!(stranger_key.contains("node = \"laptop\""));
    let rightful = Node::new(root.path(), "laptop");
    let refused = rightful.join(&code, &[]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("st fleet invites"));

    for node in [&a, &m] {
        wait_until(
            &format!("{} shows the redemption", node.name),
            60,
            || async {
                node.st_json(&["fleet", "invites"])
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|invite| {
                        invite["name"] == "laptop"
                            && invite["state"] == "redeemed"
                            && invite["redeemed_name"] == "laptop"
                            && invite["redeemed_key"].is_string()
                            && invite["redeemed_at_unix_ms"].is_number()
                    })
            },
        )
        .await;
    }

    // A second code leaks before use and is revoked from another member.
    let leaked = a.invite("tablet", &[]);
    let listed = m.st_json(&["fleet", "invites"]);
    wait_until("m sees the new invite", 60, || async {
        m.st_json(&["fleet", "invites"])
            .as_array()
            .unwrap()
            .iter()
            .any(|invite| invite["name"] == "tablet")
    })
    .await;
    let _ = listed;
    let tablet_invite = m
        .st_json(&["fleet", "invites"])
        .as_array()
        .unwrap()
        .iter()
        .find(|invite| invite["name"] == "tablet")
        .unwrap()["invite"]
        .as_str()
        .unwrap()
        .to_owned();
    m.st_ok(&[
        "fleet",
        "invites",
        "revoke",
        &tablet_invite,
        "--reason",
        "leaked",
        "--as",
        PERSON,
    ]);
    wait_until("the revocation reaches the sponsor", 60, || async {
        a.st_json(&["fleet", "invites", "--all"])
            .as_array()
            .unwrap()
            .iter()
            .any(|invite| invite["name"] == "tablet" && invite["state"] == "revoked")
    })
    .await;
    let late = Node::named(root.path(), "tablet", "tablet").join(&leaked, &[]);
    assert!(!late.status.success(), "a revoked code was redeemed");
    assert!(String::from_utf8_lossy(&late.stderr).contains("refused this code"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_secret_never_leaves_its_file() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    let laptop = joined(root.path(), &a, "laptop", &["--dial-out"]).await;
    a.note("shared").await;
    let expected = BTreeSet::from(["custom/fleet-test/shared".to_owned()]);
    wait_for_notes(&b, &expected, 60, &[&a, &b, &laptop]).await;
    wait_for_notes(&laptop, &expected, 60, &[&a, &b, &laptop]).await;

    let secret = a.secret().expect("the anchor has a secret");
    assert_eq!(b.secret().as_deref(), Some(secret.as_slice()));
    let forms = [
        secret.clone(),
        hex::encode(&secret).into_bytes(),
        hex::encode_upper(&secret).into_bytes(),
        data_encoding::BASE32_NOPAD.encode(&secret).into_bytes(),
        data_encoding::BASE32_NOPAD
            .encode(&secret)
            .to_ascii_lowercase()
            .into_bytes(),
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &secret).into_bytes(),
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &secret)
            .into_bytes(),
    ];
    let contains = |haystack: &[u8]| {
        forms.iter().any(|form| {
            haystack
                .windows(form.len())
                .any(|window| window == form.as_slice())
        })
    };

    // Every file under every node, except the secret files themselves.
    for entry in walkdir::WalkDir::new(root.path()) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() || entry.file_name() == "secret" {
            continue;
        }
        let bytes = fs::read(entry.path()).unwrap_or_default();
        assert!(
            !contains(&bytes),
            "the fleet secret is in {}",
            entry.path().display()
        );
    }
    // Every argument list this test gave an st3 process.
    for node in [&a, &b, &laptop] {
        for arguments in node.arguments.lock().unwrap().iter() {
            assert!(!contains(arguments.join(" ").as_bytes()));
        }
    }
    // On Linux, every running process's arguments too.
    if let Ok(processes) = fs::read_dir("/proc") {
        for process in processes.flatten() {
            if let Ok(arguments) = fs::read(process.path().join("cmdline")) {
                assert!(
                    !contains(&arguments),
                    "a process has the secret in its arguments"
                );
            }
        }
    }
    // Every claim and document the fleet holds.
    for node in [&a, &b, &laptop] {
        let claims = serde_json::to_vec(&node.claims().await).unwrap();
        assert!(
            !contains(&claims),
            "a claim on {} holds the secret",
            node.name
        );
        let documents = node.st_ok(&["--json", "documents", "ls", "--all"]);
        assert!(!contains(documents.as_bytes()));
    }
    let _ = json!({});
}

/// A rejoining member with tombstones from a different checkpoint lineage must still exchange
/// later live claims in both directions (#1431). The tombstone rows model already-certified
/// offline trims; this test does not manufacture certificates or transport tombstones.
#[tokio::test(flavor = "multi_thread")]
async fn rejoin_exchanges_live_claims_beyond_differing_checkpoint_tombstones() {
    if st3::test_support::supervise_test() {
        return;
    }
    rejoin_tombstone_fixture(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejoin_slow_exports_preserve_live_claims_and_show_overload_retry() {
    if st3::test_support::supervise_test() { return; }
    rejoin_tombstone_fixture(true).await;
}

async fn rejoin_tombstone_fixture(slow_export: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut birch = anchor(root.path(), "birch").await;
    let mut cedar = joined(root.path(), &birch, "cedar", &[]).await;
    birch.note("birch-shared").await;
    cedar.note("cedar-shared").await;
    let shared = BTreeSet::from([
        "custom/fleet-test/birch-shared".into(),
        "custom/fleet-test/cedar-shared".into(),
    ]);
    for node in [&birch, &cedar] {
        wait_for_notes(node, &shared, 30, &[&birch, &cedar]).await;
    }
    birch.stop();
    cedar.stop();
    for node in [&birch, &cedar] {
        let mut connection =
            rusqlite::Connection::open(node.state_dir().join("claims.sqlite3")).unwrap();
        let transaction = connection.transaction().unwrap();
        for id in smallclaims::store::test_envelope_ids("alder-retired", 0..768, &node.name) {
            transaction
                .execute(
                    "INSERT INTO checkpoint_envelopes VALUES (?1, ?2, ?3, 0, ?4)",
                    rusqlite::params![
                        id.writer,
                        id.sequence,
                        id.hash,
                        format!("checkpoint/{}", node.name)
                    ],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
    }
    // Publish on both sides with no replication listener open, then rejoin their workers.
    birch.start().await;
    birch.stop_worker();
    cedar.start().await;
    cedar.stop_worker();
    let mut expected = shared;
    for node in [&birch, &cedar] {
        for index in 0..4 {
            let text = format!("{}-partition-{index}", node.name);
            node.note(&text).await;
            expected.insert(format!("custom/fleet-test/{text}"));
        }
    }
    assert_eq!(birch.notes().await.len(), 6);
    assert_eq!(cedar.notes().await.len(), 6);
    let before = [&birch, &cedar].map(|node| {
        node.st_json(&["replication", "status"])["timings"]["exchanges"]
            .as_u64()
            .unwrap()
    });
    if slow_export {
        // Restart only the isolated daemon with the fixture-only injection. Each response
        // export takes 25 seconds, beyond the old caller's entire 20-second HTTP deadline.
        cedar.env.push(("ST3_TEST_REPLICATION_EXPORT_DELAY_MS".into(), "25000".into()));
        cedar.stop();
        cedar.start().await;
        cedar.stop_worker();
    }
    birch.start_worker();
    cedar.start_worker();
    if slow_export {
        wait_until("signed overload exposes polling phase and retry", 90, || async {
            let status = birch.st_json(&["replication", "status"]);
            status["peers"].as_array().unwrap().iter().any(|peer| {
                peer["worker"]["phase"] == "overload"
                    && peer["worker"]["last_attempt_at_unix_ms"].as_u64().is_some()
                    && peer["worker"]["next_retry_at_unix_ms"].as_u64().is_some()
            })
        }).await;
        let text = birch.st_ok(&["replication", "status"]);
        assert!(text.contains("worker overload"), "{text}");
        assert!(text.contains("next retry in"), "{text}");
    }
    for (node, before) in [&birch, &cedar].into_iter().zip(before) {
        wait_until("both rejoined workers exchange successfully", if slow_export { 180 } else { 15 }, || async {
            node.st_json(&["replication", "status"])["timings"]["exchanges"]
                .as_u64()
                .unwrap() >= before + 4
        }).await;
        let status = node.st_json(&["replication", "status"]);
        eprintln!("{} rejoin exchanges={} envelopes_received={}",
            node.name, status["timings"]["exchanges"], status["timings"]["envelopes_received"]);
    }
    for node in [&birch, &cedar] {
        wait_for_notes(node, &expected, if slow_export { 180 } else { 30 }, &[&birch, &cedar]).await;
        let connection =
            rusqlite::Connection::open(node.state_dir().join("claims.sqlite3")).unwrap();
        let count: u64 = connection
            .query_row("SELECT COUNT(*) FROM checkpoint_envelopes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 768, "no tombstone payloads replicate");
    }
}

/// `st fleet wait` gates a restart (#1022). A first sync is verified once; after a restart
/// far behind, the wait must also see an exchange since it began at which this node held
/// everything its peer held, instead of returning at once with the old verdict.
#[tokio::test(flavor = "multi_thread")]
async fn fleet_wait_after_a_restart_waits_until_the_member_has_caught_up() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut b = joined(root.path(), &a, "b", &[]).await;
    let waited = b.st_ok(&["fleet", "wait", "--timeout", "90s"]);
    assert!(waited.contains("first sync verified"), "{waited}");

    b.stop();
    for index in 0..500 {
        a.note(&format!("while-b-stopped-{index}")).await;
    }
    let held = a.st_json(&["replication", "status"])["received_envelopes"]
        .as_u64()
        .unwrap();
    b.start().await;
    let waited = b.st_ok(&["fleet", "wait", "--timeout", "120s"]);
    let received = b.st_json(&["replication", "status"])["received_envelopes"]
        .as_u64()
        .unwrap();
    assert!(
        received >= held,
        "fleet wait returned while b held {received} of a's {held} envelopes:\n{waited}"
    );
    assert!(waited.contains("this node then held the same"), "{waited}");
    assert!(
        waited.contains("caught up now: at its latest exchange with a"),
        "{waited}"
    );

    // The JSON says the same.
    let waited = b.st_json(&["fleet", "wait", "--timeout", "60s"]);
    assert_eq!(waited["state"], "verified", "{waited}");
    assert_eq!(waited["caught_up"]["peers"][0][0], "a", "{waited}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_member_is_refused() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.note("before").await;
    wait_for_notes(
        &a,
        &BTreeSet::from(["custom/fleet-test/before".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;

    a.st_ok(&["fleet", "remove", "b", "--reason", "test", "--as", PERSON]);
    // b learns it was removed from a signed refusal naming its own key, and stops dialing.
    wait_until("b records its removal", 60, || async {
        fs::read_to_string(b.state_dir().join("fleet/fleet.toml"))
            .is_ok_and(|file| file.contains("[removed]") && file.contains("member-removed"))
    })
    .await;
    b.note("after").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !a.notes().await.contains("custom/fleet-test/after"),
        "a write after the removal reached a"
    );
    let members = a.st_json(&["fleet", "status"])["view"]["members"].clone();
    assert!(
        members.as_array().unwrap().iter().any(|member| {
            member["name"] == "b" && member["state"] == "ended" && member["ended"] == "removed"
        }),
        "{members}"
    );

    // b itself says it was removed, by whom and why, everywhere a person looks (#1021).
    let fleet = b.st_json(&["fleet", "status"]);
    let removed = &fleet["removed"];
    assert_eq!(removed["code"], "member-removed", "{fleet}");
    assert_eq!(removed["reported_by"], "a", "{fleet}");
    assert!(
        removed["message"].as_str().is_some_and(
            |message| message.contains(&format!("removed from this fleet by {PERSON}: test"))
        ),
        "{fleet}"
    );
    assert!(
        removed["learned_at_unix_ms"]
            .as_u64()
            .is_some_and(|at| at > 0),
        "{fleet}"
    );
    let text = b.st_ok(&["fleet", "status"]);
    assert!(
        text.contains("REMOVED  this node was removed from fleet"),
        "{text}"
    );
    let status = b.st_json(&["replication", "status"]);
    assert_eq!(status["removed"]["code"], "member-removed", "{status}");
    let doctor = b.st(&["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "replication")
        .cloned()
        .unwrap();
    assert_eq!(check["status"], "fail", "{check}");
    let message = check["message"].as_str().unwrap();
    assert!(
        message.contains("this node was removed from fleet"),
        "{check}"
    );
    assert!(message.contains("st fleet leave --offline"), "{check}");
    let waited = b.st(&["fleet", "wait", "--timeout", "30s"]);
    assert!(!waited.status.success());
    assert!(
        String::from_utf8_lossy(&waited.stderr).contains("this node was removed from fleet"),
        "{waited:?}"
    );
    let log = fs::read_to_string(b.root.join("worker.stderr.log")).unwrap();
    assert!(
        log.contains("a refused this node (member-removed)"),
        "{log}"
    );
}

fn peer_status(node: &Node, peer: &str) -> Value {
    node.st_json(&["replication", "status"])["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|status| status["peer"] == peer)
        .cloned()
        .unwrap_or(Value::Null)
}

fn doctor_check(node: &Node, name: &str) -> Value {
    let doctor = node.st(&["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .cloned()
        .unwrap()
}

/// A member that stops answering, here frozen with SIGSTOP, must not stay `up` behind an old
/// measurement that hides what it has not received (#1020). Once it misses an exchange and an
/// attempt to reach it fails, the other member says last-seen with that failure, marks the
/// measurement stale with the envelopes written since it, and doctor warns.
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_member_is_not_reported_up() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.note("before").await;
    wait_for_notes(
        &a,
        &BTreeSet::from(["custom/fleet-test/before".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;
    wait_until("a measures b in sync", 60, || async {
        let peer = peer_status(&a, "b");
        peer["status"] == "up"
            && peer["sync"]["peer_only_envelopes"] == 0
            && peer["sync"]["local_only_envelopes"] == 0
    })
    .await;

    b.signal("STOP");
    for index in 0..20 {
        a.note(&format!("unsent-{index}")).await;
    }
    wait_until("a stops counting the frozen b as up", 120, || async {
        peer_status(&a, "b")["status"] == "last-seen"
    })
    .await;
    let peer = peer_status(&a, "b");
    assert!(
        peer["last_error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{peer}"
    );
    assert!(peer["last_failure_at_unix_ms"].is_u64(), "{peer}");
    assert_eq!(peer["sync"]["stale"], true, "{peer}");
    assert!(
        peer["sync"]["added_since_measured_envelopes"]
            .as_u64()
            .is_some_and(|added| added >= 20),
        "{peer}"
    );
    let text = a.st_ok(&["replication", "status"]);
    assert!(text.contains("peer\tb\tlast-seen"), "{text}");
    assert!(text.contains("last attempt failed"), "{text}");
    assert!(text.contains("stale: no exchange since"), "{text}");
    let check = doctor_check(&a, "replication");
    assert_eq!(check["status"], "warn", "{check}");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("b has not exchanged for"),
        "{check}"
    );
    let fleet = a.st_ok(&["fleet", "status"]);
    assert!(fleet.contains("PEER  b  last-seen"), "{fleet}");

    // Once b answers again, the next exchange clears the failure and measures afresh.
    b.signal("CONT");
    wait_until("a counts b as up again", 120, || async {
        let peer = peer_status(&a, "b");
        peer["status"] == "up"
            && peer["last_error"].is_null()
            && peer["sync"]["stale"] == false
            && peer["sync"]["local_only_envelopes"] == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn leave_drains_everything_before_it_leaves() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.wait_listening().await;
    a.stop();
    let mut expected = BTreeSet::new();
    // Written while the anchor is away, and more than an older peer's exchange carries (512),
    // so the anchor comes back catching up and defers projecting what it receives.
    for index in 0..700 {
        b.note(&format!("b-{index}")).await;
        expected.insert(format!("custom/fleet-test/b-{index}"));
    }
    a.start().await;
    a.wait_listening().await;
    b.st_ok(&[
        "fleet",
        "leave",
        "--no-service",
        "--wait",
        "2m",
        "--as",
        PERSON,
    ]);
    // Everything b wrote reached a before the leave, and the leave ended b.
    wait_for_notes(&a, &expected, 10, &[&a, &b]).await;
    let members = a.st_json(&["fleet", "status"])["view"]["members"].clone();
    assert!(
        members.as_array().unwrap().iter().any(|member| {
            member["name"] == "b" && member["state"] == "ended" && member["ended"] == "left"
        }),
        "{members}"
    );
    assert!(!b.state_dir().join("fleet").exists());
    assert!(b.state_dir().join("left-fleet.json").exists());
    // While leaving, b refused new writes; now it accepts them again, locally.
    b.note("local-after-leave").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejoining_under_a_new_name_reports_post_leave_history_as_divergent() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "orchard").await;
    let b = joined(root.path(), &a, "meadow", &[]).await;
    let mut c = joined(root.path(), &a, "grove", &[]).await;
    c.st_ok(&["fleet", "wait", "--timeout", "90s"]);
    c.note("before-leave").await;
    let before = BTreeSet::from(["custom/fleet-test/before-leave".to_owned()]);
    wait_for_notes(&a, &before, 60, &[&a, &b, &c]).await;
    wait_for_notes(&b, &before, 60, &[&a, &b, &c]).await;

    c.st_ok(&[
        "fleet",
        "leave",
        "--no-service",
        "--wait",
        "2m",
        "--as",
        PERSON,
    ]);
    c.restart().await;
    // The same local store accepts writes after leaving; its old incarnation has ended.
    c.note("after-leave-one").await;
    c.note("after-leave-two").await;
    c.stop();
    c.name = "grove-returned".into();
    fs::write(
        c.root.join("config/st3/config.toml"),
        format!("node = \"{}\"\nperson = \"{PERSON}\"\n", c.name),
    )
    .unwrap();
    let code = a.invite(&c.name, &[]);
    let joined = c.join(&code, &[]);
    assert!(
        joined.status.success(),
        "rejoin failed: {}{}",
        String::from_utf8_lossy(&joined.stdout),
        String::from_utf8_lossy(&joined.stderr)
    );
    c.start().await;
    wait_until(
        "both peers fence the old writer's post-leave envelopes",
        90,
        || async {
            a.st_json(&["replication", "status"])["fenced_envelopes"]
                .as_u64()
                .is_some_and(|count| count >= 2)
                && b.st_json(&["replication", "status"])["fenced_envelopes"]
                    .as_u64()
                    .is_some_and(|count| count >= 2)
        },
    )
    .await;
    // Equal envelope inventories do not imply equal admitted claims. The old local history
    // remains on the rejoining node; reporting must not delete it to manufacture equality.
    let after = BTreeSet::from([
        "custom/fleet-test/after-leave-one".to_owned(),
        "custom/fleet-test/after-leave-two".to_owned(),
    ]);
    assert!(after.is_subset(&c.notes().await));
    assert!(after.is_disjoint(&a.notes().await));
    assert!(after.is_disjoint(&b.notes().await));
    wait_until(
        "rejoining node compares the common inventory and settled projections",
        90,
        || async {
            let diff = c.st_json(&["replication", "diff", "orchard"]);
            diff["authority"]["equal"] == true
                && diff["status"] == "up"
                && diff["graph"]["equal"].is_boolean()
        },
    )
    .await;
    let diff = c.st_json(&["replication", "diff", "orchard"]);
    let doctor = c.st(&["--json", "doctor"]);
    let doctor: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let admission = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "fleet-admission")
        .unwrap();
    let waited = c.st(&["fleet", "wait", "--timeout", "5s"]);
    eprintln!(
        "rejoin receipt: {}",
        json!({
            "diff": diff,
            "admission": admission,
            "wait_success": waited.status.success(),
            "wait_stdout": String::from_utf8_lossy(&waited.stdout),
            "wait_stderr": String::from_utf8_lossy(&waited.stderr),
        })
    );
    assert_eq!(diff["graph"]["equal"], false, "{diff}");
    assert!(
        diff["differing_tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|table| table == "claim_sources"),
        "{diff}"
    );
    assert!(
        admission["message"]
            .as_str()
            .unwrap()
            .contains("admitted beyond high water"),
        "{admission}"
    );
    assert!(
        !waited.status.success(),
        "fleet wait accepted retained fenced history"
    );
    assert!(
        String::from_utf8_lossy(&waited.stderr).contains("projects a different graph"),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leave_already_refused_as_left_finishes_when_run_again() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.note("from-b").await;
    wait_for_notes(
        &a,
        &BTreeSet::from(["custom/fleet-test/from-b".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;

    // An earlier leave wrote its claim, and a admitted it before b heard back. From then on a
    // refuses b as left and never exchanges with it, so no digest of a's reaches b again.
    for step in ["begin", "claim"] {
        let _: Value = b
            .client()
            .post(
                &format!("/v1/internal/fleet/leave/{step}"),
                &json!({ "person": PERSON }),
            )
            .await
            .unwrap();
    }
    wait_until("a admits b's leave", 60, || async {
        a.st_json(&["fleet", "status"])["view"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| member["name"] == "b" && member["ended"] == "left")
    })
    .await;
    wait_until("a refuses b as left", 60, || async {
        fs::read_to_string(b.state_dir().join("fleet/fleet.toml"))
            .is_ok_and(|settings| settings.contains("member-left"))
    })
    .await;

    // Running the leave again finishes on that refusal and writes no second leave.
    let output = b.st_ok(&[
        "fleet",
        "leave",
        "--no-service",
        "--wait",
        "30s",
        "--as",
        PERSON,
    ]);
    assert!(output.contains("left\t"), "{output}");
    let record: Value =
        serde_json::from_str(&fs::read_to_string(b.state_dir().join("left-fleet.json")).unwrap())
            .unwrap();
    assert_eq!(record["confirmed_by"], "a");
    let leaves = b
        .claims()
        .await
        .into_iter()
        .filter(|claim| claim["subject"] == "host/b" && claim["kind"] == "fleet.member-left")
        .count();
    assert_eq!(leaves, 1, "the second leave wrote another leave claim");
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_leaves_nothing_behind() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut b = joined(root.path(), &a, "b", &[]).await;
    b.note("from-b").await;
    wait_for_notes(
        &a,
        &BTreeSet::from(["custom/fleet-test/from-b".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;

    // The dry run lists and removes nothing.
    let listed = b.st_ok(&["uninstall", "--dry-run", "--keep-binaries", "--no-service"]);
    assert!(listed.contains(&b.state_dir().display().to_string()));
    assert!(b.state_dir().exists());

    // With the daemon running, uninstall first leaves the fleet, then asks for the foreground
    // processes to stop, since no service manager stops them.
    let first = b.st(&[
        "uninstall",
        "--yes",
        "--keep-binaries",
        "--no-service",
        "--as",
        PERSON,
    ]);
    assert!(!first.status.success());
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("stop st3 up"),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    b.stop();
    b.st_ok(&[
        "uninstall",
        "--yes",
        "--keep-binaries",
        "--no-service",
        "--as",
        PERSON,
    ]);

    // Only what the test itself created remains: empty XDG roots, the stub pty, and the login
    // profiles that put it on PATH.
    let mut remaining = Vec::new();
    for entry in walkdir::WalkDir::new(&b.root) {
        let entry = entry.unwrap();
        let relative = entry.path().strip_prefix(&b.root).unwrap().to_path_buf();
        let expected = relative.as_os_str().is_empty()
            || [
                "home",
                "home/.profile",
                "home/.bash_profile",
                "home/.zprofile",
                "config",
                "state",
                "data",
                "run",
                "bin",
                "bin/pty",
            ]
            .iter()
            .any(|kept| relative == Path::new(kept))
            || relative.to_string_lossy().ends_with(".log");
        if !expected {
            remaining.push(relative);
        }
    }
    assert!(remaining.is_empty(), "uninstall left {remaining:?}");
    let members = a.st_json(&["fleet", "status"])["view"]["members"].clone();
    assert!(
        members.as_array().unwrap().iter().any(|member| {
            member["name"] == "b" && member["state"] == "ended" && member["ended"] == "left"
        }),
        "{members}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_invalid_join_requests_does_not_block_a_valid_join() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let code = a.invite("b", &[]);
    let url = format!("http://127.0.0.1:{}/v1/fleet/join", a.port);
    let http = reqwest::Client::new();
    let well_formed = |invite: String| {
        json!({
            "protocol": "st3-join-v1", "invite": invite, "name": "b", "mode": "listening",
            "member_key": "AAAA", "ephemeral": "AAAA", "build": "flood",
            "proof": "AAAA", "signature": "AAAA"
        })
    };
    for index in 0..40 {
        let response = if index % 2 == 0 {
            http.post(&url).body("not json").send().await.unwrap()
        } else {
            http.post(&url)
                .json(&well_formed(format!("{index:032x}")))
                .send()
                .await
                .unwrap()
        };
        assert_eq!(response.status().as_u16(), 403, "request {index}");
    }
    let b = Node::new(root.path(), "b");
    let output = b.join(&code, &[]);
    assert!(
        output.status.success(),
        "a valid join was blocked: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn down_observations(node: &Node, host: &str) -> usize {
    node.claims()
        .await
        .into_iter()
        .filter(|claim| {
            claim["subject"] == format!("host/{host}")
                && claim["kind"] == "transport.observed"
                && claim["body"]["fields"]["status"] == "down"
        })
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_peer_fleet_migrates_to_membership() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let fleet_id = "8f14e45f-ceea-467a-9a2b-5c3d6e7f8091";
    let secret = root.path().join("fleet.secret");
    fs::write(&secret, hex::encode([42_u8; 32])).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut a = Node::new(root.path(), "a");
    let mut b = Node::new(root.path(), "b");
    let mut l = Node::new(root.path(), "l");
    let dead = free_port();
    // The running fleet's shape: a lists b, and lists the laptop at a port nothing serves.
    a.legacy_config(fleet_id, &secret, &[("b", b.port), ("l", dead)]);
    // b names its secret relative to where its commands run.
    fs::copy(&secret, b.root.join("fleet.secret")).unwrap();
    b.legacy_config(fleet_id, Path::new("fleet.secret"), &[("a", a.port)]);
    l.legacy_config(fleet_id, &secret, &[("a", a.port)]);
    for node in [&mut a, &mut b, &mut l] {
        node.start().await;
    }
    a.note("a-0").await;
    b.note("b-0").await;
    l.note("l-0").await;
    let mut expected = BTreeSet::from([
        "custom/fleet-test/a-0".to_owned(),
        "custom/fleet-test/b-0".to_owned(),
        "custom/fleet-test/l-0".to_owned(),
    ]);
    for node in [&a, &b, &l] {
        wait_for_notes(node, &expected, 60, &[&a, &b, &l]).await;
    }
    let digest_before = a.st_json(&["replication", "status"])["fleet_id"].clone();

    // a becomes the anchor; config-peer exchanges keep working throughout.
    a.stop();
    a.migrate(&["--anchor"]);
    a.start().await;
    a.wait_listening().await;
    a.note("a-1").await;
    expected.insert("custom/fleet-test/a-1".into());
    wait_for_notes(&b, &expected, 60, &[&a, &b]).await;

    // b and the laptop migrate with codes from a.
    let code = a.invite("b", &["--migrate"]);
    b.stop();
    b.migrate(&[&code, "--via", &format!("http://127.0.0.1:{}", a.port)]);
    b.start().await;
    let code = a.invite("l", &["--migrate"]);
    l.stop();
    l.migrate(&["--dial-out", &code]);
    l.start().await;
    b.note("b-1").await;
    l.note("l-1").await;
    expected.insert("custom/fleet-test/b-1".into());
    expected.insert("custom/fleet-test/l-1".into());
    for node in [&a, &b, &l] {
        wait_for_notes(node, &expected, 60, &[&a, &b, &l]).await;
    }
    for node in [&a, &b] {
        let members = node.st_json(&["fleet", "status"])["view"]["members"].clone();
        for name in ["a", "b", "l"] {
            assert!(
                members
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|member| { member["name"] == name && member["state"] == "current" }),
                "{} does not see {name} as a member: {members}",
                node.name
            );
        }
        let status = node.st_json(&["replication", "status"]);
        assert_eq!(
            status["fleet_id"], digest_before,
            "the fleet binding changed"
        );
        assert_eq!(status["unsigned_envelopes"], 0, "{status}");
        assert_eq!(status["fenced_envelopes"], 0, "{status}");
    }

    // Finish everywhere, then replicate with member signatures only.
    let downs = down_observations(&a, "l").await;
    for node in [&a, &b, &l] {
        node.st_ok(&["fleet", "migrate", "--finish", "--no-service"]);
        // Do what --finish says: delete the config-peer lines from config.toml.
        fs::write(
            node.root.join("config/st3/config.toml"),
            format!("node = \"{}\"\nperson = \"{PERSON}\"\n", node.name),
        )
        .unwrap();
    }
    for node in [&mut a, &mut b, &mut l] {
        node.restart().await;
        assert!(
            node.worker
                .as_mut()
                .is_some_and(|worker| worker.try_wait().unwrap().is_none()),
            "{}'s replication worker did not start without the config-peer lines",
            node.name
        );
    }
    a.note("a-2").await;
    b.note("b-2").await;
    l.note("l-2").await;
    for text in ["a-2", "b-2", "l-2"] {
        expected.insert(format!("custom/fleet-test/{text}"));
    }
    for node in [&a, &b, &l] {
        wait_for_notes(node, &expected, 60, &[&a, &b, &l]).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        down_observations(&a, "l").await,
        downs,
        "a still dials the dial-out laptop at its dead config-peer port"
    );
    // A machine with the secret but no member key is refused once legacy exchanges end.
    let stranger = Node::new(root.path(), "stranger");
    stranger.legacy_config(fleet_id, &secret, &[("a", a.port)]);
    let mut stranger = stranger;
    stranger.start().await;
    stranger.note("from-stranger").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!a.notes().await.contains("custom/fleet-test/from-stranger"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leaving_member_writes_nothing_after_it_begins_to_leave() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.wait_listening().await;
    // b sponsors an invite, then begins to leave.
    let code = b.invite("c", &[]);
    let _: Value = b
        .client()
        .post("/v1/internal/fleet/leave/begin", &json!({"person": PERSON}))
        .await
        .unwrap();
    let before = b.claims().await.len();

    // Redemption, invites, endpoint announcements, and ordinary claims are all refused.
    let c = Node::new(root.path(), "c");
    assert!(
        !c.join(&code, &[]).status.success(),
        "b admitted c while leaving"
    );
    let invite = b.st(&["fleet", "invite", "d", "--code-only", "--as", PERSON]);
    assert!(!invite.status.success());
    let announced: Result<Value, _> = b
        .client()
        .post(
            "/v1/internal/fleet/endpoints",
            &json!({"mode": "listening", "endpoints": []}),
        )
        .await;
    let refusal = announced.unwrap_err().to_string();
    assert!(refusal.contains("leaving its fleet"), "{refusal}");
    let claim: Result<Value, _> = b
        .client()
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: "custom/fleet-test/while-leaving".into(),
                kind: NOTE.into(),
                actor: Some(PERSON.into()),
                fields: Default::default(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            },
        )
        .await;
    let refusal = claim.unwrap_err().to_string();
    assert!(refusal.contains("leaving its fleet"), "{refusal}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let local_writes = b
        .claims()
        .await
        .into_iter()
        .skip(before)
        .filter(|claim| claim["origin"] == "b")
        .count();
    assert_eq!(local_writes, 0, "b wrote while leaving");

    // Cancelling the leave restores writes.
    let _: Value = b
        .client()
        .post("/v1/internal/fleet/leave/cancel", &json!({}))
        .await
        .unwrap();
    b.note("after-cancel").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_switches_between_listening_and_dial_out() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut b = joined(root.path(), &a, "b", &[]).await;
    b.wait_listening().await;
    b.st_ok(&["fleet", "mode", "dial-out", "--no-service"]);
    b.restart().await;
    wait_until("a sees b as dial-out", 60, || async {
        a.st_json(&["fleet", "status"])["view"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| {
                member["name"] == "b"
                    && member["mode"] == "dial-out"
                    && member["endpoints"].as_array().is_some_and(Vec::is_empty)
            })
    })
    .await;
    b.note("while-dial-out").await;
    wait_for_notes(
        &a,
        &BTreeSet::from(["custom/fleet-test/while-dial-out".to_owned()]),
        60,
        &[&a, &b],
    )
    .await;
    let port = b.port.to_string();
    b.st_ok(&[
        "fleet",
        "mode",
        "listening",
        "--port",
        &port,
        "--no-service",
    ]);
    b.restart().await;
    b.wait_listening().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ST3_COMPAT_BIN: a real st3 build immediately before active_ask"]
async fn active_ask_is_additive_across_real_builds() {
    if st3::test_support::supervise_test() {
        return;
    }
    let old_binary = PathBuf::from(
        std::env::var("ST3_COMPAT_BIN").expect("ST3_COMPAT_BIN names the pre-active_ask st3"),
    );
    let root = tempfile::tempdir().unwrap();
    let secret = root.path().join("fleet.secret");
    fs::write(&secret, hex::encode([17_u8; 32])).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let fleet_id = "57041007-4350-4144-8370-948545399388";
    let mut old = Node::new(root.path(), "old");
    old.binary = old_binary;
    let mut new = Node::new(root.path(), "new");
    old.legacy_config(fleet_id, &secret, &[("new", new.port)]);
    new.legacy_config(fleet_id, &secret, &[("old", old.port)]);
    old.start().await;
    new.start().await;

    let publish = |subject: &str, fields| ClaimInput {
        subject: subject.into(),
        kind: "harness.observed".into(),
        actor: Some(subject.into()),
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    let legacy: Value = old
        .client()
        .post(
            "/v1/claims",
            &publish(
                "agent/old.fern",
                [("state".into(), json!("idle")), ("incarnation_id".into(), json!("old-incarnation"))]
                    .into_iter()
                    .collect(),
            ),
        )
        .await
        .unwrap();
    wait_until("new build accepts harness observation without active_ask", 60, || async {
        new.claims().await.iter().any(|claim| claim["id"] == legacy["id"])
    })
    .await;

    let prior: Value = new
        .client()
        .post(
            "/v1/claims",
            &publish(
                "agent/new.orchid",
                [
                    ("state".into(), json!("idle")),
                    ("incarnation_id".into(), json!("new-incarnation")),
                ]
                .into_iter()
                .collect(),
            ),
        )
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if old.claims().await.iter().any(|claim| claim["id"] == prior["id"]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "old build did not project prior observation {prior}; old status={}; {}",
            old.st_json(&["replication", "status"]),
            old.logs()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let current: Value = new
        .client()
        .post(
            "/v1/claims",
            &publish(
                "agent/new.orchid",
                [
                    ("state".into(), json!("blocked")),
                    ("incarnation_id".into(), json!("new-incarnation")),
                    ("active_ask".into(), json!("native-tool-call-1407")),
                ]
                .into_iter()
                .collect(),
            ),
        )
        .await
        .unwrap();
    old.note("after-active-ask").await;
    new.note("after-active-ask-new").await;
    let notes = BTreeSet::from([
        "custom/fleet-test/after-active-ask".into(),
        "custom/fleet-test/after-active-ask-new".into(),
    ]);
    for node in [&old, &new] {
        wait_for_notes(node, &notes, 60, &[&old, &new]).await;
    }
    wait_until("old build keeps the unknown active_ask claim retryable", 60, || async {
        old.st_json(&["replication", "status"])["unknown_records"]
            .as_u64()
            .unwrap_or(0)
            > 0
    })
    .await;
    let old_status = old.st_json(&["replication", "status"]);
    let new_status = new.st_json(&["replication", "status"]);
    assert_eq!(old_status["invalid_records"], 0, "{old_status}");
    assert_eq!(new_status["invalid_records"], 0, "{new_status}");
    assert!(old.claims().await.iter().any(|claim| claim["id"] == prior["id"]));
    assert!(!old.claims().await.iter().any(|claim| claim["id"] == current["id"]));
    assert!(new.claims().await.iter().any(|claim| claim["id"] == current["id"]));
    let connection = rusqlite::Connection::open_with_flags(
        old.state_dir().join("claims.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (state, error_code, raw): (String, String, Vec<u8>) = connection
        .query_row(
            "SELECT state, error_code, raw FROM replica_records WHERE claim_id = ?1",
            [current["id"].as_str().unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "unknown");
    assert_eq!(error_code, "unknown-claim-field");
    let identifier = b"native-tool-call-1407";
    assert!(raw.windows(identifier.len()).any(|window| window == identifier));
    println!(
        "active_ask mixed-build: old={} retained the entire new observation as unknown (prior idle observation remains projected), invalid_records=0; new={} accepted absence; later claims replicated both ways",
        old.binary.display(),
        new.binary.display()
    );
    drop(connection);
    old.stop();
    old.binary = new.binary.clone();
    old.start().await;
    wait_until("upgraded build admits the original active_ask observation", 60, || async {
        old.claims().await.iter().any(|claim| {
            claim["id"] == current["id"]
                && claim["body"]["fields"]["active_ask"] == "native-tool-call-1407"
        })
    })
    .await;
    let upgraded_status = old.st_json(&["replication", "status"]);
    assert_eq!(upgraded_status["invalid_records"], 0, "{upgraded_status}");
    assert_eq!(upgraded_status["unknown_records"], 0, "{upgraded_status}");
    println!("active_ask upgrade: original claim and native identifier projected; unknown_records=0; invalid_records=0");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ST3_COMPAT_BIN: the st3 of the pinned baseline release"]
async fn an_old_build_config_peer_replicates_with_new_members() {
    if st3::test_support::supervise_test() {
        return;
    }
    let old = PathBuf::from(
        std::env::var("ST3_COMPAT_BIN").expect("ST3_COMPAT_BIN names the baseline st3"),
    );
    assert!(
        !Command::new(&old)
            .args(["fleet", "--help"])
            .output()
            .unwrap()
            .status
            .success(),
        "the baseline must predate membership"
    );
    let root = tempfile::tempdir().unwrap();
    let fleet_id = "2a9d7c5e-1b3f-4e6a-8c0d-9e8f7a6b5c4d";
    let secret = root.path().join("fleet.secret");
    fs::write(&secret, hex::encode([7_u8; 32])).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut o = Node::new(root.path(), "o");
    o.binary = old;
    let mut n1 = Node::new(root.path(), "n1");
    let mut n2 = Node::new(root.path(), "n2");
    o.legacy_config(fleet_id, &secret, &[("n1", n1.port)]);
    n1.legacy_config(fleet_id, &secret, &[("o", o.port), ("n2", n2.port)]);
    n2.legacy_config(fleet_id, &secret, &[("n1", n1.port)]);
    for node in [&mut o, &mut n1, &mut n2] {
        node.start().await;
    }
    o.note("o-0").await;
    n2.note("n2-0").await;
    let mut expected = BTreeSet::from([
        "custom/fleet-test/o-0".to_owned(),
        "custom/fleet-test/n2-0".to_owned(),
    ]);
    for node in [&o, &n1, &n2] {
        wait_for_notes(node, &expected, 60, &[&o, &n1, &n2]).await;
    }

    // The baseline predates the backup CLI. Export a private SQLite snapshot of its graph
    // with the current envelope exporter, then restore the logical file at the current schema.
    // This is also run by the existing exact fleet-compat CI invocation.
    backup_baseline_graph(&o, root.path());

    // n1 and n2 move to membership while the old build keeps replicating with them.
    n1.stop();
    n1.migrate(&["--anchor"]);
    n1.start().await;
    n1.wait_listening().await;
    let code = n1.invite("n2", &["--migrate"]);
    n2.stop();
    n2.migrate(&[&code]);
    n2.start().await;
    // A newly joined member reaches the old build only through the members it can dial.
    let n4 = joined(root.path(), &n1, "n4", &[]).await;
    o.note("o-1").await;
    n2.note("n2-1").await;
    n4.note("n4-0").await;
    for text in ["o-1", "n2-1", "n4-0"] {
        expected.insert(format!("custom/fleet-test/{text}"));
    }
    for node in [&o, &n1, &n2, &n4] {
        wait_for_notes(node, &expected, 90, &[&o, &n1, &n2, &n4]).await;
    }
    // The old build keeps the fleet claims it cannot read as unknown, never invalid.
    let status = o.st_json(&["replication", "status"]);
    assert_eq!(status["invalid_records"], 0, "{status}");
    assert!(
        status["unknown_records"].as_u64().unwrap_or(0) > 0,
        "the old build admitted fleet claims it cannot know: {status}"
    );
    for node in [&n1, &n2, &n4] {
        let status = node.st_json(&["replication", "status"]);
        assert_eq!(status["unsigned_envelopes"], 0, "{}: {status}", node.name);
        assert_eq!(status["invalid_records"], 0, "{}: {status}", node.name);
    }
}

fn backup_baseline_graph(old: &Node, directory: &Path) {
    let original = old.state_dir().join("claims.sqlite3");
    let copy = directory.join("baseline-copy.db");
    let reader = rusqlite::Connection::open_with_flags(
        &original,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let old_schema: u32 = reader
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    reader
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    drop(reader);
    // Capture the baseline's unchanged sync payloads before current code opens or migrates it.
    let reader =
        rusqlite::Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let baseline_envelopes = reader
        .prepare(
            "SELECT writer,sequence,envelope_hash,previous_hash,accepted_at_unix_ms,payload
         FROM replica_envelopes WHERE batch_id IS NOT NULL ORDER BY writer,sequence,envelope_hash",
        )
        .unwrap()
        .query_map([], |row| {
            Ok(st3::model::ReplicaEnvelope {
                writer: row.get(0)?,
                sequence: row.get(1)?,
                hash: row.get(2)?,
                previous_hash: row.get(3)?,
                accepted_at_unix_ms: row.get::<_, String>(4)?.parse().unwrap(),
                payload: row.get(5)?,
                member_key: None,
                signature: None,
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    drop(reader);
    let archive = directory.join("baseline-claims.jsonl");
    let header = st3::backup::create_from_database(&copy, &archive).unwrap();
    let source = st3::store::Store::open(&copy, "backup-reader").unwrap();
    let target = directory.join("baseline-restored.db");
    let report = st3::backup::restore(&archive, &target).unwrap();
    let restored = st3::store::Store::open(&target, &report.writer).unwrap();
    assert!(old_schema <= header.source_schema);
    assert_eq!(report.graph_digest, header.graph_digest);
    assert!(report.projections_match);
    let restored_envelopes = restored
        .replica_envelopes(
            baseline_envelopes
                .iter()
                .map(|envelope| st3::model::ReplicaEnvelopeId {
                    writer: envelope.writer.clone(),
                    sequence: envelope.sequence,
                    hash: envelope.hash.clone(),
                })
                .collect(),
        )
        .unwrap();
    assert!(!baseline_envelopes.is_empty());
    assert_eq!(
        serde_json::to_value(restored_envelopes).unwrap(),
        serde_json::to_value(baseline_envelopes).unwrap()
    );
    assert_eq!(
        source.replication_inventory().unwrap().digest,
        restored.replication_inventory().unwrap().digest
    );
    assert_eq!(
        source.backup_header().unwrap().tables,
        restored.backup_header().unwrap().tables
    );
    assert!(
        !restored
            .claims_for("custom/fleet-test/o-0", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fleet_workflows_have_no_path_filter() {
    if st3::test_support::supervise_test() {
        return;
    }
    let workflows = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows");
    let compat = fs::read_to_string(workflows.join("fleet.yml")).unwrap();
    // The Linux gate runs the fleet compatibility stage from this script.
    let stages = fs::read_to_string(workflows.join("../../scripts/ci-linux")).unwrap();
    assert!(compat.contains("bash scripts/ci-linux"));
    assert!(stages.contains("fleet-compat"));
    assert!(compat.contains("pull_request:"));
    for filter in ["paths:", "paths-ignore:", "branches-ignore:"] {
        assert!(
            !compat.contains(filter),
            "fleet.yml filters its runs with {filter}"
        );
    }
    assert!(stages.contains("an_old_build_config_peer_replicates_with_new_members"));
    // The optional macOS job lives in macos.yml so label events cannot restart this gate.
    assert!(
        !compat.to_ascii_lowercase().contains("macos-"),
        "fleet.yml runs a macOS job"
    );
    let baseline: Value = serde_json::from_str(
        &fs::read_to_string(workflows.join("../fleet-compat-baseline.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(baseline["commit"].as_str().map(str::len), Some(40));
}

/// A stand-in for `fabric`: every node's shim shares one registry directory. `expose` records
/// a node's loopback listener under its protocol, `dial` prints the exposed address (all nodes
/// share this machine's loopback), and `send-file` drops a file into the peer's inbox.
fn fabric_shim(root: &Path, node: &str) -> PathBuf {
    let registry = root.join("fabric-registry");
    fs::create_dir_all(&registry).unwrap();
    let shim = root.join(format!("fabric-{node}"));
    fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
registry="{registry}"
me="{node}-fabric-id"
echo "$me $*" >> "$registry/calls"
key() {{ echo "$1.$(echo "$2" | tr '/' '_')"; }}
case "$1" in
  id) echo "$me" ;;
  addr) epoch=$(cat "$registry/epoch" 2>/dev/null || echo 0); echo "{{\"addrs\":[\"$epoch\"]}}" ;;
  expose) echo "$4" > "$registry/$(key "$me" "$2")" ;;
  unexpose) rm -f "$registry/$(key "$me" "$2")" ;;
  dial) f="$registry/$(key "$2" "$3")"; [ -f "$f" ] || {{ echo "no such exposure" >&2; exit 1; }}; cat "$f" ;;
  send-file) mkdir -p "$registry/home-$2/inbox/$me" && cp "$3" "$registry/home-$2/inbox/$me/$5" ;;
  probe) [ -f "$registry/$(key "$2" "$3")" ] ;;
  *) exit 1 ;;
esac
"#,
            registry = registry.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

/// Legacy helper ports must be unnecessary after membership advertises native Fabric routes.
#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_helper_fleet_migrates_to_native_fabric_without_losing_history() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let fleet_id = "8f14e45f-ceea-467a-9a2b-5c3d6e7f8091";
    let secret = root.path().join("fleet.secret");
    fs::write(&secret, hex::encode([42_u8; 32])).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut a = Node::new(root.path(), "a");
    let mut b = Node::new(root.path(), "b");
    let mut helpers = Vec::new();
    let mut helper_ports = Vec::new();
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for target in [a.port, b.port] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", free_port()))
            .await
            .unwrap();
        helper_ports.push(listener.local_addr().unwrap().port());
        let connections = connections.clone();
        helpers.push(tokio::spawn(async move {
            while let Ok((mut incoming, _)) = listener.accept().await {
                connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move {
                    if let Ok(mut outgoing) =
                        tokio::net::TcpStream::connect(("127.0.0.1", target)).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                    }
                });
            }
        }));
    }
    a.legacy_config(fleet_id, &secret, &[("b", helper_ports[1])]);
    b.legacy_config(fleet_id, &secret, &[("a", helper_ports[0])]);
    a.start().await;
    b.start().await;
    a.note("legacy-a").await;
    b.note("legacy-b").await;
    let mut expected = BTreeSet::from([
        "custom/fleet-test/legacy-a".to_owned(),
        "custom/fleet-test/legacy-b".to_owned(),
    ]);
    for node in [&a, &b] {
        wait_for_notes(node, &expected, 60, &[&a, &b]).await;
    }
    assert!(connections.load(std::sync::atomic::Ordering::Relaxed) > 0);

    let shim_a = fabric_shim(root.path(), "a");
    let shim_b = fabric_shim(root.path(), "b");
    let absent_tailscale = root.path().join("no-tailscale");
    a.stop();
    a.st_ok(&[
        "fleet",
        "migrate",
        "--anchor",
        "--no-service",
        "--transports",
        "fabric",
        "--fabric-protocol",
        "st3-peer-v1",
        "--fabric",
        shim_a.to_str().unwrap(),
        "--tailscale",
        absent_tailscale.to_str().unwrap(),
    ]);
    a.start().await;
    a.wait_listening().await;
    // The still-legacy member continues exchanging during the sequential upgrade.
    a.note("anchor-migrated").await;
    expected.insert("custom/fleet-test/anchor-migrated".into());
    wait_for_notes(&b, &expected, 60, &[&a, &b]).await;
    let code = a.st_ok(&[
        "fleet",
        "invite",
        "b",
        "--migrate",
        "--via",
        "fabric",
        "--code-only",
        "--as",
        PERSON,
    ]);
    b.stop();
    b.st_ok(&[
        "fleet",
        "migrate",
        code.trim(),
        "--via",
        "fabric://a-fabric-id/st3-peer-v1",
        "--no-service",
        "--transports",
        "fabric",
        "--fabric-protocol",
        "st3-peer-v1",
        "--fabric",
        shim_b.to_str().unwrap(),
        "--tailscale",
        absent_tailscale.to_str().unwrap(),
    ]);
    let file = st3::config::FleetFile::load(&b.state_dir())
        .unwrap()
        .unwrap();
    assert_eq!(file.fleet_id, fleet_id);
    assert_eq!(file.sponsor_routes, ["fabric://a-fabric-id/st3-peer-v1"]);
    assert_eq!(file.fabric_protocol.as_deref(), Some("st3-peer-v1"));
    assert_eq!(
        fs::read(&secret).unwrap(),
        hex::encode([42_u8; 32]).as_bytes()
    );
    b.start().await;
    b.wait_listening().await;
    b.note("member-migrated").await;
    expected.insert("custom/fleet-test/member-migrated".into());
    for node in [&a, &b] {
        wait_for_notes(node, &expected, 60, &[&a, &b]).await;
        let status = node.st_json(&["replication", "status"]);
        assert_eq!(status["unsigned_envelopes"], 0, "{status}");
        assert_eq!(status["fenced_envelopes"], 0, "{status}");
        node.st_ok(&["fleet", "migrate", "--finish", "--no-service"]);
        fs::write(
            node.root.join("config/st3/config.toml"),
            format!("node = \"{}\"\nperson = \"{PERSON}\"\n", node.name),
        )
        .unwrap();
    }
    // Retire every helper and restart every member with no legacy fields or peer arguments.
    for helper in helpers {
        helper.abort();
        let _ = helper.await;
    }
    for port in helper_ports {
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
        );
    }
    a.restart().await;
    b.restart().await;
    for node in [&a, &b] {
        node.note(&format!("native-{}", node.name)).await;
        expected.insert(format!("custom/fleet-test/native-{}", node.name));
    }
    for node in [&a, &b] {
        wait_for_notes(node, &expected, 60, &[&a, &b]).await;
        let file = st3::config::FleetFile::load(&node.state_dir())
            .unwrap()
            .unwrap();
        assert!(!file.legacy_peers);
        assert_eq!(file.fleet_id, fleet_id);
    }
    // Lose the Fabric declarations as on a daemon restart: the workers repair them themselves.
    let registry = root.path().join("fabric-registry");
    for name in ["a", "b"] {
        fs::remove_file(registry.join(format!("{name}-fabric-id.st3-peer-v1"))).unwrap();
    }
    fs::write(registry.join("epoch"), "1").unwrap();
    for node in [&a, &b] {
        node.note(&format!("recovered-{}", node.name)).await;
        expected.insert(format!("custom/fleet-test/recovered-{}", node.name));
    }
    for node in [&a, &b] {
        wait_for_notes(node, &expected, 10, &[&a, &b]).await;
    }
    let calls = fs::read_to_string(registry.join("calls")).unwrap();
    assert!(
        calls.contains("dial a-fabric-id st3-peer-v1")
            || calls.contains("dial b-fabric-id st3-peer-v1")
    );
    assert!(!calls.contains("--ephemeral"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fabric_transport_works_through_the_worker_alone() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("fabric-registry");
    let no_tailscale = root.path().join("no-tailscale");
    let mut a = Node::new(root.path(), "a");
    let shim_a = fabric_shim(root.path(), "a");
    let port = a.port.to_string();
    a.st_ok(&[
        "fleet",
        "create",
        "--no-service",
        "--name",
        "a",
        "--port",
        &port,
        "--transports",
        "fabric",
        "--fabric",
        shim_a.to_str().unwrap(),
        "--tailscale",
        no_tailscale.to_str().unwrap(),
    ]);
    a.start().await;
    a.wait_listening().await;
    a.note("over-fabric-a").await;

    // The code travels as a Fabric file and never appears in an argument list.
    a.st_ok(&[
        "fleet",
        "invite",
        "b",
        "--via",
        "fabric",
        "--send-fabric",
        "--as",
        PERSON,
    ]);
    let mut b = Node::new(root.path(), "b");
    b.env.push((
        "FABRIC_HOME".into(),
        registry.join("home-b").display().to_string(),
    ));
    let shim_b = fabric_shim(root.path(), "b");
    let port = b.port.to_string();
    let file = st3::config::FleetFile::load(&a.state_dir())
        .unwrap()
        .unwrap();
    let route = format!(
        "fabric://a-fabric-id/{}",
        st3::fleet::transport::default_fabric_protocol(&file.fleet_id)
    );
    b.st_ok(&[
        "fleet",
        "join",
        "--via",
        &route,
        "--fabric-inbox",
        "--no-service",
        "--name",
        "b",
        "--port",
        &port,
        "--transports",
        "fabric",
        "--fabric",
        shim_b.to_str().unwrap(),
        "--tailscale",
        no_tailscale.to_str().unwrap(),
    ]);
    b.start().await;
    b.note("over-fabric-b").await;
    let expected = BTreeSet::from([
        "custom/fleet-test/over-fabric-a".to_owned(),
        "custom/fleet-test/over-fabric-b".to_owned(),
    ]);
    wait_for_notes(&a, &expected, 60, &[&a, &b]).await;
    wait_for_notes(&b, &expected, 60, &[&a, &b]).await;

    // The shim drops live exposure mappings and changes its endpoint on restart. The running
    // workers notice local connectivity, re-expose and dial again without a helper.
    for entry in fs::read_dir(&registry).unwrap().flatten() {
        if entry.file_name().to_string_lossy().contains(".st3_fleet_") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    fs::write(registry.join("epoch"), "1").unwrap();
    a.note("after-fabric-restart-a").await;
    b.note("after-fabric-restart-b").await;
    let recovered = expected
        .iter()
        .cloned()
        .chain([
            "custom/fleet-test/after-fabric-restart-a".to_owned(),
            "custom/fleet-test/after-fabric-restart-b".to_owned(),
        ])
        .collect();
    for node in [&a, &b] {
        wait_for_notes(node, &recovered, 10, &[&a, &b]).await;
    }

    let calls = fs::read_to_string(registry.join("calls")).unwrap();
    for node in ["a", "b"] {
        assert!(
            calls.contains(&format!("{node}-fabric-id expose st3/fleet/"))
                && !calls.contains("--ephemeral"),
            "{node} did not persist its exposure:\n{calls}"
        );
    }
    // An exchange synchronizes both directions, so the first successful dial can
    // drain both notes before the other worker has any reason to dial.
    assert!(
        calls.contains("a-fabric-id dial b-fabric-id st3/fleet/")
            || calls.contains("b-fabric-id dial a-fabric-id st3/fleet/"),
        "neither node dialed through Fabric:\n{calls}"
    );
    let inbox = registry.join("home-b/inbox");
    assert!(
        walkdir::WalkDir::new(&inbox)
            .into_iter()
            .flatten()
            .all(|entry| !entry.file_type().is_file()),
        "join left the code in the Fabric inbox"
    );
    let code_arguments = [&a, &b]
        .iter()
        .flat_map(|node| node.arguments.lock().unwrap().clone())
        .flatten()
        .chain(calls.split_whitespace().map(str::to_owned))
        .filter(|argument| argument.starts_with("stj1-"))
        .count();
    assert_eq!(
        code_arguments, 0,
        "a join code appeared in an argument list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_members_writes_relayed_by_an_uninformed_member_are_refused() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "a").await;
    let mut c = joined(root.path(), &a, "c", &[]).await;
    c.wait_listening().await;
    let mut r = joined(root.path(), &a, "r", &[]).await;
    r.wait_listening().await;
    wait_until("c learns r's listening endpoint", 60, || async {
        c.st_json(&["fleet", "status"])["view"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| {
                member["name"] == "r"
                    && member["endpoints"]
                        .as_array()
                        .is_some_and(|endpoints| !endpoints.is_empty())
            })
    })
    .await;
    r.note("r-before").await;
    let before = BTreeSet::from(["custom/fleet-test/r-before".to_owned()]);
    wait_for_notes(&a, &before, 60, &[&a, &c, &r]).await;
    wait_for_notes(&c, &before, 60, &[&a, &c, &r]).await;

    // a removes r while c is away, then goes away itself before r can hear of it.
    c.stop();
    r.stop();
    a.st_ok(&["fleet", "remove", "r", "--reason", "lost", "--as", PERSON]);
    a.stop();

    // r writes on and relays through c, which has not heard of the removal.
    c.start().await;
    r.start().await;
    r.note("r-after").await;
    let after = BTreeSet::from(["custom/fleet-test/r-after".to_owned()]);
    wait_for_notes(&c, &after, 60, &[&c, &r]).await;

    // a returns: c relays r's late envelope to it, and a refuses it.
    a.start().await;
    wait_until("c hears of the removal", 60, || async {
        c.st_json(&["fleet", "status"])["view"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| member["name"] == "r" && member["state"] == "ended")
    })
    .await;
    wait_until("a holds r's late envelope as fenced", 60, || async {
        a.st_json(&["replication", "status"])["fenced_envelopes"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    })
    .await;
    assert!(
        !a.notes().await.contains("custom/fleet-test/r-after"),
        "a admitted a removed member's write relayed through c"
    );
    // c admitted it before it knew; doctor says so.
    let doctor = c.st(&["--json", "doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        report.contains("beyond high water"),
        "c's doctor does not report what it admitted from r: {report}"
    );
    // From now on c refuses r too.
    r.note("r-later").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!c.notes().await.contains("custom/fleet-test/r-later"));
}

/// No member classification: the traveller has the same legacy configuration as its peers,
/// but its network accepts no inbound connection. Two servers keep syncing while it sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_only_member_returns_after_minutes_and_aged_hours_without_alerts() {
    if st3::test_support::supervise_test() {
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let fleet_id = "8a7c55c0-e0d4-41cb-8e8c-6ab134611650";
    let secret = root.path().join("fleet.secret");
    fs::write(&secret, hex::encode([23_u8; 32])).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut a = Node::new(root.path(), "harbor");
    let mut b = Node::new(root.path(), "beacon");
    let mut traveller = Node::new(root.path(), "traveller");
    // A closed inbound service counts attempted connections without contacting a real host.
    let rejected = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rejected_port = rejected.local_addr().unwrap().port();
    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let reject_task = tokio::spawn(async move {
        while let Ok((connection, _)) = rejected.accept().await {
            count.fetch_add(1, Ordering::Relaxed);
            drop(connection);
        }
    });
    a.legacy_config(
        fleet_id,
        &secret,
        &[("beacon", b.port), ("traveller", rejected_port)],
    );
    b.legacy_config(
        fleet_id,
        &secret,
        &[("harbor", a.port), ("traveller", rejected_port)],
    );
    traveller.legacy_config(fleet_id, &secret, &[("harbor", a.port), ("beacon", b.port)]);
    let path = traveller.root.join("config/st3/config.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        config
            .lines()
            .filter(|line| !line.starts_with("peer_listen"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    for node in [&mut a, &mut b, &mut traveller] {
        node.start().await;
    }
    let mut expected = BTreeSet::new();
    for node in [&a, &b, &traveller] {
        node.note(&format!("{}-initial", node.name)).await;
        expected.insert(format!("custom/fleet-test/{}-initial", node.name));
    }
    for node in [&a, &b, &traveller] {
        wait_for_notes(node, &expected, 10, &[&a, &b, &traveller]).await;
    }
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", traveller.port))
            .await
            .is_err()
    );

    for (label, sleep, age_ms) in [("minutes", 120, 120_000), ("hours", 2, 4 * 3_600_000)] {
        // Leave its local daemon alive so it can record work while the network worker sleeps.
        let mut worker = traveller.worker.take().unwrap();
        worker.kill().unwrap();
        worker.wait().unwrap();
        let before = attempts.load(Ordering::Relaxed);
        traveller.note(&format!("traveller-{label}")).await;
        expected.insert(format!("custom/fleet-test/traveller-{label}"));
        a.note(&format!("harbor-{label}")).await;
        expected.insert(format!("custom/fleet-test/harbor-{label}"));
        b.note(&format!("beacon-{label}")).await;
        expected.insert(format!("custom/fleet-test/beacon-{label}"));
        tokio::time::sleep(Duration::from_secs(sleep)).await;
        // The minutes case uses real elapsed time. The hours case ages persistent evidence;
        // virtual-clock worker tests separately exercise the full four-hour retry schedule.
        for node in [&a, &b] {
            let db = rusqlite::Connection::open(node.state_dir().join("claims.sqlite3")).unwrap();
            db.busy_timeout(Duration::from_secs(5)).unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            db.execute(
                "UPDATE replication_peers SET last_success_at_unix_ms=?1 WHERE peer='traveller'",
                [(now - age_ms).to_string()],
            )
            .unwrap();
            let status = node.st_json(&["replication", "status"]);
            let peer = status["peers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|peer| peer["peer"] == "traveller")
                .unwrap();
            assert_eq!(peer["status"], "last-seen", "{status}");
            assert!(peer["last_success_at_unix_ms"].is_number());
            // Doctor says the traveller is away, without warning: it is not a listening member.
            let doctor = node.st_json(&["doctor"]);
            let check = doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["name"] == "replication")
                .unwrap();
            assert_eq!(check["status"], "pass", "{doctor}");
            assert!(
                check["message"]
                    .as_str()
                    .unwrap()
                    .contains("traveller has not exchanged for"),
                "{doctor}"
            );
            let machines = node.st_json(&["machines"]);
            let machine = machines["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|machine| machine["host_id"] == "host/traveller")
                .unwrap();
            assert_eq!(machine["state"], "last-seen", "{machines}");
        }
        let absent_attempts = attempts.load(Ordering::Relaxed) - before;
        eprintln!("{absent_attempts} connection attempts during {label} absence");
        if absent_attempts > 20 {
            for node in [&a, &b] {
                eprintln!(
                    "{} after {label}: {}",
                    node.name,
                    node.st_json(&["replication", "status"])
                );
            }
        }
        assert!(
            absent_attempts <= 20,
            "absence did not back off: {absent_attempts} attempts during {label}"
        );
        let always_on_notes = expected
            .iter()
            .filter(|note| !note.ends_with(&format!("traveller-{label}")))
            .cloned()
            .collect();
        for node in [&a, &b] {
            wait_for_notes(node, &always_on_notes, 10, &[&a, &b]).await;
        }
        // The returning member changes its local address and retains outbound-only networking.
        traveller.port = free_port();
        let returned = Instant::now();
        traveller.worker = Some(
            traveller
                .command(&["replication-worker"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        for node in [&a, &b, &traveller] {
            wait_for_notes(node, &expected, 10, &[&a, &b, &traveller]).await;
        }
        assert!(returned.elapsed() < Duration::from_secs(10));
        for node in [&a, &b] {
            assert_eq!(down_observations(node, "traveller").await, 0);
            let attention = node.st_json(&["attention", "ls"]);
            assert!(
                attention["value"]["items"]
                    .as_array()
                    .is_some_and(|items| items.is_empty()),
                "{attention}"
            );
        }
    }
    reject_task.abort();
}

/// A member that returns more than two pages behind catches up page after page (#1019).
/// Both members have just exchanged in the other direction, so each finished page must not
/// leave the rest to the 30-second quiet window that follows the peer's last inbound exchange.
#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_of_several_pages_is_fetched_without_waiting_for_the_quiet_window() {
    if st3::test_support::supervise_test() {
        return;
    }
    // Pages of 100 envelopes make a backlog of several pages cheap to write and to admit.
    const SETTINGS: [(&str, &str); 2] = [
        ("ST3_WORKER_INTERVAL_MS", "30000"),
        ("ST3_REPLICATION_PAGE_LIMIT", "100"),
    ];
    let root = tempfile::tempdir().unwrap();
    let mut a = Node::new(root.path(), "a");
    a.env
        .extend(SETTINGS.map(|(key, value)| (key.to_owned(), value.to_owned())));
    a.create();
    a.start().await;
    a.wait_listening().await;
    let mut b = Node::new(root.path(), "b");
    b.env
        .extend(SETTINGS.map(|(key, value)| (key.to_owned(), value.to_owned())));
    let code = a.invite("b", &[]);
    assert!(b.join(&code, &[]).status.success());
    b.start().await;
    b.st_ok(&["fleet", "wait", "--timeout", "120s"]);

    b.stop();
    let mut writers = Vec::new();
    for writer in 0..4 {
        let client = a.client();
        writers.push(tokio::spawn(async move {
            for index in 0..150 {
                let _: Value = client
                    .post(
                        "/v1/claims",
                        &ClaimInput {
                            subject: format!("custom/fleet-test/backlog-{writer}-{index}"),
                            kind: NOTE.into(),
                            actor: Some(PERSON.into()),
                            fields: Default::default(),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: None,
                        },
                    )
                    .await
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    // b writes a few envelopes of its own when it starts, so it holds at least a's.
    let target = a.st_json(&["replication", "status"])["received_envelopes"]
        .as_u64()
        .unwrap();
    b.start().await;
    let started = Instant::now();
    loop {
        let status = b.st_json(&["replication", "status"]);
        if status["received_envelopes"].as_u64().unwrap() >= target {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "b holds {} of a's {target} envelopes {:?} after it returned\n{}\n{}",
            status["received_envelopes"],
            started.elapsed(),
            a.logs(),
            b.logs()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!("caught up {:?} after returning", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn action_coverage_fleet_controls_and_checkpoint_administration_survive_restarts() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut amber = anchor(root.path(), "fixture-amber").await;
    let mut cobalt = joined(root.path(), &amber, "fixture-cobalt", &[]).await;
    amber.st_ok(&[
        "claim",
        "custom/fleet-test/coverage-before-restart",
        NOTE,
        "--actor",
        PERSON,
        "--field",
        "text=durable",
    ]);
    cobalt.st_ok(&["fleet", "wait", "--timeout", "60s"]);
    amber.restart().await;
    cobalt.restart().await;
    for node in [&amber, &cobalt] {
        node.wait_listening().await;
        if node.name != "fixture-amber" {
            node.st_ok(&["fleet", "wait", "--timeout", "60s"]);
        }
        node.st_json(&["fleet", "status"]);
        node.st_json(&["replication", "status"]);
        node.st_json(&["replication", "invalid"]);
    }
    amber.st_json(&["replication", "diff", "fixture-cobalt"]);
    cobalt.st_ok(&["fleet", "mode", "dial-out", "--no-service"]);
    cobalt.restart().await;
    assert!(
        fs::read_to_string(cobalt.state_dir().join("fleet/fleet.toml"))
            .unwrap()
            .contains("dial-out")
    );
    let port = cobalt.port.to_string();
    cobalt.st_ok(&[
        "fleet",
        "mode",
        "listening",
        "--port",
        &port,
        "--no-service",
    ]);
    cobalt.restart().await;
    cobalt.wait_listening().await;
    let code = amber.invite("fixture-revoked", &[]);
    let invitations = amber.st_json(&["fleet", "invites"]);
    let id = invitations
        .as_array()
        .unwrap()
        .iter()
        .find(|invite| invite["name"] == "fixture-revoked")
        .unwrap()["invite"]
        .as_str()
        .unwrap();
    amber.st_ok(&[
        "fleet",
        "invites",
        "revoke",
        id,
        "--as",
        PERSON,
        "--reason",
        "The fixture invitation is withdrawn.",
    ]);
    for _ in 0..2 {
        amber.restart().await;
        amber.wait_listening().await;
        let refused = Node::new(root.path(), "fixture-revoked").join(&code, &[]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("refused this code"));
    }
    amber.st_json(&["replication", "checkpoint", "plan", "--cut", "2026-10-01"]);
    amber.st_json(&[
        "replication",
        "checkpoint",
        "excuse",
        "fixture-cobalt",
        "--as",
        PERSON,
        "--reason",
        "The fixture writer may be offline.",
    ]);
    amber.restart().await;
    amber.st_json(&["replication", "checkpoint", "status"]);
    amber.st_json(&[
        "replication",
        "checkpoint",
        "resume",
        "--as",
        PERSON,
        "--reason",
        "The fixture projection was checked.",
    ]);
    amber.restart().await;
    amber.wait_listening().await;
    cobalt.st_ok(&[
        "fleet",
        "leave",
        "--no-service",
        "--wait",
        "60s",
        "--as",
        PERSON,
    ]);
    cobalt.restart().await;
    assert!(!cobalt.state_dir().join("fleet").exists());
    let mut jade = joined(root.path(), &amber, "fixture-jade", &[]).await;
    jade.st_ok(&["fleet", "wait", "--timeout", "60s"]);
    amber.st_ok(&[
        "fleet",
        "remove",
        "fixture-jade",
        "--as",
        PERSON,
        "--reason",
        "The fixture member is removed.",
    ]);
    amber.restart().await;
    wait_until(
        "jade records its removal after the sponsor restarted",
        60,
        || async {
            fs::read_to_string(jade.state_dir().join("fleet/fleet.toml"))
                .is_ok_and(|file| file.contains("[removed]"))
        },
    )
    .await;
    jade.restart().await;
    assert_eq!(
        jade.st_json(&["fleet", "status"])["removed"]["code"],
        "member-removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn action_coverage_github_watch_cli_uses_private_http_and_survives_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", http.local_addr().unwrap());
    let posts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let posted_bodies = posts.clone();
    let app = axum::Router::new().fallback(
        axum::routing::get(|uri: axum::http::Uri| async move {
            let issue = json!({"number": 12, "state": "open", "title": "Copper proof", "html_url": "https://example.org/copper", "updated_at": "2026-10-01T00:00:00Z", "user": {"login": "fixture-author"}});
            let path = uri.path();
            axum::Json(if path.ends_with("/issues/12") {
                issue
            } else if path.ends_with("/issues") {
                json!([issue])
            } else if path == "/user" {
                json!({"login": "fixture-bot"})
            } else if path.ends_with("/issues/comments/801") {
                json!({"id": 801, "html_url": "https://github.com/fixture/app/issues/12#issuecomment-801", "user": {"login": "fixture-bot"}})
            } else if path.ends_with("/pulls/12/reviews/802") {
                json!({"id": 802, "html_url": "https://github.com/fixture/app/pull/12#pullrequestreview-802", "user": {"login": "fixture-bot"}})
            } else if path.ends_with("/issues/comments/902") {
                json!({"id": 902, "user": {"login": "fixture-other"}})
            } else {
                json!([])
            })
        }).post(move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
            let posts = posted_bodies.clone();
            async move {
                posts.lock().unwrap().push(body);
                axum::Json(if uri.path().ends_with("/reviews") {
                    json!({"id": 802, "html_url": "https://github.com/fixture/app/pull/12#pullrequestreview-802", "user": {"login": "fixture-bot"}})
                } else {
                    json!({"id": 801, "html_url": "https://github.com/fixture/app/issues/12#issuecomment-801", "user": {"login": "fixture-bot"}})
                })
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(http, app).await.unwrap(); });
    let mut node = Node::new(root.path(), "fixture-watch");
    node.env.push(("GH_TOKEN".into(), "fixture-token".into()));
    node.env.push(("ST3_GITHUB_API_URL".into(), api));
    node.start().await;
    let file = node.root.join("seat.kdl");
    fs::write(&file, format!("version 2\nagent \"example/watch\" {{ host \"fixture-watch\"; workspace {:?}; command \"true\"; restart \"never\" }}\n", node.root)).unwrap();
    node.st_ok(&["agents", "apply", file.to_str().unwrap(), "--as", PERSON]);
    let cli = |args: &[&str]| -> Value {
        let output = node.command(args).env("ST_AGENT", "agent/example/watch").output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stderr), node.logs());
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let first = cli(&["--json", "gh", "watch", "fixture/app#12", "--until", "1h", "--as", "agent/example/watch"]);
    let subject = first["subject"].as_str().unwrap().to_owned();
    assert_eq!(first["state"], "active");
    let refused = node.command(&["--json", "gh", "comment", "fixture/app#12", "--body", "Foreign actor", "--as", "agent/example/other"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("agent/example/watch"));
    assert!(posts.lock().unwrap().is_empty());
    let posted = cli(&["--json", "gh", "comment", "fixture/app#12", "--body", "Copper proof", "--as", "agent/example/watch"]);
    assert_eq!(posted["kind"], "comment");
    assert_eq!(posted["id"], 801);
    assert_eq!(posted["watch"]["subject"], subject);
    node.restart().await;
    let output = node.command(&["--json", "gh", "ls", "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().iter().filter(|watch| watch["subject"] == subject).count(), 1);
    let foreign_url = "https://github.com/fixture/app/issues/12#issuecomment-902";
    let refused = node.command(&["--json", "gh", "own", foreign_url, "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("github-post-not-ours"));
    node.restart().await;
    for _ in 0..2 {
        let output = node.command(&["--json", "gh", "own", "https://github.com/fixture/app/issues/12#issuecomment-801", "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let owned: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(owned["id"], 801);
        assert_eq!(owned["agent"], "agent/example/watch");
        node.restart().await;
    }
    let review_file = node.root.join("review.txt");
    fs::write(&review_file, "The Copper proof is ready.").unwrap();
    let output = node.command(&["--json", "gh", "comment", "fixture/app#12", "--body-file", review_file.to_str().unwrap(), "--review", "approve", "--no-watch", "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let review: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(review["kind"], "review");
    assert_eq!(review["id"], 802);
    node.restart().await;
    let output = node.command(&["--json", "gh", "own", "https://github.com/fixture/app/pull/12#pullrequestreview-802", "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let store = st3::store::Store::open(&node.state_dir().join("claims.sqlite3"), "fixture-watch").unwrap();
    for (kind, id) in [("comment", 801), ("review", 802)] {
        let subject = st3::github_watch::github_post_subject("fixture/app", kind, id);
        assert_eq!(store.claims_for(&subject, Some("github.posted")).unwrap().len(), 1);
        assert_eq!(store.github_post_agent("fixture/app", kind, id).unwrap().as_deref(), Some("agent/example/watch"));
    }
    assert_eq!(store.github_post_agent("fixture/app", "comment", 902).unwrap(), None);
    drop(store);
    {
        let bodies = posts.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["body"], "Copper proof");
        assert_eq!(bodies[1]["body"], "The Copper proof is ready.");
        assert_eq!(bodies[1]["event"], "APPROVE");
    }
    for _ in 0..2 {
        let output = node.command(&["--json", "gh", "unwatch", "fixture/app#12", "--as", "agent/example/watch"]).env("ST_AGENT", "agent/example/watch").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        node.restart().await;
    }
    let listed = node.st_json(&["gh", "ls", "--all", "--as", PERSON]);
    let ended = listed.as_array().unwrap().iter().find(|watch| watch["subject"] == subject).unwrap();
    assert_eq!(ended["state"], "ended");
    assert_eq!(ended["ended"], "unwatched");
    server.abort();
}

/// Real native drivers on two private fleet daemons: the source never launches after suspend,
/// and the target launches only after receiving the Git workspace and the same native transcript.
#[tokio::test(flavor = "multi_thread")]
async fn suspended_seat_moves_between_two_daemons_with_its_workspace_and_conversation() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    struct SeatCleanup {
        pty: PathBuf,
        roots: Vec<PathBuf>,
    }
    impl Drop for SeatCleanup {
        fn drop(&mut self) {
            for root in &self.roots {
                let output = Command::new(&self.pty)
                    .args(["list", "--json"])
                    .env("PTY_ROOT", root)
                    .output();
                if let Ok(output) = output
                    && let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&output.stdout)
                {
                    for row in rows {
                        if let Some(name) = row["name"].as_str() {
                            let _ = Command::new(&self.pty)
                                .args(["kill", name])
                                .env("PTY_ROOT", root)
                                .output();
                        }
                    }
                }
            }
        }
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pty = st_runtime::resolve_executable("pty", &std::env::vars().collect()).unwrap();
    let node_program = st_runtime::resolve_executable("node", &std::env::vars().collect()).unwrap();
    let mut cleanup = SeatCleanup { pty: pty.clone(), roots: Vec::new() };
    let mut amber = anchor(root.path(), "fixture-move-amber").await;
    let mut jade = joined(root.path(), &amber, "fixture-move-jade", &[]).await;
    cleanup.roots = vec![amber.state_dir().join("pty"), jade.state_dir().join("pty")];
    for node in [&mut amber, &mut jade] {
        node.stop();
        fs::write(
            node.root.join("bin/pty"),
            format!("#!/bin/sh\nexec '{}' \"$@\"\n", pty.display()),
        )
        .unwrap();
        let stub = node.root.join("bin/omp");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nexec env STUB_HARNESS=omp '{}' '{}' \"$@\"\n",
                node_program.display(),
                repo.join("scripts/st3-boot-canaries/stub-pi-family.mjs")
                    .display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        node.start().await;
    }
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Canary")
            .env("GIT_AUTHOR_EMAIL", "canary@example.invalid")
            .env("GIT_COMMITTER_NAME", "Canary")
            .env("GIT_COMMITTER_EMAIL", "canary@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    fs::write(workspace.join("tracked"), "original\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "Original"]);
    let head = git(&["rev-parse", "HEAD"]);
    fs::write(workspace.join("tracked"), "changed\n").unwrap();
    fs::write(workspace.join("untracked"), "copper workspace\n").unwrap();
    let seat = "agent/canary/portable";
    let declaration = root.path().join("seat.kdl");
    fs::write(&declaration, format!("version 2\nagent \"canary/portable\" {{ host \"fixture-move-amber\"; workspace \"{}\"; harness \"omp\" {{}} }}", workspace.display())).unwrap();
    amber.st_ok(&[
        "agents",
        "apply",
        declaration.to_str().unwrap(),
        "--as",
        PERSON,
    ]);
    let receipts = workspace.join("receipts-agent-canary-portable.jsonl");
    wait_until("source native session becomes idle", 90, || async {
        amber.st_json(&["agents", "show", seat])["value"]["harness_state"] == "idle"
    })
    .await;
    amber.st_ok(&[
        "conversations",
        "send",
        seat,
        "--from",
        PERSON,
        "--subject",
        "copper history",
        "--body",
        "Remember the copper orchard.",
    ]);
    wait_until("source reads the first message", 60, || async {
        fs::read_to_string(&receipts)
            .unwrap_or_default()
            .contains("Remember the copper orchard")
    })
    .await;
    let suspended = amber.st_json(&[
        "agents",
        "suspend",
        seat,
        "--as",
        PERSON,
        "--timeout",
        "60s",
    ]);
    let native = suspended["suspension"]["native_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let source_sessions =
        st3::seat_snapshot::sessions(&amber.state_dir().join("drivers"), seat, "omp");
    let transcript = st3::native_resume::pi_family_transcript(&source_sessions, &native).unwrap();
    let before = fs::read(&transcript).unwrap();
    assert!(String::from_utf8_lossy(&before).contains("Remember the copper orchard"));
    wait_until("suspension replicates to target", 60, || async {
        jade.st_json(&["agents", "show", seat])["value"]["suspension"]["phase"] == "suspended"
    })
    .await;
    // Both daemons share this test's filesystem. An existing path represents an occupied target,
    // and must refuse even when it happens to be the source's checkout.
    let occupied = jade.st(&[
        "agents",
        "resume",
        seat,
        "--host",
        "fixture-move-jade",
        "--as",
        PERSON,
        "--timeout",
        "60s",
    ]);
    assert!(!occupied.status.success());
    assert!(
        String::from_utf8_lossy(&occupied.stderr).contains("workspace-occupied"),
        "{}\n{}",
        String::from_utf8_lossy(&occupied.stderr),
        jade.logs()
    );
    assert_eq!(git(&["rev-parse", "HEAD"]), head);
    // Preserve the stopped source checkout elsewhere, modelling the target's independent disk
    // with an unoccupied path at the exact same absolute cwd.
    fs::rename(&workspace, root.path().join("preserved-source-workspace")).unwrap();
    let resumed = jade.st_json(&[
        "agents",
        "resume",
        seat,
        "--host",
        "fixture-move-jade",
        "--as",
        PERSON,
        "--timeout",
        "90s",
    ]);
    assert_eq!(resumed["id"], seat);
    assert_eq!(resumed["suspension"]["phase"], "resumed");
    assert_eq!(resumed["suspension"]["native_session_id"], native);
    assert_eq!(
        fs::read_to_string(workspace.join("tracked")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("untracked")).unwrap(),
        "copper workspace\n"
    );
    assert_eq!(git(&["rev-parse", "HEAD"]), head);
    let target_sessions =
        st3::seat_snapshot::sessions(&jade.state_dir().join("drivers"), seat, "omp");
    let target_transcript =
        st3::native_resume::pi_family_transcript(&target_sessions, &native).unwrap();
    assert!(fs::read(&target_transcript).unwrap().starts_with(&before));
    jade.st_ok(&[
        "conversations",
        "send",
        seat,
        "--from",
        PERSON,
        "--subject",
        "after move",
        "--body",
        "The copper orchard continued.",
    ]);
    wait_until(
        "target reads mail in the resumed conversation",
        60,
        || async {
            fs::read_to_string(&target_transcript)
                .unwrap_or_default()
                .contains("The copper orchard continued")
        },
    )
    .await;
    assert_eq!(
        fs::read(&transcript).unwrap(),
        before,
        "source transcript changed after the move"
    );
    let source_ptys = Command::new(&pty).args(["list", "--json"])
        .env("PTY_ROOT", amber.state_dir().join("pty")).output().unwrap();
    let source_ptys: Vec<Value> = serde_json::from_slice(&source_ptys.stdout).unwrap();
    assert!(source_ptys.iter().all(|row| row["status"] != "running"), "source still has a live PTY: {source_ptys:?}");
    jade.st_ok(&[
        "agents",
        "suspend",
        seat,
        "--as",
        PERSON,
        "--timeout",
        "60s",
    ]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ST3_TERMINALS_COMPAT_BIN: an st3 build before terminal owner filters"]
async fn terminal_owner_filters_are_safe_across_mixed_builds() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut old = Node::new(root.path(), "terminal-legacy");
    old.binary = PathBuf::from(std::env::var("ST3_TERMINALS_COMPAT_BIN").unwrap());
    old.start().await;
    let current_client = st3_client::Client::unix(old.socket());
    current_client
        .terminals_list(None, Some(50), false)
        .await
        .unwrap();
    let error = current_client
        .terminals_list_filtered(
            None,
            Some(1),
            false,
            Some("agent/lookup/unknown"),
            Some("running"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, st3_client::ClientError::Protocol(message) if message.contains("upgrade the server"))
    );

    let mut current = Node::new(root.path(), "terminal-current");
    current.start().await;
    let output = old
        .command(&[
            "--endpoint",
            current.socket().to_str().unwrap(),
            "--json",
            "terminals",
            "ls",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["value"]["collection"], "terminals");
    assert_eq!(response["value"]["filters"], serde_json::json!({}));
    assert_eq!(response["value"]["page"]["limit"], 50);
}

/// This narrower compatibility boundary needs a build that already knows today's mission
/// format, but predates mission.provenance. The ancient fleet baseline is a different contract.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ST3_PROVENANCE_COMPAT_BIN: a pre-provenance build with the current mission format"]
async fn an_older_daemon_runs_a_provenance_revision_and_reads_its_sidecar_after_upgrade() {
    if st3::test_support::supervise_test() {
        return;
    }
    let old = PathBuf::from(
        std::env::var("ST3_PROVENANCE_COMPAT_BIN").expect("pre-provenance st3 binary"),
    );
    let root = tempfile::tempdir().unwrap();
    let current = anchor(root.path(), "current").await;
    let mut older = joined(root.path(), &current, "older", &[]).await;
    older.stop();
    older.binary = old;
    older.start().await;
    let file = current.root.join("provenance.kdl");
    fs::write(
        &file,
        r#"version 2
mission "orchard/release" state="ready" {
  provenance {
    reason "Approved release source."
    source {
      repository "https://example.invalid/orchard/missions"
      commit "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      path "missions/release.kdl"
      renderer "orchard-renderer-v1"
    }
    decision "decision/opaque-approved-scope"
    evidence "doc/orchard/proof@opaque-reference"
  }
  goal "Prepare the release."
  step "prepare" { goal "Prepare the release." }
}
"#,
    )
    .unwrap();
    current.st_ok(&[
        "apply",
        file.to_str().unwrap(),
        "--no-gate-check",
        "--as",
        PERSON,
    ]);
    let path = "/v1/missions/orchard%2Frelease";
    wait_until(
        "older daemon projects the provenance-bearing revision",
        60,
        || async { older.client().get::<Value>(path).await.is_ok() },
    )
    .await;
    let plain: Value = older.client().get(path).await.unwrap();
    let with: Value = current.client().get(path).await.unwrap();
    assert_eq!(plain["revision"], with["revision"]);
    assert!(
        plain.get("provenance").is_none(),
        "the binary must predate provenance: {plain}"
    );
    assert_eq!(with["provenance"]["reason"], "Approved release source.");
    let started = older.st_json(&[
        "missions",
        "start",
        "orchard/release",
        "--id",
        "orchard/older-run",
        "--as",
        PERSON,
    ]);
    let run = &started["mission_run"];
    assert_eq!(run["revision"], plain["revision"]);
    assert!(!run["steps"].as_array().unwrap().is_empty());
    let status = older.st_json(&["replication", "status"]);
    assert_eq!(status["invalid_records"], 0, "{status}");
    assert!(
        status["waiting_claims"].as_u64().unwrap_or(0) >= 1,
        "{status}"
    );
    older.stop();
    older.binary = PathBuf::from(ST3);
    older.start().await;
    wait_until("upgraded daemon reads the retained sidecar", 60, || async {
        older
            .client()
            .get::<Value>(path)
            .await
            .is_ok_and(|mission| mission["provenance"]["reason"] == "Approved release source.")
    })
    .await;
    let shown = older.st_json(&["missions", "show", run["subject"].as_str().unwrap()]);
    assert_eq!(shown["provenance"]["reason"], "Approved release source.");
}
