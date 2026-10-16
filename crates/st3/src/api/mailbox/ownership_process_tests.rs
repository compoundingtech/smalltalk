//! REAL Linux processes, kernel Unix peer credentials, PTY and installed daemon routes.
//! The provider is another copy of this test executable, NOT Claude. On-disk witness/source
//! setup is a fixture producer; no installed-provider topology claim follows from this test.
use super::*;
use crate::api::{router, serve_unix_bound_with_ready};
use crate::client::{Client, Endpoint};
use std::path::PathBuf;
use std::process::{Child, Command};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::Message;

const TEST: &str = "api::mailbox::ownership_process_tests::nested_real_peers_never_change_primary_attachment_or_recovery";
const ROLE: &str = "ST_TEST_OWNERSHIP_ROLE";
const SUBJECT: &str = "agent/node.ownership-real-peer";
const SESSION: &str = "owned-process-fixture";
const LIMIT: Duration = Duration::from_secs(60);
// Deliberately shared discovery inputs. These real peer helpers are NOT Claude plugin loaders;
// the launch selector's unit controls separately exercise the generated override.
const PLUGIN_SETTINGS: &str = r#"{"enabledPlugins":{"st-channel@st":true,"st3-channel@st3":true,"st3-channel@st2":true,"st2-channel@st2":true}}"#;

