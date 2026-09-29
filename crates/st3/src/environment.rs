//! The daemon and its children share the account's interactive login-shell environment.
//! Never mutate the process environment: the daemon has concurrent readers and children.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::model::DoctorCheck;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

type Environment = BTreeMap<String, String>;

#[derive(Default)]
struct Cache {
    captured: Option<(Instant, Result<Environment, String>)>,
}

impl Cache {
    fn get(&mut self, capture: impl FnOnce() -> Result<Environment>) -> Result<Environment> {
        if self
            .captured
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH_INTERVAL)
        {
            // Refresh on use, including shell configuration changes and credentials exported
            // by shell startup files. Keep failures visible, rather than using stale secrets.
            self.captured = Some((
                Instant::now(),
                capture().map_err(|error| format!("{error:#}")),
            ));
        }
        self.captured
            .as_ref()
            .unwrap()
            .1
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

pub fn snapshot() -> Result<Environment> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(Cache::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("daemon environment cache is poisoned"))?
        .get(st_runtime::login_environment)
        .context(
            "capture the daemon login-shell environment; check the account's shell startup files",
        )
}

const LINK_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds and CI run as this account's login shell sees it, so a tool missing from that PATH
/// stops them on this host without any error in the daemon. `mold` is required where the
/// repository links with it (`.cargo/config.toml`, Linux).
pub(crate) fn toolchain_check(environment: &Environment) -> DoctorCheck {
    toolchain_check_within(environment, LINK_TIMEOUT)
}

fn toolchain_check_within(environment: &Environment, timeout: Duration) -> DoctorCheck {
    let mold = cfg!(target_os = "linux");
    let required = ["cargo", "rustc", "mold", "sccache", "gh", "git", "nix"]
        .into_iter()
        .filter(|tool| mold || *tool != "mold");
    let missing: Vec<&str> = required
        .filter(|tool| st_runtime::resolve_executable(tool, environment).is_err())
        .collect();
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!(
            "not on the login shell PATH: {}",
            missing.join(", ")
        ));
    }
    // A link needs the compiler and linker; without them it only repeats the list above.
    let can_link = !missing
        .iter()
        .any(|tool| matches!(*tool, "cargo" | "rustc" | "mold"));
    if can_link && let Err(error) = link_small_crate(environment, mold, timeout) {
        problems.push(format!("linking a small crate failed: {error:#}"));
    }
    if problems.is_empty() {
        DoctorCheck {
            name: "toolchain".into(),
            status: "pass".into(),
            message: "the login shell PATH finds the build tools and links a small crate".into(),
        }
    } else {
        DoctorCheck {
            name: "toolchain".into(),
            status: "warn".into(),
            message: problems.join("; "),
        }
    }
}

