use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const SCHEMA: &str = "st.runtime.exec-generation.v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecGeneration {
    pub schema: String,
    pub pid: u32,
    pub created_at_unix_ms: u128,
    pub start_token: u64,
    pub generation_id: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_signal: Option<i32>,
    #[serde(default)]
    pub isolation_mode: String,
    #[serde(default)]
    pub scope_unit: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecObservation {
    Running(ExecGeneration),
    Exited(ExecGeneration),
    Indeterminate(String),
}

#[derive(Clone, Debug)]
pub struct ExecRuntime {
    state_dir: PathBuf,
    log_dir: PathBuf,
    owned_children: Arc<Mutex<BTreeMap<u32, Child>>>,
}

impl ExecRuntime {
    pub fn new(state_dir: PathBuf, log_dir: PathBuf) -> Self {
        crate::warn_if_degraded("st3");
        Self {
            state_dir,
            log_dir,
            owned_children: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn spawn(
        &self,
        id: &str,
        launch: &crate::Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<ExecGeneration> {
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(&self.log_dir)?;
        self.rotate_generation(id)?;
        let log_path = self.log_path(id, false);
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)
            .with_context(|| format!("open exec log {}", log_path.display()))?;

        let (program, arguments): (OsString, Vec<OsString>) = match launch {
            crate::Launch::Shell(source) => (
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from(source)],
            ),
            crate::Launch::Argv(argv) => {
                let (program, arguments) = argv
                    .split_first()
                    .context("an argv launch must contain a program")?;
                (
                    OsString::from(program),
                    arguments.iter().map(OsString::from).collect(),
                )
            }
        };
        let unit = crate::scope_unit("st3", id);
        let argument_refs = arguments
            .iter()
            .map(OsString::as_os_str)
            .collect::<Vec<_>>();
        let mut command = crate::wrap_isolated(&unit, program.as_os_str(), &argument_refs);
        command
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().context("spawn detached exec")?;
        let pid = child.id();
        let start_token = process_start_token(pid)?;
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let generation_id = generation_id(id, pid, start_token, created_at_unix_ms);
        let generation = ExecGeneration {
            schema: SCHEMA.into(),
            pid,
            created_at_unix_ms,
            start_token,
            generation_id,
            exit_code: None,
            exit_signal: None,
            isolation_mode: isolation_name(crate::isolation_mode()).into(),
            scope_unit: (crate::isolation_mode() == crate::Isolation::Scope).then_some(unit),
        };
        self.write_generation(id, &generation)?;
        self.owned_children
            .lock()
            .expect("exec child mutex poisoned")
            .insert(pid, child);
        Ok(generation)
    }

    pub fn observe(&self, id: &str) -> Result<Option<ExecObservation>> {
        let path = self.record_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut generation: ExecGeneration = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => return Ok(Some(ExecObservation::Indeterminate(error.to_string()))),
        };
        if generation.schema != SCHEMA {
            return Ok(Some(ExecObservation::Indeterminate(format!(
                "unsupported generation schema {}",
                generation.schema
            ))));
        }
        // A terminal generation is durable evidence about this exact exec. In particular, do not
        // let an unrelated process that later reuses the PID make a completed generation look
        // live again on platforms whose best available start token is the PID itself.
        if generation.exit_code.is_some() || generation.exit_signal.is_some() {
            return Ok(Some(ExecObservation::Exited(generation)));
        }
        let owned_exit = {
            let mut children = self
                .owned_children
                .lock()
                .expect("exec child mutex poisoned");
            let status = children
                .get_mut(&generation.pid)
                .map(Child::try_wait)
                .transpose()?
                .flatten();
            if status.is_some() {
                children.remove(&generation.pid);
            }
            status
        };
        if let Some(status) = owned_exit {
            generation.exit_code = status.code();
            generation.exit_signal = exit_signal(&status);
            self.write_generation(id, &generation)?;
            return Ok(Some(ExecObservation::Exited(generation)));
        }
        match process_identity(generation.pid) {
            Ok((state, token)) if token == generation.start_token && state != 'Z' => {
                Ok(Some(ExecObservation::Running(generation)))
            }
            Ok((_state, token)) if token == generation.start_token => {
                Ok(Some(ExecObservation::Exited(generation)))
            }
            Ok(_) => Ok(Some(ExecObservation::Exited(generation))),
            Err(error) if process_is_absent(&error) => {
                Ok(Some(ExecObservation::Exited(generation)))
            }
            Err(error) => Ok(Some(ExecObservation::Indeterminate(error.to_string()))),
        }
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        self.stop_if(id, None)
    }

