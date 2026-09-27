use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const SPAWN_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtySpawnTimeoutPhase {
    Lock,
    Publication,
}

#[derive(Debug)]
pub struct PtySpawnTimeout {
    pub runtime_id: String,
    pub phase: PtySpawnTimeoutPhase,
    pub timeout: Duration,
}

impl fmt::Display for PtySpawnTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "timed out after {}ms waiting for PTY `{}` spawn {}",
            self.timeout.as_millis(),
            self.runtime_id,
            match self.phase {
                PtySpawnTimeoutPhase::Lock => "lock",
                PtySpawnTimeoutPhase::Publication => "publication",
            }
        )
    }
}

impl std::error::Error for PtySpawnTimeout {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Launch {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PtyObservation {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStats {
    name: String,
    process: PtyStatsProcess,
    daemon: PtyStatsDaemon,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStatsProcess {
    alive: bool,
    pid: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStatsDaemon {
    pid: u32,
}

#[derive(Clone, Debug)]
pub struct PtyRuntime {
    binary: String,
    root: PathBuf,
    spawn_timeout: Duration,
}

impl PtyRuntime {
    pub fn new(root: PathBuf) -> Self {
        crate::warn_if_degraded("st3");
        Self {
            binary: "pty".into(),
            root,
            spawn_timeout: SPAWN_PUBLICATION_TIMEOUT,
        }
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    #[cfg(test)]
    fn with_spawn_timeout(mut self, timeout: Duration) -> Self {
        self.spawn_timeout = timeout;
        self
    }

    pub fn snapshot(&self) -> Result<Vec<PtyObservation>> {
        let output = self.command().args(["list", "--json"]).output()?;
        require_success("list PTYs", output).and_then(|bytes| {
            serde_json::from_slice(&bytes).context("parse the atomic PTY snapshot")
        })
    }

    pub fn spawn(
        &self,
        id: &str,
        launch: &Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        display_name: Option<&str>,
        tags: &BTreeMap<String, String>,
    ) -> Result<()> {
        let _spawn_lock = self.acquire_spawn_lock(id)?;
        let fence = self.spawn_state_path(id, "pending");
        let mut before = self
            .snapshot()?
            .into_iter()
            .find(|observation| observation.name == id);
        if fence.is_file() {
            let previous = std::fs::read_to_string(&fence)
                .with_context(|| format!("read PTY publication fence {}", fence.display()))?;
            self.wait_for_publication(id, (!previous.is_empty()).then_some(previous.as_str()))?;
            std::fs::remove_file(&fence)
                .with_context(|| format!("clear PTY publication fence {}", fence.display()))?;
            before = self
                .snapshot()?
                .into_iter()
                .find(|observation| observation.name == id);
        }
        if before.as_ref().is_some_and(observation_is_live) {
            return Ok(());
        }
        let previous_incarnation = before.as_ref().and_then(observation_incarnation);
        std::fs::write(&fence, previous_incarnation.as_deref().unwrap_or_default())
            .with_context(|| format!("write PTY publication fence {}", fence.display()))?;
        let unit = crate::scope_unit("st3", id);
        let mut arguments = vec![
            OsString::from("run"),
            OsString::from("-d"),
            OsString::from("--force"),
            OsString::from("--id"),
            OsString::from(id),
            OsString::from("--cwd"),
            cwd.as_os_str().to_os_string(),
        ];
        if let Some(display_name) = display_name {
            arguments.extend([OsString::from("--name"), OsString::from(display_name)]);
        } else {
            arguments.push(OsString::from("--no-display-name"));
        }
        let mut terminal_env = env.clone();
        terminal_env
            .entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        for (key, value) in &terminal_env {
            arguments.extend([
                OsString::from("--env"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        let mut effective_tags = tags.clone();
        effective_tags.insert(
            "st3.isolation".into(),
            isolation_name(crate::isolation_mode()).into(),
        );
        if crate::isolation_mode() == crate::Isolation::Scope {
            effective_tags.insert("st3.scope-unit".into(), unit.clone());
        }
        for (key, value) in effective_tags {
            arguments.extend([
                OsString::from("--tag"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        arguments.push(OsString::from("--"));
        match launch {
            Launch::Shell(source) => {
                arguments.extend([OsString::from("sh"), OsString::from("-c"), source.into()]);
            }
            Launch::Argv(argv) => arguments.extend(argv.iter().map(OsString::from)),
        }
        let argument_refs = arguments
            .iter()
            .map(OsString::as_os_str)
            .collect::<Vec<_>>();
        const ATTEMPTS: u32 = 4;
        let mut last_error = String::new();
        for attempt in 0..ATTEMPTS {
            let mut command =
                crate::wrap_isolated(&unit, std::ffi::OsStr::new(&self.binary), &argument_refs);
            command.env("PTY_ROOT", &self.root);
            let output = match command.output() {
                Ok(output) => output,
                Err(error) => {
                    let _ = std::fs::remove_file(&fence);
                    return Err(error.into());
                }
            };
            if output.status.success() {
                self.wait_for_publication(id, previous_incarnation.as_deref())?;
                std::fs::remove_file(&fence)
                    .with_context(|| format!("clear PTY publication fence {}", fence.display()))?;
                return Ok(());
            }
            last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !last_error.contains("already in use") || attempt + 1 == ATTEMPTS {
                break;
            }
            if self
                .snapshot()?
                .iter()
                .any(|observation| observation.name == id && observation_is_live(observation))
            {
                let _ = std::fs::remove_file(&fence);
                return Ok(());
            }
            let _ = self.remove(id);
            std::thread::sleep(Duration::from_millis(100 * u64::from(attempt + 1)));
        }
        let _ = std::fs::remove_file(&fence);
        anyhow::bail!("spawn PTY failed: {last_error}")
    }

    fn acquire_spawn_lock(&self, id: &str) -> Result<File> {
        let directory = self.spawn_state_directory();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create PTY spawn-lock directory {}", directory.display()))?;
        let path = self.spawn_state_path(id, "lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open PTY spawn lock {}", path.display()))?;
        let deadline = Instant::now() + self.spawn_timeout;
        loop {
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(lock);
            }
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                return Err(error).with_context(|| format!("lock PTY spawn {}", path.display()));
            }
            if Instant::now() >= deadline {
                return Err(PtySpawnTimeout {
                    runtime_id: id.into(),
                    phase: PtySpawnTimeoutPhase::Lock,
                    timeout: self.spawn_timeout,
                }
                .into());
            }
            std::thread::sleep(SPAWN_POLL_INTERVAL.min(self.spawn_timeout));
        }
    }

    fn spawn_state_path(&self, id: &str, extension: &str) -> PathBuf {
        let digest = Sha256::digest(id.as_bytes());
        self.spawn_state_directory()
            .join(format!("{digest:x}.{extension}"))
    }

    fn spawn_state_directory(&self) -> PathBuf {
        let root = self.root.canonicalize().unwrap_or_else(|_| {
            let parent = self
                .root
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            self.root
                .file_name()
                .map(|name| parent.join(name))
                .unwrap_or(parent)
        });
        let digest = Sha256::digest(root.as_os_str().as_bytes());
        root.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".st3-pty-spawn-locks")
            .join(format!("{digest:x}"))
    }

    fn wait_for_publication(&self, id: &str, previous_incarnation: Option<&str>) -> Result<()> {
        let deadline = Instant::now() + self.spawn_timeout;
        loop {
            if self.snapshot()?.into_iter().any(|observation| {
                observation.name == id
                    && observation_incarnation(&observation)
                        .as_deref()
                        .is_some_and(|current| Some(current) != previous_incarnation)
            }) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(PtySpawnTimeout {
                    runtime_id: id.into(),
                    phase: PtySpawnTimeoutPhase::Publication,
                    timeout: self.spawn_timeout,
                }
                .into());
            }
            std::thread::sleep(SPAWN_POLL_INTERVAL.min(self.spawn_timeout));
        }
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        self.stop_if(id, None)
    }

    pub fn stop_if(&self, id: &str, expected_incarnation: Option<&str>) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        let output = self.command().args(["kill", id]).output()?;
        require_success("stop PTY", output)?;
        Ok(())
    }

    pub fn kill(&self, id: &str) -> Result<()> {
        self.kill_if(id, None)
    }

    pub fn kill_if(&self, id: &str, expected_incarnation: Option<&str>) -> Result<()> {
        self.signal_if(id, expected_incarnation, libc::SIGKILL)
    }

    pub fn signal_if(
        &self,
        id: &str,
        expected_incarnation: Option<&str>,
        signal: i32,
    ) -> Result<()> {
        let observation = self
            .snapshot()?
            .into_iter()
            .find(|item| item.name == id)
            .with_context(|| format!("PTY `{id}` is not present"))?;
        ensure_incarnation(id, &observation, expected_incarnation)?;
        let daemon_pid = observation
            .pid
            .with_context(|| format!("PTY `{id}` has no process identity"))?;

        // `pty list` exposes the supporting daemon PID, not the process group leader running
        // inside the terminal. Signalling that PID makes the registry disappear while leaving
        // the provider tree alive. Resolve the terminal child through the same daemon and fence
        // it against the list snapshot before delivering the signal.
        let output = self.command().args(["stats", id, "--json"]).output()?;
        let bytes = require_success("read PTY process identity", output)?;
        let stats: PtyStats =
            serde_json::from_slice(&bytes).context("parse PTY process identity")?;
        anyhow::ensure!(
            stats.name == id,
            "PTY stats returned `{}` for `{id}`",
            stats.name
        );
        anyhow::ensure!(
            stats.daemon.pid == daemon_pid,
            "PTY `{id}` changed incarnation before signal {signal}"
        );
        anyhow::ensure!(
            stats.process.alive,
            "PTY `{id}` has no live terminal process"
        );
        let pid = stats
            .process
            .pid
            .with_context(|| format!("PTY `{id}` has no terminal process identity"))?;
        let group = unsafe { libc::kill(-(pid as i32), signal) };
        if group != 0 {
            let direct = unsafe { libc::kill(pid as i32, signal) };
            if direct != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).with_context(|| format!("signal {signal} to PTY process"));
                }
            }
        }
        Ok(())
    }

    fn require_incarnation(&self, id: &str, expected_incarnation: Option<&str>) -> Result<()> {
        let observation = self
            .snapshot()?
            .into_iter()
            .find(|item| item.name == id)
            .with_context(|| format!("PTY `{id}` is not present"))?;
        ensure_incarnation(id, &observation, expected_incarnation)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let output = self.command().args(["remove", id]).output()?;
        require_success("remove PTY", output)?;
        Ok(())
    }

    pub fn attach(&self, id: &str) -> Result<()> {
        let status = self
            .command()
            .args(["attach", id])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        anyhow::ensure!(status.success(), "PTY attach failed with {status}");
        Ok(())
    }

    pub fn send_line(&self, id: &str, text: &str) -> Result<()> {
        self.send_line_if(id, text, None)
    }

    pub fn send_line_if(
        &self,
        id: &str,
        text: &str,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        let output = self
            .command()
            .args(["send", id, "--seq", text, "--seq", "key:return"])
            .output()?;
        require_success("send PTY input", output)?;
        Ok(())
    }

    pub fn send_raw(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.send_raw_if(id, bytes, None)
    }

    pub fn send_raw_if(
        &self,
        id: &str,
        bytes: &[u8],
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        anyhow::ensure!(
            !bytes.contains(&0),
            "terminal input cannot contain a NUL byte"
        );
        let output = self
            .command()
            .arg("send")
            .arg(id)
            .arg(OsString::from_vec(bytes.to_vec()))
            .output()?;
        require_success("send PTY input", output)?;
        Ok(())
    }

    pub fn send_key(&self, id: &str, key: &str) -> Result<()> {
        self.send_key_if(id, key, None)
    }

    pub fn send_key_if(
        &self,
        id: &str,
        key: &str,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        let output = self
            .command()
            .args(["send", id, "--seq", &format!("key:{key}")])
            .output()?;
        require_success("send PTY key", output)?;
        Ok(())
    }

    pub fn screen(&self, id: &str) -> Result<String> {
        let output = self.command().args(["peek", "--plain", id]).output()?;
        let bytes = require_success("read PTY screen", output)?;
        String::from_utf8(bytes).context("the PTY screen is not UTF-8")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn binary(&self) -> &str {
        &self.binary
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.env("PTY_ROOT", &self.root);
        command
    }
}

fn isolation_name(mode: crate::Isolation) -> &'static str {
    match mode {
        crate::Isolation::Scope => "scope",
        crate::Isolation::Detached => "detached",
        crate::Isolation::DegradedDetached => "degraded-detached",
    }
}

fn observation_incarnation(observation: &PtyObservation) -> Option<String> {
    match (&observation.pid, &observation.created_at) {
        (Some(pid), Some(created_at)) => Some(format!("{pid}:{created_at}")),
        _ => None,
    }
}

fn observation_is_live(observation: &PtyObservation) -> bool {
    if observation.status != "running" {
        return false;
    }
    observation.pid.is_some_and(|pid| {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    })
}

fn ensure_incarnation(
    id: &str,
    observation: &PtyObservation,
    expected_incarnation: Option<&str>,
) -> Result<()> {
    let current = match (&observation.pid, &observation.created_at) {
        (Some(pid), Some(created_at)) => Some(format!("{pid}:{created_at}")),
        _ => None,
    };
    if expected_incarnation.is_some_and(|expected| current.as_deref() != Some(expected)) {
        anyhow::bail!("PTY `{id}` changed incarnation before the control action");
    }
    Ok(())
}

fn require_success(action: &str, output: Output) -> Result<Vec<u8>> {
    anyhow::ensure!(
        output.status.success(),
        "{action} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::CommandExt as _;
    use std::time::{Duration, Instant};

    fn fake_executable(root: &Path, name: &str, body: &str) -> PathBuf {
        let source = root.join(format!("{name}.source"));
        let binary = root.join(name);
        fs::write(&source, body).unwrap();
        // A parallel test can fork while this process owns a writable file.
        // Let install create the executable so no child inherits that writer.
        let output = Command::new("install")
            .args(["-m", "700"])
            .arg(&source)
            .arg(&binary)
            .output()
            .unwrap();
        assert!(output.status.success(), "install failed: {output:?}");
        binary
    }

    #[test]
    fn terminal_input_checks_the_expected_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  printf '[{"name":"work","status":"running","pid":42,"createdAt":"now"}]'
  exit 0
fi
exit 0
"#,
        );
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        runtime
            .send_line_if("work", "hello", Some("42:now"))
            .unwrap();
        runtime
            .send_raw_if("work", b"bytes", Some("42:now"))
            .unwrap();
        runtime
            .send_key_if("work", "escape", Some("42:now"))
            .unwrap();
        let error = runtime
            .send_key_if("work", "escape", Some("41:old"))
            .unwrap_err();
        assert!(error.to_string().contains("changed incarnation"));
    }

    #[test]
    fn terminal_signal_targets_the_terminal_process_group_not_the_daemon() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-signal",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  printf '[{"name":"work","status":"running","pid":42,"createdAt":"now"}]'
  exit 0
fi
if [ "$1" = stats ]; then
  process_pid="$(cat "$0.process")"
  printf '{"name":"work","process":{"alive":true,"pid":%s},"daemon":{"pid":42}}' "$process_pid"
  exit 0
fi
exit 1
"#,
        );
        let mut command = Command::new("sh");
        command.args(["-c", "trap 'exit 0' HUP; while :; do sleep 1; done"]);
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        fs::write(binary.with_extension("process"), child.id().to_string()).unwrap();
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        runtime
            .signal_if("work", Some("42:now"), libc::SIGHUP)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal process group survived SIGHUP"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn spawn_records_the_shared_isolation_mode() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-spawn",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  if [ -f "$0.published" ]; then
    pid="$(cat "$0.pid")"
    printf '[{"name":"work","status":"running","pid":%s,"createdAt":"new"}]' "$pid"
  else
    printf '[]'
  fi
  exit 0
fi
printf '%s\n' "$@" > "$0.args"
touch "$0.published"
"#,
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());
        assert!(!runtime.spawn_state_directory().starts_with(runtime.root()));
        runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["sh".into(), "-c".into(), "true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap();
        let arguments = fs::read_to_string(binary.with_extension("args")).unwrap();
        assert!(arguments.contains("st3.isolation="));
        assert!(arguments.contains("TERM=xterm-256color"));
        assert!(arguments.contains("--force"));
        if crate::isolation_mode() == crate::Isolation::Scope {
            assert!(arguments.contains("st3.scope-unit=st3-work-"));
        }
    }

