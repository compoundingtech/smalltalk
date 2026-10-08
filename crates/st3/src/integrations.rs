//! User-owned harness integrations. Native transports stay in the ordinary seat drivers.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::model::DoctorCheck;

type Environment = BTreeMap<String, String>;

fn skill_path(harness: &str, environment: &Environment) -> Result<PathBuf> {
    let home = environment.get("HOME").context("integration needs HOME")?;
    let claude = environment
        .get("CLAUDE_CONFIG_DIR")
        .filter(|s| !s.is_empty());
    Ok(
        crate::skill::skills_dir(harness, Path::new(home), claude.map(Path::new))?
            .join("st/SKILL.md"),
    )
}

/// The complete consent plan, including harnesses not chosen for onboarding.
pub fn plan(found: &[String], claude_channel: bool) -> String {
    found
        .iter()
        .map(|harness| match harness.as_str() {
            "claude" if claude_channel => {
                "Claude: st skill and user channel plugin st-channel@st (no administrator policy)"
                    .into()
            }
            "claude" => "Claude: st skill (inline channel in managed seats)".into(),
            "pi" => "Pi: st skill and managed channel extension".into(),
            "omp" => "Omp: st skill and managed channel extension".into(),
            "codex" => "Codex: st skill (built-in native app-server integration)".into(),
            "opencode" => "OpenCode: st skill (built-in native HTTP integration)".into(),
            _ => format!("{harness}: unsupported"),
        })
        .collect::<Vec<String>>()
        .join("; ")
}

/// Install only the named harness's assets. Existing skill publication is atomic and repairs
/// drift; the immutable hook set is the same one managed seats load, avoiding a second global
/// extension that would duplicate callbacks in those seats.
pub fn install(
    harness: &str,
    claude_channel: bool,
    executable: &Path,
    state_dir: &Path,
    environment: &Environment,
) -> Result<()> {
    let skill = skill_path(harness, environment)?;
    crate::skill::install_in(skill.parent().unwrap().parent().unwrap())?;
    if matches!(harness, "pi" | "omp") {
        crate::hooks::ensure_installed(&crate::hooks::root(state_dir))?;
    }
    if harness == "claude" && claude_channel {
        let status = command(executable, environment, &["claude-channel", "status"]);
        if capture(status, None, Duration::from_secs(10)).is_err() {
            capture(
                command(
                    executable,
                    environment,
                    &["claude-channel", "install", "--no-policy"],
                ),
                None,
                Duration::from_secs(60),
            )
            .context(
                "install the user Claude channel; retry with st claude-channel install --no-policy",
            )?;
        }
    }
    Ok(())
}

fn command(executable: &Path, environment: &Environment, args: &[&str]) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().envs(environment).args(args);
    // A login shell may inherit the enclosing seat. Idle/plugin checks must be ordinary user
    // processes and must never attach a managed mailbox or borrow a native session fence.
    for name in ["ST_AGENT", "ST3_SUBJECT", "ST3_IDENTITY"] {
        command.env_remove(name);
    }
    command
}

/// Bounded local CLI calls: file-backed output cannot fill a pipe, and a stalled provider CLI
/// is reaped with its process group. No provider model is called and diagnostics omit stdout
/// and stderr, which an account wrapper could otherwise populate with credential material.
fn capture(mut command: Command, input: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>> {
    let mut output = tempfile::tempfile()?;
    let errors = tempfile::tempfile()?;
    command
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?);
    let mut child = command.spawn().context("start integration check")?;
    if let Some(input) = input {
        if let Err(error) = child.stdin.take().unwrap().write_all(input) {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(error.into());
        }
    }
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(status.success(), "integration command exited {status}");
            if input.is_some() {
                anyhow::ensure!(
                    errors.metadata()?.len() == 0,
                    "idle MCP wrote error diagnostics"
                );
            }
            use std::io::Seek as _;
            output.rewind()?;
            let mut bytes = Vec::new();
            output.take(16_385).read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= 16_384,
                "integration command output is too large"
            );
            return Ok(bytes);
        }
        if started.elapsed() >= timeout {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            anyhow::bail!(
                "integration command timed out after {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Exercise the merged idle MCP path without a daemon connection, subject, or provider turn.
fn check_idle_mcp(executable: &Path, environment: &Environment) -> Result<()> {
    let requests = ["initialize", "tools/list", "ping"]
        .into_iter()
        .enumerate()
        .map(|(id, method)| {
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0", "id":id,
            "method":method, "params":{"protocolVersion":"2025-03-26"}})
            )
        })
        .collect::<String>();
    let bytes = capture(
        command(executable, environment, &["driver", "claude-mcp"]),
        Some(requests.as_bytes()),
        Duration::from_secs(2),
    )?;
    let frames = std::str::from_utf8(&bytes)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        frames.len() == 3,
        "idle MCP did not answer all three requests"
    );
    for (id, frame) in frames.iter().enumerate() {
        anyhow::ensure!(
            frame["id"] == id && frame["jsonrpc"] == "2.0" && frame.get("error").is_none(),
            "idle MCP returned an invalid response"
        );
    }
    anyhow::ensure!(
        frames[0]["result"]["capabilities"] == json!({})
            && frames[1]["result"] == json!({"tools":[]})
            && frames[2]["result"] == json!({}),
        "idle MCP advertises managed capabilities outside a seat"
    );
    Ok(())
}

