//! Temporary interrupted-ask reopening for the session upstream native continuation selected.
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
use anyhow::{Context, Result};
use std::{fs, path::Path};

/// Never choose a session: only inspect the exact native session selected by the driver.
pub fn pending_for_selected(sessions: &Path, selected: Option<&str>) -> Option<String> {
    let transcript = crate::native_resume::pi_family_transcript(sessions, selected?)?;
    read_pending(&transcript)
}

fn read_pending(transcript: &Path) -> Option<String> {
    let bytes = match fs::read(transcript) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(path = %transcript.display(), %error, "OMP interrupted-ask transcript read failed; continuing without bridge");
            return None;
        }
    };
    pending_ask(&String::from_utf8_lossy(&bytes))
}

pub fn pending_ask(transcript: &str) -> Option<String> {
    let mut pending: Option<String> = None;
    let mut exited_with_pending_ask = false;
    for line in transcript.lines() {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        if row["type"] == "custom"
            && row["customType"] == "tool_execution_start"
            && row["data"]["toolName"] == "ask"
        {
            pending = row["data"]["toolCallId"].as_str().map(str::to_owned);
            exited_with_pending_ask = false;
            continue;
        }
        if row["type"] == "custom" && row["customType"] == "session_exit" {
            exited_with_pending_ask |= pending.as_deref().is_some_and(|id| {
                row["data"]["pendingToolCalls"]
                    .as_array()
                    .is_some_and(|calls| calls.iter().any(|call| call["toolCallId"] == id))
            });
            continue;
        }
        if row["type"] != "message" {
            continue;
        }
        let message = &row["message"];
        match message["role"].as_str() {
            Some("user") => {
                pending = None;
                exited_with_pending_ask = false;
            }
            Some("assistant") if message["stopReason"] != "aborted" => {
                pending = None;
                exited_with_pending_ask = false;
            }
            Some("toolResult")
                if pending
                    .as_deref()
                    .is_some_and(|id| message["toolCallId"] == id) =>
            {
                let interrupted = message["isError"] == true
                    && (exited_with_pending_ask
                        || message["content"].as_array().is_some_and(|content| {
                            content.iter().any(|part| {
                                part["type"] == "text"
                                    && part["text"].as_str().is_some_and(|text| {
                                        text.starts_with(
                                            "Previous OMP process exited before this tool returned",
                                        )
                                    })
                            })
                        }));
                if !interrupted {
                    pending = None;
                }
            }
            _ => {}
        }
    }
    pending
}

fn healing_command(
    binary: &str,
    sessions: &Path,
    id: &str,
    environment_keys: impl Iterator<Item = std::ffi::OsString>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    // Explicit -e extensions from a launcher still load despite --no-extensions.
    // Keep them disconnected from the managed seat's PTY and channel.
    // ST_AGENT is only an identity; the fleet launcher requires it even for RPC.
    for key in environment_keys {
        let name = key.to_string_lossy();
        if name.starts_with("ST3_")
            || name.starts_with("ST2_")
            || (name.starts_with("ST_") && name != "ST_AGENT")
            || matches!(name.as_ref(), "PTY_SESSION" | "PTY_ROOT" | "PTY_SESSION_DIR")
        {
            command.env_remove(key);
        }
    }
    command
        .arg("--session-dir")
        .arg(sessions)
        .args(["--no-extensions", "--resume", id, "--mode", "rpc"]);
    command
}