    #[test]
    fn spawn_preserves_an_explicit_terminal_type() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-term",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  if [ -f "$0.published" ]; then
    pid="$(cat "$0.pid")"
    printf '[{"name":"work","status":"running","pid":%s,"createdAt":"new"}]' "$pid"
  else
    printf '[]'
  fi
  exit 0
fi
printf '%s\n' "$@" > "$0.args"
touch "$0.published"
"#,
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::from([("TERM".into(), "screen-256color".into())]),
                None,
                &BTreeMap::new(),
            )
            .unwrap();

        let arguments = fs::read_to_string(binary.with_extension("args")).unwrap();
        assert!(arguments.contains("TERM=screen-256color"));
        assert!(!arguments.contains("TERM=xterm-256color"));
    }

    #[test]
    fn spawn_reaps_a_recent_session_id_and_retries() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-retry",
            r#"#!/bin/sh
if [ "$1" = run ]; then
  count=0
  test ! -f "$0.count" || count="$(cat "$0.count")"
  count=$((count + 1))
  printf '%s\n' "$count" > "$0.count"
  if [ "$count" -eq 1 ]; then
    printf '%s\n' 'Session id "work" is already in use.' >&2
    exit 1
  fi
  touch "$0.published"
fi
if [ "$1" = remove ]; then
  touch "$0.removed"
fi
if [ "$1" = list ]; then
  if [ -f "$0.published" ]; then
    pid="$(cat "$0.pid")"
    printf '[{"name":"work","status":"running","pid":%s,"createdAt":"new"}]' "$pid"
  else
    printf '[]'
  fi
fi
exit 0
"#,
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "2"
        );
        assert!(binary.with_extension("removed").is_file());
    }

    #[test]
    fn spawn_waits_for_an_exact_new_registry_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-delayed-publication",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  if [ ! -f "$0.started" ]; then
    printf '[{"name":"work","status":"exited","pid":999,"createdAt":"old"}]'
    exit 0
  fi
  count=0
  test ! -f "$0.lists" || count="$(cat "$0.lists")"
  count=$((count + 1))
  printf '%s\n' "$count" > "$0.lists"
  if [ "$count" -lt 3 ]; then
    printf '[{"name":"work","status":"exited","pid":999,"createdAt":"old"}]'
  else
    pid="$(cat "$0.pid")"
    printf '[{"name":"work","status":"running","pid":%s,"createdAt":"new"}]' "$pid"
  fi
  exit 0
