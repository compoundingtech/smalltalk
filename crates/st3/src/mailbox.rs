//! Daemon subscriptions for native harnesses. Durable messages remain in the graph; reconnecting
//! replays the current mailbox under the same session fence and stable message subjects.
use crate::model::{DesiredSubject, MessageView};
use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Fence {
    pub subject: String,
    pub incarnation: String,
    pub component: String,
    pub epoch: u64,
    pub token: String,
}

impl Fence {
    pub fn new(subject: &str, incarnation: &str, component: &str) -> Self {
        let mut token = [0_u8; 16];
        getrandom::fill(&mut token).expect("creating a mailbox binding token");
        Self {
            subject: subject.into(),
            incarnation: incarnation.into(),
            component: component.into(),
            epoch: 0,
            token: hex::encode(token),
        }
    }
    /// Initial allocation is idempotent under this random request token. Reexec and reconnect
    /// carry the returned epoch; neither the client's clock nor a lost response changes ownership.
    pub async fn bind(&mut self, client: &crate::client::Client) -> Result<()> {
        if self.epoch != 0 {
            return Ok(());
        }
        loop {
            match client.post("/v1/mailbox/bind", &*self).await {
                Ok(bound) => {
                    *self = bound;
                    return Ok(());
                }
                Err(error)
                    if crate::client::api_error_code(&error).is_some_and(|code| {
                        !matches!(code, "internal" | "mailbox-session-starting")
                    }) =>
                {
                    return Err(error);
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }
    }
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
    pub fn acknowledge_drain(&self, operation: Option<String>) {
        self.report.send_modify(|report| {
            report["drain_operation"] = serde_json::json!(operation);
        });
    }
    pub fn report(&self, mut value: Value) {
        value["drain_operation"] = self.report.borrow()["drain_operation"].clone();
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
    fn seat_label_appends_launcher_persona_short_without_space_and_strips_controls() {
        let seat = DesiredSubject {
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
    #[test]
    fn mailbox_startup_waits_for_running_evidence_without_allocating_or_admitting_stale_sessions() {
        let store = Store::open_memory("node").unwrap();
        let request = Fence::new("agent/eval.worker", "session-1", "delivery");
        store.append_claim(&claim("agent/eval.worker", "harness.observed",
            json!({"state":"starting","driver":"claude","incarnation_id":"session-1"}), "starting")).unwrap();
        for runtime in [None, Some("starting")] {
            if let Some(status) = runtime {
                store.append_claim(&claim("agent/eval.worker", "runtime.observed",
                    json!({"status":status,"runtime_id":"eval.worker"}), "runtime-starting")).unwrap();
            }
            assert_eq!(store.bind_mailbox(&request).unwrap_err().code, "mailbox-session-starting");
            assert_eq!(store.bind_mailbox(&Fence::new("agent/eval.worker", "foreign", "delivery")).unwrap_err().code, "stale-mailbox-session");
        }
        ready(&store, "session-1");
        let bound = store.bind_mailbox(&request).unwrap();
        assert_eq!(bound.epoch, 1, "startup retries never allocate an owner");
        ready(&store, "session-2");
        assert_eq!(store.bind_mailbox(&request).unwrap_err().code, "stale-mailbox-session");
        assert_eq!(store.check_mailbox(&bound).unwrap_err().code, "stale-mailbox-session");
    }

    #[test]
    fn replacement_mailbox_waits_while_the_graph_still_describes_its_predecessor() {
        for status in ["running", "exited", "vanished"] {
            let store = Store::open_memory("node").unwrap();
            ready(&store, "previous");
            let previous = store
                .bind_mailbox(&Fence::new("agent/eval.worker", "previous", "delivery"))
                .unwrap();
            store
                .append_claim(&claim(
                    "agent/eval.worker",
                    "runtime.observed",
                    json!({"status":status,"runtime_id":"eval.worker","incarnation_id":"previous"}),
                    "previous-runtime",
                ))
                .unwrap();
            store
                .append_claim(&claim(
                    "agent/eval.worker",
                    "harness.observed",
                    json!({"state":"starting","driver":"claude","incarnation_id":"replacement"}),
                    "replacement-starting",
                ))
                .unwrap();
            let replacement = Fence::new("agent/eval.worker", "replacement", "delivery");
            assert_eq!(
                store.bind_mailbox(&replacement).unwrap_err().code,
                "mailbox-session-starting",
                "{status}"
            );
            assert_eq!(
                store
                    .bind_mailbox(&Fence::new("agent/eval.worker", "foreign", "delivery"))
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
            // Even a matching starting harness cannot revive its own exited incarnation.
            if status != "running" {
                assert_eq!(
                    store.bind_mailbox(&previous).unwrap_err().code,
                    "stale-mailbox-session"
                );
            }
            ready(&store, "replacement");
            let bound = store.bind_mailbox(&replacement).unwrap();
            assert_eq!(bound.epoch, 2, "waiting never allocates ownership");
            assert_eq!(
                store.check_mailbox(&previous).unwrap_err().code,
                "stale-mailbox-session"
            );
            store.append_claim(&claim("agent/eval.worker", "runtime.observed",
                json!({"status":"exited","runtime_id":"eval.worker","incarnation_id":"replacement"}), "replacement-exited")).unwrap();
            store
                .append_claim(&claim(
                    "agent/eval.worker",
                    "harness.observed",
                    json!({"state":"starting","driver":"claude","incarnation_id":"replacement"}),
                    "replacement-still-starting",
                ))
                .unwrap();
            assert_eq!(
                store.bind_mailbox(&bound).unwrap_err().code,
                "stale-mailbox-session"
            );
        }
    }

    #[test]
    fn replacement_mailbox_waits_when_the_graph_last_saw_an_exit_that_names_no_incarnation() {
        // After a daemon restart the new daemon records the dead seat as exited without an
        // incarnation, and the replacement's driver can bind before it publishes `starting`.
        for status in ["exited", "vanished"] {
            let store = Store::open_memory("node").unwrap();
            ready(&store, "previous");
            store
                .append_claim(&claim(
                    "agent/eval.worker",
                    "runtime.observed",
                    json!({"status":status,"runtime_id":"eval.worker"}),
                    "previous-gone",
                ))
                .unwrap();
            store
                .append_claim(&claim(
                    "agent/eval.worker",
                    "harness.observed",
                    json!({"state":"starting","driver":"opencode","incarnation_id":"replacement"}),
                    "replacement-starting",
                ))
                .unwrap();
            let replacement = Fence::new("agent/eval.worker", "replacement", "delivery");
            assert_eq!(
                store.bind_mailbox(&replacement).unwrap_err().code,
                "mailbox-session-starting",
                "{status}"
            );
            // Without the replacement's own starting evidence nothing is waited for.
            assert_eq!(
                store
                    .bind_mailbox(&Fence::new("agent/eval.worker", "foreign", "delivery"))
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
        }
    }

    #[test]
    fn mailbox_can_bind_before_the_native_provider_reports_ready() {
        let store = Store::open_memory("node").unwrap();
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "runtime.observed",
                json!({"status":"running","runtime_id":"eval.worker","incarnation_id":"session-1"}),
                "runtime:session-1",
            ))
            .unwrap();
        assert!(
            store
                .current_harness("agent/eval.worker")
                .unwrap()
                .is_none()
        );
        let bound = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        assert_eq!(bound.epoch, 1);
        ready(&store, "session-2");
        assert_eq!(
            store.check_mailbox(&bound).unwrap_err().code,
            "stale-mailbox-session"
        );
    }

    #[test]
    fn mailbox_replacement_fences_reconnect_and_receipts_after_daemon_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let request = Fence::new("agent/eval.worker", "session-1", "delivery");
        let replacement = Fence::new("agent/eval.worker", "session-1", "delivery");
        let (old, new);
        {
            let store = Store::open(&path, "node").unwrap();
            ready(&store, "session-1");
            store
                .append_claim(&claim(
                    "message/native",
                    "message.sent",
                    json!({"status":"sent",
                "from":"person/eval","to":request.subject,"content":"QUARTZ SIGNAL"}),
                    "send",
                ))
                .unwrap();
            old = store.bind_mailbox(&request).unwrap();
            assert_eq!(
                store.bind_mailbox(&request).unwrap().epoch,
                old.epoch,
                "lost bind acknowledgement retries the same epoch"
            );
            new = store.bind_mailbox(&replacement).unwrap();
            assert_eq!(new.epoch, old.epoch + 1, "the daemon owns epoch ordering");
            assert_eq!(
                store.bind_mailbox(&request).unwrap_err().code,
                "stale-mailbox-session",
                "a retired initial request cannot reallocate"
            );
            assert_eq!(
                store.bind_mailbox(&old).unwrap_err().code,
                "stale-mailbox-session"
            );
        }
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(
            store.bind_mailbox(&request).unwrap_err().code,
            "stale-mailbox-session",
            "retired binding tokens survive daemon restart"
        );
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
    fn legacy_recipient_receipt_replay_after_close_settles_without_backward_claims() {
        let store = Store::open_memory("node").unwrap();
        store.append_claim(&claim("message/native", "message.sent",
            json!({"status":"sent","from":"person/eval","to":"agent/eval.worker","content":"QUARTZ SIGNAL"}), "sent")).unwrap();
        for status in ["delivered", "read"] {
            store.append_claim(&claim("message/native", &format!("message.{status}"), json!({"status":status}), status)).unwrap();
        }
        let closed = store.append_claim(&claim("message/native", "message.closed", json!({"status":"closed"}), "closed")).unwrap();
        let before = store.claims_for("message/native", None).unwrap().len();
        for status in ["staged", "delivered", "read"] {
            let input = claim("message/native", &format!("message.{status}"), json!({"status":status}), &format!("legacy-replay-{status}"));
            assert_eq!(store.append_claim(&input).unwrap().id, closed.id);
            let mut foreign = input;
            foreign.actor = Some("agent/eval.other".into());
            foreign.idempotency_key = Some(format!("foreign-{status}"));
            assert_eq!(store.append_claim(&foreign).unwrap_err().code, "invalid-message-transition");
        }
        assert_eq!(store.claims_for("message/native", None).unwrap().len(), before);
        assert_eq!(store.message("message/native").unwrap().unwrap().status, "closed");
    }

    #[test]
    fn staged_replay_after_delivered_is_settled_without_a_backward_claim() {
        let store = Store::open_memory("node").unwrap();
        ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        store.append_claim(&claim("message/native", "message.sent",
            json!({"status":"sent","from":"person/eval","to":fence.subject,"content":"QUARTZ SIGNAL"}), "send")).unwrap();
        let delivered = claim(
            "message/native",
            "message.delivered",
            json!({"status":"delivered"}),
            "delivered",
        );
        let committed = store.append_mailbox_receipt(&delivered, &fence).unwrap();
        let staged = claim(
            "message/native",
            "message.staged",
            json!({"status":"staged","recipient":fence.subject,"transport":"omp-channel"}),
            "late-staged",
        );
        for _ in 0..2 {
            assert_eq!(
                store.append_mailbox_receipt(&staged, &fence).unwrap().id,
                committed.id
            );
            assert_eq!(
                store.message("message/native").unwrap().unwrap().status,
                "delivered"
            );
        }
        assert!(
            store
                .claims_for("message/native", Some("message.staged"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mailbox_receipts_cannot_mutate_another_recipients_message() {
        let store = Store::open_memory("node").unwrap();
        ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
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
