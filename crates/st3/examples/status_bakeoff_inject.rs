//! Disposable bakeoff input driver. Only opens an explicitly named /tmp/status-bakeoff-* DB.
use std::{io::{self, BufRead}, path::Path, time::{SystemTime, UNIX_EPOCH}};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use st3::{harness_events::Publication, model::ClaimInput, store::Store};
fn now_ms() -> u128 { SystemTime::now().duration_since(UNIX_EPOCH).expect("wall clock").as_millis() }
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let path = args.get(1).context("scratch database path required")?;
    let node = args.get(2).context("scratch node required")?;
    if !path.starts_with("/tmp/status-bakeoff-") || !node.starts_with("status-bakeoff-") { bail!("scratch-only path/node fence"); }
    if !Path::new(path).is_file() { bail!("refusing to create or guess a database"); }
    let store = Store::open(Path::new(path), node.clone())?;
    let subject = "agent/status-bakeoff/scratch";
    for line in io::stdin().lock().lines() {
        let input: Value = serde_json::from_str(&line?)?;
        let incarnation = input["incarnation"].as_str().context("incarnation")?;
        let mut fields = input["fields"].as_object().context("fields")?.clone();
        fields.insert("incarnation_id".into(), json!(incarnation));
        let runtime = input["kind"].as_str() == Some("runtime");
        if !runtime { fields.insert("driver".into(), json!("codex")); fields.insert("observed_at_ms".into(), json!(now_ms())); }
        let claim = ClaimInput { subject: subject.into(), kind: if runtime {"runtime.observed"} else {"harness.observed"}.into(), actor: Some(subject.into()), fields: fields.into_iter().collect(), evidence: Vec::new(), expected_subject: None, idempotency_key: Some(format!("bakeoff-{}-{}", incarnation, input["sequence"])) };
        let started = now_ms();
        let result = if runtime { store.append_claim(&claim).map(|record| (record, true)) } else { store.append_harness_event(&Publication { runtime_incarnation: incarnation.into(), sequence: input["sequence"].as_u64().context("sequence")?, claim }) };
        match result {
            Ok((record, appended)) => {
                let local_frontier = store.local_observations_tail(1)?.last().and_then(st3::store::local_observation_position).unwrap_or(0);
                println!("{}", json!({"kind":"accepted","input":input,"started_ms":started,"accepted_ms":now_ms(),"appended":appended,"record":record,"store_index":store.index()?,"local_frontier":local_frontier}));
            },
            Err(error) => println!("{}", json!({"kind":"rejected","input":input,"started_ms":started,"finished_ms":now_ms(),"error":error.to_string()})),
        }
    }
    Ok(())
}
