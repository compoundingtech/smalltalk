//! Install the st daemon as a native user service.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use anyhow::bail;

use crate::config::Config;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod install;


/// The names the service manager knows this install by. An instance has its own, so installing,
/// restarting or removing it can never act on the default install's service.
struct ServiceNames {
    service: String,
    replication_service: String,
    label: String,
    replication_label: String,
}

/// A service-manager command. The manager belongs to the real user session, so an instance's
/// own `XDG_*` directories are put back for it.
pub(crate) fn host_command(program: &str) -> Command {
    let mut command = Command::new(program);
    crate::instance::as_host(&mut command);
    command
}

fn service_names() -> &'static ServiceNames {
    static NAMES: std::sync::OnceLock<ServiceNames> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| match crate::instance::current() {
        Some(instance) => ServiceNames {
            service: instance.systemd_service(),
            replication_service: instance.systemd_replication_service(),
            label: instance.launchd_label(),
            replication_label: instance.launchd_replication_label(),
        },
        None => ServiceNames {
            service: "st3.service".into(),
            replication_service: "st3-replication.service".into(),
            label: "com.compoundingtech.st3".into(),
            replication_label: "com.compoundingtech.st3.replication".into(),
        },
    })
}

#[cfg(any(target_os = "linux", test))]
fn service_name() -> &'static str {
    &service_names().service
}

#[cfg(any(target_os = "linux", test))]
fn replication_service_name() -> &'static str {
    &service_names().replication_service
}

fn service_label() -> &'static str {
    &service_names().label
}

fn replication_service_label() -> &'static str {
    &service_names().replication_label
}
pub const DEFAULT_MEMORY_MAX_MB: u64 = 1024;

#[derive(Clone, Debug, Serialize)]
pub struct ServiceStatusReport {
    pub manager: &'static str,
    pub services: Vec<ServiceStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub installed: bool,
    pub running: bool,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct ServiceSpec {
    exe: PathBuf,
    config: Config,
    config_path: Option<PathBuf>,
    memory_max_mb: u64,
    read_cache_kib: Option<usize>,
}

impl ServiceSpec {
    pub fn new(exe: impl Into<PathBuf>, config: Config, memory_max_mb: u64) -> Result<Self> {
        anyhow::ensure!(
            memory_max_mb > 0,
            "the memory limit must be greater than zero"
        );
        anyhow::ensure!(
            config.state_dir.is_absolute()
                && config.socket.is_absolute()
                && config.client_gateway_socket.is_absolute(),
            "the service state directory and sockets must be absolute"
        );
        anyhow::ensure!(
            config
                .pty_root
                .as_ref()
                .is_none_or(|root| root.is_absolute()),
            "the service PTY root must be absolute"
        );
        config.validate()?;
        Ok(Self {
            exe: exe.into(),
            config,
            config_path: None,
            memory_max_mb,
            read_cache_kib: None,
        })
    }

    fn program_arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            self.exe.display().to_string(),
            "up".into(),
            "--node".into(),
            self.config.node.clone(),
            "--state-dir".into(),
            self.config.state_dir.display().to_string(),
            "--socket".into(),
            self.config.socket.display().to_string(),
            "--client-gateway-socket".into(),
            self.config.client_gateway_socket.display().to_string(),
        ];
        if let Some(path) = &self.config_path {
            arguments.extend(["--config".into(), path.display().to_string()]);
        }
        if let Some(pty_root) = &self.config.pty_root {
            arguments.extend(["--pty-root".into(), pty_root.display().to_string()]);
        }
        // Downloaded archives and setup put pty next to st3. A service must remain
        // usable before ~/.local/bin has been added to the login shell's PATH.
        if let Some(pty) = self.exe.parent().map(|parent| parent.join("pty"))
            .filter(|pty| pty.is_file())
        {
            arguments.extend(["--pty-binary".into(), pty.display().to_string()]);
        }
        self.push_fleet_arguments(&mut arguments);
        arguments
    }

    /// A config-peer node bakes its peers into the unit, as before. A fleet member reads
    /// `fleet.toml` and membership at run time, so its units carry no peer, fleet, or secret
    /// arguments and never need reinstalling when membership changes.
    fn push_fleet_arguments(&self, arguments: &mut Vec<String>) {
        if self.config.fleet.is_some() {
            return;
        }
        if let Some(peer_listen) = &self.config.peer_listen {
            arguments.extend(["--peer-listen".into(), peer_listen.clone()]);
        }
        if self.config.peer_listen_allow_plain_http {
            arguments.push("--peer-listen-allow-plain-http".into());
        }
        for peer in &self.config.peers {
            arguments.extend(["--peer".into(), format!("{}={}", peer.name, peer.url)]);
        }
        if let Some(fleet_id) = &self.config.fleet_id {
            arguments.extend(["--fleet-id".into(), fleet_id.clone()]);
        }
        if let Some(secret) = &self.config.shared_secret_file {
            arguments.extend(["--shared-secret-file".into(), secret.display().to_string()]);
        }
    }

    fn replication_program_arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            self.exe.display().to_string(),
            "replication-worker".into(),
            "--node".into(),
            self.config.node.clone(),
            "--state-dir".into(),
            self.config.state_dir.display().to_string(),
            "--socket".into(),
            self.config.socket.display().to_string(),
        ];
        if let Some(path) = &self.config_path {
            arguments.extend(["--config".into(), path.display().to_string()]);
        }
        self.push_fleet_arguments(&mut arguments);
        arguments
    }
}

