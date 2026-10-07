//! Invented, signed compatibility fixture; no daemon, real store, or checkpoint proof.
use std::{
    collections::BTreeMap,
    io::Write,
    sync::{Arc, Mutex, mpsc},
};

use anyhow::{Result, ensure};
use rusqlite::Transaction;
use serde_json::{Value, json};
use smallclaims::{
    append_group::AppendPolicy,
    claim::{ClaimInput, ClaimRecord, ReplicaEnvelopePayload},
    fleet::MemberKey,
    principal::Verdict,
    replication::ReplicationExchange,
    sqlite::WriterJob,
    store::{CANONICAL_ORDER, Store, append_claim_record_tx, runtime::Plain},
};

fn policy(kind: &str, _: Option<&str>, _: &Value) -> Option<&'static str> {
    (kind == "message.sent").then_some("durable-message-work")
}

fn append(tx: &Transaction<'_>, n: usize, actor: &str) -> Result<ClaimRecord> {
    append_claim_record_tx(
        tx,
        "fixture-example",
        &format!("message/example-{n}"),
        "message.sent",
        Some(actor),
        &json!({"fields":{"body":format!("Example {n}")}}),
        &[],
        None,
    )
}

fn write_new(path: &str, value: &Value) -> Result<usize> {
    let bytes = serde_json::to_vec(value)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(bytes.len())
}

fn run() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("new fixture output required"))?;
    ensure!(std::env::args().count() == 2);
    const FLEET: &str = "6f1e8a52-3c4d-4b7e-9a10-2d5c8e7f9b31";
    let store = Store::open_memory("fixture-example", Arc::new(Plain))?;
    store.set_write_clock_at(1234)?;
    let member = Arc::new(MemberKey::generate()?.0);
    store.bind_fleet(FLEET)?;
    store.pin_fleet_anchor(member.public())?;
    store.set_member_key(Some(member.clone()))?;
    store.append_claim(&ClaimInput {
        subject: "host/fixture-example".into(),
        kind: "fleet.member-admitted".into(),
        actor: None,
        fields: serde_json::from_value(json!({"fleet_id":FLEET,"member_key":member.public(),
            "via":"anchor","mode":"listening"}))?,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    })?;
    for actor in ["agent/example/writer", "person/ada"] {
        store.ensure_principal_key(actor)?;
    }
    let old = store.export_replication_exchange(FLEET, &Default::default())?;
    store.connection.set_shared_appends(true);
    let held = store.connection.write();
    let claims = Arc::new(Mutex::new(Vec::new()));
    let mut replies = Vec::new();
    for n in 0..8 {
        let claims = claims.clone();
        let actor = if n < 4 {
            "agent/example/writer"
        } else {
            "person/ada"
        };
        let (done, reply) = mpsc::sync_channel(1);
        store.connection.send(WriterJob::Batched {
            run: Box::new(move |tx| {
                if n == 7 {
                    append(tx, 999, actor).expect("partial failing append");
                    return false;
                }
                claims
                    .lock()
                    .unwrap()
                    .push(append(tx, n, actor).expect("fixture append"));
                true
            }),
            append_policy: Some(policy as AppendPolicy),
            profile: None,
            wait: None,
            done,
        });
        replies.push(reply);
    }
    drop(held);
    // The next FIFO loan is granted after the existing real COMMIT and job ACK machinery.
    drop(store.connection.write());
    for reply in replies {
        reply.recv()?.map_err(anyhow::Error::msg)?;
    }
    store.seal_local_batches()?;
    store.judge_claims(true)?;
    let claims = claims.lock().unwrap();
    ensure!(claims.len() == 7);
    ensure!(claims[1].batch_id == claims[3].batch_id && claims[4].batch_id == claims[6].batch_id);
    ensure!(claims[3].batch_id != claims[4].batch_id);
    for claim in claims.iter() {
        ensure!(store.claim_verdict(&claim.id)? == Verdict::Verified);
    }
    ensure!(store.claims_for("message/example-999", None)?.is_empty());
    let exchange = store.export_replication_exchange_answering(FLEET, &Default::default(), &[])?;
    for envelope in &old.envelopes {
        let surviving = exchange
            .envelopes
            .iter()
            .find(|candidate| {
                candidate.writer == envelope.writer
                    && candidate.sequence == envelope.sequence
                    && candidate.hash == envelope.hash
            })
            .ok_or_else(|| anyhow::anyhow!("old envelope missing"))?;
        ensure!(
            serde_json::to_value(envelope)? == serde_json::to_value(surviving)?,
            "old bytes/signatures changed"
        );
    }
    let connection = store.connection.write();
    let expected_canonical_ids = connection
        .prepare(&format!("SELECT id FROM claims ORDER BY {CANONICAL_ORDER}"))?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(connection);
    let mut groups = BTreeMap::new();
    for envelope in &exchange.envelopes {
        let payload: ReplicaEnvelopePayload = ciborium::from_reader(envelope.payload.bytes()?)?;
        if payload.batch.claims.len() > 1 {
            groups.insert(payload.batch.id, payload.batch.claims.len());
        }
    }
    ensure!(
        groups.len() == 2,
        "expected actual signed agent and person grouping"
    );
    let fixture = json!({"fleet_id":FLEET,"anchor":member.public(),"exchange":exchange,
        "expected_claims":&*claims,"expected_canonical_ids":expected_canonical_ids,
        "absent_subject":"message/example-999"});
    let bytes = write_new(&path, &fixture)?;
    let mut negative_paths = Vec::new();
    for mode in ["tampered-envelope-signature", "tampered-grouped-payload"] {
        let mut negative = fixture.clone();
        let mut exchange: ReplicationExchange =
            serde_json::from_value(negative["exchange"].clone())?;
        let mut target = None;
        for (index, envelope) in exchange.envelopes.iter().enumerate() {
            let payload: ReplicaEnvelopePayload = ciborium::from_reader(envelope.payload.bytes()?)?;
            if payload.batch.claims.len() > 1 {
                target = Some((index, payload));
                break;
            }
        }
        let (index, mut payload) =
            target.ok_or_else(|| anyhow::anyhow!("negative grouped target missing"))?;
        let envelope = &mut exchange.envelopes[index];
        negative["expected_rejection"] = json!({"mode":mode,
            "envelope":{"writer":envelope.writer,"sequence":envelope.sequence,"hash":envelope.hash},
            "claim_ids":payload.batch.claims.iter().map(|c| &c.id).collect::<Vec<_>>()});
        if mode == "tampered-envelope-signature" {
            envelope.signature = Some("invalid-example-signature".into());
        } else {
            payload.batch.claims[0].body["fields"]["body"] = json!("tampered example payload");
            let mut raw = Vec::new();
            ciborium::into_writer(&payload, &mut raw)?;
            // Preserve the original signed envelope hash. The previous verifier must reject
            // these altered CBOR bytes specifically, rather than accepting a new identity.
            envelope.payload = raw.into();
        }
        negative["exchange"] = serde_json::to_value(exchange)?;
        let negative_path = format!("{path}.{mode}.json");
        write_new(&negative_path, &negative)?;
        negative_paths.push(negative_path);
    }
    println!(
        "{}",
        json!({"passed":true,"fixture":path,"bytes":bytes,
        "negative_fixtures":negative_paths,
        "claims":claims.len(),"groups":groups,"proof_scope":"fixture-generation-only"})
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fixture generation failed: {error:#}");
        std::process::exit(1);
    }
}
