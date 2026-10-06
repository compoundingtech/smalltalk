//! Private, pipe-only bridge to the same admission implementation used by replay.
//! Rejected input and parser errors never reach diagnostics or telemetry.

use std::io::{Read, Write};
use std::process::ExitCode;

use serde_json::{Value, json};
use st_drivers::capture_admission::{POLICY_VERSION, sanitize_channel_payload};

const MAX_INPUT_BYTES: usize = 65_536;

fn failure(reason: &str) -> Value {
    json!({"policy_version": POLICY_VERSION, "withheld": true, "reason": reason})
}

fn sanitize(input: &[u8]) -> Value {
    if input.len() > MAX_INPUT_BYTES {
        return failure("scan-bound");
    }
    let Ok(value) = serde_json::from_slice::<Value>(input) else {
        return failure("scanner-failure");
    };
    let Some(event) = value.get("event").and_then(Value::as_str) else {
        return failure("unregistered-field");
    };
    if !matches!(event, "message_end" | "tool_call" | "tool_result")
        || value
            .as_object()
            .is_none_or(|object| object.len() != 2 || !object.contains_key("payload"))
    {
        return failure("unregistered-field");
    }
    sanitize_channel_payload(event, &value["payload"])
}

pub fn run() -> ExitCode {
    let mut input = Vec::new();
    let output = match std::io::stdin()
        .lock()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
    {
        Ok(_) => sanitize(&input),
        Err(_) => failure("scanner-failure"),
    };
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &output).is_err() || stdout.write_all(b"\n").is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_credentials_cli_parser_and_bounds_never_echo_input() {
        let fake = "invented-unregistered-cli-credential";
        let inputs = [
            format!("{{\"event\":\"{fake}\",\"payload\":{{}}}}"),
            format!(
                "{{\"event\":\"message_end\",\"payload\":{{\"message\":{{\"content\":\"{fake}\"}}}}}}"
            ),
            format!("malformed:{fake}"),
            format!("{fake}{}", "x".repeat(MAX_INPUT_BYTES)),
        ];
        for input in inputs {
            let output = sanitize(input.as_bytes());
            assert!(!output.to_string().contains(fake));
            assert_eq!(output["policy_version"], POLICY_VERSION);
        }
    }
}