pub fn install(config: Config) -> Result<()> {
    install_from_config_path(config, None)
}

pub fn install_from_config_path(mut config: Config, config_path: Option<&Path>) -> Result<()> {
    crate::node_identity::resolve(&mut config)?;
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        st_runtime::isolation_mode() != st_runtime::Isolation::DegradedDetached,
        "st service install needs a working systemd user manager and transient user scopes"
    );
    let exe = env::current_exe().context("resolve the current st executable")?;
    #[cfg(target_os = "macos")]
    let exe = exe
        .canonicalize()
        .context("resolve the installed macOS app executable")?;
    let current = env::current_dir().context("resolve the service install directory")?;
    config.state_dir = absolute_from(&current, &config.state_dir);
    config.socket = absolute_from(&current, &config.socket);
    config.client_gateway_socket = absolute_from(&current, &config.client_gateway_socket);
    config.pty_root = config
        .pty_root
        .as_ref()
        .map(|root| absolute_from(&current, root));
    config.shared_secret_file = config
        .shared_secret_file
        .as_ref()
        .map(|path| absolute_from(&current, path));
    if let (Some(fleet_id), Some(secret)) = (
        config.fleet_id.as_deref(),
        config.shared_secret_file.as_deref(),
    ) {
        crate::peer::FleetAuth::load(fleet_id, secret)?;
    }
    let mut spec = ServiceSpec::new(exe, config, DEFAULT_MEMORY_MAX_MB)?;
    spec.read_cache_kib = crate::read_cache::override_kib();
    spec.config_path = config_path.map(|path| absolute_from(&current, path));
    install_native_service(&spec)?;
    println!("installed");
    Ok(())
}

fn absolute_from(current: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current.join(path)
    }
}

fn read_existing_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn restore_file(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    match previous {
        Some(bytes) => {
            fs::write(path, bytes).with_context(|| format!("restore {}", path.display()))
        }
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        },
    }
}

pub fn status() -> Result<ServiceStatusReport> {
    status_native_service()
}

pub fn permissions(open: bool) -> Result<()> {
    print!("{}", permissions_guidance()?);
    if open {
        open_permissions_settings()?;
    }
    Ok(())
}

pub fn permissions_guidance() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let executable = env::current_exe()
            .context("resolve the current st executable")?
            .canonicalize()
            .context("resolve the installed macOS app executable")?;
        Ok(macos_permission_guidance(&executable))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok("st does not need a macOS privacy approval on this host.\n".into())
    }
}

pub fn open_permissions_settings() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        run_command(
            "open",
            &["x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"],
        )?;
        run_command(
            "open",
            &["x-apple.systempreferences:com.apple.preference.security?Privacy_DeveloperTools"],
        )?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn macos_permission_guidance(executable: &Path) -> String {
    format!(
        "st executable\t{}\n\
1. Open System Settings > Privacy & Security > Full Disk Access.\n\
2. Add the st executable and enable it.\n\
3. Open System Settings > Privacy & Security > Developer Tools.\n\
4. Add the st executable and enable it.\n\
5. Run `st service restart` after an approval changes.\n\
macOS assigns service-owned file and developer access to st, not to the terminal that installed it.\n\
A launchd property list cannot grant these approvals.\n",
        executable.display()
    )
}

pub fn restart(mut config: Config) -> Result<()> {
    let current = env::current_dir().context("resolve the service restart directory")?;
    config.state_dir = absolute_from(&current, &config.state_dir);
    config.socket = absolute_from(&current, &config.socket);
    config.client_gateway_socket = absolute_from(&current, &config.client_gateway_socket);
    config.pty_root = config
        .pty_root
        .as_ref()
        .map(|root| absolute_from(&current, root));
    restart_native_service(&config)
}

pub fn reset(mut config: Config) -> Result<()> {
    let current = env::current_dir().context("resolve the service reset directory")?;
    config.state_dir = absolute_from(&current, &config.state_dir);
    config.socket = absolute_from(&current, &config.socket);
    config.client_gateway_socket = absolute_from(&current, &config.client_gateway_socket);
    config.pty_root = config
        .pty_root
        .as_ref()
        .map(|root| absolute_from(&current, root));
    validate_reset_target(&config)?;

    stop_native_service()?;
    stop_owned_runtimes(&config)?;
    match fs::remove_dir_all(&config.state_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("erase the st state directory"),
    }
    if !config.socket.starts_with(&config.state_dir) {
        match fs::remove_file(&config.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("erase the st socket"),
        }
    }
    for path in crate::startup::paths(&config.socket) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("erase the startup readiness record"),
        }
    }
    if !config.client_gateway_socket.starts_with(&config.state_dir) {
        match fs::remove_file(&config.client_gateway_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("erase the st client gateway socket"),
        }
    }
    start_native_service()?;
    wait_for_service_sockets(&config)?;
    println!("reset\t{}", config.state_dir.display());
    Ok(())
}

