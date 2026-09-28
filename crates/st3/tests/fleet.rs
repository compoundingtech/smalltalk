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
        Self {
            name: name.into(),
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
        let mut command = Command::new(ST3);
        command
            .args(arguments)
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
        let daemon = self
            .command(&["up", "--pty-binary", pty.to_str().unwrap()])
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
        if self.state_dir().join("fleet/fleet.toml").exists() {
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
