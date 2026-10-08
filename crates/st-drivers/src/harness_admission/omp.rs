use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::{Check, Measurement, PROBE_TIMEOUT, ProbeDiagnostic, Scratch, fixture, measurement};

pub(super) fn probe(
    binary: &Path,
    extension: &Path,
    scratch: &Scratch,
) -> Result<(Vec<Measurement>, Option<ProbeDiagnostic>)> {
    probe_until(binary, extension, scratch, PROBE_TIMEOUT)
}

fn probe_until(
    binary: &Path,
    extension: &Path,
    scratch: &Scratch,
    timeout: Duration,
) -> Result<(Vec<Measurement>, Option<ProbeDiagnostic>)> {
    let model = fixture::Model::start(true)?;
    // The model is configured before extension loading: selecting a bundled provider first and
    // replacing it later could send a fixture prompt to an external API. Both configuration file
    // formats are isolated and contain only a loopback endpoint and a dummy key.
    let config = json!({"providers":{"admission":{
        "baseUrl":model.url,"api":"openai-completions","apiKey":"fixture",
        "models":[{"id":"fixture","name":"fixture","reasoning":false,"input":["text"],
            "contextWindow":100000,"maxTokens":1000}]
    }}});
    // A managed launcher may clear PI_CODING_AGENT_DIR. Use OMP's default isolated
    // profile as well, so it still discovers the loopback-only provider after that reset.
    for directory in ["agent", "home/.omp/agent"] {
        fs::create_dir_all(scratch.path(directory))?;
        for name in ["models.yml", "models.json"] {
            fs::write(
                scratch.path(&format!("{directory}/{name}")),
                serde_json::to_vec(&config)?,
            )?;
        }
    }
    fs::write(scratch.path("probe.ts"), include_str!("omp-probe.ts"))?;
    // A channel protocol peer, not a fake ExtensionAPI: the shipped adapter opens the child,
    // binds its real session, receives the nonce and calls the installed sendUserMessage itself.
    // POSIX shell builtins keep this fixture independent of a separate Node/Python installation.
    let sh = super::resolve_executable("sh")?;
    let message = json!({"type":"message","content":model.nonce,"meta":{"id":"admission_fixture"}})
        .to_string();
    // This isolated protocol peer acknowledges fixture boundaries, not production
    // storage or native continuation. Driver durability has separate owned controls.
    let script = format!(
        r#"#!{shell}
printf '%s\n' '{{"type":"hello","protocol":1,"capabilities":["durable-turn-receipts-v1"]}}'
receipt_sequence=0
while IFS= read -r frame; do
 printf '%s\n' "$frame" >> "$ST_ADMISSION_CHANNEL_TRACE"
 case "$frame" in
 *'"type":"ready"'*) printf '%s\n' '{message}';;
 *'"requestId":"receipt-'*)
  request_id=${{frame##*'"requestId":"'}}
  request_id=${{request_id%%'"'*}}
  receipt_sequence=$((receipt_sequence+1))
  printf '{{"type":"turn_recorded","requestId":"%s","receipt":{{"provider_incarnation":"fixture-omp-channel","ownership_sequence":1,"source_sequence":%s}}}}\n' "$request_id" "$receipt_sequence";;
 esac
done
"#,
        shell = sh.display(),
    );
    fs::write(scratch.path("channel"), script)?;
    fs::set_permissions(scratch.path("channel"), fs::Permissions::from_mode(0o700))?;
    let mut command = scratch.command(binary);
    command
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--no-tools",
            "--no-lsp",
            "--no-skills",
            "--no-rules",
            "--no-extensions",
            "--approval-mode",
            "always-ask",
            "-e",
        ])
        .arg(extension)
        .arg("--session-dir")
        .arg(scratch.path("sessions"))
        .arg("-e")
        .arg(scratch.path("probe.ts"))
        .args(["--provider", "admission", "--model", "fixture"])
        .env("ST_ADMISSION_TRACE", scratch.path("events.jsonl"))
        .env("ST_ADMISSION_CHANNEL_TRACE", scratch.path("channel.jsonl"))
        .env("ST_OMP_CHANNEL_BIN", scratch.path("channel"))
        .env("ST_OMP_CHANNEL_CATALOG", scratch.path("workspace"))
        .env("ST_OMP_CHANNEL_IDENTITY", "fixture.omp")
        .env("ST_AGENT", "admission.fixture");
    let started = Instant::now();
    let mut process = scratch.spawn(&mut command, "omp")?;
    let deadline = started + timeout;
    let mut answered = std::collections::BTreeSet::new();
    loop {
        // A capture can end with a partial line while the provider writes. Parse only completed
        // lines here; final malformed evidence never turns into a pass.
        for value in complete_lines(&scratch.capture("omp.stdout")?)? {
            if value["type"] == "extension_ui_request"
                && value["method"] == "select"
                && value["title"] == "Allow tool: admission_fixture"
                && let Some(id) = value["id"].as_str()
                && answered.insert(id.to_owned())
                && let Some(stdin) = process.0.stdin.as_mut()
            {
                writeln!(
                    stdin,
                    "{}",
                    json!({"type":"extension_ui_response","id":id,"value":"Deny"})
                )?;
                stdin.flush()?;
            }
        }
        let events = complete_lines(&scratch.capture("events.jsonl")?)?;
        let channel = complete_lines(&scratch.capture("channel.jsonl")?)?;
        let result = evaluate(&events, &channel, &model.nonce, model.consumed());
        if result.iter().all(|m| m.passed) {
            return Ok((result, None));
        }
        let exit = process.0.try_wait()?;
        if exit.is_some() || Instant::now() >= deadline {
            let refused = exit.is_some()
                && !events.iter().any(|event| event["type"] == "extension_load");
            let phase = if refused {
                "launch"
            } else {
                result.iter().find(|m| !m.passed).unwrap().check.as_str()
            };
            let diagnostic = ProbeDiagnostic {
                phase: phase.into(),
                outcome: if refused {
                    "launch-refused"
                } else if exit.is_some() {
                    "exit"
                } else {
                    "timeout"
                }.into(),
                exit_code: exit.and_then(|status| status.code()),
                elapsed_ms: started.elapsed().as_millis() as u64,
                stderr_tail: scratch.stderr_tail("omp.stderr")?,
                detail: format!("required {phase} evidence was not observed"),
            };
            return Ok((result, Some(diagnostic)));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn complete_lines(text: &str) -> Result<Vec<Value>> {
    text.split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

pub(super) fn evaluate(
    events: &[Value],
    channel: &[Value],
    nonce: &str,
    consumed: bool,
) -> Vec<Measurement> {
    let has = |name: &str| events.iter().position(|event| event["type"] == name);
    let extension = events.iter().any(|e| {
        e["type"] == "extension_load"
            && e["sendUserMessage"] == "function"
            && e["sendMessage"] == "function"
            && e["on"] == "function"
    }) && channel
        .iter()
        .any(|e| e["type"] == "ready" && e["sessionId"].as_str().is_some_and(|id| !id.is_empty()));
    let lifecycle = [
        "session_start",
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ]
    .iter()
    .all(|name| has(name).is_some())
        && has("session_start") < has("agent_start")
        && has("agent_start") < has("agent_end");
    let end = events
        .iter()
        .rposition(|e| e["type"] == "agent_end" && e["event"]["willContinue"] != true);
    let idle = end.is_some_and(|end| {
        events
            .iter()
            .skip(end + 1)
            .any(|e| e["type"] == "idle_sample" && e["idle"] == true)
    }) && channel
        .iter()
        .rposition(|e| e["type"] == "state" && e["state"] == "idle")
        .zip(
            channel
                .iter()
                .rposition(|e| e["type"] == "state" && e["state"] == "active"),
        )
        .is_some_and(|(idle, active)| idle > active);
    let approval = events.iter().enumerate().any(|(index, e)| {
        if e["type"] != "tool_approval_requested" {
            return false;
        }
        let ask = &e["event"];
        ask["toolName"] == "admission_fixture"
            && ask["approvalMode"] == "always-ask"
            && ask["toolCallId"] == fixture::CALL_ID
            && ask["sessionId"].as_str().is_some_and(|s| !s.is_empty())
            && events.iter().skip(index + 1).any(|e| {
                e["type"] == "tool_approval_resolved"
                    && e["event"]["sessionId"] == ask["sessionId"]
                    && e["event"]["toolName"] == ask["toolName"]
                    && e["event"]["toolCallId"] == ask["toolCallId"]
                    && e["event"]["approved"] == false
            })
            && channel.iter().any(|e| {
                e["type"] == "state" && e["blockedOn"] == "human" && e["ask"] == "permission"
            })
    });
    let native = consumed
        && end.is_some()
        && channel
            .iter()
            .filter(|e| e["type"] == "delivered" && e["meta"]["id"] == "admission_fixture")
            .count()
            == 1
        && events.iter().any(|e| {
            e["type"] == "message_end"
                && e["event"]["message"]["role"] == "user"
                && content_has(&e["event"]["message"]["content"], nonce)
        })
        && events.iter().any(|e| {
            e["type"] == "message_end"
                && e["event"]["message"]["role"] == "assistant"
                && e["event"]["message"]["stopReason"] == "stop"
                && content_has(
                    &e["event"]["message"]["content"],
                    &format!("CONSUMED:{nonce}"),
                )
        });
    vec![
        measurement(Check::ExtensionLoad, extension),
        measurement(Check::Lifecycle, lifecycle),
        measurement(Check::IdleEdge, idle),
        measurement(Check::ApprovalCorrelation, approval),
        measurement(Check::NativeConsumption, native),
    ]
}

fn content_has(content: &Value, text: &str) -> bool {
    content.as_array().is_some_and(|parts| {
        parts
            .iter()
            .any(|p| p["type"] == "text" && p["text"] == text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_admission::tests::measured_omp;

    #[test]
    fn timed_out_startup_retains_stderr_and_distinguishes_exit() {
        let scratch = Scratch::new().unwrap();
        let sh = super::super::resolve_executable("sh").unwrap();
        let binary = scratch.path("slow-omp");
        fs::write(&binary, format!(
            "#!{}\necho 'cold startup is still running' >&2\nsleep 60\n", sh.display(),
        )).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let (measurements, diagnostic) = probe_until(
            &binary, &scratch.path("extension.ts"), &scratch, Duration::from_millis(200),
        ).unwrap();
        assert!(measurements.iter().all(|m| !m.passed));
        let diagnostic = diagnostic.unwrap();
        assert_eq!(diagnostic.outcome, "timeout");
        assert_eq!(diagnostic.exit_code, None);
        assert_eq!(diagnostic.phase, "extensionLoad");
        assert_eq!(diagnostic.stderr_tail, "cold startup is still running\n");
    }

    #[test]
    fn measured_installed_capture_requires_consumption_beyond_channel_ack() {
        let fixture = measured_omp();
        let events = fixture["events"].as_array().unwrap();
        let channel = fixture["channel"].as_array().unwrap();
        let nonce = fixture["nonce"].as_str().unwrap();
        assert!(
            evaluate(events, channel, nonce, true)
                .iter()
                .all(|m| m.passed)
        );
        let no_consumption = evaluate(events, channel, nonce, false);
        assert!(!no_consumption.last().unwrap().passed);
        assert!(no_consumption[..4].iter().all(|m| m.passed));
    }
}
