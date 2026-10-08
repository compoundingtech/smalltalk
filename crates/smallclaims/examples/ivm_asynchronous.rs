//! Run with normal Cargo configuration: cargo run -p smallclaims --example ivm_asynchronous
//! Claims-only example, not a production admission/authority adapter.
use anyhow::{Result, bail};
use serde_json::json;
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    ivm::{
        Contribution, Definition, View, Views,
        after_write::{self, Target, WaitOptions, WaitOutcome, WriteOutcome},
        asynchronous::{Limits, Worker},
        events::{self, Publisher},
        runtime::ViewRuntime,
    },
    store::canonical,
};
use std::sync::Arc;
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

struct AgentStatus;
impl View for AgentStatus {
    fn definition(&self) -> Definition {
        Definition {
            name: "agent-status",
            fingerprint: "example.status.v1",
            kinds: &["agent.status"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        rank: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: "status".into(),
            value: claim.body["fields"]["status"].clone(),
            rank: canonical::sortable_key(rank),
        }])
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let runtime = Arc::new(ViewRuntime::asynchronous(
        Views::new(vec![Box::new(AgentStatus)])?,
        Limits::default(),
    )?);
    let store = Arc::new(Store::open_memory("example", runtime.clone())?);
    store
        .connection
        .batched(|tx| events::install(tx, 128))
        .map_err(anyhow::Error::msg)??;
    let publisher = Publisher::attach(&store, 8)?;
    // Explicit owner scheduling; ordinary open/read never starts catch-up.
    let worker = Worker::start(store.clone(), runtime.clone(), publisher.subscribe())?;
    let receipt = match after_write::write(
        &store,
        &ClaimInput {
            subject: "agent/ada".into(),
            kind: "agent.status".into(),
            actor: None,
            fields: serde_json::from_value(json!({"status":"working"}))?,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        },
    )? {
        WriteOutcome::Committed { receipt, .. } => receipt,
        WriteOutcome::CommittedWithoutReceipt { claim, reason } => bail!(
            "write {} committed but receipt unavailable: {}",
            claim.id,
            reason
        ),
    };
    let (_cancel, cancellation) = watch::channel(false);
    let result = after_write::wait(
        &store,
        &runtime.views,
        "agent-status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: Instant::now() + Duration::from_secs(5),
            cancellation,
        },
        |connection, boundary| {
            // Production callers check current person/owner/incarnation/authority here,
            // in this same snapshot. An external effect still requires a fresh effect fence.
            runtime.views.head(
                connection,
                "agent-status",
                "agent/ada",
                "status",
                boundary.source_cut,
            )
        },
    )
    .await?;
    match result {
        WaitOutcome::Ready {
            value,
            boundary,
            mapped_index,
        } => println!(
            "after claim {} at local index {}: {:?}, processed through {}",
            receipt.claim_id,
            mapped_index,
            value.map(|head| head.value),
            boundary.source_cut.projected
        ),
        other => bail!("read did not become ready: {other:?}"),
    }
    worker.stop().await?;
    Ok(())
}
