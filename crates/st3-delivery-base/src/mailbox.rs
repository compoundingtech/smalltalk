//! Daemon subscriptions for native harnesses. Durable messages remain in the graph; reconnecting
//! replays the current mailbox under the same session fence and stable message subjects.
use crate::model::{DesiredSubject, MessageView};
use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::client::Fence;

/// Captured by the daemon from its authenticated Unix peer, never from a request body.
/// Provider ownership comes from the current driver record under the runtime's paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    pub provider: String,
    pub session: String,
    pub sequence: u64,
    pub pid: u32,
    pub process_token: u64,
}

/// Display names belong to the seat record; the launcher supplies the persona short code
/// (`AGENT_PERSONA_SHORT`). Strip terminal control characters at the output edge.
pub fn seat_label(seat: &DesiredSubject, persona_short: Option<&str>) -> String {
    let name = seat
        .desired
        .get("display_name")
        .and_then(Value::as_str)
        .or_else(|| {
            seat.member
                .as_ref()
                .and_then(|member| member.display_name.as_deref())
        })
        .unwrap_or(seat.subject.strip_prefix("agent/").unwrap_or(&seat.subject));
    let label = match persona_short.map(str::trim).filter(|short| !short.is_empty()) {
        Some(short) => format!("{name}[{short}]"),
        None => name.to_owned(),
    };
    label
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Seat {
        seat: Box<DesiredSubject>,
    },
    Mailbox {
        messages: Vec<MessageView>,
    },
    Fenced {
        reason: String,
    },
    /// Ordered after the mailbox snapshot; only its consumer can acknowledge this nonce.
    Replay { nonce: String },
    /// Ordered after the gated mailbox snapshot. The delivery loop acknowledges consumption.
    Drain {
        operation: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    #[serde(flatten)]
    pub fence: Fence,
    pub message: String,
    pub lifecycle: String,
}

/// Positive attachment requires a live, initialized channel under the current delivery fence.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attachment {
    pub attached: bool,
}

