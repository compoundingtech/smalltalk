//! Invented-data append benchmark. Each invocation creates its own FULL/WAL store.
//! cargo run --release -p st3 --features smallclaims/test-support --example append_cost -- SCRATCH
use std::{path::PathBuf, sync::Arc, time::Instant};

use serde_json::json;
use smallclaims::{
    fleet::MemberKey,
    sqlite::{SQLITE_COMMIT_NANOS, SQLITE_COMMITS, SQLITE_NANOS, work},
};
use st3::{model::ClaimInput, store::Store};

fn input(kind: &str, n: usize, changed: bool) -> ClaimInput {
    let fields = match kind {
        "message.sent" => {
            json!({"from":"agent/sample/writer", "to":"agent/sample/reader", "status":"sent", "content":format!("Invented message {n}")})
        }
        "daemon.diagnostic" => {
            json!({"code":"append-cost", "severity":"warning", "reason":format!("Invented reading {n}")})
        }
        "harness.limits" => {
            json!({"driver":"codex", "incarnation_id":"sample", "account":"account/sample", "measured_at_unix_ms":n, "five_hour_percent": if changed { n % 90 } else { 10 }})
        }
        "harness.observed" => {
            json!({"driver":"codex", "incarnation_id":"sample", "state":if changed && n % 2 == 0 { "idle" } else { "working" }, "observed_at_ms": n})
        }
        _ => unreachable!(),
    };
    ClaimInput {
        subject: match kind {
            "message.sent" => format!("message/sample-{n}"),
            "daemon.diagnostic" => "daemon/sample".into(),
            _ => "agent/sample/writer".into(),
        },
        kind: kind.into(),
        actor: (kind == "message.sent").then(|| "agent/sample/writer".into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some(format!("{kind}-{n}")),
    }
}

fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("new scratch directory"));
    anyhow::ensure!(!root.exists(), "scratch directory must be new");
    std::fs::create_dir_all(&root)?;
    smallclaims::profile::init_from_env();
    let count = std::env::var("APPEND_COST_COUNT")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(256_usize);
    let seed = std::env::var("APPEND_COST_SEED")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(512_usize);
    anyhow::ensure!(
        count >= 100,
        "at least 100 calls are needed for the 100 concurrent case"
    );
    for kind in [
        "message.sent",
        "daemon.diagnostic",
        "harness.limits",
        "harness.observed",
    ] {
        for changed in [false, true] {
            for width in [1, 10, 100] {
                let path = root.join(format!("{kind}-{changed}-{width}.sqlite3"));
                let store = Arc::new(Store::open(&path, "sample")?);
                const FLEET: &str = "7b839ce0-bca9-41ab-8dd7-7d7786439e12";
                let member = Arc::new(MemberKey::generate()?.0);
                store.bind_fleet(FLEET)?;
                store.pin_fleet_anchor(member.public())?;
                store.set_member_key(Some(member.clone()))?;
                store.append_claim(&ClaimInput {
                    subject: "host/sample".into(), kind: "fleet.member-admitted".into(), actor: None,
                    fields: serde_json::from_value(json!({"fleet_id":FLEET,"member_key":member.public(),"via":"anchor","mode":"listening"}))?,
                    evidence: vec![], expected_subject: None, idempotency_key: None,
                })?;
                store.ensure_principal_key("agent/sample/writer")?;
                store.connection.set_shared_appends(
                    std::env::var("APPEND_COST_SHARED").is_ok_and(|value| value == "1"),
                );
                for n in 0..seed {
                    store.append_claim(&input(kind, n, true))?;
                }
                let rows = rusqlite::Connection::open(&path)?;
                let (initial_batches, initial_batch_rowid): (u64, i64) = rows.query_row(
                    "SELECT COUNT(*),COALESCE(MAX(rowid),0) FROM batches",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let initial = store.index()?;
                let grouping_before = smallclaims::append_group::counters();
                let before = work::total();
                let read = |counter: &std::sync::atomic::AtomicU64| {
                    counter.load(std::sync::atomic::Ordering::Relaxed)
                };
                let commits = read(&SQLITE_COMMITS);
                let commit_ns = read(&SQLITE_COMMIT_NANOS);
                let sql_ns = read(&SQLITE_NANOS);
                let started = Instant::now();
                let mut latencies = vec![];
                for wave in (0..count).step_by(width) {
                    let barrier = std::sync::Barrier::new((count - wave).min(width));
                    let calls = std::thread::scope(|scope| {
                        (wave..(wave + width).min(count))
                            .map(|n| {
                                let store = &store;
                                let barrier = &barrier;
                                scope.spawn(move || {
                                    barrier.wait();
                                    let started = Instant::now();
                                    let result = smallclaims::profile::task("append-cost", || {
                                        store.append_claim(&input(kind, seed + n, changed))
                                    });
                                    (started.elapsed().as_secs_f64() * 1000.0, result)
                                })
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .map(|call| call.join().unwrap())
                            .collect::<Vec<_>>()
                    });
                    for (latency, result) in calls {
                        result?;
                        latencies.push(latency);
                    }
                }
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let measured = work::total() - before;
                let grouping_after = smallclaims::append_group::counters();
                let committed = read(&SQLITE_COMMITS) - commits;
                let commit_ms = (read(&SQLITE_COMMIT_NANOS) - commit_ns) as f64 / 1e6;
                let sql_ms = (read(&SQLITE_NANOS) - sql_ns) as f64 / 1e6;
                latencies.sort_by(f64::total_cmp);
                let batches: u64 =
                    rows.query_row("SELECT COUNT(*) FROM batches", [], |r| r.get(0))?;
                // Sealing is a separate phase after all durable append acknowledgements. Its
                // signatures must be counted rather than silently charged to append latency.
                let seal_started = Instant::now();
                store.seal_local_batches()?;
                let seal_ms = seal_started.elapsed().as_secs_f64() * 1000.0;
                let signatures: u64 = rows.query_row(
                    "SELECT COUNT(*) FROM replica_envelope_signatures",
                    [],
                    |r| r.get(0),
                )?;
                let payload_bytes: u64 = rows.query_row(
                    "SELECT COALESCE(SUM(length(payload)),0) FROM replica_envelopes",
                    [],
                    |r| r.get(0),
                )?;
                let measured_signatures: u64 = rows.query_row(
                    "SELECT COUNT(*) FROM replica_envelope_signatures signatures JOIN replica_envelopes USING(writer,sequence,envelope_hash) JOIN batches ON batches.id=replica_envelopes.batch_id WHERE batches.rowid>?1",
                    [initial_batch_rowid],|r|r.get(0))?;
                let measured_batches = batches - initial_batches;
                let new_claims = store.index()? - initial;
                println!(
                    "{}",
                    json!({"kind":kind, "changed":changed, "width":width, "calls":count, "seed":seed,
                    "wall_ms":elapsed, "p50_ms":latencies[latencies.len()/2], "p99_ms":latencies[(latencies.len()*99/100).min(latencies.len()-1)],
                    "shared_restores_skipped":grouping_after.restores_skipped-grouping_before.restores_skipped,
                    "shared_unobserved_mutations":grouping_after.unobserved_mutations-grouping_before.unobserved_mutations,
                    "shared_reused_batches":grouping_after.reused_batches-grouping_before.reused_batches, "claims":new_claims, "batches_total":batches, "batches_measured":measured_batches,
                    "shared_witness_failures":grouping_after.witness_failures-grouping_before.witness_failures,
                    "claims_per_batch":if measured_batches>0 {Some(new_claims as f64/measured_batches as f64)} else {None},
                    "seal_ms":seal_ms,"signatures_total":signatures,"signatures_measured":measured_signatures,"envelope_payload_bytes":payload_bytes, "commits":committed, "commit_ms":commit_ms,
                    "sql_ms":sql_ms, "statements":measured.statements, "vm_steps":measured.vm_steps, "fullscan_steps":measured.fullscan_steps})
                );
            }
        }
    }
    Ok(())
}