async fn read(stream: &mut UnixStream) -> Value {
    tokio::time::timeout(LIMIT, async {
        let mut bytes = Vec::new();
        loop {
            let byte = stream.read_u8().await.unwrap();
            if byte == b'\n' {
                break;
            }
            assert!(bytes.len() < 8192, "fixture control bound");
            bytes.push(byte);
        }
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .expect("fixture control response")
}
async fn write(stream: &mut UnixStream, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
}
async fn call(stream: &mut UnixStream, value: Value) -> Value {
    eprintln!("ownership fixture control operation={}", value["op"]);
    write(stream, value).await;
    read(stream).await
}
fn command(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .envs(std::env::vars_os().filter(|(key, _)| {
            let key = key.to_string_lossy();
            let copied_seat = role != "stripped-secondary"
                && (key.starts_with("ST_DRIVER_")
                    || matches!(
                        key.as_ref(),
                        "ST_AGENT" | "ST_CLAUDE_SESSION" | "ST_CLAUDE_SESSION_SEQ"
                    ));
            key.starts_with("ST_TEST_OWNERSHIP_")
                || matches!(key.as_ref(), "HOME" | "PATH")
                || copied_seat
        }));
    if let Some(path) = std::env::var_os("ST_TEST_OWNERSHIP_DIAGNOSTICS") {
        let stderr = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        command.stderr(stderr);
    }
    command.args([
        "--exact",
        TEST,
        "--nocapture",
        "driver",
        if role == "wrapper" || role == "secondary-title" {
            "claude"
        } else {
            "claude-mcp"
        },
    ]);
    command.env(ROLE, role);
    command
}
fn wait_children(children: Vec<Child>) {
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
}

async fn helper(role: &str) {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_eq!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        home.canonicalize().unwrap()
    );
    for path in [".claude/settings.json", ".claude/settings.local.json"] {
        assert_eq!(
            fs::read(home.join(path)).unwrap(),
            PLUGIN_SETTINGS.as_bytes()
        );
    }
    if role == "stripped-secondary" {
        assert!(std::env::vars_os().all(|(key, _)| {
            let key = key.to_string_lossy();
            key.starts_with("ST_TEST_OWNERSHIP_")
                || !(key.starts_with("ST_")
                    || key.starts_with("ST3_")
                    || key.starts_with("CLAUDE_CODE_")
                    || key.starts_with("CLAUDECODE"))
        }));
    }
    let mut control = UnixStream::connect(std::env::var_os("ST_TEST_OWNERSHIP_CONTROL").unwrap())
        .await
        .unwrap();
    write(&mut control, json!({"role":role,"pid":std::process::id()})).await;
    let mut children = Vec::new();
    if role == "wrapper" {
        let setup = read(&mut control).await;
        let dir = PathBuf::from(setup["dir"].as_str().unwrap());
        let runtime = setup["incarnation"].as_str().unwrap();
        st_drivers::harness_events::enable(&dir, runtime).unwrap();
        let seq = st_drivers::harness_state::claim(&dir, "ownership-real-peer", "claude", SESSION)
            .unwrap();
        // claim() deliberately installs an Ended/superseded fence. This fixture's
        // live wrapper must publish provider state before exercising native admission,
        // just as the real wrapper does; a graph harness claim alone is not authority.
        use st_drivers::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};
        Writer::new(
            &dir,
            "ownership-real-peer",
            "claude",
            Some(setup["pty_session"].as_str().unwrap().to_owned()),
        )
        .with_ownership(SESSION, seq)
        .observe(Observation::new(
            Activity::Active,
            BlockedOn::None,
            InputBuffer::Empty,
        ))
        .unwrap();
        let wrapper = st_drivers::claude_process::process(std::process::id()).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("claude-primary-process.lock"))
            .unwrap();
        let owner = |provider| {
            json!({"version":1,"session":SESSION,"sequence":seq,"runtime":runtime,
            "wrapper":wrapper,"provider":provider,"closed":false})
        };
        fs::write(
            dir.join("claude-primary-process.json"),
            serde_json::to_vec(&owner(Value::Null)).unwrap(),
        )
        .unwrap();
        let child = command("provider")
            .env("ST_CLAUDE_SESSION", SESSION)
            .env("ST_CLAUDE_SESSION_SEQ", seq.to_string())
            .spawn()
            .unwrap();
        let provider = st_drivers::claude_process::process(child.id()).unwrap();
        fs::write(
            dir.join("claude-primary-process.json"),
            serde_json::to_vec(&owner(json!(provider))).unwrap(),
        )
        .unwrap();
        children.push(child);
        write(&mut control, json!({"ready":true,"sequence":seq})).await;
    } else if role == "provider" || role == "nested" {
        assert_eq!(read(&mut control).await["go"], true);
        let roles: &[&str] = if role == "provider" {
            &["primary", "nested"]
        } else {
            &["secondary", "secondary-title", "stripped-secondary"]
        };
        for role in roles {
            children.push(command(role).spawn().unwrap());
        }
        write(&mut control, json!({"ready":true})).await;
    }
    let endpoint = PathBuf::from(std::env::var_os("ST_TEST_OWNERSHIP_API").unwrap());
    let client = Client::new(Endpoint::Unix(endpoint));
    let mut socket = None;
    loop {
        let request = read(&mut control).await;
        let op = request["op"].as_str().unwrap();
        let response = match op {
            "quit" => {
                write(&mut control, json!({"ok":true})).await;
                break;
            }
            "bind" => {
                let fence: Fence = serde_json::from_value(request["fence"].clone()).unwrap();
                match client.post::<_, Fence>("/v1/mailbox/bind", &fence).await {
                    Ok(bound) => json!({"ok":true,"fence":bound}),
                    Err(error) => {
                        eprintln!("ownership fixture bind role={role}: {error}");
                        json!({"ok":false})
                    }
                }
            }
            "open" => {
                let fence: Fence = serde_json::from_value(request["fence"].clone()).unwrap();
                match client.open_mailbox(&fence).await {
                    Ok(opened) => {
                        socket = Some(opened);
                        json!({"ok":true})
                    }
                    Err(_) => json!({"ok":false}),
                }
            }
            "report" => {
                let ws = socket.as_mut().unwrap();
                ws.send(Message::Text(
                    json!({"transport":"claude-channel","pid":std::process::id(),
                    "ready":true,"channel":{"pid":std::process::id(),"age_ms":0}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Ping(b"ordered-report".to_vec().into()))
                    .await
                    .unwrap();
                tokio::time::timeout(LIMIT, async {
                    for _ in 0..64 {
                        match ws.next().await.unwrap().unwrap() {
                            Message::Pong(bytes) if bytes.as_ref() == b"ordered-report" => return,
                            Message::Ping(bytes) => ws.send(Message::Pong(bytes)).await.unwrap(),
                            _ => {}
                        }
                    }
                    panic!("report acknowledgement bound");
                })
                .await
                .unwrap();
                json!({"ok":true})
            }
            "expect-refused" => {
                let ws = socket.as_mut().unwrap();
                let _ = ws
                    .send(Message::Text(
                        json!({"transport":"claude-channel","pid":std::process::id(),"ready":true})
                            .to_string()
                            .into(),
                    ))
                    .await;
                tokio::time::timeout(LIMIT, async {
                    loop {
                        match ws.next().await {
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                            Some(Ok(Message::Ping(bytes))) => {
                                let _ = ws.send(Message::Pong(bytes)).await;
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .expect("proven refusal must close the actual stream");
                socket = None;
                json!({"ok":true})
            }
            "close" => {
                if let Some(mut ws) = socket.take() {
                    let _ = ws.close(None).await;
                }
                json!({"ok":true})
            }
            "error" => {
                socket = None;
                json!({"ok":true})
            }
            _ => panic!("unknown fixture operation"),
        };
        write(&mut control, response).await;
    }
    drop(socket);
    tokio::task::spawn_blocking(move || wait_children(children))
        .await
        .unwrap();
}

async fn accept(listener: &UnixListener) -> (String, UnixStream, u32) {
    let (mut stream, _) = tokio::time::timeout(LIMIT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let pid = crate::api::local_peer_pid(&stream).unwrap();
    let hello = read(&mut stream).await;
    assert_eq!(hello["pid"], pid, "control PID is kernel-authenticated too");
    (hello["role"].as_str().unwrap().to_owned(), stream, pid)
}
struct OwnedPty {
    runtime: st_runtime::PtyRuntime,
    name: String,
    incarnation: String,
    diagnostics: PathBuf,
}
impl Drop for OwnedPty {
    fn drop(&mut self) {
        if std::thread::panicking() {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            if let Ok(file) = fs::File::open(&self.diagnostics) {
                let _ = file.take(16_384).read_to_end(&mut bytes);
            }
            eprintln!("owned fixture helper stderr (at most 16KiB): {}",
                String::from_utf8_lossy(&bytes));
        }
        let _ = self.runtime.stop_if(&self.name, Some(&self.incarnation));
    }
}
fn count(store: &Store) -> usize {
    store
        .claims_for(SUBJECT, Some("runtime.action.requested"))
        .unwrap()
        .iter()
        .filter(|claim| claim.body["fields"]["operation"] == "claude-channel-recovery-window-v1")
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_real_peers_never_change_primary_attachment_or_recovery() {
    if let Ok(role) = std::env::var(ROLE) {
        helper(&role).await;
        return;
    }
    if crate::test_support::supervise_test() {
        return;
    }
    for nested_first in [false, true] {
        for secondary_closes_first in [false, true] {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            for path in [".claude/settings.json", ".claude/settings.local.json"] {
                fs::write(root.path().join(path), PLUGIN_SETTINGS).unwrap();
            }
            let mut state = crate::api::tests::state(root.path());
            // Hosted Linux uses the pinned devShell ptyPackage (flake.nix), never a skip.
            state.pty_binary =
                std::env::split_paths(&std::env::var_os("PATH").expect("hosted PATH"))
                    .map(|dir| dir.join("pty"))
                    .find(|path| path.is_file())
                    .and_then(|path| fs::canonicalize(path).ok())
                    .expect("pinned devShell pty must exist");
            eprintln!(
                "ownership fixture pty={} step_bound=60s; PTY library publication=5s command=10s",
                state.pty_binary.display()
            );
            let intent = crate::graph::parse_intent("version 2\nagent \"ownership-real-peer\" { host \"node\"; workspace \"/tmp\"; restart \"never\"; harness \"claude\" {} }", "node").unwrap();
            state
                .store
                .apply_internal(&intent, "ownership-real-process")
                .unwrap();
            let desired = state
                .store
                .desired_subjects_named(&[SUBJECT.into()])
                .unwrap()
                .remove(0);
            let member = desired.member.as_ref().unwrap();
            let control_path = root.path().join("control.sock");
            let listener = UnixListener::bind(&control_path).unwrap();
            let api = root.path().join("api.sock");
            let server_path = api.clone();
            let server_state = state.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let discover = server_path.with_extension("discovery");
                serve_unix_bound_with_ready(&server_path, &discover, router(server_state), || {
                    let _ = ready_tx.send(());
                })
                .await
                .unwrap();
            });
            ready_rx.await.unwrap();
            let runtime = st_runtime::PtyRuntime::new(state.pty_root.clone());
            let name = member.runtime_id.clone();
            let launch = st_runtime::Launch::Argv(
                command("wrapper")
                    .get_program()
                    .to_str()
                    .map(|binary| {
                        vec![
                            binary.into(),
                            "--exact".into(),
                            TEST.into(),
                            "--nocapture".into(),
                            "driver".into(),
                            "claude".into(),
                        ]
                    })
                    .unwrap(),
            );
            let mut env = BTreeMap::from([
                (ROLE.into(), "wrapper".into()),
                ("ST_AGENT".into(), SUBJECT.into()),
                (
                    "ST_TEST_OWNERSHIP_CONTROL".into(),
                    control_path.to_str().unwrap().into(),
                ),
                ("ST_TEST_OWNERSHIP_API".into(), api.to_str().unwrap().into()),
                ("ST_TEST_OWNERSHIP_DIAGNOSTICS".into(),
                    root.path().join("helper-stderr.log").to_str().unwrap().into()),
            ]);
            env.insert("HOME".into(), root.path().to_string_lossy().into_owned());
            let private_dir = crate::hooks::claude_agent_dir(
                &state.state_dir.join("drivers"),
                SUBJECT,
                &state.node,
            );
            let paths = st_drivers::driver_paths::Paths {
                root: state.state_dir.join("drivers"),
                agent_dir: private_dir,
                session_dir: root.path().join("sessions"),
            };
            env.extend(paths.environment(SUBJECT.strip_prefix("agent/").unwrap()));
            let start_runtime = runtime.clone();
            let start_name = name.clone();
            let cwd = root.path().to_owned();
            tokio::task::spawn_blocking(move || {
                start_runtime.spawn(
                    &start_name,
                    &launch,
                    &cwd,
                    &env,
                    None,
                    &BTreeMap::new(),
                    None,
                )
            })
            .await
            .unwrap()
            .unwrap();
            let observed = runtime
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|o| o.name == name)
                .unwrap();
            let incarnation = format!("{}:{}", observed.pid.unwrap(), observed.created_at.unwrap());
            let _owned = OwnedPty {
                runtime: runtime.clone(),
                name: name.clone(),
                incarnation: incarnation.clone(),
                diagnostics: root.path().join("helper-stderr.log"),
            };
            let dir = crate::hooks::claude_agent_dir(
                &state.state_dir.join("drivers"),
                SUBJECT,
                &state.node,
            );
            let (role, mut wrapper, wrapper_pid) = accept(&listener).await;
            assert_eq!(role, "wrapper");
            write(
                &mut wrapper,
                json!({"dir":dir,"incarnation":incarnation,"pty_session":name}),
            )
            .await;
            assert_eq!(read(&mut wrapper).await["ready"], true);
            let (role, mut provider, _) = accept(&listener).await;
            assert_eq!(role, "provider");
            assert_eq!(call(&mut provider, json!({"go":true})).await["ready"], true);
            let mut links = BTreeMap::new();
            let mut pids = BTreeMap::new();
            while links.len() < 2 {
                let (role, mut link, pid) = accept(&listener).await;
                if role == "nested" {
                    assert_eq!(call(&mut link, json!({"go":true})).await["ready"], true);
                }
                pids.insert(role.clone(), pid);
                links.insert(role, link);
            }
            while links.len() < 5 {
                let (role, link, pid) = accept(&listener).await;
                pids.insert(role.clone(), pid);
                links.insert(role, link);
            }
            for (kind, fields) in [
                (
                    "runtime.observed",
                    json!({"status":"running","runtime_id":name,"incarnation_id":incarnation}),
                ),
                (
                    "harness.observed",
                    json!({"state":"ready","driver":"claude","incarnation_id":incarnation}),
                ),
                // A stale graph diagnostic must not overcome the actual primary subscription.
                (
                    "harness.diagnostic",
                    json!({"code":"claude-channel-unattached","status":"blocked","driver":"claude","incarnation_id":incarnation}),
                ),
            ] {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: SUBJECT.into(),
                        kind: kind.into(),
                        actor: Some(SUBJECT.into()),
                        fields: serde_json::from_value(fields).unwrap(),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
            }
            // Legacy fixture cut: no witness, real kernel/PTY shape, no stream active yet.
            // Diagnostic sampling is observational: it cannot write a claim, create an epoch,
            // change attachment or charge recovery. No old admission is called by this probe.
            let witness = fs::read(dir.join("claude-primary-process.json")).unwrap();
            fs::remove_file(dir.join("claude-primary-process.json")).unwrap();
            let legacy = Fence::new(SUBJECT, &incarnation, "delivery");
            let cut = state.store.index().unwrap();
            let attempts_before = count(&state.store);
            for (role, expected) in [
                ("primary", "would-have-admitted"),
                ("secondary", "would-have-refused"),
            ] {
                let peer = NativeDeliveryPeer {
                    agent: SUBJECT.into(),
                    transport: "claude-channel",
                    pid: pids[role],
                    archives_inbox: false,
                };
                assert!(delivery_presence::begin_legacy_shape(&legacy, peer.pid));
                authority::capture_legacy_shape(&state, &legacy, &peer);
                let assessment =
                    delivery_presence::assess_current(SUBJECT, "claude", Some(&incarnation));
                assert!(
                    matches!(assessment.state, "unknown" | "stale"),
                    "shape must never fabricate healthy delivery"
                );
                assert!(assessment.reason.unwrap().contains(expected));
                assert!(!crate::api::claude_channel_attached(
                    &state.store,
                    SUBJECT,
                    &incarnation
                ));
                assert_eq!(state.store.index().unwrap(), cut);
                assert_eq!(count(&state.store), attempts_before);
            }
            fs::write(dir.join("claude-primary-process.json"), witness).unwrap();
            delivery_presence::record_ownership(&legacy, false);
            // Model a new daemon's empty process-local map, but do NOT replace physical capture.
            // The real selected PTY/stat/kernel chain is recaptured by the first actual API call.
            authority::primary_bindings()
                .lock()
                .unwrap()
                .remove(&(state.state_dir.clone(), SUBJECT.into()));
            let requested = Fence::new(SUBJECT, &incarnation, "delivery");
            if nested_first {
                for role in ["secondary", "stripped-secondary"] {
                    assert_eq!(
                        call(
                            links.get_mut(role).unwrap(),
                            json!({"op":"bind","fence":requested})
                        )
                        .await["ok"],
                        false
                    );
                    assert!(
                        !authority::primary_bindings()
                            .lock()
                            .unwrap()
                            .contains_key(&(state.state_dir.clone(), SUBJECT.into()))
                    );
                }
            }
            let bound = call(
                links.get_mut("primary").unwrap(),
                json!({"op":"bind","fence":requested}),
            )
            .await;
            assert_eq!(bound["ok"], true, "positive primary bind: {bound}");
            let fence: Fence = serde_json::from_value(bound["fence"].clone()).unwrap();
            assert_eq!(
                call(
                    links.get_mut("primary").unwrap(),
                    json!({"op":"open","fence":fence})
                )
                .await["ok"],
                true
            );
            assert_eq!(
                call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                true
            );
            assert!(crate::api::claude_channel_attached(
                &state.store,
                SUBJECT,
                &incarnation
            ));
            let title = call(
                &mut wrapper,
                json!({"op":"bind","fence":Fence::new(SUBJECT,&incarnation,"title")}),
            )
            .await;
            assert_eq!(title["ok"], true, "exact kernel wrapper title exception");
            let title: Fence = serde_json::from_value(title["fence"].clone()).unwrap();
            let captures = authority::primary_bindings()
                .lock()
                .unwrap()
                .get(&(state.state_dir.clone(), SUBJECT.into()))
                .cloned()
                .unwrap();
            assert_eq!(captures.1.pid, wrapper_pid);
            let before = count(&state.store);
            for _ in 0..16 {
                for role in ["secondary", "stripped-secondary"] {
                    assert_eq!(
                        call(
                            links.get_mut(role).unwrap(),
                            json!({"op":"bind","fence":fence})
                        )
                        .await["ok"],
                        false
                    );
                    assert_eq!(
                        call(
                            links.get_mut(role).unwrap(),
                            json!({"op":"open","fence":fence})
                        )
                        .await["ok"],
                        false
                    );
                    // The primary stays actively healthy while the bounded adversarial cohort runs.
                    assert_eq!(
                        call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                        true
                    );
                    state.store.check_mailbox(&fence).unwrap();
                    assert!(crate::api::claude_channel_attached(
                        &state.store,
                        SUBJECT,
                        &incarnation
                    ));
                    assert_eq!(count(&state.store), before);
                }
            }
            assert_eq!(
                call(
                    links.get_mut("secondary-title").unwrap(),
                    json!({"op":"bind","fence":title})
                )
                .await["ok"],
                false
            );
            assert_ne!(
                pids["secondary-title"], wrapper_pid,
                "a nested wrapper label is not its kernel peer PID"
            );
            for op in ["error", "close"] {
                for role in ["secondary", "stripped-secondary"] {
                    assert_eq!(
                        call(links.get_mut(role).unwrap(), json!({"op":op})).await["ok"],
                        true
                    );
                }
            }
            state.store.check_mailbox(&fence).unwrap();
            state.store.check_mailbox(&title).unwrap();
            assert!(crate::api::claude_channel_attached(
                &state.store,
                SUBJECT,
                &incarnation
            ));
            let recovery = crate::reconcile::Reconciler::new(
                state.store.clone(),
                Arc::new(crate::reconcile::NativeRuntime::new(
                    &state.state_dir,
                    Some(&state.pty_root),
                    &state.pty_binary,
                )),
                state.node.clone(),
                state.notify.clone(),
            );
            assert!(
                !recovery
                    .fixture_channel_recovery(&desired, &incarnation)
                    .unwrap()
            );
            assert_eq!(
                count(&state.store),
                before,
                "non-primary refusal/flood/EOF cannot charge primary recovery"
            );
            // REAL lock contention on an already admitted primary. Ordered pong confirms
            // each unavailable report was consumed without closing the WebSocket; no sleeps.
            use std::os::fd::AsRawFd as _;
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join("claude-primary-process.lock"))
                .unwrap();
            // SAFETY: this exact lock belongs only to the owned temporary fixture generation.
            assert_eq!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            for _ in 0..3 {
                assert_eq!(
                    call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                    true
                );
                assert!(crate::api::claude_channel_proof_pending(
                    &state.store,
                    SUBJECT,
                    &incarnation
                ));
                assert!(
                    !recovery
                        .fixture_channel_recovery(&desired, &incarnation)
                        .unwrap()
                );
                assert_eq!(count(&state.store), before);
                state.store.check_mailbox(&fence).unwrap();
            }
            let unknown = delivery_presence::assess_current(SUBJECT, "claude", Some(&incarnation));
            assert_eq!(unknown.state, "unknown");
            assert!(
                unknown
                    .reason
                    .unwrap()
                    .contains("suspect after three checks")
            );
            drop(lock);
            assert_eq!(
                call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                true
            );
            assert!(crate::api::claude_channel_attached(
                &state.store,
                SUBJECT,
                &incarnation
            ));
            // A budget fault and automatic request recorded BEFORE genuine reattachment must
            // not become a back door that stops this now-healthy primary incarnation.
            let token = state
                .store
                .selected_desired_token(SUBJECT)
                .unwrap()
                .unwrap();
            let mut ids = Vec::new();
            for index in 0..3 {
                let claim=state.store.append_claim(&ClaimInput {
                    subject:SUBJECT.into(),kind:"runtime.action.requested".into(),actor:Some("agent/st3/reconciler".into()),
                    fields:serde_json::from_value(json!({"action":"restart","operation":"claude-channel-recovery-window-v1",
                        "incarnation_id":incarnation,"reason":format!("retained fixture attempt {index}")})).unwrap(),
                    evidence:vec![token.clone()],expected_subject:None,idempotency_key:None,
                }).unwrap();
                ids.push(claim.id);
            }
            state
                .store
                .append_claim(&ClaimInput {
                    subject: SUBJECT.into(),
                    kind: "runtime.action.failed".into(),
                    actor: Some("agent/st3/reconciler".into()),
                    fields: serde_json::from_value(
                        json!({"action":"claude-channel-recovery","desired_token":token,
                    "incarnation_id":incarnation,"reason":"fixture parked budget"}),
                    )
                    .unwrap(),
                    evidence: ids,
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            // The SAME retained automatic request/fault must also be held while proof is
            // temporarily unknown, not settled as success or used to stop this live process.
            let pending_lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join("claude-primary-process.lock"))
                .unwrap();
            // SAFETY: the owned fixture's generation lock, not any installed seat's file.
            assert_eq!(
                unsafe { libc::flock(pending_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(
                call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                true
            );
            assert!(
                !recovery
                    .fixture_channel_recovery(&desired, &incarnation)
                    .unwrap()
            );
            assert!(
                !recovery
                    .fixture_requested_channel_restart(&desired, &incarnation)
                    .unwrap()
            );
            assert_eq!(count(&state.store), 3);
            drop(pending_lock);
            assert_eq!(
                call(links.get_mut("primary").unwrap(), json!({"op":"report"})).await["ok"],
                true
            );
            assert!(
                !recovery
                    .fixture_channel_recovery(&desired, &incarnation)
                    .unwrap()
            );
            assert!(
                !recovery
                    .fixture_requested_channel_restart(&desired, &incarnation)
                    .unwrap()
            );
            assert!(
                runtime
                    .snapshot()
                    .unwrap()
                    .iter()
                    .any(|r| r.name == name && r.status == "running")
            );
            assert!(crate::api::claude_channel_attached(
                &state.store,
                SUBJECT,
                &incarnation
            ));
            state.store.check_mailbox(&fence).unwrap();
            let before = count(&state.store);

            if secondary_closes_first {
                for role in ["secondary", "stripped-secondary"] {
                    assert_eq!(
                        call(links.get_mut(role).unwrap(), json!({"op":"quit"})).await["ok"],
                        true
                    );
                }
                assert!(crate::api::claude_channel_attached(
                    &state.store,
                    SUBJECT,
                    &incarnation
                ));
            }
            if nested_first {
                let mut closed: Value = serde_json::from_slice(
                    &fs::read(dir.join("claude-primary-process.json")).unwrap(),
                )
                .unwrap();
                closed["closed"] = json!(true);
                fs::write(
                    dir.join("claude-primary-process.json"),
                    serde_json::to_vec(&closed).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    call(
                        links.get_mut("primary").unwrap(),
                        json!({"op":"expect-refused"})
                    )
                    .await["ok"],
                    true
                );
            } else {
                assert_eq!(
                    call(links.get_mut("primary").unwrap(), json!({"op":"close"})).await["ok"],
                    true
                );
            }
            assert_eq!(
                call(links.get_mut("primary").unwrap(), json!({"op":"quit"})).await["ok"],
                true
            );
            if !secondary_closes_first {
                for role in ["secondary", "stripped-secondary"] {
                    assert_eq!(
                        call(links.get_mut(role).unwrap(), json!({"op":"quit"})).await["ok"],
                        true
                    );
                }
            }
            assert_eq!(count(&state.store), before);
            assert_eq!(
                call(
                    links.get_mut("secondary-title").unwrap(),
                    json!({"op":"quit"})
                )
                .await["ok"],
                true
            );
            assert_eq!(
                call(links.get_mut("nested").unwrap(), json!({"op":"quit"})).await["ok"],
                true
            );
            assert_eq!(call(&mut provider, json!({"op":"quit"})).await["ok"], true);
            assert_eq!(call(&mut wrapper, json!({"op":"quit"})).await["ok"], true);
            for path in [".claude/settings.json", ".claude/settings.local.json"] {
                assert_eq!(
                    fs::read(root.path().join(path)).unwrap(),
                    PLUGIN_SETTINGS.as_bytes()
                );
            }
            server.abort();
        }
    }
}
