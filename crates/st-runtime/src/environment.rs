//! The environment that a new interactive login shell gives to user work.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SHELL_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHELL_OUTPUT_LIMIT: usize = 1024 * 1024;
const ENVIRONMENT_BEGIN: &[u8] = b"ST3_ENV_BEGIN";
const ENVIRONMENT_END: &[u8] = b"ST3_ENV_END";

pub fn login_environment() -> Result<BTreeMap<String, String>> {
    let shell = account_shell().context("the user account has no default shell")?;
    login_environment_from(&shell, SHELL_STARTUP_TIMEOUT)
}

fn login_environment_from(shell: &Path, timeout: Duration) -> Result<BTreeMap<String, String>> {
    login_environment_with_args(
        shell,
        timeout,
        None,
        &[
            "-l",
            "-i",
            "-c",
            "/usr/bin/printf '\\0ST3_ENV_BEGIN\\0'; /usr/bin/env -0; /usr/bin/printf 'ST3_ENV_END\\0'",
        ],
    )
}

fn login_environment_with_args(
    shell: &Path,
    timeout: Duration,
    cwd: Option<&Path>,
    arguments: &[&str],
) -> Result<BTreeMap<String, String>> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("create a terminal for the default shell")?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("read the default shell terminal")?;
    let reader_thread = std::thread::spawn(move || -> std::io::Result<(Vec<u8>, bool)> {
        let mut output = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let remaining = SHELL_OUTPUT_LIMIT.saturating_sub(output.len());
                    output.extend_from_slice(&buffer[..count.min(remaining)]);
                    truncated |= count > remaining;
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => return Err(error),
            }
        }
        Ok((output, truncated))
    });
    let mut command = CommandBuilder::new(shell);
    if let Some(cwd) = cwd {
        command.cwd(cwd);
    }
    command.args(arguments);
    command.env("TERM", "xterm-256color");
    command.env("TERM_PROGRAM", "st3");
    let mut child = pair
        .slave
        .spawn_command(command)
        .with_context(|| format!("run the default shell {}", shell.display()))?;
    drop(pair.slave);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for the default shell")? {
            break status;
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            if let Some(group) = pair.master.process_group_leader() {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            } else {
                let _ = child.kill();
            }
            #[cfg(not(unix))]
            let _ = child.kill();
            let _ = child.wait();
            drop(pair.master);
            let _ = reader_thread.join();
            anyhow::bail!(
                "the default shell {} did not finish startup within {} seconds",
                shell.display(),
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    anyhow::ensure!(
        status.success(),
        "the default shell {} exited with {}",
        shell.display(),
        status
    );
    drop(pair.master);
    let (bytes, truncated) = reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("the shell terminal reader panicked"))?
        .context("read the default shell output")?;
    anyhow::ensure!(
        !truncated,
        "the default shell wrote more than {} bytes during startup",
        SHELL_OUTPUT_LIMIT
    );
    parse_login_environment(&bytes)
}

pub fn resolve_executable(
    program: &str,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
    }
    let path = environment
        .get("PATH")
        .context("the default shell did not export PATH")?;
    for directory in std::env::split_paths(path) {
        let candidate = directory.join(program);
        if candidate.is_file() && is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("`{program}` is not executable on the default shell PATH")
}

pub fn materialize_environment(
    declared: &BTreeMap<String, String>,
    executable: &Path,
) -> Result<BTreeMap<String, String>> {
    let environment = login_environment()?;
    overlay_environment(environment, declared, executable)
}

fn overlay_environment(
    mut environment: BTreeMap<String, String>,
    declared: &BTreeMap<String, String>,
    executable: &Path,
) -> Result<BTreeMap<String, String>> {
    let shell_path = environment
        .get("PATH")
        .cloned()
        .context("the default shell did not export PATH")?;
    environment.extend(
        declared
            .iter()
            .map(|(name, value)| (name.clone(), value.replace("${PATH}", &shell_path))),
    );
    prepend_executable_dir(&mut environment, executable)?;
    for name in [
        "OLDPWD",
        "PWD",
        "SHLVL",
        "TERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_SESSION_ID",
        "_",
    ] {
        if !declared.contains_key(name) {
            environment.remove(name);
        }
    }
    Ok(environment)
}

pub fn expand_path_placeholder(value: &mut String, environment: &BTreeMap<String, String>) {
    if let Some(path) = environment.get("PATH") {
        *value = value.replace("${PATH}", path);
    }
}