pub async fn heal(binary: &str, sessions: &Path, id: &str) -> Result<String> {
    let stdout = tempfile::NamedTempFile::new()?;
    let stderr = tempfile::NamedTempFile::new()?;
    let mut child = healing_command(binary, sessions, id, std::env::vars_os().map(|(key, _)| key))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout.as_file().try_clone()?))
        .stderr(std::process::Stdio::from(stderr.as_file().try_clone()?))
        .kill_on_drop(true)
        .spawn()?;
    let status = match tokio::time::timeout(std::time::Duration::from_secs(60), child.wait()).await
    {
        Ok(status) => Some(status?),
        Err(_) => {
            child
                .kill()
                .await
                .context("kill timed-out OMP healing process")?;
            child
                .wait()
                .await
                .context("reap timed-out OMP healing process")?;
            None
        }
    };
    let captured = format!(
        "{}{}",
        String::from_utf8_lossy(&fs::read(stdout.path())?),
        String::from_utf8_lossy(&fs::read(stderr.path())?)
    );
    match status {
        Some(status) => anyhow::ensure!(
            status.success(),
            "OMP session healing failed ({status}): {captured}"
        ),
        None => anyhow::bail!("OMP session healing exceeded 60 seconds: {captured}"),
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_canary_interruptions_survive_heal_and_retry() {
        for fixture in [
            include_str!("../fixtures/omp-resume/run3-03-after-kill.jsonl"),
            include_str!("../fixtures/omp-resume/run3-05-after-heal.jsonl"),
            include_str!("../fixtures/omp-resume/run4-03-after-kill.jsonl"),
            include_str!("../fixtures/omp-resume/run4-05-after-heal.jsonl"),
        ] {
            assert!(pending_ask(fixture).is_some());
        }
        for fixture in [
            include_str!("../fixtures/omp-resume/run3-08-after-retry.jsonl"),
            include_str!("../fixtures/omp-resume/run4-08-after-retry.jsonl"),
        ] {
            assert!(pending_ask(fixture).is_some());
            let id = pending_ask(fixture).unwrap();
            let answer = serde_json::json!({"type":"message","message":{"role":"toolResult","toolCallId":id,"isError":false,"content":[{"type":"text","text":"Red"}]}});
            assert!(pending_ask(&format!("{fixture}{answer}\n")).is_none());
        }
    }

    #[test]
    fn pending_last_ask_requires_no_real_subsequent_answer() {
        let start = "{\"type\":\"custom\",\"customType\":\"tool_execution_start\",\"data\":{\"toolName\":\"ask\",\"toolCallId\":\"X\"}}\n";
        for message in [
            r#"{"role":"user"}"#,
            r#"{"role":"assistant","stopReason":"stop"}"#,
            r#"{"role":"toolResult","toolCallId":"X","isError":false,"content":[{"type":"text","text":"Red"}]}"#,
            r#"{"role":"toolResult","toolCallId":"X","isError":false,"content":[{"type":"text","text":"Previous OMP process exited before this tool returned"}]}"#,
            r#"{"role":"toolResult","toolCallId":"X","isError":true,"content":[{"type":"text","text":"Other error"}]}"#,
        ] {
            assert!(
                pending_ask(&format!(
                    "{start}{{\"type\":\"message\",\"message\":{message}}}\n"
                ))
                .is_none()
            );
        }
        assert_eq!(pending_ask(start).as_deref(), Some("X"));
        assert_eq!(pending_ask(&format!("{start}{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"stopReason\":\"aborted\"}}}}\n")).as_deref(), Some("X"));
        assert_eq!(
            pending_ask(&format!("{start}{}", start.replace("\"X\"", "\"Y\""))).as_deref(),
            Some("Y")
        );
        assert!(pending_ask("malformed").is_none());
    }

    #[test]
    fn shutdown_cancellation_is_interrupted_but_person_cancellation_is_answered() {
        let fixture = include_str!("../fixtures/omp-resume/managed-stop-cancelled.jsonl");
        assert_eq!(
            pending_ask(fixture).as_deref(),
            Some("toolu_0179rsQAbpQmnqiZKiU8WRaA")
        );
        let retried_then_cancelled = format!(
            "{fixture}{}\n{}\n",
            fixture.lines().nth(1).unwrap(),
            fixture.lines().last().unwrap()
        );
        assert!(pending_ask(&retried_then_cancelled).is_none());
        let without_exit = fixture
            .lines()
            .filter(|line| !line.contains("\"customType\":\"session_exit\""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pending_ask(&without_exit).is_none());
        for message in [
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Cancel it"}]}}"#,
            r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[]}}"#,
        ] {
            let mut rows: Vec<_> = fixture.lines().collect();
            rows.insert(3, message);
            assert!(pending_ask(&rows.join("\n")).is_none());
        }
        let wrong_exit = fixture.replace(
            r#""pendingToolCalls":[{"toolName":"ask","toolCallId":"toolu_0179rsQAbpQmnqiZKiU8WRaA""#,
            r#""pendingToolCalls":[{"toolName":"ask","toolCallId":"another-call""#,
        );
        assert!(pending_ask(&wrong_exit).is_none());
        let answered = fixture.replace(r#""isError":true"#, r#""isError":false"#);
        assert!(pending_ask(&answered).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn heal_runs_rpc_without_input_and_reports_child_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("omp-canary");
        fs::write(&binary, "#!/bin/sh\nif read -r input; then exit 17; fi\nprintf '%s\\n' \"$@\"\nprintf 'stdin-eof\\n' >&2\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let captured = heal(binary.to_str().unwrap(), root.path(), "native-id")
            .await
            .unwrap();
        assert_eq!(
            captured,
            format!(
                "--session-dir\n{}\n--no-extensions\n--resume\nnative-id\n--mode\nrpc\nstdin-eof\n",
                root.path().display()
            )
        );
        fs::write(
            &binary,
            "#!/bin/sh\nprintf 'heal-rejected\\n' >&2\nexit 23\n",
        )
        .unwrap();
        let error = heal(binary.to_str().unwrap(), root.path(), "native-id")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("23"));
        assert!(error.contains("heal-rejected"));
    }

    #[test]
    fn healing_is_disconnected_from_seat_handles_but_keeps_launcher_identity() {
        let handles = [
            "PTY_SESSION", "PTY_ROOT", "PTY_SESSION_DIR", "ST3_ENDPOINT", "ST3_NATIVE_CONTINUE_SESSION",
            "ST2_CHANNEL", "ST_HOOKS", "ST_OMP_CHANNEL_BIN", "ST_DRIVER_AGENT_DIR",
        ];
        let keys = handles.into_iter().chain(["ST_AGENT", "HOME", "PATH"]).map(Into::into);
        let command = healing_command("omp", Path::new("/sessions"), "native-id", keys);
        let removed: std::collections::BTreeSet<_> = command.as_std().get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key.to_str().unwrap()))
            .collect();
        assert_eq!(removed, handles.into_iter().collect());
        assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), [
            "--session-dir", "/sessions", "--no-extensions", "--resume", "native-id", "--mode", "rpc",
        ]);
    }

    #[test]
    fn transcript_read_fails_open_and_never_selects_a_session() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("timestamp_selected.jsonl");
        let start = "{\"type\":\"custom\",\"customType\":\"tool_execution_start\",\"data\":{\"toolName\":\"ask\",\"toolCallId\":\"X\"}}\n";
        fs::write(&path, format!("{{\"type\":\"session\",\"id\":\"selected\"}}\n{start}")).unwrap();
        assert_eq!(pending_for_selected(root.path(), Some("selected")).as_deref(), Some("X"));
        assert!(pending_for_selected(root.path(), None).is_none());
        assert!(pending_for_selected(root.path(), Some("other")).is_none());
        assert!(read_pending(&root.path().join("missing")).is_none());
        assert!(read_pending(root.path()).is_none());
        fs::write(&path, [start.as_bytes(), &[0xff]].concat()).unwrap();
        assert!(read_pending(&path).is_none());
        fs::write(&path, format!("{{\"type\":\"session\",\"id\":\"selected\",\"label\":\"�\"}}\n{start}").as_bytes()).unwrap();
        let bytes = fs::read(&path).unwrap();
        let invalid = bytes.windows(3).position(|part| part == [0xef,0xbf,0xbd]).unwrap();
        let mut bytes = bytes;
        bytes.splice(invalid..invalid+3,[0xff]);
        fs::write(&path, bytes).unwrap();
        assert_eq!(read_pending(&path).as_deref(), Some("X"));
    }

    #[test]
    fn native_reopen_contraction_probe_when_omp_is_supplied() {
        let Some(binary) = std::env::var_os("OMP_BIN") else {
            eprintln!("SKIP native reopen contraction probe: set OMP_BIN to the raw pinned OMP executable; smalltalk does not provide OMP");
            return;
        };
        // The hermetic contraction gate belongs to dotfiles/flakes/external/omp: invoke this
        // same standalone probe on every OMP pin bump, including patches. No version assertion.
        let probe = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/omp-resume/native-reopen-probe.py");
        let output = std::process::Command::new("python3")
            .arg(probe)
            .env("OMP_BIN", binary)
            .output()
            .expect("run model-free native reopen probe (requires python3)");
        let evidence = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        match output.status.code() {
            Some(0) => eprintln!("{evidence}"),
            Some(1) => panic!("contract arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge: native reopen no longer needs F5\n{evidence}"),
            _ => panic!("native reopen contraction probe failed to measure behavior\n{evidence}"),
        }
    }
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
