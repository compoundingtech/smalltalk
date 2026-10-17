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
        let root = st3::test_support::canonical_tempdir();
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
        self.command(Path::new(test_env!("CARGO_BIN_EXE_st3-fixture")))
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
        let pid = daemon_pid(&self.path("run/st3.sock")).expect("setup started a live daemon");
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
        fs::copy(test_env!("CARGO_BIN_EXE_st3-fixture"), self.path("archive/st3")).unwrap();
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

/// The pid of the daemon serving `socket`. Linux daemons publish a startup receipt; the receipt
/// does not exist on other platforms, where the socket's own peer is the daemon.
fn daemon_pid(socket: &Path) -> Option<u32> {
    if cfg!(target_os = "linux") {
        return st3::startup::read(socket).map(|readiness| readiness.pid);
    }
    #[cfg(target_os = "macos")]
    use std::os::fd::AsRawFd as _;
    let stream = std::os::unix::net::UnixStream::connect(socket).ok()?;
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    #[cfg(target_os = "macos")]
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let status = {
        let _ = (&stream, &mut pid, &mut length);
        -1
    };
    (status == 0 && pid > 0).then_some(pid as u32)
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

#[cfg(target_os = "linux")]
#[test]
fn detached_daemon_receives_the_selected_reader_cache() {
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    let output = fixture
        .command(&fixture.path("archive/st"))
        .env("SMALLCLAIMS_READ_CACHE_KIB", "1024")
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
        ])
        .output()
        .unwrap();
    let stdout = success(&output);
    assert!(stdout.contains("Read cache: 1024 KiB per reader."));
    let pid = fixture.capture_pid();
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(
        environ
            .split(|byte| *byte == 0)
            .any(|value| value == b"SMALLCLAIMS_READ_CACHE_KIB=1024")
    );
}

#[cfg(target_os = "linux")]
fn instance_command(fixture: &Fixture, name: Option<&str>) -> Command {
    let mut command = fixture.command(&fixture.path("archive/st"));
    if let Some(name) = name {
        command.args(["--instance", name]);
    }
    command
}

#[cfg(target_os = "linux")]
fn setup_without_service(fixture: &Fixture, name: Option<&str>) -> Output {
    instance_command(fixture, name)
        .args([
            "setup", "--person", "ada", "--yes", "--install", "false", "--service", "false",
            "--harness", "none",
        ])
        .output()
        .unwrap()
}

#[cfg(target_os = "linux")]
fn erase(fixture: &Fixture, name: Option<&str>) -> Output {
    instance_command(fixture, name)
        .args(["uninstall", "--yes", "--erase-local-graph"])
        .output()
        .unwrap()
}

/// An instance whose person declined the service runs a plain `st3 up`. Uninstall stops that
/// daemon itself, and only it; the same situation in the default install is still refused.
#[cfg(target_os = "linux")]
#[test]
fn uninstall_stops_an_instance_daemon_that_runs_without_a_service() {
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    let instance_socket = fixture.path("home/.st-instance/kept/run/st3.sock");
    success(&setup_without_service(&fixture, Some("kept")));
    let pid = st3::startup::read(&instance_socket)
        .expect("instance setup started a live daemon")
        .pid;
    fixture.daemon_pid = Some(pid);
    // A second daemon in the default install must not be touched by the instance's uninstall.
    success(&setup_without_service(&fixture, None));
    let default_pid = st3::startup::read(&fixture.path("run/st3.sock"))
        .expect("default setup started a live daemon")
        .pid;
    assert_ne!(pid, default_pid);

    let refused = erase(&fixture, None);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("stop st3 up"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(st3::startup::read(&fixture.path("run/st3.sock")).is_some());

    let stdout = success(&erase(&fixture, Some("kept")));
    assert!(stdout.contains("stopped\t"), "{stdout}");
    assert!(stdout.contains("uninstalled"), "{stdout}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists()
            || fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.contains(") Z")),
        "the instance daemon is still running"
    );
    assert!(!fixture.path("home/.st-instance").exists());
    assert!(
        st3::startup::read(&fixture.path("run/st3.sock")).is_some(),
        "the default install's daemon must keep running"
    );
    unsafe {
        libc::kill(default_pid as libc::pid_t, libc::SIGTERM);
    }
}

