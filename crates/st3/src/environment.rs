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
