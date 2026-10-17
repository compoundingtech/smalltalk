//! The actual stui binary on a daemon-less device, talking only to isolated members.
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term},
    vte::ansi::Processor,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use st3::{api::AppState, model::PersonAskRequest, store::Store};
use st3_client::{Client, PersonPairingBegin};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, UnixStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
};

struct Size;
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        32
    }
    fn screen_lines(&self) -> usize {
        32
    }
    fn columns(&self) -> usize {
        140
    }
}
struct Events;
impl EventListener for Events {
    fn send_event(&self, _: Event) {}
}

struct Tui {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Tui {
    fn start(root: &Path) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 32,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(test_env!("CARGO_BIN_EXE_st3"));
        // This proves pairing and control, read off the classic layout's screens.
        command.args(["ui", "--classic"]);
        command.env_clear();
        command.env("HOME", root);
        command.env("XDG_CONFIG_HOME", root.join("config"));
        command.env("XDG_CACHE_HOME", root.join("cache"));
        command.env("XDG_STATE_HOME", root.join("state"));
        command.env("TERM", "xterm-256color");
        // A real restart of st reads "connecting" for a minute; this test only waits 25 s for "offline".
        command.env("STUI_OFFLINE_GRACE_MS", "0");
        // These deliberately unusable local settings must never select local authority.
        command.env("ST3_ENDPOINT", root.join("absent.sock"));
        command.env("ST3_PERSON", "person/wrong-local-actor");
        command.cwd(root);
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(String::new()));
        let observed = screen.clone();
        std::thread::spawn(move || {
            let mut term = Term::new(Config::default(), &Size, Events);
            let mut parser: Processor = Processor::new();
            let mut bytes = [0; 16384];
            while let Ok(length) = reader.read(&mut bytes) {
                if length == 0 {
                    break;
                }
                parser.advance(&mut term, &bytes[..length]);
                let text = (0..32)
                    .map(|row| {
                        (0..140)
                            .map(|col| term.grid()[Line(row)][Column(col)].c)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                *observed.lock().unwrap() = text;
            }
        });
        Self {
            child,
            writer,
            screen,
            _master: pty.master,
        }
    }
    fn send(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
    async fn wait(&self, label: &str, predicate: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            let screen = self.screen.lock().unwrap().clone();
            if predicate(&screen) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{label} did not appear:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn state(root: &Path, node: &str) -> AppState {
    // These servers read while client actions write; shared-memory stores are sequential fixtures.
    let store = Store::open(&root.join("graph.db"), node).unwrap();
    // Real pairing now enrolls a key; the member must hold its node authority to grant it.
    store
        .set_node_key(Arc::new(
            smallclaims::fleet::MemberKey::generate().unwrap().0,
        ))
        .unwrap();
    AppState {
        owning_person: None,
        store: Arc::new(store),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: node.into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    }
}
fn person_ask(state: &AppState, name: &str, title: &str) -> String {
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "tui-person-asker")
        .unwrap();
    let step = state
        .store
        .ask_person(&PersonAskRequest {
            legacy_request: None,
            person: "person/avery".into(),
            title: title.into(),
            reason: "Confirm on the remote member".into(),
            actor: format!("agent/{}.asker", state.store.origin()),
            step: None,
            new_run: Some(name.into()),
            incarnation: None,
            idempotency_key: name.into(),
            request: None,
        })
        .unwrap();
    state
        .event_notify
        .send_modify(|index| *index = state.store.index().unwrap());
    step.subject
}

// The production-like carrier forwards the paired-only socket, never the trusted socket.
// Dropping the carrier closes its existing streams too, unlike aborting an axum accept loop.
fn carrier(listener: TcpListener, socket: std::path::PathBuf) -> JoinHandle<()> {
    carrier_with_loss(listener, socket, watch::channel(false).1)
}

fn carrier_with_loss(
    listener: TcpListener,
    socket: std::path::PathBuf,
    loss: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { return; };
                    let socket = socket.clone();
                    let mut loss = loss.clone();
                    connections.spawn(async move {
                        if let Ok(mut upstream) = UnixStream::connect(socket).await {
                            loop {
                                if *loss.borrow() {
                                    if loss.changed().await.is_err() { break; }
                                    continue;
                                }
                                tokio::select! {
                                    _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream) => break,
                                    changed = loss.changed() => {
                                        if changed.is_err() {
                                            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    })
}

async fn pair(root: &Path, local: &Client, url: &str) {
    let challenge = local.pairing_begin_person(&PersonPairingBegin { api_version: st3_client::API_VERSION.into(),
    device_name: "Demo laptop".into(),
    person_id: "person/avery".into(),
    full_control: Some(true),
    scopes: None, })
        .await
        .unwrap()
        .value;
    complete_challenge(root, url, &challenge).await;
}

async fn complete_challenge(root: &Path, url: &str, challenge: &st3_client::PairingChallenge) {
    let mut child = Command::new(test_env!("CARGO_BIN_EXE_st3"))
        .args([
            "devices",
            "complete",
            url,
            &challenge.pairing_id,
            "--fingerprint",
            challenge.person_root_fingerprint.as_deref().unwrap(),
        ])
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(challenge.code.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "pair failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&challenge.code));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stui_pair_persists_a_real_signing_key_and_leaves_read_only_devices_without_one() {
    use st3_client::{Fence, MessageSendParameters, device::Profile};
    use std::os::unix::fs::PermissionsExt as _;
    let member = tempfile::tempdir().unwrap();
    let device = tempfile::tempdir().unwrap();
    let state = state(member.path(), "member");
    state
        .store
        .bind_fleet("3c9a1f2e-8b7d-4e6c-a5f4-1d2e3c4b5a69")
        .unwrap();
    state
        .store
        .set_node_key(Arc::new(
            smallclaims::fleet::MemberKey::generate().unwrap().0,
        ))
        .unwrap();
    let trusted = member.path().join("trusted.sock");
    let app = st3::api::router(state.clone());
    let served = trusted.clone();
    let local_server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
    let local = Client::unix_as(&trusted, "person/avery");
    for _ in 0..100 {
        if local.capabilities().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = st3::api::fabric_router(state.clone());
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await });
    pair(device.path(), &local, &url).await;
    let path = device.path().join("config/st3/stui-devices.json");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let profile = Profile::load(&path).unwrap().unwrap();
    let key = profile.devices[0]
        .signing_key
        .as_ref()
        .unwrap()
        .public_key()
        .unwrap();
    assert!(key.starts_with("p256:"));
    state.store.replication_snapshot().unwrap();
    for grant in &profile.devices[0].session.device_key_chain {
        assert_eq!(
            state.store.claim_verdict(grant).unwrap(),
            smallclaims::principal::Verdict::Verified
        );
    }
    let client = profile.clients().unwrap().pop().unwrap();
    let snapshot = client.capabilities().await.unwrap().snapshot.id;
    let result = client
        .message_send(
            "action/stui-device-pair-proof",
            "stui-device-pair-proof",
            Fence {
                snapshot_id: snapshot,
                ..Fence::default()
            },
            MessageSendParameters {
                to: "agent/alder".into(),
                content: "Signed after stui pairing".into(),
                title: None,
                in_reply_to: None,
                session_id: None,
                tags: vec![],
                kind: st3_client::MessageKind::Wake,
                attachments: vec![],
                signature: None,
            },
        )
        .await
        .unwrap();
    let subject = result
        .value
        .affected_ids
        .iter()
        .find(|id| id.starts_with("message/"))
        .unwrap();
    let claim = state
        .store
        .latest_claim(subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    state.store.replication_snapshot().unwrap();
    assert_eq!(
        state.store.claim_signature(&claim.id).unwrap().unwrap().key,
        key
    );
    assert_eq!(
        state.store.claim_verdict(&claim.id).unwrap(),
        smallclaims::principal::Verdict::Verified
    );
    let challenge = local.pairing_begin_person(&PersonPairingBegin { api_version: st3_client::API_VERSION.into(),
    device_name: "Display".into(),
    person_id: "person/avery".into(),
    full_control: None,
    scopes: Some(vec!["read.projections".into()]), })
        .await
        .unwrap()
        .value;
    complete_challenge(device.path(), &url, &challenge).await;
    let profile = Profile::load(&path).unwrap().unwrap();
    assert!(profile.devices[0].signing_key.is_none());
    assert!(profile.devices[0].session.device_key_chain.is_empty());
    local_server.abort();
    http_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_less_stui_pairs_controls_loses_recovers_and_uses_another_member() {
    let member = tempfile::tempdir().unwrap();
    let device = tempfile::tempdir().unwrap();
    let state = state(member.path(), "member-cedar");
    let trusted = member.path().join("trusted.sock");
    let gateway = member.path().join("gateway.sock");
    let trusted_server = tokio::spawn({
        let path = trusted.clone();
        let app = st3::api::router(state.clone());
        async move { st3::api::serve_unix(&path, app).await }
    });
    let gateway_server = tokio::spawn({
        let path = gateway.clone();
        let app = st3::api::fabric_router(state.clone());
        async move { st3::api::serve_unix(&path, app).await }
    });
    let local = Client::unix_as(&trusted, "person/avery");
    for _ in 0..100 {
        if local.capabilities().await.is_ok() && gateway.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}");
    let route = carrier(listener, gateway.clone());
    let preflight = Client::fabric_pairing(&url);
    // The anonymous advertisement is deliberately not a normal authenticated envelope.
    assert!(preflight.capabilities().await.is_err());
    assert!(preflight.agents_list(None, None, false).await.is_err());
    pair(device.path(), &local, &url).await;
    let online_step = person_ask(&state, "online-proof", "Online proof");
    let mut tui = Tui::start(device.path());
    tui.wait("paired live view", |screen| {
        screen.contains("live") && screen.contains("Online proof") && screen.contains("avery")
    })
    .await;
    tui.send("cConfirmed on the remote member\r");
    tui.wait("confirmed remote action", |screen| {
        !screen.contains("Online proof")
    })
    .await;
    assert_eq!(
        state.store.step_run(&online_step).unwrap().unwrap().status,
        "completed"
    );

    let offline_step = person_ask(&state, "offline-proof", "Retained proof");
    tui.wait("new data", |screen| screen.contains("Retained proof"))
        .await;
    route.abort();
    let _ = route.await;
    tui.wait("offline cached view", |screen| {
        screen.contains("offline")
            && screen.contains("Last connected at")
            && screen.contains("Retained proof")
    })
    .await;
    let before = state.store.index().unwrap();
    tui.send("cConfirmed on the remote member\r");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        state.store.index().unwrap(),
        before,
        "offline keys must queue no mutation"
    );
    assert!(!device.path().join("absent.sock").exists());
    assert!(
        !device.path().join("state").exists(),
        "the device must create no daemon or replica"
    );
    drop(tui);
    let mut tui = Tui::start(device.path());
    tui.wait("cache after device restart", |screen| {
        screen.contains("offline")
            && screen.contains("Last connected at")
            && screen.contains("Retained proof")
    })
    .await;
    let (loss, loss_rx) = watch::channel(false);
    let route = carrier_with_loss(
        TcpListener::bind(address).await.unwrap(),
        gateway.clone(),
        loss_rx,
    );
    tui.wait("automatic recovery", |screen| {
        screen.contains("live")
            && screen.contains("Retained proof")
            && !screen.contains("Last connected at")
    })
    .await;
    assert_eq!(
        state.store.step_run(&offline_step).unwrap().unwrap().status,
        "ready",
        "recovery must not replay offline input"
    );
    tui.send("cConfirmed on the remote member\r");
    tui.wait("remote action after recovery", |screen| {
        !screen.contains("Retained proof")
    })
    .await;
    let _blackhole_step = person_ask(&state, "blackhole-proof", "Blackhole proof");
    tui.wait("data before a silent outage", |screen| {
        screen.contains("Blackhole proof")
    })
    .await;
    loss.send(true).unwrap();
    tui.wait("idle blackhole detection", |screen| {
        screen.contains("offline")
            && screen.contains("Blackhole proof")
            && screen.contains("Last connected at")
    })
    .await;
    loss.send(false).unwrap();
    tui.wait("automatic blackhole recovery", |screen| {
        screen.contains("live") && screen.contains("Blackhole proof")
    })
    .await;
    tui.send("cConfirmed on the remote member\r");
    tui.wait("remote action after blackhole recovery", |screen| {
        !screen.contains("Blackhole proof")
    })
    .await;
    drop(tui);

    // Pair a second independent member, with its own grant, then lose the first route.
    let second = tempfile::tempdir().unwrap();
    let alternate = self::state(second.path(), "member-willow");
    let alternate_gateway = second.path().join("gateway.sock");
    let alternate_server = tokio::spawn({
        let path = alternate_gateway.clone();
        let app = st3::api::fabric_router(alternate.clone());
        async move { st3::api::serve_unix(&path, app).await }
    });
    let alternate_trusted = second.path().join("trusted.sock");
    let alternate_local_server = tokio::spawn({
        let path = alternate_trusted.clone();
        let app = st3::api::router(alternate.clone());
        async move { st3::api::serve_unix(&path, app).await }
    });
    for _ in 0..100 {
        if alternate_gateway.exists() && alternate_trusted.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let alternate_url = format!("http://{}", listener.local_addr().unwrap());
    let alternate_route = carrier(listener, alternate_gateway);
    pair(
        device.path(),
        &Client::unix_as(alternate_trusted, "person/avery"),
        &alternate_url,
    )
    .await;
    route.abort();
    let _ = route.await;
    let alternate_step = person_ask(&alternate, "alternate-proof", "Alternate proof");
    let mut tui = Tui::start(device.path());
    tui.wait("another reachable member", |screen| {
        screen.contains("live")
            && screen.contains("Alternate proof")
            && screen.contains("member-willow")
    })
    .await;
    tui.send("cConfirmed on the remote member\r");
    tui.wait("action on alternate", |screen| {
        !screen.contains("Alternate proof")
    })
    .await;
    assert_eq!(
        alternate
            .store
            .step_run(&alternate_step)
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    // Fail over while the UI is running; actions must follow its new selected member.
    let return_step = person_ask(&state, "return-proof", "Returned member proof");
    let route = carrier(TcpListener::bind(address).await.unwrap(), gateway);
    alternate_route.abort();
    let _ = alternate_route.await;
    tui.wait("live failover", |screen| {
        screen.contains("live")
            && screen.contains("Returned member proof")
            && screen.contains("member-cedar")
    })
    .await;
    tui.send("cConfirmed on the remote member\r");
    tui.wait("action after live failover", |screen| {
        !screen.contains("Returned member proof")
    })
    .await;
    assert_eq!(
        state.store.step_run(&return_step).unwrap().unwrap().status,
        "completed"
    );
    drop(tui);
    for server in [
        trusted_server,
        gateway_server,
        alternate_server,
        alternate_local_server,
    ] {
        server.abort();
    }
    route.abort();
}

/// The designated timing client drives the actual binary against an isolated daemon. Both
/// old timer intervals are exceeded; protocol pings keep the stream live without a read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fleet_inventory_screen_sends_no_request_after_its_first_frames_for_more_than_a_minute()
{
    inventory_screen_sends_no_request_after_its_first_frames_for_more_than_a_minute(
        "4",
        "inventory-fleet",
        &["clients", "machines", "devices", "sessions"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_usage_inventory_screen_sends_no_request_after_its_first_frames_for_more_than_a_minute()
{
    inventory_screen_sends_no_request_after_its_first_frames_for_more_than_a_minute(
        "5",
        "inventory-usage",
        &["usage-24"],
    )
    .await;
}

/// One screen of the real stui client. The two screens run as two tests, so they idle their
/// minute side by side instead of one after the other.
async fn inventory_screen_sends_no_request_after_its_first_frames_for_more_than_a_minute(
    key: &str,
    label: &str,
    windows: &[&str],
) {
    use serde_json::Value;
    let member = tempfile::tempdir().unwrap();
    let mut state = state(member.path(), "inventory-idle");
    state.native_session_home = Some(member.path().join("native"));
    std::fs::create_dir_all(state.native_session_home.as_ref().unwrap()).unwrap();
    let socket = member.path().join("inventory.sock");
    let app = st3::api::router(state);
    let served = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
    let client = Client::unix_as(&socket, "person/avery");
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.capabilities().await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let repo = Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../..");
    let device = tempfile::tempdir().unwrap();
    let log = device.path().join("timing.jsonl");
    let events = device.path().join("events.jsonl");
    let mut command = Command::new("python3");
    command
        .arg(repo.join("scripts/stui-test-client"))
        .args([
            "--binary",
            test_env!("CARGO_BIN_EXE_st3"),
            "--classic",
            "--seconds",
            "72",
            "--label",
            label,
            "--keys",
            key,
        ])
        .arg("--timing")
        .arg(&log)
        .arg("--events")
        .arg(&events)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", device.path())
        .env("XDG_CONFIG_HOME", device.path().join("config"))
        .env("XDG_CACHE_HOME", device.path().join("cache"))
        .env("XDG_STATE_HOME", device.path().join("state"))
        .env("ST3_ENDPOINT", &socket)
        .env("ST3_PERSON", "person/avery");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let recorded = std::fs::read_to_string(&events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let origin = rows.iter().find(|row| row["type"] == "start").unwrap()["epoch_ms"]
        .as_u64()
        .unwrap();
    let idle = recorded
        .iter()
        .find(|row| row["type"] == "idle_start")
        .unwrap()["epoch_ms"]
        .as_u64()
        .unwrap();
    let end = recorded
        .iter()
        .find(|row| row["type"] == "idle_end")
        .unwrap();
    assert_eq!(
        end["running"], true,
        "the real client stayed alive throughout idle"
    );
    let mut ready = idle;
    for &window in windows {
        let frames = rows
            .iter()
            .filter(|row| {
                row["type"] == "frame" && row["id"] == window && row["kind"] == "snapshot"
            })
            .collect::<Vec<_>>();
        assert_eq!(
            frames.len(),
            1,
            "{label} must subscribe {window} once, without polling or resubscription: {rows:?}"
        );
        ready = ready.max(origin + frames[0]["at_ms"].as_u64().unwrap());
    }
    assert!(
        end["epoch_ms"].as_u64().unwrap() - ready > 60_000,
        "the idle oracle exceeds the longest old interval after all first frames"
    );
    let requests = rows
        .iter()
        .filter(|row| {
            row["type"] == "request"
                && origin + row["at_ms"].as_u64().unwrap() > ready
                && origin + row["at_ms"].as_u64().unwrap() <= end["epoch_ms"].as_u64().unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        requests.is_empty(),
        "idle {label} issued requests: {requests:?}"
    );
    let inventory_http = rows
        .iter()
        .filter(|row| row["type"] == "request")
        .filter(|row| {
            [
                "/v1/client/clients",
                "/v1/client/machines",
                "/v1/client/devices",
                "/v1/client/sessions",
                "/v1/client/terminals",
                "/v1/client/usage",
            ]
            .iter()
            .any(|route| {
                row["route"]
                    .as_str()
                    .is_some_and(|actual| actual.starts_with(route))
            })
        })
        .collect::<Vec<_>>();
    assert!(
        inventory_http.is_empty(),
        "a granted member uses the stream even on initial focus: {inventory_http:?}"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_member_reads_fleet_on_focus_without_attempting_inventory_subscriptions() {
    use axum::{
        body::to_bytes,
        middleware::{Next, from_fn},
        response::IntoResponse,
    };
    use serde_json::Value;
    let member = tempfile::tempdir().unwrap();
    let mut state = state(member.path(), "older-inventory");
    state.native_session_home = Some(member.path().join("native"));
    std::fs::create_dir_all(state.native_session_home.as_ref().unwrap()).unwrap();
    let app = st3::api::router(state).layer(from_fn(
        |request: axum::extract::Request, next: Next| async move {
            let capabilities = request.uri().path() == "/v1/client/capabilities";
            let response = next.run(request).await;
            if !capabilities {
                return response;
            }
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            value["value"]["capabilities"]
                .as_array_mut()
                .unwrap()
                .retain(|capability| capability["id"] != "inventory-collections");
            (status, axum::Json(value)).into_response()
        },
    ));
    let socket = member.path().join("older.sock");
    let served = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
    let client = Client::unix_as(&socket, "person/avery");
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.capabilities().await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let device = tempfile::tempdir().unwrap();
    let log = device.path().join("legacy.jsonl");
    let repo = Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new("python3");
    command
        .arg(repo.join("scripts/stui-test-client"))
        .args([
            "--binary",
            test_env!("CARGO_BIN_EXE_st3"),
            "--classic",
            "--seconds",
            "4",
            "--label",
            "older-inventory",
            "--keys",
            "4",
        ])
        .arg("--timing")
        .arg(&log)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", device.path())
        .env("XDG_CONFIG_HOME", device.path().join("config"))
        .env("XDG_CACHE_HOME", device.path().join("cache"))
        .env("XDG_STATE_HOME", device.path().join("state"))
        .env("ST3_ENDPOINT", &socket)
        .env("ST3_PERSON", "person/avery");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for route in [
        "/v1/client/clients",
        "/v1/client/machines",
        "/v1/client/devices",
        "/v1/client/sessions",
    ] {
        assert_eq!(
            rows.iter()
                .filter(|row| row["type"] == "request"
                    && row["route"]
                        .as_str()
                        .is_some_and(|actual| actual.starts_with(route)))
                .count(),
            1,
            "older member reads {route} once on focus: {rows:?}"
        );
    }
    assert!(
        !rows.iter().any(|row| row["type"] == "frame"
            && ["clients", "machines", "devices", "sessions"]
                .contains(&row["id"].as_str().unwrap_or_default())),
        "unadvertised inventory windows are not attempted"
    );
    server.abort();
}