#[test]
fn archive_setup_records_its_files_for_uninstall_in_the_selected_data_home() {
    if st3::test_support::supervise_test() {
        return;
    }
    for custom_data_home in [false, true] {
        let fixture = Fixture::new();
        fixture.unpack_archive();
        let data = if custom_data_home {
            fixture.path("custom-data")
        } else {
            fixture.path("home/.local/share")
        };
        let run = |executable: &Path, args: &[&str]| {
            let mut command = fixture.command(executable);
            if custom_data_home {
                command.env("XDG_DATA_HOME", &data);
            }
            command.args(args).output().unwrap()
        };
        success(&run(
            &fixture.path("archive/st"),
            &[
                "setup",
                "--person",
                "ada",
                "--node",
                "studio",
                "--yes",
                "--service",
                "false",
                "--start",
                "false",
            ],
        ));
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(data.join("st3/install.json")).unwrap()).unwrap();
        for name in ["st3", "st", "pty"] {
            let installed = fixture.path(&format!("home/.local/bin/{name}"));
            assert!(
                manifest["files"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(installed))
            );
        }
        let installed = fixture.path("home/.local/bin/st");
        if custom_data_home {
            success(&run(
                &installed,
                &[
                    "uninstall",
                    "--yes",
                    "--erase-local-graph",
                    "--no-service",
                    "--keep-binaries",
                ],
            ));
            for name in ["st3", "st", "pty"] {
                assert!(fixture.path(&format!("home/.local/bin/{name}")).exists());
            }
            // Setup is the archive installer again, so it can record this second install.
            success(&run(
                &fixture.path("archive/st"),
                &[
                    "setup",
                    "--person",
                    "ada",
                    "--node",
                    "studio",
                    "--yes",
                    "--service",
                    "false",
                    "--start",
                    "false",
                ],
            ));
        }
        let dry = success(&run(&installed, &["uninstall", "--dry-run"]));
        assert!(!dry.contains("no release install manifest"), "{dry}");
        for name in ["st3", "st", "pty"] {
            assert!(
                dry.contains(&format!(
                    "remove\t{}",
                    fixture.path(&format!("home/.local/bin/{name}")).display()
                )),
                "{dry}"
            );
        }
        success(&run(
            &installed,
            &["uninstall", "--yes", "--erase-local-graph", "--no-service"],
        ));
        for name in ["st3", "st", "pty"] {
            assert!(
                fs::symlink_metadata(fixture.path(&format!("home/.local/bin/{name}"))).is_err()
            );
            assert!(
                fixture.path(&format!("archive/{name}")).exists(),
                "source archive is preserved"
            );
        }
        assert!(!data.join("st3").exists());
    }
}