    pub fn stop_if(&self, id: &str, expected_generation: Option<&str>) -> Result<()> {
        self.signal_if(id, expected_generation, libc::SIGTERM)
    }

    pub fn signal_if(
        &self,
        id: &str,
        expected_generation: Option<&str>,
        signal: i32,
    ) -> Result<()> {
        let Some(ExecObservation::Running(generation)) = self.observe(id)? else {
            return Ok(());
        };
        if expected_generation.is_some_and(|expected| expected != generation.generation_id) {
            anyhow::bail!("exec `{id}` changed incarnation before signal {signal}");
        }
        let result = unsafe { libc::kill(-(generation.pid as i32), signal) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error)
                    .with_context(|| format!("signal {signal} to exec process group"));
            }
        }
        Ok(())
    }

    pub fn kill(&self, id: &str) -> Result<()> {
        self.kill_if(id, None)
    }

    pub fn kill_if(&self, id: &str, expected_generation: Option<&str>) -> Result<()> {
        self.signal_if(id, expected_generation, libc::SIGKILL)
    }

    pub fn read_log(&self, id: &str) -> Result<Option<String>> {
        let path = self.log_path(id, false);
        match fs::read_to_string(&path) {
            Ok(log) => Ok(Some(log)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read exec log {}", path.display())),
        }
    }

    pub fn read_log_bytes(&self, id: &str, previous: bool) -> Result<Option<Vec<u8>>> {
        let path = self.log_path(id, previous);
        match fs::read(&path) {
            Ok(log) => Ok(Some(log)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read exec log {}", path.display())),
        }
    }

    pub fn previous_generation(&self, id: &str) -> Result<Option<ExecGeneration>> {
        self.read_generation_path(&self.previous_record_path(id))
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        for path in [
            self.record_path(id),
            self.previous_record_path(id),
            self.log_path(id, false),
            self.log_path(id, true),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.state_dir.join(format!("{id}.json"))
    }

    fn previous_record_path(&self, id: &str) -> PathBuf {
        self.state_dir.join(format!("{id}.json.1"))
    }

    fn log_path(&self, id: &str, previous: bool) -> PathBuf {
        self.log_dir.join(if previous {
            format!("{id}.log.1")
        } else {
            format!("{id}.log")
        })
    }

    fn rotate_generation(&self, id: &str) -> Result<()> {
        rotate_file(&self.record_path(id), &self.previous_record_path(id))?;
        rotate_file(&self.log_path(id, false), &self.log_path(id, true))
    }

    fn read_generation_path(&self, path: &Path) -> Result<Option<ExecGeneration>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(Into::into),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_generation(&self, id: &str, generation: &ExecGeneration) -> Result<()> {
        let path = self.record_path(id);
        let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, generation)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(())
    }
}

fn process_is_absent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
    })
}

