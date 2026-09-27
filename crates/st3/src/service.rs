//! Install the st3 daemon as a native user service.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use anyhow::bail;

use crate::config::Config;

#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "st3.service";
#[cfg(target_os = "linux")]
const REPLICATION_SERVICE_NAME: &str = "st3-replication.service";
const SERVICE_LABEL: &str = "com.compoundingtech.st3";
const REPLICATION_SERVICE_LABEL: &str = "com.compoundingtech.st3.replication";
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
    memory_max_mb: u64,
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
            memory_max_mb,
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
        if let Some(pty_root) = &self.config.pty_root {
            arguments.extend(["--pty-root".into(), pty_root.display().to_string()]);
        }
        if let Some(peer_listen) = &self.config.peer_listen {
            arguments.extend(["--peer-listen".into(), peer_listen.clone()]);
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
        arguments
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
        if let Some(peer_listen) = &self.config.peer_listen {
            arguments.extend(["--peer-listen".into(), peer_listen.clone()]);
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
        arguments
    }
}

pub fn install(mut config: Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        st_runtime::isolation_mode() != st_runtime::Isolation::DegradedDetached,
        "st3 service install needs a working systemd user manager and transient user scopes"
    );
    let exe = env::current_exe().context("resolve the current st3 executable")?;
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
    let spec = ServiceSpec::new(exe, config, DEFAULT_MEMORY_MAX_MB)?;
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
        let executable = env::current_exe().context("resolve the current st3 executable")?;
        Ok(macos_permission_guidance(&executable))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok("st3 does not need a macOS privacy approval on this host.\n".into())
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
        "st3 executable\t{}\n\
1. Open System Settings > Privacy & Security > Full Disk Access.\n\
2. Add the st3 executable and enable it.\n\
3. Open System Settings > Privacy & Security > Developer Tools.\n\
4. Add the st3 executable and enable it.\n\
5. Run `st3 service restart` after an approval changes.\n\
macOS assigns service-owned file and developer access to st3, not to the terminal that installed it.\n\
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
        Err(error) => return Err(error).context("erase the st3 state directory"),
    }
    if !config.socket.starts_with(&config.state_dir) {
        match fs::remove_file(&config.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("erase the st3 socket"),
        }
    }
    if !config.client_gateway_socket.starts_with(&config.state_dir) {
        match fs::remove_file(&config.client_gateway_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("erase the st3 client gateway socket"),
        }
    }
    start_native_service()?;
    wait_for_service_sockets(&config)?;
    println!("reset\t{}", config.state_dir.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    uninstall_native_service()?;
    println!("uninstalled");
    Ok(())
}

fn validate_reset_target(config: &Config) -> Result<()> {
    anyhow::ensure!(
        config.state_dir.is_absolute(),
        "the st3 state directory must be absolute"
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
        "the st3 state directory is too broad to erase"
    );
    Ok(())
}

fn stop_owned_runtimes(config: &Config) -> Result<()> {
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
    let _ = Command::new("systemctl")
        .args(["--user", "stop", REPLICATION_SERVICE_NAME])
        .status();
    run_command("systemctl", &["--user", "restart", SERVICE_NAME])?;
    if config.fleet_id.is_some() {
        run_command("systemctl", &["--user", "start", REPLICATION_SERVICE_NAME])?;
    }
    wait_for_service_sockets(config)
}

#[cfg(target_os = "linux")]
fn uninstall_native_service() -> Result<()> {
    uninstall_systemd_user()
}

#[cfg(target_os = "linux")]
fn stop_native_service() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", REPLICATION_SERVICE_NAME])
        .status();
    run_command("systemctl", &["--user", "stop", SERVICE_NAME])
}