#[test]
fn setup_does_not_claim_managed_executables_or_no_install_runs() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    fixture.unpack_archive();
    success(&fixture.setup_config_only(&[]));
    assert!(!fixture.path("home/.local/share/st3/install.json").exists());
    let record = fixture.path("home/.local/share/st3/install.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(&record, b"existing managed installation record").unwrap();
    let output = fixture
        .command(&fixture.path("archive/st"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.path("archive").display(),
                fixture.path("bin").display()
            ),
        )
        .args(["setup", "--yes", "--service", "false", "--start", "false"])
        .output()
        .unwrap();
    success(&output);
    assert!(!fixture.path("home/.local/bin/st3").exists());
    assert_eq!(
        fs::read(&record).unwrap(),
        b"existing managed installation record"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn removing_services_skips_absent_replication_but_preserves_installed_unit_errors() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    fs::write(
        fixture.path("bin/systemctl"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/manager.log"
if [ "$2" = show ]; then
    if [ -f "$HOME/.config/systemd/user/$3" ] || { [ "$3" = st3-replication.service ] && [ -f "$HOME/loaded-replication" ]; }; then
        printf 'LoadState=loaded\nActiveState=active\nSubState=running\n'
        exit 0
    fi
    printf 'LoadState=not-found\nActiveState=inactive\nSubState=dead\n'
    exit 4
fi
case "$*" in
    *st3-replication.service*) echo 'replication unit denied' >&2; exit 7;;
    *st3.service*) echo 'Unit not loaded' >&2; exit 5;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(
        fixture.path("bin/systemctl"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let output = fixture
        .cli()
        .args(["service", "uninstall"])
        .output()
        .unwrap();
    success(&output);
    assert!(output.stderr.is_empty());
    let log = fixture.path("home/manager.log");
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .ends_with("--user daemon-reload\n")
    );
    fs::write(&log, "").unwrap();
    let unit = fixture.path("home/.config/systemd/user/st3-replication.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "installed replication unit").unwrap();
    for file_present in [true, false] {
        if !file_present {
            fs::remove_file(&unit).unwrap();
            fs::write(fixture.path("home/loaded-replication"), "loaded service").unwrap();
        }
        let output = fixture
            .cli()
            .args(["service", "uninstall"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("replication unit denied"));
        assert_eq!(unit.exists(), file_present);
        assert!(!fs::read_to_string(&log).unwrap().contains("daemon-reload"));
    }
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
    // A config that existed without an access mode keeps reading as bypass, and says so.
    assert!(text.contains("Access mode: bypass. Agents run without permission prompts."), "{text}");
    let bytes = fs::read_to_string(&config).unwrap();
    assert!(!bytes.contains("access_mode"), "{bytes}");
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
fn a_config_setup_creates_records_auto_and_an_existing_one_is_not_given_a_mode() {
    let fixture = Fixture::new();
    let config = fixture.path("home/.config/st3/config.toml");
    let text = success(&fixture.setup_config_only(&[]));
    assert!(text.contains("Access mode: auto. Agents use their provider's managed permission checks."), "{text}");
    assert!(!text.contains("without permission prompts"), "{text}");
    let table: toml::Table = toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(table["access_mode"].as_str(), Some("auto"));
    // Repeating setup keeps the recorded mode.
    let again = success(&fixture.setup_config_only(&[]));
    assert!(again.contains("Access mode: auto."), "{again}");
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
    assert!(success(&fixture.setup_config_only(&["--harness", "omp"])).contains("Selected harness: omp"));
    assert!(success(&fixture.setup_config_only(&["--harness", "none"])).contains("Harness setup skipped"));
    let missing = fixture.setup_config_only(&["--harness", "claude"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not installed on the daemon's login PATH"));
    let claude = fixture.path("bin/claude");
    fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let inline = success(&fixture.setup_config_only(&["--harness", "claude", "--claude-channel", "false"]));
    assert_eq!(inline.matches("Selected harness: claude").count(), 1);
    assert!(inline.contains("inline server:st3 development channel"));
    assert!(!fixture.path("home/.claude/plugins").exists());
    assert!(!fixture.path("home/.local/state/st3/claims.sqlite3").exists());
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
fn an_instance_setup_installs_below_the_instance_and_leaves_the_default_install_alone() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    fixture.unpack_archive();
    let text = success(
        &fixture
            .command(&fixture.path("archive/st3"))
            .args([
                "--instance",
                "demo",
                "setup",
                "--person",
                "ada",
                "--yes",
                "--service",
                "false",
                "--start",
                "false",
            ])
            .output()
            .unwrap(),
    );
    let instance = fixture.path("home/.st-instance/demo");
    assert_eq!(fs::read_link(instance.join("bin/st")).unwrap(), Path::new("st3"));
    assert!(instance.join("bin/st3").is_file() && instance.join("bin/pty").is_file());
    assert!(instance.join("config/st3/config.toml").is_file());
    assert!(text.contains("This instance's commands are in"), "{text}");
    assert!(!text.contains("export PATH"), "{text}");
    // The default install's places stay exactly as the fixture made them.
    assert!(!fixture.path("home/.local").exists());
    assert!(!fixture.path("home/.config/st3/config.toml").exists());
    assert!(!fixture.path("home/.config/systemd").exists());
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
    let missing = fixture.cli().args(["agents", "new", "fixture-missing", "--harness", "claude"]).output().unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not installed on the daemon's login PATH"));
    assert!(before.elapsed() < Duration::from_secs(5), "missing harness must fail before waiting for a seat");
    // Socket readiness is not a roster publication; this fixture explicitly needs one.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(st3_client::Client::unix(fixture.path("run/st3.sock")).agents_list_after(None, None, false, 0))
        .expect("the fixture must publish its initial roster");
    let agents = fixture.cli().args(["agents", "ls", "--json"]).output().unwrap();
    assert!(!success(&agents).contains("fixture-missing"), "missing binary must not publish a restarting seat");
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
fn plain_st_first_run_shows_the_setup_screen_starts_daemon_and_opens_home() {
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
    // The checklist screen, driven by keys: the machine's name is cleared and typed, "Start st when
    // I log in" is unticked, and the one action is pressed. Without any agent installed it ends
    // by itself and the interface opens.
    for (prompt, answer) in [
        ("Smalltalk setup", "\t\u{15}studio\t \t\r"),
        (stui::HOME_IDLE_TEXT, "\u{11}"),
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
            if let Some(pid) = daemon_pid(&fixture.path("run/st3.sock")) {
                fixture.daemon_pid = Some(pid);
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

/// A daemon API on the fixture's socket whose provider list is simulated: only codex.
async fn onboarding_api(
    fixture: &Fixture,
) -> (
    std::sync::Arc<st3::store::Store>,
    st3::client::Client,
    tokio::task::JoinHandle<()>,
) {
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
        owning_person: None,
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
    let server = tokio::spawn(async move {
        let _ = st3::api::serve_unix(&server_socket, router).await;
    });
    let client = st3::client::Client::unix(&socket);
    for _ in 0..100 {
        if client.get::<serde_json::Value>("/v1/health").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (store, client, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_publication_is_graph_decided_and_preserves_stopped_assistant() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    let (store, client, server) = onboarding_api(&fixture).await;
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
        .selected_desired_token("agent/st/assistant")
        .unwrap()
        .unwrap();
    let claim: st3::model::ClaimRecord = client
        .get(&format!("/v1/claims/by-id/{token}"))
        .await
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some("person/ada"));
    let declarations = store.desired_subjects().unwrap();
    let assistant = declarations
        .iter()
        .find(|subject| subject.subject == "agent/st/assistant")
        .unwrap();
    // Permissions come from the install access mode when the driver starts the harness, so
    // the declaration pins no permission flag of its own.
    let declared = serde_json::to_string(assistant).unwrap();
    assert!(!declared.contains("--dangerously-bypass-approvals-and-sandbox"), "{declared}");
    assert!(!declared.contains("--dangerously-skip-permissions"), "{declared}");
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
    stop.args(["agents", "stop", "agent/st/assistant", "--as", "person/ada"]);
    success(&run(stop).await);
    let stopped = store.selected_desired_token("agent/st/assistant").unwrap();
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
        store.selected_desired_token("agent/st/assistant").unwrap(),
        stopped
    );
    assert_eq!(
        store.index().unwrap(),
        index,
        "ordinary setup must not resurrect the Assistant or restart finished onboarding"
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
        store.selected_desired_token("agent/st/assistant").unwrap(),
        stopped
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_assistant_asks_first_and_an_existing_setup_never_starts_it_on_its_own() {
    if st3::test_support::supervise_test() {
        return;
    }
    let fixture = Fixture::new();
    let (store, client, server) = onboarding_api(&fixture).await;
    let setup = |extra: &[&str]| {
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
        command.args(extra);
        command
    };
    let run = |mut command: Command| async move {
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    };
    // The first run asks, naming the account it uses; declining publishes nothing.
    let declined = success(&run(setup(&["--assistant", "false"])).await);
    assert!(
        declined.contains("agent that runs on your own codex account, so it uses your codex usage and quota"),
        "{declined}"
    );
    assert!(declined.contains("Not started. Run st setup --onboarding"), "{declined}");
    assert!(store.mission_run("st/onboarding").unwrap().is_none());
    assert!(store.selected_desired_token("agent/st/assistant").unwrap().is_none());
    // The configuration exists now: ordinary setup, with the daemon answering and no history,
    // still does not start it.
    let existing = success(&run(setup(&[])).await);
    assert!(existing.contains("not started on its own"), "{existing}");
    assert!(store.mission_run("st/onboarding").unwrap().is_none());
    assert!(store.selected_desired_token("agent/st/assistant").unwrap().is_none());
    // Asking for it by name starts it and says what was created and how to stop it.
    let explicit = success(&run(setup(&["--onboarding"])).await);
    assert!(explicit.contains("Created: the agent agent/st/assistant (codex"), "{explicit}");
    assert!(
        explicit.contains("Stop it with: st agents stop agent/st/assistant --as person/ada"),
        "{explicit}"
    );
    // An explicit start is a distinct run, counted in the mission's history.
    let history: serde_json::Value = client
        .get("/v1/mission-overview?mission=st%2Fonboarding")
        .await
        .unwrap();
    assert_eq!(history["total_runs"], 1, "{history}");
    server.abort();
}

/// Run `command` in a PTY and play `steps` at it: for each, wait until the screen has shown
/// `prompt`, then type `keys`. Returns the exit status text and all the output.
fn play(
    mut command: portable_pty::CommandBuilder,
    steps: &[(&str, &str)],
    fixture: &mut Fixture,
) -> (bool, String) {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 50,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
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
    let mut child = pair.slave.spawn_command(command).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(60);
    for (prompt, keys) in steps {
        while !String::from_utf8_lossy(&output).contains(prompt) {
            assert!(
                Instant::now() < deadline,
                "missing {prompt}: {}",
                String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..])
            );
            if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(100)) {
                output.extend(bytes);
            }
            if let Some(readiness) = st3::startup::read(&fixture.path("run/st3.sock")) {
                fixture.daemon_pid = Some(readiness.pid);
            }
        }
        writer.write_all(keys.as_bytes()).unwrap();
        writer.flush().unwrap();
    }
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(50)) {
            output.extend(bytes);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "the command did not finish: {}",
                String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..])
            );
        }
    };
    drop(writer);
    drop(pair.master);
    let _ = reading.join();
    while let Ok(bytes) = receiver.try_recv() {
        output.extend(bytes);
    }
    (status.success(), String::from_utf8_lossy(&output).into_owned())
}

fn fake(fixture: &Fixture, name: &str, script: &str) {
    let path = fixture.path("bin").join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_setup_screen_sets_up_each_agent_on_its_own_and_stores_an_unticked_codex_box() {
    if st3::test_support::supervise_test() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    // Codex has never been signed in: its own login writes the file setup looks for. Pi starts but
    // keeps its sign-in where st cannot read it. Omp is installed and broken.
    // The fake programs run with only the fixture's PATH, so they use shell built-ins alone.
    fs::create_dir_all(fixture.path("home/.codex")).unwrap();
    fake(
        &fixture,
        "codex",
        "case \"$1\" in --version) echo codex-cli 0.0.0-test;; login) echo '{\"tokens\":{}}' > \"$HOME/.codex/auth.json\";; esac",
    );
    fake(&fixture, "pi", "echo pi 0.0.0-test");
    fake(&fixture, "omp", "echo 'cannot load libomp.so.1' >&2; exit 3");
    let mut command = portable_pty::CommandBuilder::new(fixture.path("archive/st"));
    command.args([
        "setup", "--person", "ada", "--node", "studio", "--install", "false", "--start", "false",
        "--service", "false",
    ]);
    // Name, machine, start-at-login, the three found agents, then the Codex box (ticked by
    // default): Space unticks it, Tab moves to the action and Enter presses it.
    let (success, output) = play(
        command,
        &[
            ("Smalltalk setup", "\t\t\t\t\t\t \t\r"),
            ("Codex is asking you to sign in", ""),
            ("did not finish", "\x1b"),
        ],
        &mut fixture,
    );
    let text = String::from_utf8_lossy(output.as_bytes());
    assert!(text.contains("Let Codex work without asking"), "{text}");
    // The terminal went to Codex for its sign-in and came back to the same screen.
    assert!(text.contains("Codex is asking you to sign in"), "{text}");
    assert!(text.contains("Ready. codex-cli 0.0.0-test") || text.contains("codex starts (codex-cli 0.0.0-test)"), "{text}");
    assert!(text.contains("Sign-in is checked when the first agent starts"), "{text}");
    // A broken agent is told whole, and does not stop the others.
    assert!(text.contains("cannot load libomp.so.1"), "{text}");
    assert!(text.contains("Codex: ready."), "{text}");
    assert!(text.contains("Pi: ready."), "{text}");
    assert!(text.contains("Omp: failed."), "{text}");
    assert!(success || text.contains("Omp: failed."), "{text}");
    let table: toml::Table =
        toml::from_str(&fs::read_to_string(fixture.path("home/.config/st3/config.toml")).unwrap())
            .unwrap();
    assert_eq!(table["person"].as_str(), Some("person/ada"));
    assert_eq!(table["node"].as_str(), Some("studio"));
    assert_eq!(table["codex_work_without_asking"].as_bool(), Some(false));
}

#[test]
fn leaving_the_setup_screen_before_the_button_changes_nothing() {
    if st3::test_support::supervise_test() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    let mut command = portable_pty::CommandBuilder::new(fixture.path("archive/st"));
    command.args(["setup", "--install", "false", "--start", "false", "--service", "false"]);
    let (success, output) = play(command, &[("Smalltalk setup", "\x1b")], &mut fixture);
    assert!(!success, "{output}");
    assert!(output.contains("setup was cancelled; nothing was changed"), "{output}");
    assert!(!fixture.path("home/.config/st3/config.toml").exists());
    assert!(!fixture.path("home/.local").exists(), "no state was created");
}

#[test]
fn skipping_sets_up_st_and_no_agent() {
    if st3::test_support::supervise_test() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.unpack_archive();
    fake(&fixture, "pi", "echo pi 0.0.0-test");
    let mut command = portable_pty::CommandBuilder::new(fixture.path("archive/st"));
    command.args([
        "setup", "--person", "ada", "--node", "studio", "--install", "false", "--start", "false",
        "--service", "false",
    ]);
    // Name, machine, login box, the Pi row, then the two buttons: Tab to Skip for now.
    let (success, output) = play(
        command,
        &[("Smalltalk setup", "\t\t\t\t\t\r"), ("st is set up", "\r")],
        &mut fixture,
    );
    assert!(success, "{output}");
    assert!(!output.contains("Pi: ready"), "no agent was set up:\n{output}");
    let table: toml::Table =
        toml::from_str(&fs::read_to_string(fixture.path("home/.config/st3/config.toml")).unwrap())
            .unwrap();
    assert_eq!(table["person"].as_str(), Some("person/ada"));
    assert!(table.get("codex_work_without_asking").is_none(), "no Codex, so no setting");
}
