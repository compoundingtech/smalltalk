//! Service installation with an injectable command boundary: tests never use host services.

use super::*;
use std::process::Output;
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};

pub(super) trait Commands {
    fn program(&self) -> &str;
    fn output(&mut self, arguments: &[&str]) -> Result<Output>;

    fn run(&mut self, arguments: &[&str]) -> Result<()> {
        let output = self.output(arguments)?;
        check_output(self.program(), arguments, &output)
    }
}

pub(super) struct NativeCommands<'a>(pub &'a str);

impl Commands for NativeCommands<'_> {
    fn program(&self) -> &str {
        self.0
    }

    fn output(&mut self, arguments: &[&str]) -> Result<Output> {
        super::host_command(self.0)
            .args(arguments)
            .output()
            .with_context(|| format!("run {} {}", self.0, arguments.join(" ")))
    }
}

fn check_output(program: &str, arguments: &[&str], output: &Output) -> Result<()> {
    anyhow::ensure!(
        output.status.success(),
        "{program} {} failed with {}: {}{}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim(),
        if output.stdout.is_empty() {
            String::new()
        } else {
            format!("; {}", String::from_utf8_lossy(&output.stdout).trim())
        }
    );
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn launchd_loaded(commands: &mut impl Commands, target: &str) -> Result<bool> {
    let arguments = ["print", target];
    let output = commands.output(&arguments)?;
    if output.status.success() {
        return Ok(true);
    }
    // launchctl's service-target lookup returns 113 when the service is absent.
    // Permission and domain errors must not be mistaken for completed teardown.
    if output.status.code() == Some(113) {
        return Ok(false);
    }
    check_output(commands.program(), &arguments, &output)?;
    unreachable!()
}

#[cfg(any(target_os = "macos", test))]
fn bootout_and_wait(commands: &mut impl Commands, target: &str, timeout: Duration) -> Result<()> {
    let arguments = ["bootout", target];
    let output = commands.output(&arguments)?;
    let deadline = Instant::now() + timeout;
    while launchd_loaded(commands, target)? {
        check_output(commands.program(), &arguments, &output)?;
        anyhow::ensure!(
            Instant::now() < deadline,
            "launchctl bootout {target} did not remove the service within {} seconds; refusing to bootstrap over it",
            timeout.as_secs_f64()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn activate_launchd(
    commands: &mut impl Commands,
    domain: &str,
    label: &str,
    plist: &Path,
    unchanged: bool,
) -> Result<()> {
    let target = format!("{domain}/{label}");
    if unchanged && launchd_loaded(commands, &target)? {
        commands.run(&["enable", &target])?;
        // Copying a new binary to the same path does not change the definition.
        // Restart it without removing and immediately re-registering its label.
        return commands.run(&["kickstart", "-k", &target]);
    }
    bootout_and_wait(commands, &target, Duration::from_secs(10))?;
    commands.run(&["enable", &target])?;
    commands.run(&["bootstrap", domain, &plist.display().to_string()])?;
    commands.run(&["kickstart", &target])
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn launchd(
    spec: &ServiceSpec,
    plist: &Path,
    replication_plist: &Path,
    domain: &str,
    commands: &mut impl Commands,
) -> Result<()> {
    fs::create_dir_all(spec.config.state_dir.join("logs"))?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    let previous = read_existing_file(plist)?;
    let previous_replication = read_existing_file(replication_plist)?;
    let candidate = render_launchd_plist(spec);
    let replication_candidate = render_launchd_replication_plist(spec);
    fs::write(plist, &candidate)?;
    if spec.config.fleet_id.is_some() {
        fs::write(replication_plist, &replication_candidate)?;
    }
    let replication_target = format!("{domain}/{}", replication_service_label());
    let install = (|| -> Result<()> {
        activate_launchd(
            commands,
            domain,
            service_label(),
            plist,
            previous.as_deref() == Some(candidate.as_bytes()),
        )?;
        if spec.config.fleet_id.is_some() {
            activate_launchd(
                commands,
                domain,
                replication_service_label(),
                replication_plist,
                previous_replication.as_deref() == Some(replication_candidate.as_bytes()),
            )?;
        } else {
            bootout_and_wait(commands, &replication_target, Duration::from_secs(10))?;
            commands.run(&["disable", &replication_target])?;
            restore_file(replication_plist, None)?;
        }
        wait_for_service_sockets(&spec.config)
    })();
    if let Err(error) = install {
        let rollback = (|| -> Result<()> {
            restore_file(plist, previous.as_deref())?;
            restore_file(replication_plist, previous_replication.as_deref())?;
            for (label, path, prior) in [
                (service_label(), plist, &previous),
                (
                    replication_service_label(),
                    replication_plist,
                    &previous_replication,
                ),
            ] {
                if prior.is_some() {
                    activate_launchd(commands, domain, label, path, false)?;
                } else {
                    let target = format!("{domain}/{label}");
                    bootout_and_wait(commands, &target, Duration::from_secs(10))?;
                    commands.run(&["disable", &target])?;
                }
            }
            // The prior config may use different sockets. Its readiness cannot
            // be checked with the candidate config; report only restoration.
            Ok(())
        })();
        if let Err(rollback) = rollback {
            return Err(error).context(format!(
                "the launchd install failed, and rollback also failed: {rollback:#}"
            ));
        }
        return Err(error)
            .context("the launchd install failed; st restored the prior service definition");
    }
    println!("plist\t{}", plist.display());
    if spec.config.fleet_id.is_some() {
        println!("replication-plist\t{}", replication_plist.display());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn systemd(
    spec: &ServiceSpec,
    unit_path: &Path,
    replication_path: &Path,
    commands: &mut impl Commands,
) -> Result<()> {
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let previous = read_existing_file(unit_path)?;
    let previous_replication = read_existing_file(replication_path)?;
    fs::write(unit_path, render_systemd_user_unit(spec))?;
    if spec.config.fleet_id.is_some() {
        fs::write(replication_path, render_systemd_replication_unit(spec))?;
    }
    let install = (|| -> Result<()> {
        commands.run(&["--user", "daemon-reload"])?;
        commands.run(&["--user", "enable", service_name()])?;
        // `enable --now` alone would leave an already-running old binary alive.
        commands.run(&["--user", "restart", service_name()])?;
        if spec.config.fleet_id.is_some() {
            commands.run(&["--user", "enable", replication_service_name()])?;
            commands.run(&["--user", "restart", replication_service_name()])?;
        } else {
            if previous_replication.is_some() {
                commands.run(&["--user", "disable", "--now", replication_service_name()])?;
            }
            restore_file(replication_path, None)?;
            commands.run(&["--user", "daemon-reload"])?;
        }
        wait_for_service_sockets(&spec.config)
    })();
    if let Err(error) = install {
        let rollback = (|| -> Result<()> {
            // Stop candidate-only services while their definitions are still installed.
            for (name, prior) in [
                (service_name(), &previous),
                (replication_service_name(), &previous_replication),
            ] {
                if prior.is_none() {
                    let _ = commands.output(&["--user", "disable", "--now", name]);
                }
            }
            restore_file(unit_path, previous.as_deref())?;
            restore_file(replication_path, previous_replication.as_deref())?;
            commands.run(&["--user", "daemon-reload"])?;
            for (name, prior) in [
                (service_name(), &previous),
                (replication_service_name(), &previous_replication),
            ] {
                if prior.is_some() {
                    commands.run(&["--user", "restart", name])?;
                }
            }
            Ok(())
        })();
        if let Err(rollback) = rollback {
            return Err(error).context(format!(
                "the systemd install failed, and rollback also failed: {rollback:#}"
            ));
        }
        return Err(error)
            .context("the systemd install failed; st restored the prior service definition");
    }
    println!("unit\t{}", unit_path.display());
    if spec.config.fleet_id.is_some() {
        println!("replication-unit\t{}", replication_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::ExitStatusExt as _;

    fn output(code: i32, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        spec: ServiceSpec,
        _sockets: [UnixListener; 2],
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let config = Config {
                node: "orchid".into(),
                fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
                shared_secret_file: Some(root.path().join("fleet.secret")),
                peer_listen: Some("127.0.0.1:31313".into()),
                peers: vec![crate::config::PeerConfig {
                    name: "willow".into(),
                    url: "http://127.0.0.1:31314".into(),
                }],
                state_dir: root.path().join("state"),
                socket: root.path().join("api.sock"),
                client_gateway_socket: root.path().join("client.sock"),
                ..Config::default()
            };
            let sockets = [
                UnixListener::bind(&config.socket).unwrap(),
                UnixListener::bind(&config.client_gateway_socket).unwrap(),
            ];
            Self {
                spec: ServiceSpec::new("/opt/orchid/st", config, 1024).unwrap(),
                root,
                _sockets: sockets,
            }
        }

        fn paths(&self) -> (PathBuf, PathBuf) {
            (
                self.root.path().join("daemon"),
                self.root.path().join("replication"),
            )
        }
    }

    #[derive(Default)]
    struct Launchd {
        loaded: BTreeMap<String, String>,
        removing: BTreeMap<String, usize>,
        calls: Vec<Vec<String>>,
        fail_bootstrap: bool,
        print_error: bool,
        never_removes: bool,
    }

    impl Commands for Launchd {
        fn program(&self) -> &str {
            "launchctl"
        }

        fn output(&mut self, args: &[&str]) -> Result<Output> {
            self.calls
                .push(args.iter().map(|x| x.to_string()).collect());
            match args[0] {
                "print" => {
                    if self.print_error {
                        return Ok(output(1, "permission denied"));
                    }
                    if let Some(left) = self.removing.get_mut(args[1]) {
                        if *left == 0 && !self.never_removes {
                            self.loaded.remove(args[1]);
                        } else {
                            *left = left.saturating_sub(1);
                        }
                    }
                    Ok(output(
                        if self.loaded.contains_key(args[1]) {
                            0
                        } else {
                            113
                        },
                        "",
                    ))
                }
                "bootout" => {
                    if !self.loaded.contains_key(args[1]) {
                        return Ok(output(3, "not loaded"));
                    }
                    // Removal takes multiple observations even after bootout succeeds.
                    self.removing.entry(args[1].into()).or_insert(2);
                    Ok(output(0, ""))
                }
                "bootstrap" => {
                    if std::mem::take(&mut self.fail_bootstrap) {
                        return Ok(output(5, "fixture bootstrap refused"));
                    }
                    let definition = fs::read_to_string(args[2])?;
                    let label = if definition.contains(replication_service_label()) {
                        replication_service_label()
                    } else {
                        service_label()
                    };
                    let target = format!("{}/{label}", args[1]);
                    if self.loaded.contains_key(&target) {
                        return Ok(output(5, "old registration is still being removed"));
                    }
                    self.removing.remove(&target);
                    self.loaded.insert(target, definition);
                    Ok(output(0, ""))
                }
                "kickstart" => {
                    assert!(self.loaded.contains_key(*args.last().unwrap()));
                    Ok(output(0, ""))
                }
                "enable" | "disable" => Ok(output(0, "")),
                command => panic!("unexpected fake launchctl command: {command}"),
            }
        }
    }

    fn install_launchd(fixture: &Fixture, manager: &mut Launchd) -> Result<()> {
        let (daemon, replication) = fixture.paths();
        launchd(&fixture.spec, &daemon, &replication, "gui/4242", manager)
    }

    #[test]
    fn repeated_launchd_install_restarts_both_unchanged_definitions_in_place() {
        let fixture = Fixture::new();
        let mut manager = Launchd::default();
        install_launchd(&fixture, &mut manager).unwrap();
        manager.calls.clear();
        install_launchd(&fixture, &mut manager).unwrap();
        assert_eq!(manager.loaded.len(), 2);
        assert!(
            !manager
                .calls
                .iter()
                .any(|args| matches!(args[0].as_str(), "bootout" | "bootstrap"))
        );
        assert_eq!(
            manager
                .calls
                .iter()
                .filter(|args| args[0] == "kickstart" && args[1] == "-k")
                .count(),
            2
        );
    }

    #[test]
    fn changed_launchd_definitions_wait_for_removal_and_load_the_new_binary() {
        let mut fixture = Fixture::new();
        let mut manager = Launchd::default();
        install_launchd(&fixture, &mut manager).unwrap();
        fixture.spec.exe = "/opt/orchid/release-two/st".into();
        manager.calls.clear();
        install_launchd(&fixture, &mut manager).unwrap();
        assert_eq!(manager.loaded.len(), 2);
        assert!(
            manager
                .loaded
                .values()
                .all(|definition| definition.contains("/release-two/st"))
        );
        assert_eq!(
            manager
                .calls
                .iter()
                .filter(|args| args[0] == "print")
                .count(),
            6
        );
    }

    #[test]
    fn an_unchanged_but_unloaded_launchd_service_is_bootstrapped() {
        let fixture = Fixture::new();
        let mut manager = Launchd::default();
        install_launchd(&fixture, &mut manager).unwrap();
        manager.loaded.clear();
        install_launchd(&fixture, &mut manager).unwrap();
        assert_eq!(manager.loaded.len(), 2);
    }

    #[test]
    fn launchd_failure_restores_both_definitions_and_keeps_command_diagnostics() {
        let mut fixture = Fixture::new();
        let mut manager = Launchd::default();
        install_launchd(&fixture, &mut manager).unwrap();
        let before = manager.loaded.clone();
        fixture.spec.exe = "/opt/orchid/release-two/st".into();
        manager.fail_bootstrap = true;
        let error = format!("{:#}", install_launchd(&fixture, &mut manager).unwrap_err());
        assert!(error.contains("launchctl bootstrap gui/4242"), "{error}");
        assert!(error.contains("fixture bootstrap refused"), "{error}");
        assert_eq!(manager.loaded, before);
        let (daemon, replication) = fixture.paths();
        assert!(
            fs::read_to_string(daemon)
                .unwrap()
                .contains("/opt/orchid/st")
        );
        assert!(
            fs::read_to_string(replication)
                .unwrap()
                .contains("/opt/orchid/st")
        );
    }

    #[test]
    fn launchd_removal_times_out_and_query_errors_are_not_absence() {
        let target = "gui/4242/com.example.orchid";
        let mut manager = Launchd::default();
        manager.loaded.insert(target.into(), "definition".into());
        manager.never_removes = true;
        let error = bootout_and_wait(&mut manager, target, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("refusing to bootstrap"));
        manager.print_error = true;
        let error = bootout_and_wait(&mut manager, target, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("launchctl print"));
        assert!(error.to_string().contains("permission denied"));
    }

    #[derive(Default)]
    struct Systemd {
        calls: Vec<Vec<String>>,
        fail_restart: bool,
    }

    impl Commands for Systemd {
        fn program(&self) -> &str {
            "systemctl"
        }

        fn output(&mut self, args: &[&str]) -> Result<Output> {
            self.calls
                .push(args.iter().map(|x| x.to_string()).collect());
            assert_eq!(args[0], "--user");
            if args[1] == "restart" && std::mem::take(&mut self.fail_restart) {
                return Ok(output(1, "fixture executable unavailable"));
            }
            Ok(output(0, ""))
        }
    }

    #[test]
    fn repeated_systemd_install_restarts_both_services_and_reloads_changed_definitions() {
        let mut fixture = Fixture::new();
        let (daemon, replication) = fixture.paths();
        let mut manager = Systemd::default();
        for _ in 0..2 {
            systemd(&fixture.spec, &daemon, &replication, &mut manager).unwrap();
        }
        assert_eq!(
            manager
                .calls
                .iter()
                .filter(|args| args[1] == "restart")
                .count(),
            4
        );
        fixture.spec.exe = "/opt/orchid/release-two/st".into();
        systemd(&fixture.spec, &daemon, &replication, &mut manager).unwrap();
        assert!(
            fs::read_to_string(&daemon)
                .unwrap()
                .contains("/release-two/st")
        );
        assert!(
            fs::read_to_string(&replication)
                .unwrap()
                .contains("/release-two/st")
        );
        assert_eq!(
            manager
                .calls
                .iter()
                .filter(|args| args[1] == "daemon-reload")
                .count(),
            3
        );
    }

    #[test]
    fn systemd_failure_restores_prior_files_and_keeps_command_diagnostics() {
        let mut fixture = Fixture::new();
        let (daemon, replication) = fixture.paths();
        let mut manager = Systemd::default();
        systemd(&fixture.spec, &daemon, &replication, &mut manager).unwrap();
        let before = (fs::read(&daemon).unwrap(), fs::read(&replication).unwrap());
        fixture.spec.exe = "/opt/orchid/release-two/st".into();
        manager.fail_restart = true;
        let error = format!(
            "{:#}",
            systemd(&fixture.spec, &daemon, &replication, &mut manager).unwrap_err()
        );
        assert!(
            error.contains("systemctl --user restart st3.service"),
            "{error}"
        );
        assert!(error.contains("fixture executable unavailable"), "{error}");
        assert_eq!(
            (fs::read(daemon).unwrap(), fs::read(replication).unwrap()),
            before
        );
    }

    #[test]
    fn reinstall_without_a_fleet_removes_the_old_replication_service() {
        let mut fixture = Fixture::new();
        let mut launchd_manager = Launchd::default();
        install_launchd(&fixture, &mut launchd_manager).unwrap();
        fixture.spec.config.fleet_id = None;
        fixture.spec.config.shared_secret_file = None;
        fixture.spec.config.peers.clear();
        fixture.spec.config.peer_listen = None;
        install_launchd(&fixture, &mut launchd_manager).unwrap();
        assert_eq!(launchd_manager.loaded.len(), 1);
        assert!(!fixture.paths().1.exists());

        let mut fixture = Fixture::new();
        let (daemon, replication) = fixture.paths();
        let mut systemd_manager = Systemd::default();
        systemd(&fixture.spec, &daemon, &replication, &mut systemd_manager).unwrap();
        fixture.spec.config.fleet_id = None;
        fixture.spec.config.shared_secret_file = None;
        fixture.spec.config.peers.clear();
        fixture.spec.config.peer_listen = None;
        systemd(&fixture.spec, &daemon, &replication, &mut systemd_manager).unwrap();
        assert!(!replication.exists());
        assert!(
            systemd_manager
                .calls
                .iter()
                .any(|args| args == &["--user", "disable", "--now", replication_service_name(),])
        );
    }
}
