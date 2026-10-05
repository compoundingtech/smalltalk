//! Run in release mode on a private SQLite backup, never a live store.
//! Prints only timings, counts and authority digests, not stored content.
use st3::{
    model::{ClaimInput, ReplicationInventory},
    store::Store,
};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let path = Path::new(&args[1]);
    if args.get(2).is_some_and(|arg| arg == "seed") {
        let store = Store::open(path, "alder")?;
        for n in 0..4000 {
            store.append_claim(&ClaimInput {
                subject: "daemon/alder".into(),
                kind: "daemon.diagnostic".into(),
                actor: Some("daemon/alder".into()),
                fields: BTreeMap::from([
                    ("code".into(), serde_json::json!("binary-payload-bench")),
                    ("severity".into(), serde_json::json!("warning")),
                    (
                        "reason".into(),
                        serde_json::json!(format!("{n}: {}", "sample content ".repeat(80))),
                    ),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("note-{n}")),
            })?;
        }
        store.replication_snapshot()?;
        return Ok(());
    }
    let origin: String = rusqlite::Connection::open(path)?.query_row(
        "SELECT origin FROM batches ORDER BY rowid LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let cpu_started = process_time();
    let started = Instant::now();
    let source = Store::open(path, &origin)?;
    println!("open_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
    println!(
        "open_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    if args.get(2).is_some_and(|arg| arg == "convert") {
        let started = Instant::now();
        let mut converted = 0;
        let mut scanned = 0;
        loop {
            let page = source.convert_envelope_payloads()?;
            converted += page.converted;
            scanned += page.scanned;
            if page.done {
                break;
            }
        }
        println!(
            "conversion_ms={:.3} converted={converted} scanned={scanned}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        return Ok(());
    }
    let fleet = source
        .bound_fleet()?
        .unwrap_or_else(|| "sample-fleet".into());
    let cpu_started = process_time();
    let started = Instant::now();
    let mut exchange =
        source.export_replication_exchange(&fleet, &ReplicationInventory::default())?;
    let export = started.elapsed();
    let export_cpu = process_time() - cpu_started;
    let wire = serde_json::to_vec(&exchange)?;
    println!(
        "export_ms={:.3} export_wire_ms={:.3} envelopes={}",
        export.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0,
        exchange.envelopes.len()
    );
    println!(
        "export_cpu_ms={:.3} export_wire_cpu_ms={:.3}",
        export_cpu.as_secs_f64() * 1000.0,
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    let mut remote = ReplicationInventory {
        digest: "0".repeat(64),
        buckets: source.replication_snapshot()?.buckets.clone(),
        ..Default::default()
    };
    if let Some(bucket) = remote.buckets.first_mut() {
        bucket.digest = "0".repeat(64);
    }
    let cpu_started = process_time();
    let started = Instant::now();
    let modern = source.export_replication_exchange(&fleet, &remote)?;
    let export = started.elapsed();
    let export_cpu = process_time() - cpu_started;
    let modern_wire = serde_json::to_vec(&modern)?;
    println!(
        "modern_export_ms={:.3} modern_export_wire_ms={:.3} modern_export_cpu_ms={:.3} modern_export_wire_cpu_ms={:.3}",
        export.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0,
        export_cpu.as_secs_f64() * 1000.0,
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    std::hint::black_box(modern_wire);
    let authority = source
        .replication_status(true, Some(&fleet), &[])?
        .authority_digest;
    println!("authority={authority}");
    // Exercise payloads throughout the log, including documents and blobs, alongside the
    // ordinary first receive page. Identity selection is outside the timed export.
    let inventory = source.replication_inventory()?;
    let identities = inventory
        .envelopes
        .iter()
        .step_by((inventory.envelopes.len() / 512).max(1))
        .take(512)
        .cloned()
        .collect();
    let cpu_started = process_time();
    let started = Instant::now();
    let sample = source.replica_envelopes(identities)?;
    let export = started.elapsed();
    let export_cpu = process_time() - cpu_started;
    let sample_wire = serde_json::to_vec(&sample)?;
    println!(
        "sample_export_ms={:.3} sample_export_wire_ms={:.3} sample_wire_bytes={}",
        export.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0,
        sample_wire.len()
    );
    println!(
        "sample_export_cpu_ms={:.3} sample_export_wire_cpu_ms={:.3}",
        export_cpu.as_secs_f64() * 1000.0,
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    let cpu_started = process_time();
    let started = Instant::now();
    for envelope in &sample {
        let bytes = envelope.payload.bytes()?;
        anyhow::ensure!(
            envelope.hash
                == smallclaims::hash::replica_envelope_hash(
                    &envelope.writer,
                    envelope.sequence,
                    envelope.previous_hash.as_deref(),
                    envelope.accepted_at_unix_ms,
                    bytes,
                )
        );
        if let (Some(key), Some(signature)) = (&envelope.member_key, &envelope.signature) {
            anyhow::ensure!(st3::fleet::verify_signature(
                key,
                &st3::fleet::envelope_signature_message(
                    &fleet,
                    &envelope.writer,
                    envelope.sequence,
                    &envelope.hash,
                ),
                signature
            ));
        }
    }
    println!(
        "sample_verify_ms={:.3} signed_sample={}",
        started.elapsed().as_secs_f64() * 1000.0,
        sample.iter().filter(|e| e.signature.is_some()).count()
    );
    println!(
        "sample_verify_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    exchange.envelopes.truncate(2000);
    exchange.signatures.clear();
    std::hint::black_box(wire);
    let cpu_started = process_time();
    let started = Instant::now();
    for envelope in &exchange.envelopes {
        let bytes = envelope.payload.bytes()?;
        anyhow::ensure!(
            envelope.hash
                == smallclaims::hash::replica_envelope_hash(
                    &envelope.writer,
                    envelope.sequence,
                    envelope.previous_hash.as_deref(),
                    envelope.accepted_at_unix_ms,
                    bytes,
                )
        );
        if let (Some(key), Some(signature)) = (&envelope.member_key, &envelope.signature) {
            anyhow::ensure!(st3::fleet::verify_signature(
                key,
                &st3::fleet::envelope_signature_message(
                    &fleet,
                    &envelope.writer,
                    envelope.sequence,
                    &envelope.hash,
                ),
                signature
            ));
        }
    }
    println!(
        "verify_payload_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "verify_payload_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    let bytes = serde_json::to_vec(&exchange)?;
    let root = tempfile::tempdir()?;
    let target = Store::open(&root.path().join("target.sqlite3"), "birch")?;
    let cpu_started = process_time();
    let started = Instant::now();
    let exchange = serde_json::from_slice(&bytes)?;
    let receipt = target.receive_replication_exchange(&origin, &fleet, &exchange)?;
    println!(
        "wire_receive_ms={:.3} received={}",
        started.elapsed().as_secs_f64() * 1000.0,
        receipt.received
    );
    println!(
        "wire_receive_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    let cpu_started = process_time();
    let started = Instant::now();
    let admission = target.validate_replication_backlog()?;
    println!(
        "admission_ms={:.3} verify_ms={:.3} valid={} unknown={} invalid={} held={}",
        started.elapsed().as_secs_f64() * 1000.0,
        admission.verify.as_secs_f64() * 1000.0,
        admission.valid,
        admission.unknown,
        admission.invalid,
        admission.held,
    );
    println!(
        "admission_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    target.project_replication_backlog()?;
    let cpu_started = process_time();
    let started = Instant::now();
    target.readmit_envelopes(&origin, &exchange.envelopes)?;
    println!("heal_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
    println!(
        "heal_cpu_ms={:.3}",
        (process_time() - cpu_started).as_secs_f64() * 1000.0
    );
    Ok(())
}

fn process_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a valid writable timespec; this clock counts all this process's threads.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
        0
    );
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}
