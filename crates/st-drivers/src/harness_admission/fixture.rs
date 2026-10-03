//! A deterministic loopback model, not a harness mock. The installed harness still loads its
//! adapter, runs the agent loop, gates the harmless tool, and consumes the native prompt.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

pub(crate) const CALL_ID: &str = "admission_call";
pub(crate) const COMMAND: &str = "printf ADMISSION_FIXTURE";

#[derive(Default)]
struct Evidence {
    prompt_seen: bool,
    continuation_seen: bool,
}

pub(crate) struct Model {
    pub(crate) url: String,
    pub(crate) nonce: String,
    evidence: Arc<Mutex<Evidence>>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl Model {
    pub(crate) fn start(omp: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://127.0.0.1:{}/v1", listener.local_addr()?.port());
        let nonce = format!("ADMISSION_NATIVE_{}", crate::harness_state::session_token());
        let stop = Arc::new(AtomicBool::new(false));
        let evidence = Arc::new(Mutex::new(Evidence::default()));
        let (stop_reader, evidence_writer, expected) =
            (stop.clone(), evidence.clone(), nonce.clone());
        let task = thread::spawn(move || {
            while !stop_reader.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                        let _ = serve(&mut stream, &expected, omp, &evidence_writer);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            url,
            nonce,
            evidence,
            stop,
            task: Some(task),
        })
    }

    pub(crate) fn consumed(&self) -> bool {
        self.evidence
            .lock()
            .is_ok_and(|e| e.prompt_seen && e.continuation_seen)
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn serve(stream: &mut TcpStream, nonce: &str, omp: bool, evidence: &Mutex<Evidence>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, length) = loop {
        anyhow::ensure!(Instant::now() < deadline, "model fixture request timed out");
        let n = stream.read(&mut buffer)?;
        anyhow::ensure!(
            n > 0 && raw.len() + n <= 1024 * 1024,
            "invalid model fixture request"
        );
        raw.extend_from_slice(&buffer[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&raw[..end]);
            anyhow::ensure!(
                headers.starts_with("POST /v1/chat/completions "),
                "unexpected model fixture endpoint"
            );
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, size)| size.trim().parse().ok())
                })
                .context("missing model fixture content length")?;
            anyhow::ensure!(
                end + 4 + length <= 1024 * 1024,
                "model fixture request too large"
            );
            break (end + 4, length);
        }
    };
    while raw.len() < header_end + length {
        anyhow::ensure!(Instant::now() < deadline, "model fixture request timed out");
        let n = stream.read(&mut buffer)?;
        anyhow::ensure!(n > 0, "truncated model fixture request");
        raw.extend_from_slice(&buffer[..n]);
    }
    let request: Value = serde_json::from_slice(&raw[header_end..header_end + length])?;
    let messages = request["messages"]
        .as_array()
        .context("missing model fixture messages")?;
    let prompt_seen = messages.iter().any(|m| {
        m["role"] == "user"
            && serde_json::to_string(&m["content"]).is_ok_and(|content| content.contains(nonce))
    });
    anyhow::ensure!(prompt_seen, "model never received the native nonce");
    let continuation = messages
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == CALL_ID);
    {
        let mut measured = evidence
            .lock()
            .map_err(|_| anyhow::anyhow!("model fixture evidence poisoned"))?;
        measured.prompt_seen = true;
        measured.continuation_seen |= continuation;
    }
    let delta = if continuation {
        json!({"content":format!("CONSUMED:{nonce}")})
    } else {
        json!({"tool_calls":[{"index":0,"id":CALL_ID,"type":"function","function":{
            "name":if omp {"admission_fixture"} else {"bash"},
            "arguments":if omp {"{}".to_owned()} else {json!({"command":COMMAND,"description":"Harmless admission fixture"}).to_string()}
        }}]})
    };
    let chunk = |delta: Value, finish: Value| {
        json!({"id":"admission","object":"chat.completion.chunk",
        "created":0,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        .to_string()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role":"assistant"}), Value::Null),
        chunk(delta, Value::Null),
        chunk(
            json!({}),
            json!(if continuation { "stop" } else { "tool_calls" })
        )
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}