/// Verify the assets each installed harness will use. Missing optional installations warn;
/// corrupted skill/extension bytes fail. These checks do not claim provider authentication or
/// model consumption; live channel delivery remains observable in the ordinary seat checks.
pub fn doctor_checks(
    executable: &Path,
    state_dir: &Path,
    environment: &Environment,
) -> Vec<DoctorCheck> {
    crate::environment::HARNESSES.iter()
        .filter(|h| st_runtime::resolve_executable(h, environment).is_ok())
        .map(|harness| {
            let result = (|| -> Result<Option<String>> {
                let skill = skill_path(harness, environment)?;
                if !skill.try_exists()? { return Ok(None); }
                anyhow::ensure!(fs::read(&skill)? == crate::skill::SKILL.as_bytes(),
                    "st skill differs from this binary at {}", skill.display());
                if matches!(*harness, "pi" | "omp") {
                    crate::hooks::verify(&crate::hooks::set_dir(&crate::hooks::root(state_dir)))?;
                }
                if *harness == "claude" {
                    check_idle_mcp(executable, environment)?;
                    if capture(command(executable, environment, &["claude-channel", "status"]), None,
                        Duration::from_secs(5)).is_err() {
                        return Ok(Some("st skill and idle MCP work; user plugin is absent or needs repair; managed seats can use the inline development channel. Run st setup to install integrations".into()));
                    }
                }
                Ok(Some(String::new()))
            })();
            let (status, message) = match result {
                Ok(Some(message)) if message.is_empty() => ("pass", format!(
                    "{} verified; provider login and live seat delivery are checked separately", plan(&[harness.to_string()], true))),
                Ok(Some(message)) => ("warn", message),
                Ok(None) => ("warn", "st integration is not installed; run st setup".into()),
                Err(error) => ("fail", format!("st integration needs repair: {error:#}; run st setup")),
            };
            DoctorCheck { name: format!("integration/{harness}"), status: status.into(), message }
        }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_cli_is_bounded_and_error_output_is_not_repeated() {
        let mut stalled = Command::new("/bin/sh");
        stalled.args(["-c", "exec /bin/sleep 60"]);
        let started = Instant::now();
        let error = capture(stalled, None, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut rejected = Command::new("/bin/sh");
        rejected.args(["-c", "printf private-fixture-marker >&2; exit 1"]);
        let error = capture(rejected, None, Duration::from_secs(2)).unwrap_err();
        assert!(!error.to_string().contains("private-fixture-marker"));
    }

    #[test]
    fn consent_lists_every_detected_integration_and_respects_declined_claude_plugin() {
        let found = ["claude", "codex", "omp", "pi", "opencode"].map(str::to_owned);
        let all = plan(&found, true);
        for name in ["Claude", "Codex", "Omp", "Pi", "OpenCode"] {
            assert!(all.contains(name));
        }
        assert!(all.contains("user channel plugin"));
        assert!(!plan(&found, false).contains("user channel plugin"));
        assert!(plan(&found, false).contains("inline channel"));
    }

    #[test]
    fn install_and_doctor_share_paths_and_detect_missing_or_corrupt_assets() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["codex", "omp", "pi", "opencode"] {
            use std::os::unix::fs::PermissionsExt as _;
            fs::write(bin.join(name), "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let environment = BTreeMap::from([
            ("HOME".into(), root.path().display().to_string()),
            ("PATH".into(), bin.display().to_string()),
        ]);
        let executable = Path::new("/unused");
        let state = root.path().join("state");
        assert!(
            doctor_checks(executable, &state, &environment)
                .iter()
                .all(|c| c.status == "warn")
        );
        for name in ["codex", "omp", "pi", "opencode"] {
            install(name, false, executable, &state, &environment).unwrap();
        }
        assert!(
            doctor_checks(executable, &state, &environment)
                .iter()
                .all(|c| c.status == "pass")
        );
        fs::write(
            crate::hooks::set_dir(&crate::hooks::root(&state)).join("pi-channel.ts"),
            "broken",
        )
        .unwrap();
        let checks = doctor_checks(executable, &state, &environment);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "integration/pi")
                .unwrap()
                .status,
            "fail"
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "integration/codex")
                .unwrap()
                .status,
            "pass"
        );
        install("pi", false, executable, &state, &environment).unwrap();
        fs::write(skill_path("codex", &environment).unwrap(), "stale").unwrap();
        assert!(
            doctor_checks(executable, &state, &environment)
                .iter()
                .all(|c| c.status == "fail")
        );
        install("codex", false, executable, &state, &environment).unwrap();
        assert!(
            doctor_checks(executable, &state, &environment)
                .iter()
                .all(|c| c.status == "pass")
        );
    }
}
