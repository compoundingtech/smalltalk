//! `st2 service install|render-unit|status|uninstall` — manage or render `st2 up` as a
//! **systemd-user** unit so the supervisor comes back on boot and on crash.
//!
//! **Lifecycle commands are Linux-only, by design (the maintainer).** The Mac stays MANUAL — the
//! maintainer runs `st2 up` themselves there (TCC: a launchd-owned process can't inherit their
//! GUI/keychain trust), so we deliberately do NOT ship a launchd path. Read-only rendering works
//! anywhere because it performs no service-manager operation.
//!
//! A service restart is safe because st2 spawns each task in its own transient scope
//! (`systemd-run --user --scope`, see `isolate.rs`) —
//! a SIBLING of this unit, not a child. So stopping/restarting `st2.service` reaps only the
//! supervisor loop; the agents survive in their scopes and a fresh supervisor ADOPTS them
//! (tests/nomad_survival.rs + tests/transport_isolation.rs). That is the whole reason this unit is
//! now a safe thing to install.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use std::process::Command;

use anyhow::{Context, Result, bail};

#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "st2.service";
/// 1 GiB is generous headroom for an I/O-light sync supervisor (folder-watch + sleep + shell-outs);
/// the agents themselves live in sibling scopes and are NOT bounded by this.
pub const DEFAULT_MEMORY_MAX_MB: u64 = 1024;

/// Everything the unit's `ExecStart` needs, resolved from the invoking environment.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// Absolute path to the invoking `st2` binary (`env::current_exe()`).
    exe: PathBuf,
    /// Absolute catalog (or spec-file) path handed to `st2 up`.
    catalog: PathBuf,
    /// Baked `--host`; `None` lets `st2 up` auto-detect the hostname (matching a manual `st2 up`).
    host: Option<String>,
    /// Explicit command search path determined by st2 for the supervisor and every launched task.
    /// It includes the installed st2 directory, common user tool directories, and system paths.
    path: String,
    /// Optional machine-local pty registry. This is deliberately independent of the synced catalog:
    /// an explicit adoption can use an existing registry without syncing pid/socket state.
    pty_root: Option<PathBuf>,
    /// Optional host residency policy baked into the supervised `st2 up` command.
    residency_policy: Option<crate::residency_host::HostPolicy>,
    memory_max_mb: u64,
    /// Ambient `OTEL_*` environment captured at install time and re-serialized into the unit, so
    /// the supervised supervisor reaches the same OTLP endpoint as an interactive `st2 up`.
    otel_env: Vec<(String, String)>,
}

impl ServiceSpec {
    pub fn new(
        exe: impl Into<PathBuf>,
        catalog: impl Into<PathBuf>,
        host: Option<String>,
        path: impl Into<String>,
        pty_root: Option<PathBuf>,
        residency_policy: Option<crate::residency_host::HostPolicy>,
        memory_max_mb: u64,
        otel_env: Vec<(String, String)>,
    ) -> Result<Self> {
        if memory_max_mb == 0 {
            bail!("--memory-max-mb must be greater than zero");
        }
        let path = path.into();
        if path.is_empty() {
            bail!("service PATH cannot be empty");
        }
        if pty_root.as_ref().is_some_and(|root| !root.is_absolute()) {
            bail!("--pty-root must be an absolute path");
        }
        Ok(Self {
            exe: exe.into(),
            catalog: catalog.into(),
            host,
            path,
            pty_root,
            residency_policy,
            memory_max_mb,
            otel_env,
        })
    }

    /// The `ExecStart` argv: `<st2> up --catalog <catalog> [--host <h>] [residency policy]`.
    fn program_arguments(&self) -> Vec<String> {
        let mut args = vec![
            self.exe.display().to_string(),
            "up".to_string(),
            "--catalog".to_string(),
            self.catalog.display().to_string(),
        ];
        if let Some(host) = &self.host {
            args.push("--host".to_string());
            args.push(host.clone());
        }
        if let Some(policy) = self.residency_policy {
            args.push("--residency-idle-after".to_string());
            args.push(format!("{}ms", policy.idle_after.as_millis()));
            args.push("--residency-warm-capacity".to_string());
            args.push(policy.warm_capacity.to_string());
        }
        args
    }
}

