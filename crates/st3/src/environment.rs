//! The daemon and its children share the account's interactive login-shell environment.
//! Never mutate the process environment: the daemon has concurrent readers and children.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

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

/// What building and gating this repository needs on the login PATH. The repository selects
/// mold for Linux links only, so other platforms do not need it.
fn build_tools() -> Vec<&'static str> {
    let mut tools = vec!["cargo", "rustc"];
    if cfg!(target_os = "linux") {
        tools.push("mold");
    }
    tools.extend(["sccache", "gh", "git", "nix"]);
    tools
}

const LINK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct BuildTools {
    pub found: Vec<&'static str>,
    pub missing: Vec<&'static str>,
    pub link: LinkResult,
}

pub(crate) enum LinkResult {
    Linked,
    /// Building needs cargo and rustc, so no crate was built.
    NotAttempted,
    Failed(String),
}

/// Look for each build tool on the environment's PATH, then build and link a small crate the way
/// the repository does.
pub(crate) fn check_build_tools(environment: &Environment) -> BuildTools {
    let (found, missing): (Vec<_>, Vec<_>) = build_tools()
        .into_iter()
        .partition(|tool| st_runtime::resolve_executable(tool, environment).is_ok());
    let can_link = ["cargo", "rustc"].iter().all(|tool| found.contains(tool));
    let link = if can_link {
        match link_small_crate(environment, LINK_TIMEOUT) {
            Ok(()) => LinkResult::Linked,
            Err(error) => LinkResult::Failed(error),
        }
    } else {
        LinkResult::NotAttempted
    };
    BuildTools {
        found,
        missing,
        link,
    }
}

fn link_small_crate(environment: &Environment, timeout: Duration) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let scratch =
        tempfile::tempdir().map_err(|error| format!("create a scratch crate: {error}"))?;
    let write = |path: &str, content: &str| {
        let path = scratch.path().join(path);
        std::fs::create_dir_all(path.parent().expect("scratch files have a parent"))
            .and_then(|()| std::fs::write(&path, content))
            .map_err(|error| format!("write {}: {error}", path.display()))
    };
    // The empty workspace keeps cargo from adopting a workspace above a temporary directory.
    write(
        "Cargo.toml",
        "[package]\nname = \"st-doctor-link\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )?;
    write("src/main.rs", "fn main() {}\n")?;
    write(
        ".cargo/config.toml",
        "[target.'cfg(target_os = \"linux\")']\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n",
    )?;
    let errors = scratch.path().join("cargo.stderr");
    let mut command = command_in("cargo", environment).map_err(|error| format!("{error:#}"))?;
    let mut child = command
        .args(["build", "--offline", "--quiet"])
        .current_dir(scratch.path())
        .env("CARGO_TARGET_DIR", scratch.path().join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&errors).map_err(|error| error.to_string())?)
        // Its own group lets a timeout stop cargo and the compiler and linker it started.
        .process_group(0)
        .spawn()
        .map_err(|error| format!("start cargo: {error}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                // SAFETY: signalling the process group that this function created.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!(
                    "cargo build took longer than {}s",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("wait for cargo: {error}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let stderr = std::fs::read_to_string(&errors).unwrap_or_default();
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    Err(format!(
        "cargo build {status}: {}",
        tail.chars().take(400).collect::<String>()
    ))
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

    fn fake_tool(directory: &std::path::Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn tools_environment(directory: &std::path::Path, absent: &[&str], cargo: &str) -> Environment {
        for tool in build_tools() {
            if !absent.contains(&tool) {
                fake_tool(
                    directory,
                    tool,
                    if tool == "cargo" { cargo } else { "exit 0" },
                );
            }
        }
        BTreeMap::from([("PATH".into(), directory.display().to_string())])
    }

    #[test]
    fn reports_each_build_tool_missing_from_the_login_path() {
        let root = tempfile::tempdir().unwrap();
        let environment = tools_environment(root.path(), &["sccache", "nix"], "exit 0");
        let report = check_build_tools(&environment);
        assert_eq!(report.missing, ["sccache", "nix"]);
        assert!(report.found.contains(&"cargo") && report.found.contains(&"git"));
        assert!(matches!(report.link, LinkResult::Linked));
    }

    #[test]
    fn nothing_is_built_without_cargo_or_rustc() {
        for absent in ["cargo", "rustc"] {
            let root = tempfile::tempdir().unwrap();
            let environment = tools_environment(root.path(), &[absent], "exit 0");
            let report = check_build_tools(&environment);
            assert_eq!(report.missing, [absent]);
            assert!(matches!(report.link, LinkResult::NotAttempted));
        }
    }

    #[test]
    fn the_scratch_crate_is_built_offline_with_the_repository_link_setup() {
        let root = tempfile::tempdir().unwrap();
        // Only shell builtins: the environment's PATH holds nothing else.
        let cargo = r#"[ "$1 $2 $3" = "build --offline --quiet" ] || exit 7
[ -f Cargo.toml ] && [ -f src/main.rs ] || exit 8
[ -n "$CARGO_TARGET_DIR" ] || exit 9
mold=no
while IFS= read -r line; do case $line in *link-arg=-fuse-ld=mold*) mold=yes;; esac; done < .cargo/config.toml
[ "$mold" = yes ] || exit 10"#;
        let environment = tools_environment(root.path(), &[], cargo);
        assert!(matches!(
            check_build_tools(&environment).link,
            LinkResult::Linked
        ));
    }

    #[test]
    fn a_failed_build_reports_the_last_lines_of_cargo_stderr() {
        let root = tempfile::tempdir().unwrap();
        let cargo = "for n in 1 2 3 4 5; do echo \"line $n\" >&2; done\nexit 101";
        let environment = tools_environment(root.path(), &[], cargo);
        let LinkResult::Failed(error) = check_build_tools(&environment).link else {
            panic!("the build should fail");
        };
        assert!(error.contains("line 3 | line 4 | line 5"), "{error}");
        assert!(!error.contains("line 2"), "{error}");
    }

    #[test]
    fn a_slow_build_is_stopped_with_the_processes_it_started() {
        let root = tempfile::tempdir().unwrap();
        let environment = tools_environment(root.path(), &[], "while :; do :; done");
        let started = Instant::now();
        let error = link_small_crate(&environment, Duration::from_millis(200)).unwrap_err();
        assert!(error.contains("longer than"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
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
