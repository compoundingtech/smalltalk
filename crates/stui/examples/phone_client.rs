//! A phone-shaped test client for measuring what one attached phone costs the daemon.
//!
//! The phone's feed (apps/ios/feed.ts) holds one collections socket with the attention,
//! missions and agents windows (200 rows each); when quiet for 10 s it probes with a
//! capabilities read every 10 s (5 s wait). Screens load launches, machines, devices and
//! sessions only when opened, so an idle phone on Home reads nothing else: no usage, no clients,
//! no glasses or conversation unless a screen follows one. This client does exactly that and
//! names itself `smalltalk-ios 0.1.0 [LABEL]` in `x-st3-client`. Its transport is the local
//! Unix socket, not the paired gateway; that difference is the limit of this measurement.
//!
//! cargo run --release -p stui --example phone_client -- --seconds 180 --label measure-phone
use std::time::{Duration, Instant};

use st3_client::{Client, set_client_name};

/// One JSON line for an observation: routes, outcomes, statuses, sizes and durations, with every
/// string escaped by serde_json, never contents.
fn timing_line(at_ms: u64, observation: &st3_client::Observation<'_>) -> String {
    match observation {
        st3_client::Observation::Request { method, route, outcome, status, took } => serde_json::json!({
            "at_ms": at_ms, "type": "request", "method": method, "route": route,
            "outcome": format!("{outcome:?}"), "status": status,
            "took_ms": took.as_secs_f64() * 1000.0,
        }),
        st3_client::Observation::Frame { id, kind, bytes, since_subscribe } => serde_json::json!({
            "at_ms": at_ms, "type": "frame", "id": id, "kind": kind, "bytes": bytes,
            "since_subscribe_ms": since_subscribe.map(|d| d.as_secs_f64() * 1000.0),
        }),
    }
    .to_string()
}

#[tokio::main]
async fn main() {
    let mut seconds = 180_u64;
    let mut label = "measure-phone".to_owned();
    let mut timing: Option<String> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--seconds" => seconds = arguments.next().and_then(|v| v.parse().ok()).unwrap_or(seconds),
            "--label" => label = arguments.next().unwrap_or(label),
            // Log the timing of this client's requests and frames: routes, statuses, sizes and
            // durations as JSON lines, never contents.
            "--timing" => timing = arguments.next(),
            _ => {}
        }
    }
    set_client_name(format!("smalltalk-ios 0.1.0 [{label}]"));
    if let Some(path) = timing {
        use std::io::Write as _;
        let started = Instant::now();
        let file = std::sync::Mutex::new(
            std::fs::OpenOptions::new().create(true).append(true).open(path).expect("timing log"),
        );
        st3_client::set_observer(move |observation| {
            let line = timing_line(started.elapsed().as_millis() as u64, observation);
            if let Ok(mut file) = file.lock() {
                let _ = writeln!(file, "{line}");
            }
        });
    }
    let path = st3_client::discover_unix_endpoint(None).expect("a local st endpoint");
    // The person as stui resolves it: ST3_PERSON, else `person` in the st config.
    let person = std::env::var("ST3_PERSON").ok().or_else(|| {
        let home = std::env::var_os("HOME")?;
        let raw = std::fs::read_to_string(std::path::Path::new(&home).join(".config/st3/config.toml")).ok()?;
        raw.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "person").then(|| value.trim().trim_matches('"').to_owned())
        })
    });
    let person = person.expect("ST3_PERSON or person in ~/.config/st3/config.toml");
    let client = Client::unix_as(&path, &person);
    let mut stream = client.collection_stream().await.expect("collections stream");
    for (id, collection) in [("attention", "attention"), ("missions", "missions"), ("agents", "agents")] {
        stream.subscribe(id, collection, 200, None, None).await.expect("subscribe");
    }
    println!("phone client up: label {label:?}, idle for {seconds}s");
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut last_frame = Instant::now();
    let mut last_probe = Instant::now();
    let mut frames = 0_u64;
    let mut probes = 0_u64;
    while Instant::now() < end {
        match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
            Ok(Ok(Some(_))) => {
                frames += 1;
                last_frame = Instant::now();
            }
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => {}
        }
        // The phone probes only when the socket has been quiet, then every ten seconds.
        if last_frame.elapsed() >= Duration::from_secs(10) && last_probe.elapsed() >= Duration::from_secs(10) {
            let _ = tokio::time::timeout(Duration::from_secs(5), client.capabilities()).await;
            probes += 1;
            last_probe = Instant::now();
        }
    }
    println!("phone client done: {frames} frames, {probes} capability probes");
}

#[cfg(test)]
mod tests {
    #[test]
    fn identifiers_with_quotes_and_backslashes_stay_one_valid_json_line() {
        let line = super::timing_line(
            7,
            &st3_client::Observation::Frame {
                id: "a\"b\\c\nd",
                kind: "snap\"shot",
                bytes: 3,
                since_subscribe: None,
            },
        );
        assert!(!line.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], "a\"b\\c\nd");
        assert_eq!(parsed["kind"], "snap\"shot");
    }
}