/// Ambient `OTEL_*` variables worth carrying into a unit. Captured from the invoking environment
/// because the systemd user manager has no shell to expand them from.
pub(crate) fn collect_otel_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("OTEL_"))
        .collect()
}

fn resolve_service_spec(
    catalog: &Path,
    host: Option<String>,
    pty_root: Option<PathBuf>,
    residency_policy: Option<crate::residency_host::HostPolicy>,
    memory_max_mb: u64,
    catalog_action: &str,
) -> Result<ServiceSpec> {
    let exe = env::current_exe().context("failed to resolve the current st2 executable")?;
    let path = service_path(&exe)?;
    // A systemd unit runs from no shell and no cwd — the catalog MUST be absolute, and it must
    // exist when the unit is rendered.
    let catalog = catalog.canonicalize().with_context(|| {
        format!(
            "catalog {} does not exist — create it before {catalog_action}",
            catalog.display()
        )
    })?;
    let pty_root = pty_root
        .map(|root| {
            root.canonicalize()
                .with_context(|| format!("pty root {} does not exist", root.display()))
        })
        .transpose()?;
    ServiceSpec::new(
        exe,
        catalog,
        host,
        path,
        pty_root,
        residency_policy,
        memory_max_mb,
        collect_otel_env(),
    )
}

/// `st2 service install [--catalog <catalog>] [--host H] [--pty-root PATH]
/// [--residency-idle-after DURATION --residency-warm-capacity N] [--memory-max-mb N]`.
pub fn install(
    catalog: &Path,
    host: Option<String>,
    pty_root: Option<PathBuf>,
    residency_policy: Option<crate::residency_host::HostPolicy>,
    memory_max_mb: u64,
) -> Result<()> {
    let spec = resolve_service_spec(
        catalog,
        host,
        pty_root,
        residency_policy,
        memory_max_mb,
        "installing the service",
    )?;

    install_systemd_user(&spec)?;

    println!("installed");
    println!("catalog\t{}", spec.catalog.display());
    match &spec.host {
        Some(h) => println!("host\t{h}"),
        None => println!("host\t(auto-detected at runtime)"),
    }
    match &spec.pty_root {
        Some(root) => println!("pty-root\t{}", root.display()),
        None => println!("pty-root\t{}/pty (catalog default)", spec.catalog.display()),
    }
    println!("memory-max-mb\t{memory_max_mb}");
    Ok(())
}

/// Render exactly the unit `install` would write for the same invoking environment and options.
pub fn render_unit(
    catalog: &Path,
    host: Option<String>,
    pty_root: Option<PathBuf>,
    residency_policy: Option<crate::residency_host::HostPolicy>,
    memory_max_mb: u64,
) -> Result<String> {
    let spec = resolve_service_spec(
        catalog,
        host,
        pty_root,
        residency_policy,
        memory_max_mb,
        "rendering the service unit",
    )?;
    Ok(render_systemd_user_unit(&spec))
}

/// Build a stable service PATH without reading the caller's PATH. Reinstalling from an agent must
/// not retain an older release directory, workspace wrappers, temporary Codex paths, or duplicates.
fn service_path(exe: &Path) -> Result<String> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    service_path_for(exe, &home)
}

fn service_path_for(exe: &Path, home: &Path) -> Result<String> {
    let mut entries = Vec::new();
    let mut push = |entry: PathBuf| {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    };

    if let Some(parent) = exe.parent() {
        push(parent.to_path_buf());
    }
    if let Some(nvm_bin) = selected_nvm_bin(home) {
        push(nvm_bin);
    }
    for relative in [".cargo/bin", ".local/bin", "bin", ".deno/bin"] {
        push(home.join(relative));
    }
    push(PathBuf::from("/usr/local/go/bin"));
    push(home.join("go/bin"));
    for system in [
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        "/snap/bin",
    ] {
        push(PathBuf::from(system));
    }

    env::join_paths(entries)
        .context("service PATH contains an unsupported byte")
        .map(|path| path.to_string_lossy().into_owned())
}