/// A single stream task owns reconnection. Dropping it ends the subscription.
pub struct Subscription {
    pub receiver: tokio::sync::mpsc::Receiver<Frame>,
    report: tokio::sync::watch::Sender<Value>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Subscription {
    pub fn start(client: crate::client::Client, fence: Fence, mut report: Value) -> Self {
        report["mailbox_replay_ack"] = serde_json::json!(true);
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (report_tx, mut report_rx) = tokio::sync::watch::channel(report);
        let task = tokio::spawn(crate::task_cpu::account("mailbox.subscription", async move {
            let mut retry = crate::client::MailboxRetry::default();
            loop {
                retry.begin_attempt();
                let result: Result<()> = async {
                    let mut socket = client.open_mailbox(&fence).await?;
                    // open_mailbox returns only after HTTP 101, before the first snapshot.
                    // A completed upgrade does not clear the retained open-failure count.
                    retry.upgraded();
                    let report = serde_json::to_string(&*report_rx.borrow_and_update())?;
                    socket.send(tokio_tungstenite::tungstenite::Message::Text(report.into())).await?;
                    loop {
                        tokio::select! {
                            message = socket.next() => {
                                match message.context("mailbox disconnected")?? {
                                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                                        let frame: Frame = serde_json::from_str(&text)?;
                                        retry.received_frame();
                                        let fenced = matches!(frame, Frame::Fenced { .. });
                                        sender.send(frame).await.context("mailbox receiver ended")?;
                                        if fenced { return Ok(()); }
                                    },
                                    tokio_tungstenite::tungstenite::Message::Ping(bytes) => {
                                        socket.send(tokio_tungstenite::tungstenite::Message::Pong(bytes)).await?;
                                        let report = serde_json::to_string(&*report_rx.borrow())?;
                                        socket.send(tokio_tungstenite::tungstenite::Message::Text(report.into())).await?;
                                    },
                                    tokio_tungstenite::tungstenite::Message::Close(_) => anyhow::bail!("mailbox closed"),
                                    _ => {},
                                }
                            },
                            changed = report_rx.changed() => {
                                changed.context("report sender ended")?;
                                let report = serde_json::to_string(&*report_rx.borrow_and_update())?;
                                socket.send(tokio_tungstenite::tungstenite::Message::Text(report.into())).await?;
                            }
                        }
                    }
                }.await;
                if let Err(error) = &result
                    && matches!(crate::client::api_error_code(error), Some(
                        "stale-mailbox-session" | "foreign-mailbox" | "unbound-mailbox" | "invalid-mailbox-token"
                    ))
                {
                    let _ = sender.send(Frame::Fenced { reason: error.to_string() }).await;
                    return;
                }
                if result.is_ok() || sender.is_closed() {
                    return;
                }
                // Transport loss changes no delivery state. The next connection replays the graph.
                if let Err(error) = result {
                    tokio::time::sleep(retry.failed(&error)).await;
                }
            }
        }));
        Self {
            receiver,
            report: report_tx,
            task,
        }
    }
    pub fn acknowledge_replay(&self, nonce: String) {
        self.report.send_modify(|report| report["mailbox_replay_nonce"] = serde_json::json!(nonce));
    }
    pub fn acknowledge_drain(&self, operation: Option<String>) {
        self.report.send_modify(|report| {
            report["drain_operation"] = serde_json::json!(operation);
        });
    }
    pub fn report(&self, mut value: Value) {
        value["mailbox_replay_ack"] = serde_json::json!(true);
        self.report.send_modify(|current| {
            value["mailbox_replay_nonce"] = current["mailbox_replay_nonce"].clone();
            value["drain_operation"] = current["drain_operation"].clone();
            *current = value;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    

    use serde_json::json;

    

    #[tokio::test]
    async fn typed_contention_bind_retries_the_same_fence_and_refusal_does_not_retry() {
        use axum::{Json, Router, routing::post};
        use axum::http::StatusCode;
        use std::sync::{Arc, Mutex};
        for refused in [false, true] {
            let requests = Arc::new(Mutex::new(Vec::<Fence>::new()));
            let captured = requests.clone();
            let app = Router::new().route("/v1/mailbox/bind", post(move |Json(mut fence): Json<Fence>| {
                let captured = captured.clone();
                async move {
                    let count = {
                        let mut requests = captured.lock().unwrap();
                        requests.push(fence.clone());
                        requests.len()
                    };
                    if refused || count <= 2 {
                        let code = if refused { "stale-incarnation" } else if count == 1 {
                            "database-busy"
                        } else { "database-locked" };
                        return (if refused { StatusCode::CONFLICT } else { StatusCode::SERVICE_UNAVAILABLE },
                            Json(json!({"code":code,"message":"fixture refusal","details":{}})));
                    }
                    fence.epoch = 42;
                    (StatusCode::OK, Json(json!({"api_version":"st3.v1","value":fence})))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
            let client = crate::client::Client::new(crate::client::Endpoint::Http(format!("http://{address}")));
            let mut fence = Fence::new("agent/fixture", "incarnation/fixture", "component/fixture");
            let token = fence.token.clone();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), fence.bind(&client)).await.unwrap();
            server.abort();
            let requests = requests.lock().unwrap();
            if refused {
                assert_eq!(crate::client::api_error_code(&result.unwrap_err()), Some("stale-incarnation"));
                assert_eq!(requests.len(), 1);
                assert_eq!(fence.epoch, 0);
            } else {
                result.unwrap();
                assert_eq!(requests.len(), 3);
                assert_eq!(fence.epoch, 42);
            }
            assert_eq!(fence.token, token);
            for request in requests.iter() {
                assert_eq!(request.epoch, 0);
                assert_eq!(request.token, token);
                assert_eq!(request.subject, fence.subject);
                assert_eq!(request.incarnation, fence.incarnation);
                assert_eq!(request.component, fence.component);
            }
        }
    }

    #[test]
    fn seat_label_appends_launcher_persona_short_without_space_and_strips_controls() {
        let seat = DesiredSubject {
            accountable: None,
            subject: "agent/eval.worker".into(),
            kind: "agent".into(),
            desired: json!({"display_name":"Qu\u{1b}]0;x\u{7}artz"}),
            member: None,
            owner_run: None,
            owner_generation: None,
            owner_step: None,
        };
        assert_eq!(seat_label(&seat, Some("gen")), "Qu]0;xartz[gen]");
        assert_eq!(seat_label(&seat, Some("  ")), "Qu]0;xartz");
        assert_eq!(seat_label(&seat, None), "Qu]0;xartz");
    }
}
