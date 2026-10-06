//! The categorical latest-state lane never waits for the ordered observation history drain.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;
use st3::client::Client;
use st3::model::{ClaimInput, ClaimRecord};
use tokio::io::unix::AsyncFd;
use tokio::time::Instant;

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
        let pipe = AsyncFd::new(st_drivers::harness_events::bind_state_wake_pipe(dir)?)?;
        // Retries re-read the durable latest snapshot rather than repeat bytes held before an outage.
        let client = client.clone().with_outage_wait(Duration::ZERO, false);
        let subject = subject.to_owned();
        let driver = driver.to_owned();
        let dir = dir.to_owned();
        let runtime = runtime.to_owned();
        let task = tokio::spawn(async move {
            let mut accepted = None;
            let mut deadline = Some(Instant::now());
            let mut warning = None;
            loop {
                let wake = tokio::select! {
                    wake = recv_wake(&pipe) => wake,
                    () = wait_until(deadline) => Ok(()),
                };
                let result = async {
                    wake?;
                    let Some(raw) = st_drivers::harness_events::read_runtime_state(&dir, &runtime)? else {
                        return Ok(None);
                    };
                    let now = st_drivers::message::now_ms();
                    let Some((claim, expiry)) = current_claim(&subject, &driver, &runtime, &raw, now)? else {
                        return Ok(None);
                    };
                    if accepted.as_ref() != Some(&claim.fields) {
                        let publication = st3::harness_events::CurrentPublication {
                            runtime_incarnation: runtime.clone(),
                            claim,
                        };
                        let _: ClaimRecord = client.post("/v1/harness-state", &publication).await?;
                        accepted = Some(publication.claim.fields);
                    }
                    Ok::<_, anyhow::Error>(expiry.map(|at| {
                        Instant::now() + Duration::from_millis(at.saturating_sub(st_drivers::message::now_ms()))
                    }))
                }.await;
                deadline = match result {
                    Ok(expiry) => expiry,
                    Err(error) => {
                        super::note_driver_tick_failure(&subject, error, &mut warning);
                        Some(Instant::now() + Duration::from_secs(1))
                    }
                };
            }
        });
        Ok(Some(Self { task }))
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn recv_wake(pipe: &AsyncFd<std::fs::File>) -> Result<()> {
    use std::io::Read as _;
    loop {
        let mut ready = pipe.readable().await?;
        match ready.try_io(|descriptor| {
            let mut file = descriptor.get_ref();
            let mut bytes = [0; 256];
            file.read(&mut bytes)
        }) {
            Ok(Ok(count)) if count > 0 => return Ok(()),
            Ok(Ok(_)) => anyhow::bail!("current state wake pipe closed"),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => continue,
        }
    }
}

