//! Daemon subscriptions for native harnesses. Durable messages remain in the graph; reconnecting
//! replays the current mailbox under the same session fence and stable message subjects.
use crate::model::{DesiredSubject, MessageView};
use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Fence {
    pub subject: String,
    pub incarnation: String,
    pub component: String,
    pub epoch: u64,
}

impl Fence {
    pub fn new(subject: &str, incarnation: &str, component: &str) -> Self {
        Self {
            subject: subject.into(),
            incarnation: incarnation.into(),
            component: component.into(),
            epoch: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
        }
    }
}

/// Display names belong to the seat record. Strip terminal control characters at the output edge.
pub fn seat_label(seat: &DesiredSubject) -> String {
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
    let persona = seat
        .member
        .as_ref()
        .and_then(|member| member.tags.get("persona"))
        .map(String::as_str);
    let label = match persona {
        Some(persona) => format!("{name} [{persona}]"),
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
    Seat { seat: Box<DesiredSubject> },
    Mailbox { messages: Vec<MessageView> },
    Fenced { reason: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    #[serde(flatten)]
    pub fence: Fence,
    pub message: String,
    pub lifecycle: String,
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
    pub fn start(client: crate::client::Client, fence: Fence, report: Value) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (report_tx, mut report_rx) = tokio::sync::watch::channel(report);
        let task = tokio::spawn(async move {
            loop {
                let result: Result<()> = async {
                    let mut socket = client.open_mailbox(&fence).await?;
                    let report = serde_json::to_string(&*report_rx.borrow_and_update())?;
                    socket.send(tokio_tungstenite::tungstenite::Message::Text(report.into())).await?;
                    loop {
                        tokio::select! {
                            message = socket.next() => {
                                match message.context("mailbox disconnected")?? {
                                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                                        let frame: Frame = serde_json::from_str(&text)?;
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
                if result.is_ok() || sender.is_closed() {
                    return;
                }
                // Transport loss changes no delivery state. The next connection replays the graph.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        Self {
            receiver,
            report: report_tx,
            task,
        }
    }
    pub fn report(&self, value: Value) {
        self.report.send_replace(value);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::ClaimInput;
    use crate::store::Store;
    use serde_json::json;
    use std::collections::BTreeMap;
    fn claim(subject: &str, kind: &str, fields: Value, key: &str) -> ClaimInput {
        ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some("agent/eval.worker".into()),
            fields: serde_json::from_value::<BTreeMap<String, Value>>(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        }
    }
    pub(crate) fn ready(store: &Store, incarnation: &str) {
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "runtime.observed",
                json!({"status":"running",
            "runtime_id":"eval.worker","incarnation_id":incarnation}),
                &format!("runtime:{incarnation}"),
            ))
            .unwrap();
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "harness.observed",
                json!({"state":"ready",
            "driver":"omp","incarnation_id":incarnation}),
                &format!("harness:{incarnation}"),
            ))
            .unwrap();
    }
    #[test]
    fn mailbox_replacement_fences_reconnect_and_receipts_after_daemon_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let old = Fence {
            subject: "agent/eval.worker".into(),
            incarnation: "session-1".into(),
            component: "delivery".into(),
            epoch: 1,
        };
        let new = Fence {
            epoch: 2,
            ..old.clone()
        };
        {
            let store = Store::open(&path, "node").unwrap();
            ready(&store, "session-1");
            store
                .append_claim(&claim(
                    "message/native",
                    "message.sent",
                    json!({"status":"sent",
                "from":"person/eval","to":old.subject,"content":"QUARTZ SIGNAL"}),
                    "send",
                ))
                .unwrap();
            store.bind_mailbox(&old).unwrap();
            store.bind_mailbox(&new).unwrap();
            assert_eq!(
                store.bind_mailbox(&old).unwrap_err().code,
                "stale-mailbox-session"
            );
        }
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(
            store.bind_mailbox(&old).unwrap_err().code,
            "stale-mailbox-session"
        );
        let delivered = claim(
            "message/native",
            "message.delivered",
            json!({"status":"delivered"}),
            "native-delivered",
        );
        assert_eq!(
            store
                .append_mailbox_receipt(&delivered, &old)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        assert_eq!(
            store.message("message/native").unwrap().unwrap().status,
            "sent"
        );
        let first = store.append_mailbox_receipt(&delivered, &new).unwrap();
        // Native handoff succeeded; the daemon committed but its HTTP acknowledgement was lost.
        assert_eq!(
            store.append_mailbox_receipt(&delivered, &new).unwrap().id,
            first.id
        );
        assert_eq!(
            store
                .claims_for("message/native", Some("message.delivered"))
                .unwrap()
                .len(),
            1
        );
        let read = claim(
            "message/native",
            "message.read",
            json!({"status":"read"}),
            "native-read",
        );
        store.append_mailbox_receipt(&read, &new).unwrap();
        ready(&store, "session-2");
        assert_eq!(
            store.append_mailbox_receipt(&read, &new).unwrap_err().code,
            "stale-mailbox-session"
        );
    }
    #[test]
    fn mailbox_receipts_cannot_mutate_another_recipients_message() {
        let store = Store::open_memory("node").unwrap();
        ready(&store, "session-1");
        let fence = Fence::new("agent/eval.worker", "session-1", "delivery");
        store.bind_mailbox(&fence).unwrap();
        store
            .append_claim(&claim(
                "message/foreign",
                "message.sent",
                json!({"status":"sent",
            "from":"person/eval","to":"agent/eval.other","content":"private"}),
                "foreign-send",
            ))
            .unwrap();
        let input = claim(
            "message/foreign",
            "message.delivered",
            json!({"status":"delivered"}),
            "foreign-delivered",
        );
        assert_eq!(
            store
                .append_mailbox_receipt(&input, &fence)
                .unwrap_err()
                .code,
            "wrong-message-recipient"
        );
        assert_eq!(
            store.message("message/foreign").unwrap().unwrap().status,
            "sent"
        );
    }
}