fn parse_login_environment(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let records = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let start = records
        .iter()
        .position(|record| *record == ENVIRONMENT_BEGIN)
        .context("the default shell did not start the environment record")?;
    let mut environment = BTreeMap::new();
    let mut ended = false;
    for record in &records[start + 1..] {
        if *record == ENVIRONMENT_END {
            ended = true;
            break;
        }
        if record.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(record).context("the shell environment is not UTF-8")?;
        let Some((name, value)) = record.split_once('=') else {
            continue;
        };
        if valid_name(name) {
            environment.insert(name.into(), value.into());
        }
    }
    anyhow::ensure!(
        ended,
        "the default shell did not finish the environment record"
    );
    anyhow::ensure!(
        environment.contains_key("PATH"),
        "the default shell did not export PATH"
    );
    Ok(environment)
}

fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(unix)]
fn account_shell() -> Option<PathBuf> {
    let entry = unsafe { libc::getpwuid(libc::getuid()) };
    if entry.is_null() {
        return std::env::var_os("SHELL").map(PathBuf::from);
    }
    let shell = unsafe { CStr::from_ptr((*entry).pw_shell) };
    let shell = shell.to_string_lossy();
    (!shell.is_empty()).then(|| PathBuf::from(shell.as_ref()))
}

#[cfg(not(unix))]
fn account_shell() -> Option<PathBuf> {
    std::env::var_os("SHELL").map(PathBuf::from)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn prepend_executable_dir(
    environment: &mut BTreeMap<String, String>,
    executable: &Path,
) -> Result<()> {
    let Some(directory) = executable.parent() else {
        return Ok(());
    };
    let current = environment.get("PATH").cloned().unwrap_or_default();
    let paths = std::iter::once(directory.to_path_buf())
        .chain(std::env::split_paths(&current).filter(|path| path != directory));
    environment.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_shell() -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("sh"))
            .find(|candidate| is_executable(candidate))
            .expect("the test environment must provide executable `sh` on PATH")
    }

    #[test]
    fn parser_ignores_shell_startup_output_before_the_record() {
        let parsed = parse_login_environment(
            b"welcome from the shell\n\0ST3_ENV_BEGIN\0PATH=/custom/bin:/usr/bin\0LANG=en_US.UTF-8\0BAD-NAME=x\0ST3_ENV_END\0logout\r\n",
        )
        .unwrap();
        assert_eq!(parsed["PATH"], "/custom/bin:/usr/bin");
        assert_eq!(parsed["LANG"], "en_US.UTF-8");
        assert!(!parsed.contains_key("BAD-NAME"));
    }

    #[test]
    fn declared_values_overlay_the_shell_and_expand_its_path() {
        let environment = BTreeMap::from([
            ("PATH".into(), "/shell/bin:/usr/bin".into()),
            ("VALUE".into(), "shell".into()),
            ("TERM".into(), "stale".into()),
        ]);
        let declared = BTreeMap::from([
            ("CUSTOM".into(), "/shim:${PATH}".into()),
            ("VALUE".into(), "declared".into()),
        ]);
        let environment =
            overlay_environment(environment, &declared, Path::new("/st3/bin/st3")).unwrap();
        assert_eq!(environment["CUSTOM"], "/shim:/shell/bin:/usr/bin");
        assert_eq!(environment["VALUE"], "declared");
        assert_eq!(environment["PATH"], "/st3/bin:/shell/bin:/usr/bin");
        assert!(!environment.contains_key("TERM"));
    }

    #[cfg(unix)]
    #[test]
    fn login_probe_accepts_startup_output_and_reads_the_exported_environment() {
        let shell = test_shell();
        let environment = login_environment_with_args(
            &shell,
            Duration::from_secs(1),
            Some(&std::env::current_dir().unwrap()),
            &[
                "-c",
                "printf 'startup output\\n'; printf '\\0ST3_ENV_BEGIN\\0PATH=/fresh/bin:/usr/bin\\0FRESH=yes\\0ST3_ENV_END\\0'",
            ],
        )
        .unwrap();

        assert_eq!(environment["PATH"], "/fresh/bin:/usr/bin");
        assert_eq!(environment["FRESH"], "yes");
    }

    #[cfg(unix)]
    #[test]
    fn login_probe_stops_a_shell_whose_startup_does_not_finish() {
        let shell = test_shell();
        let error = login_environment_with_args(
            &shell,
            Duration::from_millis(25),
            Some(&std::env::current_dir().unwrap()),
            &["-c", "sleep 5"],
        )
        .unwrap_err();

        assert!(error.to_string().contains("did not finish startup"));
    }
}