/// Stop the st3 services without removing them.
pub fn stop() -> Result<()> {
    stop_native_service()
}

pub fn uninstall() -> Result<()> {
    uninstall_native_service()?;
    println!("uninstalled");
    Ok(())
}

fn validate_reset_target(config: &Config) -> Result<()> {
    anyhow::ensure!(
        config.state_dir.is_absolute(),
        "the st state directory must be absolute"
    );
    anyhow::ensure!(
        config.state_dir != Path::new("/"),
        "refusing to erase the filesystem root"
    );
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        anyhow::ensure!(
            config.state_dir != home,
            "refusing to erase the home directory"
        );
    }
    anyhow::ensure!(
        config.state_dir.components().count() >= 3,
        "the st state directory is too broad to erase"
    );
    Ok(())
}

/// Stop the PTY runtimes this daemon owns, as `st3 service reset` does.
pub fn stop_owned_runtimes(config: &Config) -> Result<()> {
    let pty_root = config
        .pty_root
        .clone()
        .unwrap_or_else(|| config.state_dir.join("pty"));
    let pty = st_runtime::PtyRuntime::new(pty_root.clone());
    if pty_root.exists() {
        let owned = pty
            .snapshot()?
            .into_iter()
            .filter(|item| item.tags.contains_key("st3.subject"))
            .map(|item| {
                let incarnation = match (item.pid, item.created_at) {
                    (Some(pid), Some(created)) => Some(format!("{pid}:{created}")),
                    _ => None,
                };
                (item.name, incarnation)
            })
            .collect::<Vec<_>>();
        for (id, incarnation) in &owned {
            let _ = pty.stop_if(id, incarnation.as_deref());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        let survivors = running_pty_ids(pty.snapshot()?);
        for (id, incarnation) in &owned {
            if survivors.contains(id) {
                pty.kill_if(id, incarnation.as_deref())?;
            }
            let _ = pty.remove(id);
        }
    }

    let exec_root = config.state_dir.join("exec");
    let exec = st_runtime::ExecRuntime::new(exec_root.clone(), config.state_dir.join("logs"));
    if exec_root.is_dir() {
        let mut ids = fs::read_dir(&exec_root)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_owned)
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        for id in &ids {
            if let Some(st_runtime::ExecObservation::Running(generation)) = exec.observe(id)? {
                exec.stop_if(id, Some(&generation.generation_id))?;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        for id in &ids {
            if let Some(st_runtime::ExecObservation::Running(generation)) = exec.observe(id)? {
                exec.kill_if(id, Some(&generation.generation_id))?;
            }
        }
    }
    Ok(())
}

fn running_pty_ids(
    snapshot: impl IntoIterator<Item = st_runtime::PtyObservation>,
) -> std::collections::HashSet<String> {
    snapshot
        .into_iter()
        .filter(|item| item.status == "running")
        .map(|item| item.name)
        .collect()
}

#[cfg(target_os = "linux")]
fn install_native_service(spec: &ServiceSpec) -> Result<()> {
    install_systemd_user(spec)
}

#[cfg(target_os = "linux")]
fn status_native_service() -> Result<ServiceStatusReport> {
    status_systemd_user()
}

#[cfg(target_os = "linux")]
fn restart_native_service(config: &Config) -> Result<()> {
    optional_systemd_command(
        replication_service_name(),
        &["--user", "stop", replication_service_name()],
    )?;
    run_command("systemctl", &["--user", "restart", service_name()])?;
    if config.fleet_id.is_some() {
        run_command("systemctl", &["--user", "start", replication_service_name()])?;
    }
    wait_for_service_sockets(config)
}

#[cfg(target_os = "linux")]
fn uninstall_native_service() -> Result<()> {
    uninstall_systemd_user()
}

#[cfg(target_os = "linux")]
fn stop_native_service() -> Result<()> {
    optional_systemd_command(
        replication_service_name(),
        &["--user", "stop", replication_service_name()],
    )?;
    run_command("systemctl", &["--user", "stop", service_name()])
}

#[cfg(target_os = "linux")]
fn optional_systemd_command(name: &str, arguments: &[&str]) -> Result<()> {
    let output = host_command("systemctl")
        .args(arguments)
        .output()
        .with_context(|| format!("run systemctl {}", arguments.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    // A unit file may be gone while its service remains loaded. Only the manager can
    // establish that a failed stop/disable addressed an absent, inactive unit.
    let status = host_command("systemctl")
        .args([
            "--user",
            "show",
            name,
            "--property=LoadState",
            "--property=ActiveState",
            "--no-pager",
        ])
        .output()
        .with_context(|| format!("check whether {name} is absent"))?;
    let states = String::from_utf8_lossy(&status.stdout);
    anyhow::ensure!(
        states.lines().any(|line| line == "LoadState=not-found")
            && states.lines().any(|line| line == "ActiveState=inactive"),
        "systemctl {} failed with {}: {}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_native_service() -> Result<()> {
    run_command("systemctl", &["--user", "start", service_name()])?;
    if replication_systemd_user_unit_path()?.exists() {
        run_command("systemctl", &["--user", "start", replication_service_name()])?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_native_service(spec: &ServiceSpec) -> Result<()> {
    install::launchd(
        spec,
        &launch_agent_path()?,
        &replication_launch_agent_path()?,
        &launch_domain(),
        &mut install::NativeCommands("launchctl"),
    )
}

#[cfg(target_os = "macos")]
fn status_native_service() -> Result<ServiceStatusReport> {
    Ok(ServiceStatusReport {
        manager: "launchd-user",
        services: vec![
            launchd_service_status(service_label(), &launch_agent_path()?)?,
            launchd_service_status(replication_service_label(), &replication_launch_agent_path()?)?,
        ],
    })
}

#[cfg(target_os = "macos")]
fn launchd_service_status(label: &str, plist: &Path) -> Result<ServiceStatus> {
    if !plist.exists() {
        return Ok(ServiceStatus {
            name: label.into(),
            installed: false,
            running: false,
            state: "not-installed".into(),
        });
    }
    let target = format!("{}/{label}", launch_domain());
    let output = host_command("launchctl")
        .args(["print", &target])
        .output()
        .with_context(|| format!("run launchctl print {target}"))?;
    if !output.status.success() {
        return Ok(ServiceStatus {
            name: label.into(),
            installed: true,
            running: false,
            state: "not-loaded".into(),
        });
    }
    let state = parse_launchd_state(&String::from_utf8_lossy(&output.stdout));
    Ok(ServiceStatus {
        name: label.into(),
        installed: true,
        running: state == "running",
        state,
    })
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_state(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .map(str::to_owned)
        .unwrap_or_else(|| "loaded".into())
}

#[cfg(target_os = "macos")]
fn restart_native_service(config: &Config) -> Result<()> {
    let domain = launch_domain();
    let replication_service = format!("{domain}/{}", replication_service_label());
    let _ = host_command("launchctl")
        .args(["bootout", &replication_service])
        .status();
    run_command(
        "launchctl",
        &["kickstart", "-k", &format!("{domain}/{}", service_label())],
    )?;
    if config.fleet_id.is_some() {
        let plist = replication_launch_agent_path()?;
        run_command(
            "launchctl",
            &["bootstrap", &domain, &plist.display().to_string()],
        )?;
    }
    wait_for_service_sockets(config)
}

#[cfg(target_os = "macos")]
fn uninstall_native_service() -> Result<()> {
    let service = format!("{}/{}", launch_domain(), service_label());
    let _ = host_command("launchctl")
        .args(["bootout", &service])
        .status();
    let _ = host_command("launchctl")
        .args(["disable", &service])
        .status();
    let replication_service = format!("{}/{}", launch_domain(), replication_service_label());
    let _ = host_command("launchctl")
        .args(["bootout", &replication_service])
        .status();
    let _ = host_command("launchctl")
        .args(["disable", &replication_service])
        .status();
    let plist = launch_agent_path()?;
    if plist.exists() {
        fs::remove_file(plist)?;
    }
    let replication_plist = replication_launch_agent_path()?;
    if replication_plist.exists() {
        fs::remove_file(replication_plist)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_native_service() -> Result<()> {
    let _ = host_command("launchctl")
        .args([
            "bootout",
            &format!("{}/{}", launch_domain(), replication_service_label()),
        ])
        .status();
    run_command(
        "launchctl",
        &["bootout", &format!("{}/{}", launch_domain(), service_label())],
    )
}

#[cfg(target_os = "macos")]
fn start_native_service() -> Result<()> {
    let domain = launch_domain();
    let plist = launch_agent_path()?;
    run_command(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
    )?;
    run_command(
        "launchctl",
        &["kickstart", &format!("{domain}/{}", service_label())],
    )?;
    let replication_plist = replication_launch_agent_path()?;
    if replication_plist.exists() {
        run_command(
            "launchctl",
            &[
                "bootstrap",
                &domain,
                &replication_plist.display().to_string(),
            ],
        )?;
        run_command(
            "launchctl",
            &[
                "kickstart",
                &format!("{domain}/{}", replication_service_label()),
            ],
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd_user(spec: &ServiceSpec) -> Result<()> {
    install::systemd(
        spec,
        &systemd_user_unit_path()?,
        &replication_systemd_user_unit_path()?,
        &mut install::NativeCommands("systemctl"),
    )
}

#[cfg(target_os = "linux")]
fn status_systemd_user() -> Result<ServiceStatusReport> {
    Ok(ServiceStatusReport {
        manager: "systemd-user",
        services: vec![
            systemd_service_status(service_name())?,
            systemd_service_status(replication_service_name())?,
        ],
    })
}

#[cfg(target_os = "linux")]
fn systemd_service_status(name: &str) -> Result<ServiceStatus> {
    let output = host_command("systemctl")
        .args([
            "--user",
            "show",
            name,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--no-pager",
        ])
        .output()
        .with_context(|| format!("run systemctl --user show {name}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::ensure!(
        output.status.success() || !stdout.trim().is_empty(),
        "systemctl failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let value = |key: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or("unknown")
    };
    let load = value("LoadState");
    let active = value("ActiveState");
    let sub = value("SubState");
    Ok(ServiceStatus {
        name: name.into(),
        installed: load != "not-found",
        running: active == "active",
        state: if load == "not-found" {
            "not-installed".into()
        } else {
            format!("{active}/{sub}")
        },
    })
}

#[cfg(target_os = "linux")]
fn uninstall_systemd_user() -> Result<()> {
    optional_systemd_command(service_name(), &["--user", "disable", "--now", service_name()])?;
    optional_systemd_command(
        replication_service_name(),
        &["--user", "disable", "--now", replication_service_name()],
    )?;
    let unit_path = systemd_user_unit_path()?;
    if unit_path.exists() {
        fs::remove_file(unit_path)?;
    }
    let replication_path = replication_systemd_user_unit_path()?;
    if replication_path.exists() {
        fs::remove_file(replication_path)?;
    }
    run_command("systemctl", &["--user", "daemon-reload"])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported() -> Result<()> {
    bail!("st service is available only on Linux and macOS")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn install_native_service(_spec: &ServiceSpec) -> Result<()> {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn status_native_service() -> Result<ServiceStatusReport> {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn restart_native_service(_config: &Config) -> Result<()> {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn uninstall_native_service() -> Result<()> {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stop_native_service() -> Result<()> {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn start_native_service() -> Result<()> {
    unsupported()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_command(program: &str, arguments: &[&str]) -> Result<()> {
    install::Commands::run(&mut install::NativeCommands(program), arguments)
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", service_label())),
    )
}

#[cfg(target_os = "macos")]
fn replication_launch_agent_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", replication_service_label())),
    )
}

#[cfg(target_os = "macos")]
fn launch_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_path() -> Result<PathBuf> {
    // systemd reads units from the real user config directory, never an instance's.
    Ok(crate::instance::host_config_home()
        .context("HOME and XDG_CONFIG_HOME are not set")?
        .join("systemd/user")
        .join(service_name()))
}

#[cfg(target_os = "linux")]
fn replication_systemd_user_unit_path() -> Result<PathBuf> {
    // systemd reads units from the real user config directory, never an instance's.
    Ok(crate::instance::host_config_home()
        .context("HOME and XDG_CONFIG_HOME are not set")?
        .join("systemd/user")
        .join(replication_service_name()))
}

fn wait_for_service_sockets(config: &Config) -> Result<()> {
    wait_for_socket(&config.socket)?;
    wait_for_socket(&config.client_gateway_socket)
}

fn wait_for_socket(socket: &Path) -> Result<()> {
    wait_for_socket_for(socket, std::time::Duration::from_secs(10))
}

fn wait_for_socket_for(socket: &Path, timeout: std::time::Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if service_socket_accepts(socket) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    anyhow::bail!(
        "the st service did not accept connections at {}",
        socket.display()
    )
}

#[cfg(unix)]
fn service_socket_accepts(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(not(unix))]
fn service_socket_accepts(socket: &Path) -> bool {
    socket.exists()
}

/// The daemon answers every command and attach, so it outranks the builds and tests its
/// harnesses run at systemd's default weight of 100.
pub fn render_systemd_user_unit(spec: &ServiceSpec) -> String {
    let exec_start = spec
        .program_arguments()
        .iter()
        .map(|argument| systemd_quote_arg(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let cache_environment = spec.read_cache_kib
        .map(|kib| format!("Environment=SMALLCLAIMS_READ_CACHE_KIB={kib}\n"))
        .unwrap_or_default();
    let weight = st_runtime::LIVE_WEIGHT;
    let cache_environment = format!("{cache_environment}{}", systemd_instance_environment());
    format!(
        "[Unit]\n\
Description=st claims graph daemon\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec_start}\n\
Environment=MALLOC_ARENA_MAX=2\n\
{cache_environment}Restart=on-failure\n\
RestartSec=5s\n\
Nice=0\n\
CPUWeight={weight}\n\
IOWeight={weight}\n\
KillMode=control-group\n\
MemoryMax={}M\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        spec.memory_max_mb,
    )
}

/// The lines that start an instance's daemon inside the instance. Empty for the default install.
fn systemd_instance_environment() -> String {
    systemd_environment_lines(&crate::instance::service_environment())
}

fn systemd_environment_lines(variables: &[(String, String)]) -> String {
    variables
        .iter()
        .map(|(name, value)| format!("Environment={}\n", systemd_quote_arg(&format!("{name}={value}"))))
        .collect()
}

pub fn render_systemd_replication_unit(spec: &ServiceSpec) -> String {
    render_systemd_program_unit(
        "st authenticated replication worker",
        &spec.replication_program_arguments(),
        spec,
    )
}

fn render_systemd_program_unit(
    description: &str,
    arguments: &[String],
    spec: &ServiceSpec,
) -> String {
    let exec_start = arguments
        .iter()
        .map(|argument| systemd_quote_arg(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let instance_environment = systemd_instance_environment();
    format!(
        "[Unit]\n\
Description={description}\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec_start}\n\
Environment=MALLOC_ARENA_MAX=2\n\
{instance_environment}Restart=on-failure\n\
RestartSec=5s\n\
Nice=0\n\
CPUWeight=100\n\
KillMode=control-group\n\
MemoryMax={}M\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        spec.memory_max_mb,
    )
}

pub fn render_launchd_plist(spec: &ServiceSpec) -> String {
    render_launchd_program_plist(
        service_label(),
        &spec.program_arguments(),
        "st3.stdout.log",
        "st3.stderr.log",
        "Interactive",
        spec,
    )
}

pub fn render_launchd_replication_plist(spec: &ServiceSpec) -> String {
    render_launchd_program_plist(
        replication_service_label(),
        &spec.replication_program_arguments(),
        "st3-replication.stdout.log",
        "st3-replication.stderr.log",
        "Background",
        spec,
    )
}

fn render_launchd_program_plist(
    label: &str,
    program_arguments: &[String],
    stdout_name: &str,
    stderr_name: &str,
    process_type: &str,
    spec: &ServiceSpec,
) -> String {
    let arguments = program_arguments
        .iter()
        .map(|argument| format!("    <string>{}</string>\n", xml_escape(argument)))
        .collect::<String>();
    let mut variables = Vec::new();
    if label == service_label()
        && let Some(kib) = spec.read_cache_kib
    {
        variables.push(("SMALLCLAIMS_READ_CACHE_KIB".to_owned(), kib.to_string()));
    }
    variables.extend(crate::instance::service_environment());
    let cache_environment = plist_environment(&variables);
    let stdout = spec.config.state_dir.join("logs").join(stdout_name);
    let stderr = spec.config.state_dir.join("logs").join(stderr_name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n{arguments}  </array>\n\
{cache_environment}  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
  <key>ProcessType</key><string>{process_type}</string>\n\
  <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>8192</integer></dict>\n\
  <key>StandardOutPath</key><string>{}</string>\n\
  <key>StandardErrorPath</key><string>{}</string>\n\
</dict>\n\
</plist>\n",
        xml_escape(&stdout.display().to_string()),
        xml_escape(&stderr.display().to_string()),
    )
}

fn plist_environment(variables: &[(String, String)]) -> String {
    if variables.is_empty() {
        return String::new();
    }
    let entries = variables
        .iter()
        .map(|(name, value)| format!("<key>{}</key><string>{}</string>", xml_escape(name), xml_escape(value)))
        .collect::<String>();
    format!("  <key>EnvironmentVariables</key><dict>{entries}</dict>\n")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote_arg(argument: &str) -> String {
    if !argument.is_empty()
        && argument.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'_' | b':' | b'-' | b'+' | b'=')
        })
    {
        return argument.into();
    }
    let mut quoted = String::from("\"");
    for character in argument.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PeerConfig;

    #[test]
    fn reader_cache_override_is_persisted_for_the_daemon_on_both_platforms() -> Result<()> {
        let mut spec = ServiceSpec::new("/usr/bin/st3", Config::default(), 1024)?;
        assert!(!render_systemd_user_unit(&spec).contains("SMALLCLAIMS_READ_CACHE_KIB"));
        assert!(!render_launchd_plist(&spec).contains("SMALLCLAIMS_READ_CACHE_KIB"));
        spec.read_cache_kib = Some(1024);
        assert!(render_systemd_user_unit(&spec)
            .contains("Environment=SMALLCLAIMS_READ_CACHE_KIB=1024\n"));
        assert!(render_launchd_plist(&spec).contains(
            "<key>EnvironmentVariables</key><dict><key>SMALLCLAIMS_READ_CACHE_KIB</key><string>1024</string></dict>"
        ));
        assert!(!render_systemd_replication_unit(&spec).contains("SMALLCLAIMS_READ_CACHE_KIB"));
        assert!(!render_launchd_replication_plist(&spec).contains("SMALLCLAIMS_READ_CACHE_KIB"));
        Ok(())
    }

    #[test]
    fn setup_service_pins_the_config_and_sibling_pty() -> Result<()> {
        let root = tempfile::tempdir()?;
        let executable = root.path().join("st3");
        fs::write(root.path().join("pty"), b"fixture")?;
        let mut spec = ServiceSpec::new(executable, Config::default(), 1024)?;
        spec.config_path = Some(root.path().join("custom/config.toml"));
        let arguments = spec.program_arguments();
        assert!(arguments.windows(2).any(|pair| pair[0] == "--config" && pair[1].ends_with("custom/config.toml")));
        assert!(arguments.windows(2).any(|pair| pair[0] == "--pty-binary" && pair[1].ends_with("/pty")));
        assert!(spec.replication_program_arguments().contains(&"--config".into()));
        Ok(())
    }

    #[test]
    fn membership_units_carry_no_peer_fleet_or_secret_arguments() -> Result<()> {
        let config = Config {
            node: "node-a".into(),
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/var/lib/st3/fleet/secret".into()),
            state_dir: "/var/lib/st3".into(),
            socket: "/run/user/1000/st3.sock".into(),
            client_gateway_socket: "/run/user/1000/st3-client.sock".into(),
            peer_listen: Some("127.0.0.1:31313".into()),
            peers: vec![PeerConfig {
                name: "node-b".into(),
                url: "http://127.0.0.1:31314".into(),
            }],
            fleet: Some(crate::config::FleetFile {
                fleet_id: "1f91ca65-7793-48cc-866e-ac15690130e1".into(),
                port: Some(31313),
                ..Default::default()
            }),
            ..Config::default()
        };
        let spec = ServiceSpec::new("/usr/bin/st3", config, 1024)?;
        for unit in [
            render_systemd_user_unit(&spec),
            render_systemd_replication_unit(&spec),
            render_launchd_plist(&spec),
            render_launchd_replication_plist(&spec),
        ] {
            for argument in [
                "--peer",
                "--peer-listen",
                "--fleet-id",
                "--shared-secret-file",
                "fleet/secret",
            ] {
                assert!(!unit.contains(argument), "{argument} in {unit}");
            }
            assert!(unit.contains("--state-dir"));
        }
        Ok(())
    }

    #[test]
    fn unit_bakes_the_effective_config_and_limit() -> Result<()> {
        let config = Config {
            node: "node-a".into(),
            person: None,
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/var/lib/st3/fleet.secret".into()),
            state_dir: "/var/lib/st3".into(),
            pty_root: Some("/var/lib/pty".into()),
            socket: "/run/user/1000/st3.sock".into(),
            client_gateway_socket: "/run/user/1000/st3-client.sock".into(),
            peer_listen: Some("0.0.0.0:31313".into()),
            peer_listen_allow_plain_http: true,
            peers: vec![PeerConfig {
                name: "node-b".into(),
                url: "http://127.0.0.1:31314".into(),
            }],
            planner: crate::model::PlannerSpec::default(),
            observations: crate::config::ObservationsConfig::default(),
            github: crate::config::GithubConfig::default(),
            checkpoint: crate::config::CheckpointConfig::default(),
            limits: crate::config::LimitsConfig::default(),
            claude_permission_mode: None,
            fleet: None,
        };
        let spec = ServiceSpec::new("/usr/bin/st3", config, 1024)?;
        let unit = render_systemd_user_unit(&spec);
        assert!(unit.contains("ExecStart=/usr/bin/st3 up --node node-a"));
        assert!(unit.contains("--state-dir /var/lib/st3"));
        assert!(unit.contains("--pty-root /var/lib/pty"));
        assert!(unit.contains("--client-gateway-socket /run/user/1000/st3-client.sock"));
        assert!(unit.contains("--peer node-b=http://127.0.0.1:31314"));
        assert!(unit.contains("--peer-listen 0.0.0.0:31313 --peer-listen-allow-plain-http"));
        assert!(unit.contains("MemoryMax=1024M"));
        assert!(unit.contains("Environment=MALLOC_ARENA_MAX=2"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("Nice=0"));
        assert!(unit.contains("CPUWeight=1000\nIOWeight=1000\n"));
        assert!(unit.contains("KillMode=control-group"));
        let replication = render_systemd_replication_unit(&spec);
        assert!(replication.contains("replication-worker"));
        assert!(replication.contains("--peer-listen 0.0.0.0:31313 --peer-listen-allow-plain-http"));
        assert!(replication.contains("Nice=0"));
        assert!(replication.contains("CPUWeight=100\n"));
        assert!(!replication.contains("IOWeight"));
        assert!(replication.contains("KillMode=control-group"));
        assert!(replication.contains("Environment=MALLOC_ARENA_MAX=2"));
        assert!(replication.contains("--fleet-id 1f91ca65-7793-48cc-866e-ac15690130e1"));
        assert!(replication.contains("--shared-secret-file /var/lib/st3/fleet.secret"));
        Ok(())
    }

    #[test]
    fn unit_quotes_spaces_and_systemd_specifiers() -> Result<()> {
        let config = Config {
            state_dir: "/tmp/st3 state 100%".into(),
            socket: "/tmp/st3 socket".into(),
            ..Config::default()
        };
        let spec = ServiceSpec::new("/opt/st3 tools/st3", config, 1024)?;
        let unit = render_systemd_user_unit(&spec);
        assert!(unit.contains("\"/opt/st3 tools/st3\""));
        assert!(unit.contains("\"/tmp/st3 state 100%%\""));
        Ok(())
    }

    #[test]
    fn launchd_plist_has_supervision_logs_and_a_file_limit() -> Result<()> {
        let config = Config {
            node: "node-a".into(),
            state_dir: "/Users/example/Library/Application Support/st3".into(),
            socket: "/tmp/st3.sock".into(),
            ..Config::default()
        };
        let spec = ServiceSpec::new("/Users/example/bin/st3", config, 1024)?;
        let plist = render_launchd_plist(&spec);
        assert!(plist.contains("<string>com.compoundingtech.st3</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(plist.contains("<key>ProcessType</key><string>Interactive</string>"));
        assert!(!plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>NumberOfFiles</key><integer>8192</integer>"));
        assert!(plist.contains("st3.stdout.log"));
        assert!(plist.contains("Application Support/st3"));

        let replication = render_launchd_replication_plist(&spec);
        assert!(replication.contains("<key>ProcessType</key><string>Background</string>"));
        Ok(())
    }

    #[test]
    fn macos_permission_guidance_names_the_exact_binary_and_manual_steps() {
        let guidance = macos_permission_guidance(Path::new("/Users/example/bin/st3"));
        assert!(guidance.contains("st executable\t/Users/example/bin/st3"));
        assert!(guidance.contains("Full Disk Access"));
        assert!(guidance.contains("Developer Tools"));
        assert!(guidance.contains("cannot grant"));
    }

    #[test]
    fn launchd_state_is_reduced_to_one_stable_value() {
        assert_eq!(
            parse_launchd_state("service = {\n\tstate = running\n}\n"),
            "running"
        );
        assert_eq!(parse_launchd_state("service = {}\n"), "loaded");
    }

    #[test]
    fn state_reset_rejects_broad_directories() {
        let mut config = Config {
            state_dir: "/".into(),
            socket: "/tmp/st3.sock".into(),
            ..Config::default()
        };
        assert!(validate_reset_target(&config).is_err());
        config.state_dir = "/var/lib/st3".into();
        validate_reset_target(&config).unwrap();
    }

    #[test]
    fn service_file_rollback_restores_or_removes_the_candidate() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("st3.service");
        fs::write(&file, b"candidate").unwrap();
        restore_file(&file, Some(b"previous")).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"previous");

        restore_file(&file, None).unwrap();
        assert!(!file.exists());
        restore_file(&file, None).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn service_readiness_requires_an_accepting_socket() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        fs::write(&socket, b"stale").unwrap();
        let error = wait_for_socket_for(&socket, std::time::Duration::from_millis(40))
            .expect_err("a stale path is not a ready service");
        assert!(error.to_string().contains("did not accept connections"));

        fs::remove_file(&socket).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        wait_for_socket_for(&socket, std::time::Duration::from_millis(40)).unwrap();
    }

    #[test]
    fn reset_does_not_force_stop_an_already_exited_pty() {
        let ids = running_pty_ids([
            st_runtime::PtyObservation {
                name: "running".into(),
                status: "running".into(),
                exit_code: None,
                pid: Some(1),
                created_at: Some("now".into()),
                display_name: None,
                tags: Default::default(),
            },
            st_runtime::PtyObservation {
                name: "exited".into(),
                status: "exited".into(),
                exit_code: Some(129),
                pid: None,
                created_at: Some("before".into()),
                display_name: None,
                tags: Default::default(),
            },
        ]);
        assert_eq!(ids, ["running".to_owned()].into_iter().collect());
    }

    #[test]
    fn an_instances_units_carry_its_environment_and_the_default_units_carry_none() {
        let instance = crate::instance::Instance::new(Path::new("/home/example"), "try").unwrap();
        let mut variables = vec![("ST_INSTANCE".to_owned(), "try".to_owned())];
        variables.extend(
            instance
                .environment()
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.display().to_string())),
        );
        variables.push(("ST_INSTANCE_HOST_XDG_RUNTIME_DIR".into(), String::new()));
        let unit = systemd_environment_lines(&variables);
        assert!(unit.contains("Environment=ST_INSTANCE=try\n"), "{unit}");
        assert!(unit.contains("Environment=XDG_STATE_HOME=/home/example/.st-instance/try/state\n"), "{unit}");
        assert!(unit.contains("Environment=CODEX_HOME=/home/example/.st-instance/try/codex\n"), "{unit}");
        assert!(unit.contains("Environment=ST_INSTANCE_HOST_XDG_RUNTIME_DIR=\n"), "{unit}");
        let plist = plist_environment(&variables);
        assert!(plist.contains("<key>ST_INSTANCE</key><string>try</string>"), "{plist}");
        assert!(plist.contains("<key>XDG_RUNTIME_DIR</key><string>/home/example/.st-instance/try/run</string>"), "{plist}");
        assert_eq!(systemd_environment_lines(&[]), "");
        assert_eq!(plist_environment(&[]), "");
        // Values with systemd specifiers cannot expand into something else.
        assert_eq!(
            systemd_environment_lines(&[("A".into(), "100%$HOME".into())]),
            "Environment=\"A=100%%$$HOME\"\n"
        );
    }

    #[test]
    fn the_default_install_keeps_its_service_names() {
        assert_eq!(service_name(), "st3.service");
        assert_eq!(replication_service_name(), "st3-replication.service");
        assert_eq!(service_label(), "com.compoundingtech.st3");
        assert_eq!(replication_service_label(), "com.compoundingtech.st3.replication");
    }
}