/// Select the highest installed NVM version that matches its numeric default alias. Non-numeric
/// aliases such as `node` fall back to the highest installed version. This reads no shell state.
fn selected_nvm_bin(home: &Path) -> Option<PathBuf> {
    let nvm = home.join(".nvm");
    let versions_root = nvm.join("versions/node");
    let alias = fs::read_to_string(nvm.join("alias/default")).unwrap_or_default();
    let prefix = numeric_version(alias.trim());
    let installed = fs::read_dir(&versions_root)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = numeric_version(entry.file_name().to_str()?);
            (!version.is_empty() && entry.path().join("bin").is_dir())
                .then_some((version, entry.path().join("bin")))
        })
        .collect::<Vec<_>>();
    if prefix.is_empty() {
        installed
            .into_iter()
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, bin)| bin)
    } else {
        installed
            .into_iter()
            .filter(|(version, _)| version.starts_with(&prefix))
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, bin)| bin)
    }
}

fn numeric_version(value: &str) -> Vec<u64> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts = value
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    parts.unwrap_or_default()
}

/// `st2 service status` — show the unit's systemd status.
pub fn status() -> Result<()> {
    status_systemd_user()
}

/// `st2 service uninstall` — stop, disable, and remove the unit. Idempotent.
pub fn uninstall() -> Result<()> {
    uninstall_systemd_user()?;
    println!("uninstalled");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd_user(spec: &ServiceSpec) -> Result<()> {
    let unit_path = systemd_user_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&unit_path, render_systemd_user_unit(spec))
        .with_context(|| format!("failed to write {}", unit_path.display()))?;

    // daemon-reload → enable (boot) → restart (start now, idempotent over a running instance).
    run_command("systemctl", &["--user", "daemon-reload"])?;
    run_command("systemctl", &["--user", "enable", SERVICE_NAME])?;
    run_command("systemctl", &["--user", "restart", SERVICE_NAME])?;
    println!("unit\t{}", unit_path.display());
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_systemd_user(_spec: &ServiceSpec) -> Result<()> {
    unsupported()
}

#[cfg(target_os = "linux")]
fn status_systemd_user() -> Result<()> {
    run_command(
        "systemctl",
        &["--user", "status", SERVICE_NAME, "--no-pager"],
    )
}

#[cfg(not(target_os = "linux"))]
fn status_systemd_user() -> Result<()> {
    unsupported()
}

#[cfg(target_os = "linux")]
fn uninstall_systemd_user() -> Result<()> {
    // Best-effort stop+disable (ignore "not loaded"), then remove the unit and reload.
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .status();
    let unit_path = systemd_user_unit_path()?;
    if unit_path.exists() {
        fs::remove_file(&unit_path)
            .with_context(|| format!("failed to remove {}", unit_path.display()))?;
    }
    run_command("systemctl", &["--user", "daemon-reload"])?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn uninstall_systemd_user() -> Result<()> {
    unsupported()
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> Result<()> {
    bail!(
        "st2 service is Linux/systemd-user only (headless hosts like hetz). On macOS, run \
         `st2 up --catalog <catalog>` yourself — the Mac stays manual for TCC reasons (a launchd-owned \
         process can't inherit your GUI/keychain trust)."
    )
}

#[cfg(target_os = "linux")]
fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program} {}", args.join(" ")))?;
    if !status.success() {
        bail!("{program} {} failed with status {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_path() -> Result<PathBuf> {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("systemd/user").join(SERVICE_NAME))
}

/// Render the systemd-user unit. Pure (no I/O) so it is unit-testable on any OS.
pub fn render_systemd_user_unit(spec: &ServiceSpec) -> String {
    let exec_start = spec
        .program_arguments()
        .iter()
        .map(|arg| systemd_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let otel_env = spec
        .otel_env
        .iter()
        .map(|(key, value)| {
            format!(
                "Environment={}\n",
                systemd_quote_arg(&format!("{key}={value}"))
            )
        })
        .collect::<String>();
    format!(
        "[Unit]\n\
Description=st2 supervisor (st2 up)\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
Environment={}\n\
{}\
{otel_env}\
ExecStart={exec_start}\n\
Restart=on-failure\n\
RestartSec=5s\n\
MemoryMax={}M\n\
WorkingDirectory={}\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        systemd_quote_arg(&format!("PATH={}", spec.path)),
        spec.pty_root
            .as_ref()
            .map(|root| format!(
                "Environment={}\n",
                systemd_quote_arg(&format!("PTY_ROOT={}", root.display()))
            ))
            .unwrap_or_default(),
        spec.memory_max_mb,
        systemd_quote_arg(&spec.catalog.display().to_string())
    )
}

/// Quote an argument for a systemd `ExecStart` line: bare if it is all "safe" chars, else
/// double-quoted with systemd's escapes (`\`, `"`, `$`→`$$`, `%`→`%%`). Mirrors fabric.
fn systemd_quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'_' | b':' | b'-' | b'+' | b'=')
        })
    {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    for ch in arg.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn unit_runs_st2_up_with_restart_and_memory_limit() -> Result<()> {
        let spec = ServiceSpec::new(
            "/home/user/.cargo/bin/st2",
            "/home/user/catalog",
            None,
            "/home/user/.cargo/bin:/home/user/.local/bin:/usr/bin",
            None,
            None,
            DEFAULT_MEMORY_MAX_MB,
            Vec::new(),
        )?;

        let unit = render_systemd_user_unit(&spec);

        assert!(
            unit.contains("ExecStart=/home/user/.cargo/bin/st2 up --catalog /home/user/catalog")
        );
        // No --host baked when unset → st2 up auto-detects, same as a manual run.
        assert!(!unit.contains("--host"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=5s"));
        assert!(unit.contains("MemoryMax=1024M"));
        assert!(unit.contains("WorkingDirectory=/home/user/catalog"));
        assert!(
            unit.contains("Environment=PATH=/home/user/.cargo/bin:/home/user/.local/bin:/usr/bin")
        );
        assert!(!unit.contains("Environment=PTY_ROOT="));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("Description=st2 supervisor (st2 up)"));
        Ok(())
    }

    #[test]
    fn unit_bakes_host_when_provided() -> Result<()> {
        let spec = ServiceSpec::new(
            "/usr/local/bin/st2",
            "/srv/catalog",
            Some("hetz".to_string()),
            "/usr/local/bin:/usr/bin",
            Some(PathBuf::from("/srv/legacy-pty")),
            Some(crate::residency_host::HostPolicy {
                idle_after: Duration::from_secs(300),
                warm_capacity: 2,
            }),
            512,
            Vec::new(),
        )?;

        let unit = render_systemd_user_unit(&spec);

        assert!(unit.contains(
            "ExecStart=/usr/local/bin/st2 up --catalog /srv/catalog --host hetz \
             --residency-idle-after 300000ms --residency-warm-capacity 2"
        ));
        assert!(unit.contains("MemoryMax=512M"));
        assert!(unit.contains("Environment=PTY_ROOT=/srv/legacy-pty"));
        Ok(())
    }

    #[test]
    fn unit_quotes_paths_with_spaces_and_escapes_specifiers() -> Result<()> {
        let spec = ServiceSpec::new(
            "/opt/st2 tools/st2",
            "/srv/cat 100%",
            None,
            "/opt/st2 tools:/usr/bin",
            Some(PathBuf::from("/srv/pty 100%")),
            None,
            256,
            Vec::new(),
        )?;

        let unit = render_systemd_user_unit(&spec);

        assert!(unit.contains("ExecStart=\"/opt/st2 tools/st2\" up --catalog \"/srv/cat 100%%\""));
        assert!(unit.contains("WorkingDirectory=\"/srv/cat 100%%\""));
        assert!(unit.contains("Environment=\"PATH=/opt/st2 tools:/usr/bin\""));
        assert!(unit.contains("Environment=\"PTY_ROOT=/srv/pty 100%%\""));
        Ok(())
    }

    #[test]
    fn zero_memory_max_is_rejected() {
        let err = ServiceSpec::new("/bin/st2", "/cat", None, "/bin", None, None, 0, Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn empty_path_and_relative_pty_root_are_rejected() {
        let err =
            ServiceSpec::new("/bin/st2", "/cat", None, "", None, None, 1, Vec::new()).unwrap_err();
        assert!(err.to_string().contains("PATH cannot be empty"));

        let err = ServiceSpec::new(
            "/bin/st2",
            "/cat",
            None,
            "/bin",
            Some(PathBuf::from("relative")),
            None,
            1,
            Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn service_path_is_deterministic_and_deduplicates_the_executable_directory() -> Result<()> {
        let home = Path::new("/home/operator");
        let path = service_path_for(Path::new("/home/operator/.cargo/bin/st2"), home)?;
        let entries = env::split_paths(&path).collect::<Vec<_>>();

        assert_eq!(
            entries,
            vec![
                home.join(".cargo/bin"),
                home.join(".local/bin"),
                home.join("bin"),
                home.join(".deno/bin"),
                PathBuf::from("/usr/local/go/bin"),
                home.join("go/bin"),
                PathBuf::from("/usr/local/sbin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/sbin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/sbin"),
                PathBuf::from("/bin"),
                PathBuf::from("/snap/bin"),
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| *entry == &home.join(".cargo/bin"))
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn nvm_default_selects_the_highest_matching_installed_version() -> Result<()> {
        let home = tempfile::tempdir()?;
        let nvm = home.path().join(".nvm");
        fs::create_dir_all(nvm.join("alias"))?;
        fs::write(nvm.join("alias/default"), "24\n")?;
        for version in ["v22.23.1", "v24.9.0", "v24.18.0", "v25.0.0"] {
            fs::create_dir_all(nvm.join("versions/node").join(version).join("bin"))?;
        }

        assert_eq!(
            selected_nvm_bin(home.path()),
            Some(nvm.join("versions/node/v24.18.0/bin"))
        );
        Ok(())
    }

    #[test]
    fn nvm_symbolic_default_uses_the_highest_installed_version() -> Result<()> {
        let home = tempfile::tempdir()?;
        let nvm = home.path().join(".nvm");
        fs::create_dir_all(nvm.join("alias"))?;
        fs::write(nvm.join("alias/default"), "node\n")?;
        for version in ["v22.23.1", "v24.18.0"] {
            fs::create_dir_all(nvm.join("versions/node").join(version).join("bin"))?;
        }

        assert_eq!(
            selected_nvm_bin(home.path()),
            Some(nvm.join("versions/node/v24.18.0/bin"))
        );
        Ok(())
    }

    #[test]
    fn nvm_numeric_default_never_falls_back_to_another_major() -> Result<()> {
        let home = tempfile::tempdir()?;
        let nvm = home.path().join(".nvm");
        fs::create_dir_all(nvm.join("alias"))?;
        fs::write(nvm.join("alias/default"), "25\n")?;
        fs::create_dir_all(nvm.join("versions/node/v24.18.0/bin"))?;

        assert_eq!(selected_nvm_bin(home.path()), None);
        Ok(())
    }

    #[test]
    fn unit_emits_one_quoted_environment_line_per_captured_otel_var() -> Result<()> {
        let spec = ServiceSpec::new(
            "/usr/local/bin/st2",
            "/srv/catalog",
            None,
            "/usr/local/bin:/usr/bin",
            Some(PathBuf::from("/srv/pty")),
            None,
            DEFAULT_MEMORY_MAX_MB,
            vec![
                (
                    "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
                    "http://127.0.0.1:4317".to_string(),
                ),
                (
                    "OTEL_SERVICE_NAME".to_string(),
                    "st2 supervisor 100%".to_string(),
                ),
            ],
        )?;

        let unit = render_systemd_user_unit(&spec);

        // One `Environment=` line per captured var. All-safe-char values stay bare (quote rule),
        // values with spaces/`%` are double-quoted via systemd_quote_arg.
        assert!(unit.contains("Environment=OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317"));
        assert!(
            unit.contains("Environment=\"OTEL_SERVICE_NAME=st2 supervisor 100%%\""),
            "value with space/percent must go through systemd_quote_arg"
        );

        // Ordering: after the PTY_ROOT line, before ExecStart.
        let pty_root_at = unit.find("Environment=PTY_ROOT=/srv/pty").unwrap();
        let first_otel_at = unit.find("Environment=\"OTEL_").unwrap();
        let exec_start_at = unit.find("ExecStart=").unwrap();
        assert!(pty_root_at < first_otel_at && first_otel_at < exec_start_at);
        Ok(())
    }

    #[test]
    fn unit_without_otel_vars_emits_no_extra_environment_lines() -> Result<()> {
        let spec = ServiceSpec::new(
            "/usr/local/bin/st2",
            "/srv/catalog",
            None,
            "/usr/local/bin:/usr/bin",
            None,
            None,
            DEFAULT_MEMORY_MAX_MB,
            Vec::new(),
        )?;

        let unit = render_systemd_user_unit(&spec);

        assert!(!unit.contains("OTEL_"));
        Ok(())
    }
}
