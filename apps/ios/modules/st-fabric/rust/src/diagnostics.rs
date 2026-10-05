//! Bounded, on-demand Debug measurements. Never retain a connection or an address.
use iroh::endpoint::{Connection, WeakConnectionHandle};
use serde_json::{Value, json};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Attempt {
    id: u64,
    elapsed_ms: Option<u64>,
    path: &'static str,
    result: &'static str,
    connection: Option<WeakConnectionHandle>,
}
static ATTEMPTS: Mutex<Vec<Attempt>> = Mutex::new(Vec::new());
static ORIGIN: OnceLock<Instant> = OnceLock::new();

pub fn started() -> u64 {
    ORIGIN.get_or_init(Instant::now);
    let mut attempts = ATTEMPTS.lock().expect("measurements");
    let id = attempts.last().map_or(1, |a| a.id + 1);
    if attempts.len() == 80 {
        let index = attempts
            .iter()
            .position(|a| a.connection.is_none() && a.result != "connecting")
            .unwrap_or(0);
        attempts.remove(index);
    }
    attempts.push(Attempt {
        id,
        elapsed_ms: None,
        path: "unknown",
        result: "connecting",
        connection: None,
    });
    id
}

fn path(connection: &Connection) -> &'static str {
    connection
        .paths()
        .iter()
        .find(|p| p.is_selected())
        .map_or("unknown", |p| {
            if p.is_ip() {
                "direct"
            } else if p.is_relay() {
                "relay"
            } else {
                "other"
            }
        })
}

pub fn connected(id: u64, elapsed_ms: u64, connection: &Connection) {
    if let Some(a) = ATTEMPTS
        .lock()
        .expect("measurements")
        .iter_mut()
        .find(|a| a.id == id)
    {
        a.elapsed_ms = Some(elapsed_ms);
        a.path = path(connection);
        a.result = "connected";
        a.connection = Some(connection.weak_handle());
    }
}

pub fn ended(id: u64, result: &'static str) {
    if let Some(a) = ATTEMPTS
        .lock()
        .expect("measurements")
        .iter_mut()
        .find(|a| a.id == id)
    {
        if let Some(c) = a
            .connection
            .as_ref()
            .and_then(WeakConnectionHandle::upgrade)
        {
            a.path = path(&c);
        }
        a.connection = None;
        a.result = result;
    }
}

pub fn snapshot() -> Value {
    let mut attempts = ATTEMPTS.lock().expect("measurements");
    let entries: Vec<_> = attempts.iter_mut().map(|a| {
        let mut rtt_ms = None;
        if let Some(c) = a.connection.as_ref().and_then(WeakConnectionHandle::upgrade) {
            a.path = path(&c);
            let paths = c.paths();
            rtt_ms = paths.iter().find(|p| p.is_selected()).and_then(|p| c.rtt(p.id())).map(|r| r.as_secs_f64() * 1000.0);
        }
        json!({"id": a.id, "handshakeMs": a.elapsed_ms, "selectedPath": a.path, "result": a.result, "quicRttMs": rtt_ms})
    }).collect();
    json!({"elapsedMs": ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64, "attempts": entries})
}