fn link_small_crate(environment: &Environment, mold: bool, timeout: Duration) -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix(".st3-toolchain-")
        .tempdir()
        .context("create a scratch crate")?;
    std::fs::create_dir(root.path().join("src"))?;
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"st3-toolchain-check\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n")?;
    if mold {
        std::fs::create_dir(root.path().join(".cargo"))?;
        std::fs::write(
            root.path().join(".cargo/config.toml"),
            "[target.'cfg(target_os = \"linux\")']\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n",
        )?;
    }
    let log = root.path().join("cargo.log");
    let output = std::fs::File::create(&log)?;
    let mut child = command_in("cargo", environment)?
        .args(["build", "--offline", "--quiet"])
        .current_dir(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .spawn()
        .context("run cargo")?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("cargo did not finish within {} seconds", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(6).collect();
        anyhow::bail!(
            "cargo exited with {status}: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(())
}

pub(crate) fn command(program: &str) -> Result<Command> {
    command_in(program, &snapshot()?)
}

pub(crate) fn command_in(program: &str, environment: &Environment) -> Result<Command> {
    let executable = st_runtime::resolve_executable(program, environment)?;
    let mut command = Command::new(executable);
    command.env_clear().envs(environment);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshes_success_and_failure_after_the_interval() {
        let mut cache = Cache::default();
        assert!(cache.get(|| anyhow::bail!("shell unavailable")).is_err());
        cache.captured.as_mut().unwrap().0 = Instant::now() - REFRESH_INTERVAL;
        let first = BTreeMap::from([("PATH".into(), "/first".into())]);
        assert_eq!(cache.get(|| Ok(first.clone())).unwrap(), first);
        assert_eq!(
            cache.get(|| panic!("reuse the fresh snapshot")).unwrap(),
            first
        );
        cache.captured.as_mut().unwrap().0 = Instant::now() - REFRESH_INTERVAL;
        let second = BTreeMap::from([("PATH".into(), "/second".into())]);
        assert_eq!(cache.get(|| Ok(second.clone())).unwrap(), second);
    }

    fn tools(root: &std::path::Path, scripts: &[(&str, &str)]) -> Environment {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(root).unwrap();
        for (name, body) in scripts {
            let path = root.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        BTreeMap::from([("PATH".into(), root.display().to_string())])
    }

    const TOOLS: [&str; 7] = ["cargo", "rustc", "mold", "sccache", "gh", "git", "nix"];

    #[test]
    fn toolchain_check_reports_what_the_login_path_lacks() {
        let root = tempfile::tempdir().unwrap();
        let environment = tools(root.path(), &[("git", "exit 0"), ("gh", "exit 0")]);
        let check = toolchain_check_within(&environment, Duration::from_secs(5));
        assert_eq!(check.name, "toolchain");
        assert_eq!(check.status, "warn");
        for missing in ["cargo", "rustc", "sccache", "nix"] {
            assert!(check.message.contains(missing), "{}", check.message);
        }
        assert_eq!(check.message.contains("mold"), cfg!(target_os = "linux"));
        assert!(!check.message.contains("git"), "{}", check.message);
        assert!(!check.message.contains("linking"), "{}", check.message);
    }

    #[test]
    fn toolchain_check_links_a_small_crate_and_reports_a_failed_link() {
        let root = tempfile::tempdir().unwrap();
        let every = TOOLS.map(|tool| (tool, "exit 0"));
        let mut scripts = every.to_vec();
        scripts.retain(|(tool, _)| *tool != "cargo");
        scripts.push((
            "cargo",
            "[ \"$1 $2 $3\" = 'build --offline --quiet' ] && [ -f Cargo.toml ] && [ -f src/main.rs ]",
        ));
        let environment = tools(&root.path().join("ok"), &scripts);
        let check = toolchain_check_within(&environment, Duration::from_secs(5));
        assert_eq!(check.status, "pass", "{}", check.message);

        scripts.retain(|(tool, _)| *tool != "cargo");
        scripts.push(("cargo", "echo 'linker orchid not found' >&2; exit 101"));
        let environment = tools(&root.path().join("broken"), &scripts);
        let check = toolchain_check_within(&environment, Duration::from_secs(5));
        assert_eq!(check.status, "warn");
        assert!(
            check.message.contains("linking a small crate failed")
                && check.message.contains("linker orchid not found"),
            "{}",
            check.message
        );

        scripts.retain(|(tool, _)| *tool != "cargo");
        // The PATH holds only these scripts, so hang with a shell builtin rather than `sleep`.
        scripts.push(("cargo", "while :; do :; done"));
        let environment = tools(&root.path().join("hung"), &scripts);
        let started = Instant::now();
        let check = toolchain_check_within(&environment, Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(check.status, "warn");
        assert!(
            check.message.contains("did not finish"),
            "{}",
            check.message
        );
    }

    #[test]
    fn commands_use_only_the_captured_environment_and_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("orchid-tool");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s' \"$ORCHID_VALUE\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let environment = BTreeMap::from([
            ("PATH".into(), root.path().display().to_string()),
            ("ORCHID_VALUE".into(), "from-shell".into()),
        ]);
        let output = command_in("orchid-tool", &environment)
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"from-shell");
        assert!(command_in("missing-orchid-tool", &environment).is_err());
    }
}
