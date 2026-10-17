//! Mailbox tests that bind and replay through the store; they live here because the mailbox module sits below the store.

#![allow(unused_imports, dead_code)]

use crate::mailbox::*;
use crate::store::Store;
use crate::model::{DesiredSubject, MessageView};
use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::client::Fence;
use crate::model::ClaimInput;
use serde_json::json;
use std::collections::BTreeMap;
use axum::{Json, Router, routing::post};
use axum::http::StatusCode;
use std::sync::{Arc, Mutex};


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

pub fn ready(store: &Store, incarnation: &str) {
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
fn claude_attachment_fences_idle_hooks_and_clears_only_on_attach_or_replacement() {
    let store = Store::open_memory("node").unwrap();
    ready(&store, "current");
    let diagnostic = |code: &str, status: &str, incarnation: &str, key: &str| {
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "harness.diagnostic",
                json!({"severity":"warning","code":code,"status":status,
                "reason":"attachment probe","incarnation_id":incarnation}),
                key,
            ))
            .unwrap();
    };
    diagnostic(
        "claude-channel-unattached",
        "starting",
        "current",
        "starting",
    );
    assert_eq!(
        store
            .current_harness("agent/eval.worker")
            .unwrap()
            .unwrap()
            .state,
        "starting"
    );
    diagnostic("claude-channel-unattached", "blocked", "current", "blocked");
    store
        .append_claim(&claim(
            "agent/eval.worker",
            "harness.observed",
            json!({"state":"idle","driver":"claude","incarnation_id":"current"}),
            "idle-hook",
        ))
        .unwrap();
    let blocked = store.current_harness("agent/eval.worker").unwrap().unwrap();
    assert_eq!(blocked.state, "blocked");
    assert_eq!(blocked.reason.as_deref(), Some("claude-channel-unattached"));
    diagnostic(
        "claude-channel-attached",
        "recovered",
        "previous",
        "foreign-recovery",
    );
    assert_eq!(
        store
            .current_harness("agent/eval.worker")
            .unwrap()
            .unwrap()
            .state,
        "blocked"
    );
    diagnostic(
        "claude-channel-attached",
        "recovered",
        "current",
        "recovered",
    );
    assert_eq!(
        store
            .current_harness("agent/eval.worker")
            .unwrap()
            .unwrap()
            .state,
        "idle"
    );
    diagnostic(
        "claude-channel-unattached",
        "blocked",
        "current",
        "blocked-again",
    );
    ready(&store, "replacement");
    assert_eq!(
        store
            .current_harness("agent/eval.worker")
            .unwrap()
            .unwrap()
            .state,
        "ready"
    );
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
fn startup_hook_activity_waits_for_runtime_evidence_without_allocating_ownership() {
    for (state, expected_error) in [
        ("starting", "mailbox-session-starting"),
        ("ready", "mailbox-session-starting"),
        ("idle", "mailbox-session-starting"),
        ("working", "mailbox-session-starting"),
        ("blocked", "mailbox-session-starting"),
        ("indeterminate", "stale-mailbox-session"),
    ] {
        let store = Store::open_memory("node").unwrap();
        ready(&store, "previous");
        let previous = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "previous", "delivery"))
            .unwrap();
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "harness.observed",
                json!({"state":state,"driver":"claude","incarnation_id":"replacement"}),
                "replacement-hook",
            ))
            .unwrap();
        let replacement = Fence::new("agent/eval.worker", "replacement", "delivery");
        assert_eq!(
            store.bind_mailbox(&replacement).unwrap_err().code,
            expected_error,
            "{state}"
        );
        assert_eq!(
            store
                .bind_mailbox(&Fence::new("agent/eval.worker", "foreign", "delivery"))
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        let mut already_bound = replacement.clone();
        already_bound.epoch = 1;
        assert_eq!(
            store.bind_mailbox(&already_bound).unwrap_err().code,
            "stale-mailbox-session",
            "a bound owner is never a pending startup"
        );
        store
            .append_claim(&claim(
                "agent/eval.worker",
                "harness.observed",
                json!({"state":"ended","driver":"claude","incarnation_id":"replacement"}),
                "replacement-ended",
            ))
            .unwrap();
        assert_eq!(
            store.bind_mailbox(&replacement).unwrap_err().code,
            "stale-mailbox-session"
        );
        ready(&store, "replacement");
        let bound = store.bind_mailbox(&replacement).unwrap();
        assert_eq!(bound.epoch, 2, "hook activity never allocated an owner");
        assert_eq!(
            store.check_mailbox(&previous).unwrap_err().code,
            "stale-mailbox-session"
        );
        store.append_claim(&claim("agent/eval.worker", "runtime.observed",
                json!({"status":"exited","runtime_id":"eval.worker","incarnation_id":"replacement"}), "replacement-exited")).unwrap();
        assert_eq!(
            store.bind_mailbox(&bound).unwrap_err().code,
            "stale-mailbox-session"
        );
    }
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
