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

const ST3: &str = env!("CARGO_BIN_EXE_st3");
const PERSON: &str = "person/fleet-tester";
const NOTE: &str = "custom.fleet-test.note";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        let mut command = Command::new(&self.binary);
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
            "st3 {} on {} failed:\n{}{}",
            arguments.join(" "),
            self.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn st_json(&self, arguments: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(arguments);
        serde_json::from_str(&self.st_ok(&all)).unwrap()
    }

    fn logs(&self) -> String {
        ["daemon.log", "worker.log"]
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
        wait_until(&format!("{} daemon starts", self.name), 30, || {
            let client = client.clone();
            async move { client.get::<Value>("/v1/health").await.is_ok() }
        })
        .await;
        let legacy = fs::read_to_string(self.root.join("config/st3/config.toml"))
            .is_ok_and(|config| config.contains("fleet_id"));
        if legacy || self.state_dir().join("fleet/fleet.toml").exists() {
            let worker = self
                .command(&["replication-worker"])
                .stdin(Stdio::null())
                .stdout(log("worker.log"))
                .stderr(log("worker.stderr.log"))
                .spawn()
                .unwrap();
            self.worker = Some(worker);
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

    /// Wait until this member announces a loopback endpoint, so its invites can carry it.
    async fn wait_listening(&self) {
        wait_until(
            &format!("{} announces its endpoint", self.name),
            30,
            || async {
                let status: Value = match self.client().get("/v1/internal/fleet/status").await {
                    Ok(status) => status,
                    Err(_) => return false,
                };
                status["view"]["members"].as_array().is_some_and(|members| {
                    members.iter().any(|member| {
                        member["name"] == self.name.as_str()
                            && member["endpoints"]
                                .as_array()
                                .is_some_and(|endpoints| !endpoints.is_empty())
                    })
                })
            },
        )
        .await;
    }

    async fn note(&self, text: &str) -> String {
        let claim: Value = self
            .client()
            .post(
                "/v1/claims",
                &ClaimInput {
                    subject: format!("custom/fleet-test/{text}"),
                    kind: NOTE.into(),
                    actor: Some(PERSON.into()),
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

fn authority_digest(node: &Node) -> String {
    node.st_json(&["replication", "status"])["authority_digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_and_join_sync_full_history() {
    let root = tempfile::tempdir().unwrap();
    let a = anchor(root.path(), "a").await;
    let mut expected = BTreeSet::new();
    for index in 0..60 {
        a.note(&format!("a-{index}")).await;
        expected.insert(format!("custom/fleet-test/a-{index}"));
    }

    let b = joined(root.path(), &a, "b", &[]).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_dial_out_member_is_caught_up_and_never_reported_down() {
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
            .all(|peer| peer["peer"] != "laptop"),
        "a reports on the dial-out laptop: {peers}"
    );
    let machines = a.st_json(&["machines"]);
    let laptop_machine = machines["value"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|machine| machine["host_id"] == "host/laptop")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(laptop_machine["state"], "dial-out", "{machines}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_join_resumes_with_the_same_code() {
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

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_member_is_refused() {
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
}

#[tokio::test(flavor = "multi_thread")]
async fn leave_drains_everything_before_it_leaves() {
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "a").await;
    let b = joined(root.path(), &a, "b", &[]).await;
    b.wait_listening().await;
    a.stop();
    let mut expected = BTreeSet::new();
    // More than one exchange carries, written while the anchor is away.
    for index in 0..700 {
        b.note(&format!("b-{index}")).await;
        expected.insert(format!("custom/fleet-test/b-{index}"));
    }
    a.start().await;
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
async fn uninstall_leaves_nothing_behind() {
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
    b.migrate(&[&code]);
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
    assert!(announced.is_err());
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
    assert!(claim.is_err());
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
#[ignore = "needs ST3_COMPAT_BIN: the st3 of the pinned baseline release"]
async fn an_old_build_config_peer_replicates_with_new_members() {
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

#[test]
fn fleet_workflows_have_no_path_filter() {
    let workflows = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows");
    let compat = fs::read_to_string(workflows.join("fleet.yml")).unwrap();
    assert!(compat.contains("fleet-compat"));
    assert!(compat.contains("pull_request:"));
    for filter in ["paths:", "paths-ignore:", "branches-ignore:"] {
        assert!(
            !compat.contains(filter),
            "fleet.yml filters its runs with {filter}"
        );
    }
    assert!(compat.contains("an_old_build_config_peer_replicates_with_new_members"));
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

#[tokio::test(flavor = "multi_thread")]
async fn the_fabric_transport_works_through_the_worker_alone() {
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
    b.st_ok(&[
        "fleet",
        "join",
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

    let calls = fs::read_to_string(registry.join("calls")).unwrap();
    for (node, peer) in [("a", "b"), ("b", "a")] {
        assert!(
            calls.contains(&format!("{node}-fabric-id expose st3/fleet/"))
                && calls.contains("--ephemeral"),
            "{node} did not expose itself ephemerally:\n{calls}"
        );
        assert!(
            calls.contains(&format!(
                "{node}-fabric-id dial {peer}-fabric-id st3/fleet/"
            )),
            "{node} never dialed {peer} through Fabric:\n{calls}"
        );
    }
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
    let root = tempfile::tempdir().unwrap();
    let mut a = anchor(root.path(), "a").await;
    let mut c = joined(root.path(), &a, "c", &[]).await;
    c.wait_listening().await;
    let mut r = joined(root.path(), &a, "r", &[]).await;
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