#[cfg(target_os = "linux")]
fn start_native_service() -> Result<()> {
    run_command("systemctl", &["--user", "start", SERVICE_NAME])?;
    if replication_systemd_user_unit_path()?.exists() {
        run_command("systemctl", &["--user", "start", REPLICATION_SERVICE_NAME])?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_native_service(spec: &ServiceSpec) -> Result<()> {
    let plist = launch_agent_path()?;
    let replication_plist = replication_launch_agent_path()?;
    let logs = spec.config.state_dir.join("logs");
    fs::create_dir_all(&logs)?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    let previous = read_existing_file(&plist)?;
    let previous_replication = read_existing_file(&replication_plist)?;
    fs::write(&plist, render_launchd_plist(spec))?;
    if spec.config.fleet_id.is_some() {
        fs::write(&replication_plist, render_launchd_replication_plist(spec))?;
    }
    let domain = launch_domain();
    let service = format!("{domain}/{SERVICE_LABEL}");
    let replication_service = format!("{domain}/{REPLICATION_SERVICE_LABEL}");
    let install = (|| -> Result<()> {
        let _ = Command::new("launchctl")
            .args(["bootout", &service])
            .status();
        let _ = Command::new("launchctl")
            .args(["bootout", &replication_service])
            .status();
        run_command("launchctl", &["enable", &service])?;
        run_command(
            "launchctl",
            &["bootstrap", &domain, &plist.display().to_string()],
        )?;
        run_command("launchctl", &["kickstart", &service])?;
        if spec.config.fleet_id.is_some() {
            run_command("launchctl", &["enable", &replication_service])?;
            run_command(
                "launchctl",
                &[
                    "bootstrap",
                    &domain,
                    &replication_plist.display().to_string(),
                ],
            )?;
            run_command("launchctl", &["kickstart", &replication_service])?;
        } else {
            let _ = Command::new("launchctl")
                .args(["disable", &replication_service])
                .status();
            if replication_plist.exists() {
                fs::remove_file(&replication_plist)?;
            }
        }
        wait_for_service_sockets(&spec.config)
    })();
    if let Err(error) = install {
        let rollback = (|| -> Result<()> {
            let _ = Command::new("launchctl")
                .args(["bootout", &service])
                .status();
            let _ = Command::new("launchctl")
                .args(["bootout", &replication_service])
                .status();
            restore_file(&plist, previous.as_deref())?;
            restore_file(&replication_plist, previous_replication.as_deref())?;
            if previous.is_some() {
                run_command("launchctl", &["enable", &service])?;
                run_command(
                    "launchctl",
                    &["bootstrap", &domain, &plist.display().to_string()],
                )?;
                run_command("launchctl", &["kickstart", &service])?;
            } else {
                let _ = Command::new("launchctl")
                    .args(["disable", &service])
                    .status();
            }
            if previous_replication.is_some() {
                run_command("launchctl", &["enable", &replication_service])?;
                run_command(
                    "launchctl",
                    &[
                        "bootstrap",
                        &domain,
                        &replication_plist.display().to_string(),
                    ],
                )?;
                run_command("launchctl", &["kickstart", &replication_service])?;
            }
            Ok(())
        })();
        if let Err(rollback) = rollback {
            return Err(error).context(format!(
                "the launchd install failed, and rollback also failed: {rollback:#}"
            ));
        }
        return Err(error).context("the launchd install failed; st3 restored the prior service");
    }
    println!("plist\t{}", plist.display());
    if spec.config.fleet_id.is_some() {
        println!("replication-plist\t{}", replication_plist.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_native_service() -> Result<ServiceStatusReport> {
    Ok(ServiceStatusReport {
        manager: "launchd-user",
        services: vec![
            launchd_service_status(SERVICE_LABEL, &launch_agent_path()?)?,
            launchd_service_status(REPLICATION_SERVICE_LABEL, &replication_launch_agent_path()?)?,
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
    let output = Command::new("launchctl")
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
    let replication_service = format!("{domain}/{REPLICATION_SERVICE_LABEL}");
    let _ = Command::new("launchctl")
        .args(["bootout", &replication_service])
        .status();
    run_command(
        "launchctl",
        &["kickstart", "-k", &format!("{domain}/{SERVICE_LABEL}")],
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
    let service = format!("{}/{SERVICE_LABEL}", launch_domain());
    let _ = Command::new("launchctl")
        .args(["bootout", &service])
        .status();
    let _ = Command::new("launchctl")
        .args(["disable", &service])
        .status();
    let replication_service = format!("{}/{REPLICATION_SERVICE_LABEL}", launch_domain());
    let _ = Command::new("launchctl")
        .args(["bootout", &replication_service])
        .status();
    let _ = Command::new("launchctl")
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
    let _ = Command::new("launchctl")
        .args([
            "bootout",
            &format!("{}/{REPLICATION_SERVICE_LABEL}", launch_domain()),
        ])
        .status();
    run_command(
        "launchctl",
        &["bootout", &format!("{}/{SERVICE_LABEL}", launch_domain())],
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
        &["kickstart", &format!("{domain}/{SERVICE_LABEL}")],
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
                &format!("{domain}/{REPLICATION_SERVICE_LABEL}"),
            ],
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd_user(spec: &ServiceSpec) -> Result<()> {
    let unit_path = systemd_user_unit_path()?;
    let replication_path = replication_systemd_user_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let previous = read_existing_file(&unit_path)?;
    let previous_replication = read_existing_file(&replication_path)?;
    fs::write(&unit_path, render_systemd_user_unit(spec))?;
    if spec.config.fleet_id.is_some() {
        fs::write(&replication_path, render_systemd_replication_unit(spec))?;
    }
    let install = (|| -> Result<()> {
        run_command("systemctl", &["--user", "daemon-reload"])?;
        run_command("systemctl", &["--user", "enable", SERVICE_NAME])?;
        run_command("systemctl", &["--user", "restart", SERVICE_NAME])?;
        if spec.config.fleet_id.is_some() {
            run_command("systemctl", &["--user", "enable", REPLICATION_SERVICE_NAME])?;
            run_command(
                "systemctl",
                &["--user", "restart", REPLICATION_SERVICE_NAME],
            )?;
        } else {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", REPLICATION_SERVICE_NAME])
                .status();
            if replication_path.exists() {
                fs::remove_file(&replication_path)?;
                run_command("systemctl", &["--user", "daemon-reload"])?;
            }
        }
        wait_for_service_sockets(&spec.config)
    })();
    if let Err(error) = install {
        let rollback = (|| -> Result<()> {
            if previous.is_some() {
                restore_file(&unit_path, previous.as_deref())?;
                restore_file(&replication_path, previous_replication.as_deref())?;
                run_command("systemctl", &["--user", "daemon-reload"])?;
                run_command("systemctl", &["--user", "restart", SERVICE_NAME])?;
                if previous_replication.is_some() {
                    run_command(
                        "systemctl",
                        &["--user", "restart", REPLICATION_SERVICE_NAME],
                    )?;
                }
            } else {
                let _ = Command::new("systemctl")
                    .args(["--user", "disable", "--now", SERVICE_NAME])
                    .status();
                let _ = Command::new("systemctl")
                    .args(["--user", "disable", "--now", REPLICATION_SERVICE_NAME])
                    .status();
                restore_file(&unit_path, None)?;
                restore_file(&replication_path, previous_replication.as_deref())?;
                run_command("systemctl", &["--user", "daemon-reload"])?;
            }
            Ok(())
        })();
        if let Err(rollback) = rollback {
            return Err(error).context(format!(
                "the systemd install failed, and rollback also failed: {rollback:#}"
            ));
        }
        return Err(error).context("the systemd install failed; st3 restored the prior service");
    }
    println!("unit\t{}", unit_path.display());
    if spec.config.fleet_id.is_some() {
        println!("replication-unit\t{}", replication_path.display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn status_systemd_user() -> Result<ServiceStatusReport> {
    Ok(ServiceStatusReport {
        manager: "systemd-user",
        services: vec![
            systemd_service_status(SERVICE_NAME)?,
            systemd_service_status(REPLICATION_SERVICE_NAME)?,
        ],
    })
}

#[cfg(target_os = "linux")]
fn systemd_service_status(name: &str) -> Result<ServiceStatus> {
    let output = Command::new("systemctl")
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
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", REPLICATION_SERVICE_NAME])
        .status();
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
    bail!("st3 service is available only on Linux and macOS")
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
    let status = Command::new(program)
        .args(arguments)
        .status()
        .with_context(|| format!("run {program} {}", arguments.join(" ")))?;
    anyhow::ensure!(status.success(), "{program} failed with {status}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist")),
    )
}

#[cfg(target_os = "macos")]
fn replication_launch_agent_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join("Library/LaunchAgents")
            .join(format!("{REPLICATION_SERVICE_LABEL}.plist")),
    )
}

#[cfg(target_os = "macos")]
fn launch_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    Ok(base
        .context("HOME and XDG_CONFIG_HOME are not set")?
        .join("systemd/user")
        .join(SERVICE_NAME))
}

#[cfg(target_os = "linux")]
fn replication_systemd_user_unit_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    Ok(base
        .context("HOME and XDG_CONFIG_HOME are not set")?
        .join("systemd/user")
        .join(REPLICATION_SERVICE_NAME))
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
        "the st3 service did not accept connections at {}",
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

pub fn render_systemd_user_unit(spec: &ServiceSpec) -> String {
    let exec_start = spec
        .program_arguments()
        .iter()
        .map(|argument| systemd_quote_arg(argument))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
Description=st3 claims graph daemon\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec_start}\n\
Environment=MALLOC_ARENA_MAX=2\n\
Restart=on-failure\n\
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

pub fn render_systemd_replication_unit(spec: &ServiceSpec) -> String {
    render_systemd_program_unit(
        "st3 authenticated replication worker",
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
    format!(
        "[Unit]\n\
Description={description}\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exec_start}\n\
Environment=MALLOC_ARENA_MAX=2\n\
Restart=on-failure\n\
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
        SERVICE_LABEL,
        &spec.program_arguments(),
        "st3.stdout.log",
        "st3.stderr.log",
        "Interactive",
        spec,
    )
}

pub fn render_launchd_replication_plist(spec: &ServiceSpec) -> String {
    render_launchd_program_plist(
        REPLICATION_SERVICE_LABEL,
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
    let stdout = spec.config.state_dir.join("logs").join(stdout_name);
    let stderr = spec.config.state_dir.join("logs").join(stderr_name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n{arguments}  </array>\n\
  <key>RunAtLoad</key><true/>\n\
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
            peer_listen: Some("127.0.0.1:31313".into()),
            peers: vec![PeerConfig {
                name: "node-b".into(),
                url: "http://127.0.0.1:31314".into(),
            }],
            planner: crate::model::PlannerSpec::default(),
        };
        let spec = ServiceSpec::new("/usr/bin/st3", config, 1024)?;
        let unit = render_systemd_user_unit(&spec);
        assert!(unit.contains("ExecStart=/usr/bin/st3 up --node node-a"));
        assert!(unit.contains("--state-dir /var/lib/st3"));
        assert!(unit.contains("--pty-root /var/lib/pty"));
        assert!(unit.contains("--client-gateway-socket /run/user/1000/st3-client.sock"));
        assert!(unit.contains("--peer node-b=http://127.0.0.1:31314"));
        assert!(unit.contains("MemoryMax=1024M"));
        assert!(unit.contains("Environment=MALLOC_ARENA_MAX=2"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("Nice=0"));
        assert!(unit.contains("CPUWeight=100"));
        assert!(unit.contains("KillMode=control-group"));
        let replication = render_systemd_replication_unit(&spec);
        assert!(replication.contains("replication-worker"));
        assert!(replication.contains("Nice=0"));
        assert!(replication.contains("CPUWeight=100"));
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
            state_dir: "/Users/test/Library/Application Support/st3".into(),
            socket: "/tmp/st3.sock".into(),
            ..Config::default()
        };
        let spec = ServiceSpec::new("/Users/test/bin/st3", config, 1024)?;
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
        let guidance = macos_permission_guidance(Path::new("/Users/test/bin/st3"));
        assert!(guidance.contains("st3 executable\t/Users/test/bin/st3"));
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
}
