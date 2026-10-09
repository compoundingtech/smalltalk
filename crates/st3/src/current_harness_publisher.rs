//! Independent current snapshots, adapted from the parked #1546 publisher.
//! Each source revision gets one attempt; failures never arm retries or hold publication bytes.
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use st3::client::Client;

pub(super) struct Publisher {
    task: tokio::task::JoinHandle<()>,
}

impl Publisher {
    pub(super) fn start(
        client: &Client,
        subject: &str,
        driver: &str,
        dir: &Path,
        runtime: &str,
    ) -> Result<Option<Self>> {
        if !st_drivers::harness_events::enabled(dir) {
            return Ok(None);
        }
        let mut observations = super::NativeObservations::start_with_pipe(dir, runtime, true)?;
        let client = client.clone().with_outage_wait(Duration::ZERO, false);
        let (subject, driver) = (subject.to_owned(), driver.to_owned());
        let task = tokio::spawn(async move {
            loop {
                let deadline = observations
                    .evidence_deadline
                    .as_ref()
                    .and_then(|state| state["writtenAtMs"].as_u64())
                    .map(|stamp| {
                        Duration::from_millis(
                            stamp
                                .saturating_add(
                                    st_drivers::harness_state::HARNESS_STATE_STALE.as_millis()
                                        as u64,
                                )
                                .saturating_sub(st_drivers::message::now_ms()),
                        )
                    });
                tokio::select! {
                    result = observations.recv() => { if result.is_err() { break; } }
                    () = wait(deadline) => { let _ = observations.expire_due(); }
                }
                let _ = observations
                    .publish_snapshots(&client, &subject, &driver, &mut false)
                    .await;
            }
        });
        Ok(Some(Self { task }))
    }
}

async fn wait(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::json;
    use st_drivers::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};
    use st3::client::Endpoint;
    use st3::model::ClaimInput;
    use std::sync::Arc;
    use tokio::sync::{Notify, mpsc};

    #[tokio::test]
    async fn current_publisher_advances_while_durable_publication_is_stalled() {
        let root = tempfile::tempdir().unwrap();
        st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
        let seq =
            st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-a")
                .unwrap();
        let mut writer = Writer::new(root.path(), "example/seat", "claude", Some("pty".into()))
            .with_ownership("provider-a", seq);
        let sample = |activity| Observation::new(activity, BlockedOn::None, InputBuffer::Unknown);
        writer.observe(sample(Activity::Active)).unwrap();
        let mut context = st_drivers::harness_context::Writer::new_paths(
            root.path(),
            "example/seat",
            st_drivers::harness_context::Harness::Claude,
        )
        .unwrap()
        .with_session("provider-a");
        context
            .observe(st_drivers::harness_context::Reading {
                session_total_tokens: Some(123),
                ..Default::default()
            })
            .unwrap();
        let entered = Arc::new(Notify::new());
        let (sender, mut updates) = mpsc::unbounded_channel();
        let store = Arc::new(st3::store::Store::open_memory("example").unwrap());
        let app = Router::new()
            .route(
                "/v1/claims",
                post(move |Json(input): Json<ClaimInput>| {
                    let store = store.clone();
                    let sender = sender.clone();
                    async move {
                        let record = store.append_claim(&input).unwrap();
                        sender.send(input).unwrap();
                        Json(json!({"api_version":"st3.v1", "value":record}))
                    }
                }),
            )
            .route(
                "/v1/harness-events",
                post({
                    let entered = entered.clone();
                    move || {
                        let entered = entered.clone();
                        async move {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                            StatusCode::SERVICE_UNAVAILABLE
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(Endpoint::Http(format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut native = super::super::NativeObservations::start(root.path(), "runtime-a").unwrap();
        native.current_publisher = Publisher::start(
            &client,
            "agent/example/seat",
            "claude",
            root.path(),
            "runtime-a",
        )
        .unwrap();
        let drain = tokio::spawn(async move {
            native
                .drain(&client, "agent/example/seat", "claude", &mut false)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.fields["state"], "working");
        writer.observe(sample(Activity::Idle)).unwrap();
        let current = tokio::time::timeout(Duration::from_secs(2), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.fields["state"], "idle");
        assert!(!drain.is_finished());
        let pending = st_drivers::harness_events::pending(root.path(), 100).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].kind, "harness-accounting");
        assert_eq!(
            pending[1].kind, "harness-state-control",
            "the reliable accounting stop stays queued independently of current status"
        );
        assert!(pending[0].payload.get("usedTokens").is_none());
        assert_eq!(pending[0].payload["sessionTotalTokens"], 123);
        drain.abort();
        server.abort();
    }
}