fn rotate_file(current: &Path, previous: &Path) -> Result<()> {
    if !current.exists() {
        return Ok(());
    }
    match fs::remove_file(previous) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::rename(current, previous) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn isolation_name(mode: crate::Isolation) -> &'static str {
    match mode {
        crate::Isolation::Scope => "scope",
        crate::Isolation::Detached => "detached",
        crate::Isolation::DegradedDetached => "degraded-detached",
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(target_os = "linux")]
pub fn process_start_token(pid: u32) -> Result<u64> {
    process_identity(pid).map(|(_, token)| token)
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Result<(char, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat.rfind(')').context("malformed process stat")?;
    let tail = stat.get(end + 2..).context("malformed process stat tail")?;
    let mut fields = tail.split_whitespace();
    let state = fields
        .next()
        .and_then(|value| value.chars().next())
        .context("process stat lacks a state")?;
    let token = fields
        .nth(18)
        .context("process stat lacks a start token")?
        .parse()?;
    Ok((state, token))
}

#[cfg(not(target_os = "linux"))]
pub fn process_start_token(pid: u32) -> Result<u64> {
    // This platform has no portable process start token. The runtime uses the
    // PID as its best available token, so a fast exit must not make spawn fail.
    Ok(pid as u64)
}

#[cfg(not(target_os = "linux"))]
fn process_identity(pid: u32) -> Result<(char, u64)> {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        Ok(('R', pid as u64))
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn generation_id(id: &str, pid: u32, start_token: u64, created_at_unix_ms: u128) -> String {
    let mut hash = Sha256::new();
    hash.update(id.as_bytes());
    hash.update(pid.to_be_bytes());
    hash.update(start_token.to_be_bytes());
    hash.update(created_at_unix_ms.to_be_bytes());
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_start_token_survives_a_fast_process_exit() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();

        assert_eq!(process_start_token(pid).unwrap(), pid as u64);
    }

    #[test]
    fn an_absent_process_is_a_normal_exit_on_every_unix_platform() {
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ESRCH));

        assert!(process_is_absent(&error));
    }

    #[test]
    fn a_terminal_record_stays_exited_when_its_pid_is_live() {
        let root = tempfile::tempdir().unwrap();
        let runtime = ExecRuntime::new(root.path().join("exec"), root.path().join("logs"));
        let pid = std::process::id();
        let generation = ExecGeneration {
            schema: SCHEMA.into(),
            pid,
            created_at_unix_ms: 1,
            start_token: process_start_token(pid).unwrap(),
            generation_id: "completed-generation".into(),
            exit_code: Some(0),
            exit_signal: None,
            isolation_mode: "detached".into(),
            scope_unit: None,
        };
        fs::create_dir_all(&runtime.state_dir).unwrap();
        runtime.write_generation("completed", &generation).unwrap();

        assert_eq!(
            runtime.observe("completed").unwrap(),
            Some(ExecObservation::Exited(generation))
        );
    }

    fn wait_for_exit(runtime: &ExecRuntime, id: &str) -> ExecGeneration {
        for _ in 0..200 {
            match runtime.observe(id).unwrap() {
                Some(ExecObservation::Exited(generation)) => return generation,
                Some(ExecObservation::Running(_)) => thread::sleep(Duration::from_millis(10)),
                other => panic!("unexpected exec observation: {other:?}"),
            }
        }
        panic!("the exec did not exit");
    }

    #[test]
    fn retains_the_current_and_previous_log_generations() {
        let root = tempfile::tempdir().unwrap();
        let runtime = ExecRuntime::new(root.path().join("exec"), root.path().join("logs"));
        let environment = BTreeMap::new();
        let cwd = root.path();
        let first = runtime
            .spawn(
                "work",
                &crate::Launch::Argv(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf first; exit 3".into(),
                ]),
                cwd,
                &environment,
            )
            .unwrap();
        let first_exit = wait_for_exit(&runtime, "work");
        assert_eq!(first_exit.exit_code, Some(3));

        let second = runtime
            .spawn(
                "work",
                &crate::Launch::Argv(vec!["sh".into(), "-c".into(), "printf second".into()]),
                cwd,
                &environment,
            )
            .unwrap();
        let second_exit = wait_for_exit(&runtime, "work");

        assert_ne!(first.generation_id, second.generation_id);
        assert_eq!(second_exit.exit_code, Some(0));
        assert_eq!(
            runtime.previous_generation("work").unwrap().unwrap(),
            first_exit
        );
        assert_eq!(
            runtime.read_log_bytes("work", true).unwrap().unwrap(),
            b"first"
        );
        assert_eq!(
            runtime.read_log_bytes("work", false).unwrap().unwrap(),
            b"second"
        );
    }
}