fi
if [ "$1" = run ]; then
  touch "$0.started"
fi
exit 0
"#,
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap();

        assert!(
            fs::read_to_string(binary.with_extension("lists"))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
                >= 3
        );
    }

    #[test]
    fn concurrent_spawns_for_one_runtime_launch_only_once() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-concurrent",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  if [ -f "$0.published" ]; then
    pid="$(cat "$0.pid")"
    printf '[{"name":"work","status":"running","pid":%s,"createdAt":"new"}]' "$pid"
  else
    printf '[]'
  fi
  exit 0
fi
if [ "$1" = run ]; then
  count=0
  test ! -f "$0.count" || count="$(cat "$0.count")"
  count=$((count + 1))
  printf '%s\n' "$count" > "$0.count"
  touch "$0.published"
fi
exit 0
"#,
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        let runtime = std::sync::Arc::new(
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy()),
        );
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let cwd = root.path().to_path_buf();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                runtime.spawn(
                    "work",
                    &Launch::Argv(vec!["true".into()]),
                    &cwd,
                    &BTreeMap::new(),
                    None,
                    &BTreeMap::new(),
                )
            }));
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "1"
        );
    }

    #[test]
    fn publication_timeout_is_typed_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-never-publishes",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  printf '[]'
fi
exit 0
"#,
        );
        let timeout = Duration::from_millis(30);
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(timeout);
        let started = Instant::now();

        let error = runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap_err();

        let timeout_error = error.downcast_ref::<PtySpawnTimeout>().unwrap();
        assert_eq!(timeout_error.phase, PtySpawnTimeoutPhase::Publication);
        assert_eq!(timeout_error.timeout, timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_unresolved_publication_fences_followup_launches() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(
            root.path(),
            "fake-pty-unresolved-publication",
            r#"#!/bin/sh
if [ "$1" = list ]; then
  printf '[]'
  exit 0
fi
if [ "$1" = run ]; then
  count=0
  test ! -f "$0.count" || count="$(cat "$0.count")"
  count=$((count + 1))
  printf '%s\n' "$count" > "$0.count"
fi
exit 0
"#,
        );
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(Duration::from_millis(30));
        let spawn = || {
            runtime.spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
        };

        let first = spawn().unwrap_err();
        let second = spawn().unwrap_err();

        assert_eq!(
            first.downcast_ref::<PtySpawnTimeout>().unwrap().phase,
            PtySpawnTimeoutPhase::Publication
        );
        assert_eq!(
            second.downcast_ref::<PtySpawnTimeout>().unwrap().phase,
            PtySpawnTimeoutPhase::Publication
        );
        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "1",
            "the unresolved first launch must fence later callers"
        );
    }

    #[test]
    fn spawn_lock_timeout_is_typed_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(root.path(), "fake-pty-lock-timeout", "#!/bin/sh\nexit 0\n");
        let timeout = Duration::from_millis(30);
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(timeout);
        let _held = runtime.acquire_spawn_lock("work").unwrap();
        let started = Instant::now();

        let error = runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap_err();

        let timeout_error = error.downcast_ref::<PtySpawnTimeout>().unwrap();
        assert_eq!(timeout_error.phase, PtySpawnTimeoutPhase::Lock);
        assert_eq!(timeout_error.timeout, timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