fn current_claim(
    subject: &str,
    driver: &str,
    runtime: &str,
    raw: &[u8],
    now: u64,
) -> Result<Option<(ClaimInput, Option<u64>)>> {
    use st_drivers::harness_state::Activity;
    let observed = st_drivers::harness_state::read_raw_at(raw, None, now);
    if observed.harness.as_deref() != Some(driver)
        || observed.reason.as_deref() == Some("claimed")
    {
        return Ok(None);
    }
    let stamp = observed.observed_at_ms.context("current state has no source timestamp")?;
    let owner = observed.evidence_incarnation.as_deref().context("current state has no provider owner")?;
    let ownership = observed.ownership_sequence.context("current state has no ownership sequence")?;
    let state = super::harness_activity_state(observed.state);
    // Forward the existing active-ask reference only while its human question is live.
    #[derive(serde::Deserialize)]
    struct CurrentExtras {
        #[serde(default, rename = "activeAsk")]
        active_ask: Option<String>,
        #[serde(default)]
        history_gap_count: Option<u64>,
        #[serde(default)]
        history_gap_from_ms: Option<u64>,
        #[serde(default)]
        history_gap_to_ms: Option<u64>,
        #[serde(default)]
        history_gap_reason: Option<String>,
    }
    let extras: CurrentExtras = serde_json::from_slice(raw)?;
    let active_ask = if matches!(observed.state, Activity::Active | Activity::Child)
        && observed.blocked_on == st_drivers::harness_state::BlockedOn::Human
        && observed.ask == st_drivers::harness_state::Ask::Question
    {
        extras.active_ask.map(Value::String).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let transport = match driver {
        "claude" => "claude-channel",
        "codex" => "app-server",
        "pi" => "pi-channel",
        "omp" => "omp-channel",
        _ => "native",
    };
    let mut fields = BTreeMap::from([
        ("state".into(), Value::from(state)),
        ("driver".into(), Value::from(driver)),
        ("transport".into(), Value::from(transport)),
        ("incarnation_id".into(), Value::from(runtime)),
        ("observed_at_ms".into(), Value::from(stamp)),
        ("observed_since_ms".into(), observed.since_ms.map(Value::from).unwrap_or(Value::Null)),
        ("ownership_sequence".into(), Value::from(ownership)),
        ("transition_sequence".into(), observed.transition_sequence.map(Value::from).unwrap_or(Value::Null)),
        ("evidence_incarnation".into(), Value::from(owner)),
        ("provider_auth".into(), observed.provider_auth.map(Value::from).unwrap_or(Value::Null)),
        ("provider_auth_sequence".into(), Value::from(observed.provider_auth_sequence)),
        ("blocked_on".into(), Value::from(observed.blocked_on.as_str())),
        ("ask".into(), Value::from(observed.ask.as_str())),
        ("active_ask".into(), active_ask),
        ("background_jobs".into(), observed.background_jobs.map(Value::from).unwrap_or(Value::Null)),
        ("running_subagents".into(), observed.running_subagents.map(Value::from).unwrap_or(Value::Null)),
        ("history_gap_count".into(), extras.history_gap_count.map(Value::from).unwrap_or(Value::Null)),
        ("history_gap_from_ms".into(), extras.history_gap_from_ms.map(Value::from).unwrap_or(Value::Null)),
        ("history_gap_to_ms".into(), extras.history_gap_to_ms.map(Value::from).unwrap_or(Value::Null)),
        ("history_gap_reason".into(), extras.history_gap_reason.map(Value::from).unwrap_or(Value::Null)),
        ("input_buffer".into(), Value::from(observed.input_buffer.as_str())),
    ]);
    st3::suspension::annotate_quiescence(&mut fields);
    fields.remove("rollout_operation");
    let expiry = (observed.state != Activity::Unknown).then(|| {
        stamp.saturating_add(st_drivers::harness_state::HARNESS_STATE_STALE.as_millis() as u64)
    }).filter(|expiry| *expiry > now);
    Ok(Some((ClaimInput {
        subject: subject.into(),
        kind: "harness.current".into(),
        actor: Some(subject.into()),
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }, expiry)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use st_drivers::harness_state::{Activity, Ask, BlockedOn, InputBuffer, Observation, Writer};

    fn snapshot() -> Vec<u8> {
        let root = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(root.path(), "example.worker", "omp", Some("fixture".into()));
        let mut observation = Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
            .with_ask(Ask::Question)
            .with_reason("diagnostic question text");
        observation.running_subagents = Some(3);
        writer.observe(observation).unwrap();
        let mut raw: Value = serde_json::from_slice(
            &std::fs::read(st_drivers::harness_state::harness_state_path(root.path())).unwrap(),
        ).unwrap();
        raw["activeAsk"] = Value::from("ask/current");
        raw["history_gap_count"] = Value::from(2);
        raw["history_gap_from_ms"] = Value::from(1);
        raw["history_gap_to_ms"] = Value::from(2);
        raw["history_gap_reason"] = Value::from("cap-full");
        serde_json::to_vec(&raw).unwrap()
    }

    #[test]
    fn expiry_clears_a_question_and_count_without_restamping_old_evidence() {
        let raw = snapshot();
        let stamp = st_drivers::harness_state::read_raw_at(&raw, None, st_drivers::message::now_ms())
            .observed_at_ms.unwrap();
        let (live, expiry) = current_claim("example.worker", "omp", "runtime-a", &raw, stamp).unwrap().unwrap();
        assert_eq!(live.fields["blocked_on"], "human");
        assert_eq!(live.fields["running_subagents"], 3);
        assert_eq!(live.fields["active_ask"], "ask/current");
        assert!(!live.fields.contains_key("reason"));
        let (expired, next_expiry) = current_claim(
            "example.worker", "omp", "runtime-a", &raw, expiry.unwrap(),
        ).unwrap().unwrap();
        assert_eq!(expired.fields["state"], "unknown");
        assert_eq!(expired.fields["blocked_on"], "unknown");
        assert_eq!(expired.fields["ask"], "unknown");
        assert_eq!(expired.fields["running_subagents"], Value::Null);
        assert_eq!(expired.fields["active_ask"], Value::Null);
        assert_eq!(expired.fields["observed_at_ms"], stamp);
        assert_eq!(expired.fields["history_gap_count"], 2);
        assert_eq!(expired.fields["history_gap_from_ms"], 1);
        assert_eq!(expired.fields["history_gap_to_ms"], 2);
        assert_eq!(expired.fields["history_gap_reason"], "cap-full");
        assert_eq!(next_expiry, None);
    }

    #[tokio::test]
    async fn latest_state_and_restart_do_not_wait_for_or_acknowledge_ordered_history() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use axum::{Json, Router, routing::post};
        use st3::client::Endpoint;
        use tokio::sync::{Mutex, Notify, mpsc};

        let subject = "agent/example/current-publisher";
        let root = tempfile::tempdir().unwrap();
        st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
        let mut writer = Writer::new(root.path(), subject, "omp", Some("fixture".into()));
        for n in 0..128 {
            let mut observation = Observation::new(Activity::Active, BlockedOn::None, InputBuffer::Empty);
            observation.running_subagents = Some(n % 2 + 1);
            writer.observe(observation).unwrap();
        }
        let original: Vec<_> = st_drivers::harness_events::pending(root.path(), 256).unwrap()
            .into_iter().map(|event| event.sequence).collect();
        assert_eq!(original.len(), 128);
        let store = Arc::new(st3::store::Store::open_memory("current-publisher").unwrap());
        store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("status".into(), Value::from("running")),
                ("incarnation_id".into(), Value::from("runtime-a")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();

        let history_entered = Arc::new(Notify::new());
        let release_history = Arc::new(Notify::new());
        let history_waiting = Arc::new(AtomicBool::new(true));
        let history_sequences = Arc::new(Mutex::new(Vec::new()));
        let history_cards = Arc::new(Mutex::new(Vec::new()));
        let release_current = Arc::new(Notify::new());
        let (current_tx, mut current_rx) = mpsc::unbounded_channel();
        let app = Router::new().route("/v1/harness-events", post({
            let store = store.clone();
            let entered = history_entered.clone();
            let release = release_history.clone();
            let waiting = history_waiting.clone();
            let sequences = history_sequences.clone();
            let cards = history_cards.clone();
            move |Json(publication): Json<st3::harness_events::Publication>| {
                let (store, entered, release, waiting, sequences, cards) =
                    (store.clone(), entered.clone(), release.clone(), waiting.clone(), sequences.clone(), cards.clone());
                async move {
                    if waiting.swap(false, Ordering::SeqCst) {
                        entered.notify_one();
                        release.notified().await;
                    }
                    let (claim, _) = store.append_harness_event(&publication).unwrap();
                    sequences.lock().await.push(publication.sequence);
                    cards.lock().await.push(serde_json::to_value(store.current_harness(subject).unwrap()).unwrap());
                    Json(serde_json::json!({"api_version":"st3.v1", "value":claim}))
                }
            }
        })).route("/v1/harness-state", post({
            let store = store.clone();
            let release = release_current.clone();
            move |Json(publication): Json<st3::harness_events::CurrentPublication>| {
                let (store, release, tx) = (store.clone(), release.clone(), current_tx.clone());
                async move {
                    let (claim, _) = store.append_harness_current(&publication).unwrap();
                    let hold = claim.body["fields"]["running_subagents"] == 3;
                    tx.send(claim.clone()).unwrap();
                    if hold {
                        release.notified().await;
                    }
                    Json(serde_json::json!({"api_version":"st3.v1", "value":claim}))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(Endpoint::Http(format!("http://{}", listener.local_addr().unwrap())));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut history = super::super::NativeObservations::start(root.path(), "runtime-a").unwrap();
        let mut current = Publisher::start(&client, subject, "omp", root.path(), "runtime-a").unwrap();
        let first = tokio::time::timeout(Duration::from_secs(3), current_rx.recv()).await.unwrap().unwrap();
        assert_eq!(first.body["fields"]["running_subagents"], 2);
        let (history_client, history_dir) = (client.clone(), root.path().to_owned());
        let drain = tokio::spawn(async move {
            let mut ready = false;
            while !st_drivers::harness_events::pending(&history_dir, 1).unwrap().is_empty() {
                history.drain(&history_client, subject, "omp", &mut ready).await.unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(3), history_entered.notified()).await.unwrap();
        let mut question = Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
            .with_ask(Ask::Question);
        question.running_subagents = Some(3);
        writer.observe(question).unwrap();
        let blocked = tokio::time::timeout(Duration::from_secs(3), current_rx.recv()).await.unwrap().unwrap();
        assert_eq!(blocked.body["fields"]["blocked_on"], "human");
        assert_eq!(blocked.body["fields"]["ask"], "question");
        assert_eq!(blocked.body["fields"]["running_subagents"], 3);
        assert_eq!(st_drivers::harness_events::pending(root.path(), 256).unwrap()[0].sequence, original[0]);
        let mut intermediate = Observation::new(Activity::Active, BlockedOn::None, InputBuffer::Empty);
        intermediate.running_subagents = Some(4);
        writer.observe(intermediate).unwrap();
        let mut idle = Observation::new(Activity::Idle, BlockedOn::None, InputBuffer::Empty);
        idle.background_jobs = Some(0);
        idle.running_subagents = Some(0);
        writer.observe(idle).unwrap();
        release_current.notify_one();
        let cleared = tokio::time::timeout(Duration::from_secs(3), current_rx.recv()).await.unwrap().unwrap();
        assert_eq!(cleared.body["fields"]["blocked_on"], "none");
        assert_eq!(cleared.body["fields"]["ask"], "none");
        assert_eq!(cleared.body["fields"]["running_subagents"], 0);
        assert_eq!(cleared.body["fields"]["active_ask"], Value::Null);
        drop(current.take());
        let mut question = Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
            .with_ask(Ask::Question);
        question.running_subagents = Some(5);
        writer.observe(question).unwrap();
        current = Publisher::start(&client, subject, "omp", root.path(), "runtime-a").unwrap();
        let resumed = tokio::time::timeout(Duration::from_secs(3), current_rx.recv()).await.unwrap().unwrap();
        assert_eq!(resumed.body["fields"]["blocked_on"], "human");
        assert_eq!(resumed.body["fields"]["running_subagents"], 5);
        let expected: Vec<_> = st_drivers::harness_events::pending(root.path(), 256).unwrap()
            .into_iter().map(|event| event.sequence).collect();
        release_history.notify_one();
        tokio::time::timeout(Duration::from_secs(10), drain).await.unwrap().unwrap();
        assert_eq!(*history_sequences.lock().await, expected);
        for card in history_cards.lock().await.iter() {
            assert_eq!(card["blocked_on"], "human", "{card}");
            assert_eq!(card["ask"], "question", "{card}");
            assert_eq!(card["running_subagents"], 5, "{card}");
        }
        assert!(st_drivers::harness_events::pending(root.path(), 1).unwrap().is_empty());
        drop(current);
        server.abort();
    }
}

