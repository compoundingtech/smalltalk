#![cfg(unix)]
//! Setup uses only this HOME, state directory and sockets; no host daemon or service.

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    root: tempfile::TempDir,
    daemon_pid: Option<u32>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["home/.config/st3", "run", "archive", "bin"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        symlink("/usr/bin/env", root.path().join("bin/env")).unwrap();
        Self {
            root,
            daemon_pid: None,
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }

    fn command(&self, executable: &Path) -> Command {
        let mut command = st3::test_support::command(executable);
        command
            .env_clear()
            .env("HOME", self.path("home"))
            .env("USER", "ada")
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_STATE_HOME", self.path("home/.local/state"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("PATH", self.path("bin"))
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    fn cli(&self) -> Command {
        self.command(Path::new(env!("CARGO_BIN_EXE_st3-fixture")))
    }

    fn setup_config_only(&self, extra: &[&str]) -> Output {
        self.cli()
            .args([
                "setup",
                "--person",
                "ada",
                "--node",
                "studio",
                "--yes",
                "--install",
                "false",
                "--service",
                "false",
                "--start",
                "false",
            ])
            .args(extra)
            .output()
            .unwrap()
    }

    fn capture_pid(&mut self) -> u32 {
        let pid = st3::startup::read(&self.path("run/st3.sock"))
            .expect("setup started a live daemon")
            .pid;
        self.daemon_pid = Some(pid);
        pid
    }

    fn unpack_archive(&self) {
        let search = |name: &str| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(name))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("test needs {name}"))
        };
        fs::copy(env!("CARGO_BIN_EXE_st3-fixture"), self.path("archive/st3")).unwrap();
        fs::set_permissions(self.path("archive/st3"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(search("pty"), self.path("archive/pty")).unwrap();
        symlink("st3", self.path("archive/st")).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid {
            // This PID comes from our isolated startup receipt, never a host lookup.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn flags_merge_config_without_prompting_and_invalid_names_do_not_write() {
    let fixture = Fixture::new();
    let config = fixture.path("home/.config/st3/config.toml");
    fs::write(
        &config,
        "node = 'studio'\n[observations]\nretention = '2d'\n",
    )
    .unwrap();
    let text = success(&fixture.setup_config_only(&[]));
    assert!(text.contains("without permission prompts"));
    let bytes = fs::read_to_string(&config).unwrap();
    let table: toml::Table = toml::from_str(&bytes).unwrap();
    assert_eq!(table["person"].as_str(), Some("person/ada"));
    assert_eq!(table["node"].as_str(), Some("studio"));
    assert_eq!(table["observations"]["retention"].as_str(), Some("2d"));
    success(&fixture.setup_config_only(&[]));
    assert_eq!(fs::read_to_string(&config).unwrap(), bytes);
    for name in ["local", "studio's", "two words", "a/b"] {
        let output = fixture
            .cli()
            .args(["setup", "--person", "ada", "--node", name, "--yes"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_to_string(&config).unwrap(), bytes);
    }
}

#[test]
fn harness_probe_uses_login_path_order_and_flags_without_starting_a_seat() {
    let fixture = Fixture::new();
    let none = success(&fixture.setup_config_only(&[]));
    assert!(none.contains("No supported harness is installed"), "{none}");
    for harness in ["omp", "codex"] {
        let path = fixture.path(&format!("bin/{harness}"));
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let selected = success(&fixture.setup_config_only(&[]));
    assert_eq!(selected.matches("Selected harness: codex").count(), 1);
    assert!(!selected.contains("Which harness"));
    assert!(
        success(&fixture.setup_config_only(&["--harness", "omp"]))
            .contains("Selected harness: omp")
    );
    assert!(
        success(&fixture.setup_config_only(&["--harness", "none"]))
            .contains("Harness setup skipped")
    );
    let missing = fixture.setup_config_only(&["--harness", "claude"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("not installed on the daemon's login PATH")
    );
    let claude = fixture.path("bin/claude");
    fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let inline =
        success(&fixture.setup_config_only(&["--harness", "claude", "--claude-channel", "false"]));
    assert_eq!(inline.matches("Selected harness: claude").count(), 1);
    assert!(inline.contains("inline server:st3 development channel"));
    assert!(!fixture.path("home/.claude/plugins").exists());
    assert!(
        !fixture
            .path("home/.local/state/st3/claims.sqlite3")
            .exists()
    );
}

#[test]
fn setup_installs_integrations_for_all_found_harnesses_and_decline_writes_none() {
    let fixture = Fixture::new();
    for name in ["codex", "omp", "pi", "opencode"] {
        let path = fixture.path(&format!("bin/{name}"));
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let skipped =
        success(&fixture.setup_config_only(&["--harness", "codex", "--integrations", "false"]));
    assert!(skipped.contains("Integrations: skipped"), "{skipped}");
    assert!(!fixture.path("home/.agents/skills/st/SKILL.md").exists());
    assert!(!fixture.path("home/.local/state/st3/hooks").exists());
    let installed =
        success(&fixture.setup_config_only(&["--harness", "codex", "--integrations", "true"]));
    for name in ["codex", "omp", "pi", "opencode"] {
        assert!(
            installed.contains(&format!("Integration: {name} installed")),
            "{installed}"
        );
    }
    assert_eq!(installed.matches("Selected harness: codex").count(), 1);
    assert_eq!(
        fs::read_to_string(fixture.path("home/.agents/skills/st/SKILL.md")).unwrap(),
        st3::skill::SKILL
    );
    let hooks = st3::hooks::set_dir(&fixture.path("home/.local/state/st3/hooks"));
    st3::hooks::verify(&hooks).unwrap();
    let skill = fixture.path("home/.agents/skills/st/SKILL.md");
    let modified = fs::metadata(&skill).unwrap().modified().unwrap();
    success(&fixture.setup_config_only(&["--harness", "omp", "--integrations", "true"]));
    assert_eq!(fs::metadata(skill).unwrap().modified().unwrap(), modified);
    assert!(
        !fixture
            .path("home/.local/state/st3/claims.sqlite3")
            .exists()
    );
}

#[test]
fn scripts_can_create_a_custom_config_and_missing_answers_never_prompt() {
    let fixture = Fixture::new();
    let output = fixture.cli().arg("setup").output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("setup never prompts without a terminal")
    );
    assert!(!fixture.path("home/.config/st3/config.toml").exists());
    success(&fixture.setup_config_only(&["--config", "custom/config.toml"]));
    assert!(fixture.path("custom/config.toml").is_file());
    assert!(!fixture.path("home/.config/st3/config.toml").exists());
}

#[test]
fn seats_cannot_run_setup_or_open_a_plain_st_prompt() {
    let fixture = Fixture::new();
    let output = fixture
        .cli()
        .env("ST_AGENT", "agent/example/helper")
        .args(["setup", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable inside an agent seat"));
    let output = fixture
        .cli()
        .env("ST_AGENT", "agent/example/helper")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.path("home/.config/st3/config.toml").exists());
}

#[test]
fn archive_setup_installs_st_and_starts_one_isolated_daemon_without_gh_or_harness() {
    if st3::test_support::supervise_test() {
        return;
    }
    let mut fixture = Fixture::new();
    // Only env is needed by the fixture login-shell capture. There are no harnesses or gh.
    fixture.unpack_archive();
    let output = fixture
        .command(&fixture.path("archive/st3"))
        .args([
            "setup",
            "--person",
            "ada",
            "--node",
            "studio",
            "--yes",
            "--service",
            "false",
        ])
        .output()
        .unwrap();
    let text = success(&output);
    let first_pid = fixture.capture_pid();
    assert!(text.contains("It stops when you reboot"), "{text}");
    assert!(
        text.contains("export PATH=\"$HOME/.local/bin:$PATH\""),
        "{text}"
    );
    assert_eq!(
        fs::read_link(fixture.path("home/.local/bin/st")).unwrap(),
        Path::new("st3")
    );
    assert!(fixture.path("home/.local/bin/pty").is_file());
    assert!(
        !fixture
            .path("home/.config/systemd/user/st3.service")
            .exists()
    );
    let config: toml::Table =
        toml::from_str(&fs::read_to_string(fixture.path("home/.config/st3/config.toml")).unwrap())
            .unwrap();
    assert_eq!(config["node"].as_str(), Some("studio"));
    let before = Instant::now();
    let missing = fixture
        .cli()
        .args(["agents", "new", "fixture-missing", "--harness", "claude"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("not installed on the daemon's login PATH")
    );
    assert!(
        before.elapsed() < Duration::from_secs(5),
        "missing harness must fail before waiting for a seat"
    );
    let agents = fixture
        .cli()
        .args(["agents", "ls", "--json"])
        .output()
        .unwrap();
    assert!(
        !success(&agents).contains("fixture-missing"),
        "missing binary must not publish a restarting seat"
    );
    let output = fixture
        .command(&fixture.path("home/.local/bin/st"))
        .args(["setup", "--yes", "--service", "false"])
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        fixture.capture_pid(),
        first_pid,
        "setup does not launch another daemon"
    );
    let output = fixture
        .cli()
        .args(["setup", "--person", "ada", "--node", "other", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("this store belongs to `studio`"));
}

#[test]
fn plain_st_first_run_asks_names_starts_daemon_and_opens_home() {
    if st3::test_support::supervise_test() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(fixture.path("archive/st"));
    command.env_clear();
    for (name, value) in [
        ("HOME", fixture.path("home")),
        ("PATH", fixture.path("bin")),
        ("XDG_CONFIG_HOME", fixture.path("home/.config")),
        ("XDG_STATE_HOME", fixture.path("home/.local/state")),
        ("XDG_RUNTIME_DIR", fixture.path("run")),
    ] {
        command.env(name, value);
    }
    command.env("USER", "ada");
    command.env("TERM", "xterm-256color");
    command.cwd(fixture.root.path());
    struct TerminalChild(Box<dyn portable_pty::Child + Send + Sync>);
    impl Drop for TerminalChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = TerminalChild(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(45);
    for (prompt, answer) in [
        ("Your name", "ada\n"),
        ("This machine's name", "studio\n"),
        ("Keep st running in the background", "n\n"),
        ("Nothing needs you right now.", "\u{11}"),
    ] {
        while !String::from_utf8_lossy(&output).contains(prompt) {
            assert!(
                Instant::now() < deadline,
                "missing {prompt}: {}",
                String::from_utf8_lossy(&output[..output.len().min(4096)])
            );
            if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(100)) {
                output.extend(bytes);
            }
            if let Some(readiness) = st3::startup::read(&fixture.path("run/st3.sock")) {
                fixture.daemon_pid = Some(readiness.pid);
            }
        }
        writer.write_all(answer.as_bytes()).unwrap();
        writer.flush().unwrap();
    }
    while child.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "plain st did not quit after Ctrl+Q"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    reading.join().unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("It stops when you reboot"), "{text}");
    fixture.capture_pid();
    let table: toml::Table =
        toml::from_str(&fs::read_to_string(fixture.path("home/.config/st3/config.toml")).unwrap())
            .unwrap();
    assert_eq!(table["person"].as_str(), Some("person/ada"));
    assert_eq!(table["node"].as_str(), Some("studio"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_publication_is_graph_decided_and_preserves_stopped_expert() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    let state_dir = fixture.path("home/.local/state/st3");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        fixture.path("home/.config/st3/config.toml"),
        "node = 'studio'\n",
    )
    .unwrap();
    let store = std::sync::Arc::new(
        st3::store::Store::open(&state_dir.join("claims.sqlite3"), "studio").unwrap(),
    );
    let state = st3::api::AppState {
        store: store.clone(),
        notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        event_notify: tokio::sync::watch::channel(0_u64).0,
        node: "studio".into(),
        state_dir: state_dir.clone(),
        pty_root: state_dir.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    // These declarations use a simulated provider; no host provider or daemon is touched.
    let router = st3::api::router(state).layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            if request.uri().path() == "/v1/harnesses" {
                return axum::response::IntoResponse::into_response(axum::Json(
                    serde_json::json!({
                        "api_version": "st3.v1", "value": ["codex"]
                    }),
                ));
            }
            next.run(request).await
        },
    ));
    let socket = fixture.path("run/st3.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, router).await });
    let client = st3::client::Client::unix(&socket);
    for _ in 0..100 {
        if client.get::<serde_json::Value>("/v1/health").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let setup = || {
        let mut command = fixture.cli();
        command.args([
            "setup",
            "--person",
            "ada",
            "--node",
            "studio",
            "--yes",
            "--install",
            "false",
            "--service",
            "false",
            "--start",
            "false",
        ]);
        command
    };
    let run = |mut command: Command| async move {
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    };
    let (first, simultaneous) = tokio::join!(run(setup()), run(setup()));
    let text = success(&first) + &success(&simultaneous);
    assert!(text.contains("Started mission/st/onboarding"), "{text}");
    let history: serde_json::Value = client
        .get("/v1/mission-overview?mission=st%2Fonboarding")
        .await
        .unwrap();
    assert_eq!(history["total_runs"], 1);
    let initial = store.mission_run("st/onboarding").unwrap().unwrap();
    assert_eq!(initial.requester, "person/ada");
    let token = store
        .selected_desired_token("agent/st/expert")
        .unwrap()
        .unwrap();
    let claim: st3::model::ClaimRecord = client
        .get(&format!("/v1/claims/by-id/{token}"))
        .await
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some("person/ada"));
    let declarations = store.desired_subjects().unwrap();
    let expert = declarations
        .iter()
        .find(|subject| subject.subject == "agent/st/expert")
        .unwrap();
    assert!(
        serde_json::to_string(expert)
            .unwrap()
            .contains("--dangerously-bypass-approvals-and-sandbox")
    );
    let guides: st3::model::DocumentListResponse = client
        .get("/v1/documents?name=doc%2Fst%2Fguide")
        .await
        .unwrap();
    assert_eq!(guides.items.len(), 1);
    let spec = store
        .mission_spec("st/onboarding", Some(&initial.revision))
        .unwrap()
        .unwrap();
    assert!(
        spec.constraints
            .iter()
            .any(|constraint| constraint.contains(&guides.items[0].hash))
    );
    let mut stop = fixture.cli();
    stop.args(["agents", "stop", "agent/st/expert", "--as", "person/ada"]);
    success(&run(stop).await);
    let stopped = store.selected_desired_token("agent/st/expert").unwrap();
    let mut cancel = fixture.cli();
    cancel.args([
        "missions",
        "cancel",
        "mission-run/st/onboarding",
        "--reason",
        "fixture completed",
        "--as",
        "person/ada",
    ]);
    success(&run(cancel).await);
    // This API fixture has no reconciler to finish cancellation cleanup.
    store
        .set_mission_run_state(
            "st/onboarding",
            "cancelled",
            "terminal",
            Some("fixture cleanup completed"),
        )
        .unwrap();
    let index = store.index().unwrap();
    success(&run(setup()).await);
    assert_eq!(
        store.selected_desired_token("agent/st/expert").unwrap(),
        stopped
    );
    assert_eq!(
        store.index().unwrap(),
        index,
        "ordinary setup must not resurrect the expert or restart finished onboarding"
    );
    let mut rerun = setup();
    rerun.arg("--onboarding");
    assert!(success(&run(rerun).await).contains("Started mission/st/onboarding"));
    let history: serde_json::Value = client
        .get("/v1/mission-overview?mission=st%2Fonboarding")
        .await
        .unwrap();
    assert_eq!(history["total_runs"], 2);
    assert_ne!(
        store.selected_desired_token("agent/st/expert").unwrap(),
        stopped
    );
    server.abort();
}
