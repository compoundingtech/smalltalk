//! Measure the installed loopback server's contract with disposable state and a local model.

use std::fs;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Client, EventMachine, allocate_port, check_openapi_subset, random_password};
use crate::harness_admission::{Check, Measurement, PROBE_TIMEOUT, Scratch, fixture, measurement};

pub(super) fn probe(binary: &Path, scratch: &Scratch) -> Result<Vec<Measurement>> {
    let model = fixture::Model::start(false)?;
    let config = json!({"model":"admission/fixture", "enabled_providers":["admission"],
        "provider":{"admission":{"npm":"@ai-sdk/openai-compatible","name":"Admission fixture",
            "options":{"baseURL":model.url,"apiKey":"fixture"},
            "models":{"fixture":{"name":"fixture","limit":{"context":100000,"output":1000}}}}},
        "permission":{"*":"deny","bash":"ask"},"autoupdate":false,"share":"disabled","snapshot":false});
    fs::write(scratch.path("opencode.json"), serde_json::to_vec(&config)?)?;
    let port = allocate_port()?;
    let password = random_password()?;
    let mut client = Client::new(port, &password);
    client.response_limit = Some(8 * 1024 * 1024);
    let mut command = scratch.command(binary);
    command
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .env("OPENCODE_CONFIG", scratch.path("opencode.json"))
        .env("OPENCODE_CONFIG_DIR", scratch.path("config"))
        .env("OPENCODE_CONFIG_CONTENT", config.to_string())
        .env("OPENCODE_DISABLE_PROJECT_CONFIG", "true")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true");
    let mut process = scratch.spawn(&mut command, "opencode")?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut api = false;
    loop {
        if let Ok(doc) = client.get_json("/doc") {
            let subset = check_openapi_subset(&doc);
            let statuses = client.get_json("/session/status");
            let permissions = client.get_json("/permission");
            let questions = client.get_json("/question");
            fs::write(
                scratch.path("api.txt"),
                format!(
                    "subset={subset:?}\nstatuses={statuses:?}\npermissions={permissions:?}\nquestions={questions:?}\n"
                ),
            )?;
            let subset_ok = subset.is_ok();
            api = subset_ok
                && statuses.is_ok_and(|v| v.is_object())
                && permissions.is_ok_and(|v| v.is_array())
                && questions.is_ok_and(|v| v.is_array());
            if api || !subset_ok {
                break;
            }
        }
        if process.0.try_wait()?.is_some() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let mut events = Vec::new();
    let mut messages = Value::Null;
    let mut session = String::new();
    if api {
        // Establish the stream before prompting. An acknowledgement, persisted user message,
        // and busy event alone are not proof of completed consumption.
        let mut stream_client = client.clone();
        stream_client.sse_silence = Duration::from_millis(200);
        let mut reader = std::io::Read::take(stream_client.open_sse()?, 8 * 1024 * 1024);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(128);
        let invalid = Arc::new(AtomicBool::new(false));
        let invalid_stream = invalid.clone();
        let stopped = stop.clone();
        let task = thread::spawn(move || {
            use std::io::BufRead;
            let mut line = String::new();
            let mut data = String::new();
            while !stopped.load(Ordering::SeqCst) {
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if let Some(value) = trimmed.strip_prefix("data:") {
                            data.push_str(value.trim_start());
                        } else if trimmed.is_empty() && !data.is_empty() {
                            let queued = serde_json::from_str::<Value>(&data)
                                .ok()
                                .is_some_and(|event| tx.try_send(event).is_ok());
                            if !queued {
                                invalid_stream.store(true, Ordering::SeqCst);
                                break;
                            }
                            data.clear();
                        }
                        line.clear();
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });
        struct StreamGuard(Arc<AtomicBool>, Option<thread::JoinHandle<()>>);
        impl Drop for StreamGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
                if let Some(task) = self.1.take() {
                    let _ = task.join();
                }
            }
        }
        let _stream = StreamGuard(stop, Some(task));
        let (_, body) = client.request("POST", "/session", Some(&json!({})))?;
        let created: Value = serde_json::from_slice(&body)?;
        session = created["id"]
            .as_str()
            .context("admission server returned no session id")?
            .to_owned();
        let status = client.post_json(
            &format!("/session/{session}/prompt_async"),
            &json!({
                "model":{"providerID":"admission","modelID":"fixture"},
                "parts":[{"type":"text","text":model.nonce}]
            }),
        )?;
        let accepted = (200..300).contains(&status);
        let mut answered = std::collections::BTreeSet::new();
        while accepted && Instant::now() < deadline && process.0.try_wait()?.is_none() {
            events.extend(rx.try_iter());
            anyhow::ensure!(
                !invalid.load(Ordering::SeqCst),
                "malformed or excessive admission event stream"
            );
            if let Ok(asks) = client.get_json("/permission") {
                for ask in asks.as_array().into_iter().flatten() {
                    // Only this exact harmless command in this isolated session is approved.
                    // An unexpected policy/tool/input changes the measured contract and fails.
                    if ask["sessionID"] == session
                        && ask["permission"] == "bash"
                        && ask["metadata"]["command"] == fixture::COMMAND
                        && ask["tool"]["callID"] == fixture::CALL_ID
                        && let Some(id) = ask["id"].as_str()
                        && answered.insert(id.to_owned())
                    {
                        client.post_json(
                            &format!("/permission/{id}/reply"),
                            &json!({"reply":"once"}),
                        )?;
                    }
                }
            }
            if let Ok(value) = client.get_json(&format!("/session/{session}/message")) {
                messages = value;
            }
            events.extend(rx.try_iter());
            let result = evaluate(
                api,
                &events,
                &messages,
                &session,
                &model.nonce,
                model.consumed(),
            );
            if result.iter().all(|m| m.passed) {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        events.extend(rx.try_iter());
        anyhow::ensure!(
            !invalid.load(Ordering::SeqCst),
            "malformed or excessive admission event stream"
        );
    }
    fs::write(
        scratch.path("events.jsonl"),
        events
            .iter()
            .map(|event| format!("{event}\n"))
            .collect::<String>(),
    )?;
    fs::write(
        scratch.path("messages.json"),
        serde_json::to_vec(&messages)?,
    )?;
    Ok(evaluate(
        api,
        &events,
        &messages,
        &session,
        &model.nonce,
        model.consumed(),
    ))
}

fn evaluate(
    api: bool,
    events: &[Value],
    messages: &Value,
    session: &str,
    nonce: &str,
    consumed: bool,
) -> Vec<Measurement> {
    let relevant = events
        .iter()
        .filter(|e| e["properties"]["sessionID"] == session)
        .collect::<Vec<_>>();
    let busy = relevant.iter().rposition(|e| {
        e["type"] == "session.status" && e["properties"]["status"]["type"] == "busy"
    });
    let idle = relevant.iter().rposition(|e| {
        e["type"] == "session.status" && e["properties"]["status"]["type"] == "idle"
    });
    let lifecycle = busy.is_some()
        && relevant.iter().any(|e| e["type"] == "session.idle")
        && !relevant.iter().any(|e| e["type"] == "session.error");
    let mut machine = EventMachine::default();
    for event in &relevant {
        machine.apply(event);
    }
    let idle_edge = busy.zip(idle).is_some_and(|(busy, idle)| idle > busy)
        && machine.observation().is_some_and(|o| {
            o.state == crate::harness_state::Activity::Idle
                && o.blocked_on == crate::harness_state::BlockedOn::None
        });
    let approval = relevant.iter().enumerate().any(|(index, e)| {
        let ask = &e["properties"];
        e["type"] == "permission.asked"
            && ask["permission"] == "bash"
            && ask["metadata"]["command"] == fixture::COMMAND
            && ask["tool"]["callID"] == fixture::CALL_ID
            && ask["id"].as_str().is_some_and(|id| !id.is_empty())
            && relevant.iter().skip(index + 1).any(|e| {
                e["type"] == "permission.replied"
                    && e["properties"]["requestID"] == ask["id"]
                    && e["properties"]["reply"] == "once"
            })
    });
    let content = |message: &Value, expected: &str| {
        message["parts"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|p| p["type"] == "text" && p["text"] == expected)
        })
    };
    let native = consumed
        && messages.as_array().is_some_and(|messages| {
            messages.iter().any(|user| {
                user["info"]["role"] == "user"
                    && user["info"]["sessionID"] == session
                    && content(user, nonce)
                    && messages.iter().any(|reply| {
                        reply["info"]["role"] == "assistant"
                            && reply["info"]["sessionID"] == session
                            && reply["info"]["parentID"] == user["info"]["id"]
                            && reply["info"]["time"]["completed"].is_number()
                            && reply["info"]["finish"] == "stop"
                            && reply["info"]["error"].is_null()
                            && content(reply, &format!("CONSUMED:{nonce}"))
                    })
            })
        });
    vec![
        measurement(Check::ApiContract, api),
        measurement(Check::Lifecycle, lifecycle),
        measurement(Check::IdleEdge, idle_edge),
        measurement(Check::ApprovalCorrelation, approval),
        measurement(Check::NativeConsumption, native),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/harness-admission/opencode-1.18.34.json"
        ))
        .unwrap()
    }
    fn measurements(fixture: &Value, consumed: bool) -> Vec<Measurement> {
        evaluate(
            true,
            fixture["events"].as_array().unwrap(),
            &fixture["messages"],
            fixture["session"].as_str().unwrap(),
            fixture["nonce"].as_str().unwrap(),
            consumed,
        )
    }

    #[test]
    fn measured_installed_capture_passes_all_checks() {
        assert!(measurements(&measured(), true).iter().all(|m| m.passed));
    }

    #[test]
    fn transport_acceptance_without_completed_consumption_cannot_pass() {
        let mut fixture = measured();
        assert!(!measurements(&fixture, false).last().unwrap().passed);
        for message in fixture["messages"].as_array_mut().unwrap() {
            if message["info"]["role"] == "assistant" {
                message["info"]["time"]["completed"] = Value::Null;
            }
        }
        assert!(!measurements(&fixture, true).last().unwrap().passed);
    }

    #[test]
    fn foreign_session_or_parent_cannot_prove_consumption() {
        let mut fixture = measured();
        for message in fixture["messages"].as_array_mut().unwrap() {
            if message["info"]["role"] == "assistant" {
                message["info"]["parentID"] = json!("different-user");
            }
        }
        assert!(!measurements(&fixture, true).last().unwrap().passed);
    }

    #[test]
    fn idle_must_follow_the_last_busy_observation() {
        let mut fixture = measured();
        let session = fixture["session"].clone();
        fixture["events"].as_array_mut().unwrap().push(json!({"type":"session.status","properties":{"sessionID":session,"status":{"type":"busy"}}}));
        assert!(!measurements(&fixture, true)[2].passed);
    }
}
