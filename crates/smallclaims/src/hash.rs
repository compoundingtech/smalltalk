//! Content hashes: canonical JSON, claim IDs, batch headers, and replica envelopes.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::claim::{ClaimRecord, ReplicaBatch};
use crate::error::{Error, internal};

pub fn canonical_hash(value: &impl Serialize) -> Result<String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn batch_header_hash(
    origin: &str,
    sequence: u64,
    previous_hash: Option<&str>,
    accepted_at_unix_ms: u128,
) -> Result<String> {
    canonical_hash(&(
        "st3.replica-batch.v1",
        origin,
        sequence,
        previous_hash,
        accepted_at_unix_ms.to_string(),
    ))
}

pub fn claim_hash(
    batch_id: &str,
    subject: &str,
    kind: &str,
    origin: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
) -> Result<String> {
    let body = canonical_json_value(body);
    canonical_hash(&(batch_id, subject, kind, origin, actor, body, predecessors))
}

/// Whether a replicated claim's ID is the hash of its content. On 2026-09-16, builds between
/// eaec66a and 2537978d hashed each body in field insertion order: a new dependency switched
/// serde_json to `preserve_order` before claim hashes sorted object keys. Those claims are
/// genuine, so a node that verifies them later accepts that hash too. Bodies still decode with
/// `preserve_order`, so the writer's field order survives to reproduce it.
pub fn claim_id_is_content_hash(claim: &ClaimRecord) -> Result<bool> {
    let canonical = claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )?;
    if canonical == claim.id {
        return Ok(true);
    }
    let insertion_order = canonical_hash(&(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    ))?;
    Ok(insertion_order == claim.id)
}

pub fn canonical_json_value(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json_value).collect()),
        Value::Object(fields) => {
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(left, _)| *left);
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json_value(value)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

pub fn canonical_json_text(value: &Value) -> Result<String> {
    Ok(serde_json::to_string(&canonical_json_value(value))?)
}

pub fn canonical_serialized_json_text(value: &impl Serialize) -> Result<String> {
    canonical_json_text(&serde_json::to_value(value)?)
}

pub fn replica_envelope_hash(
    writer: &str,
    sequence: u64,
    previous_hash: Option<&str>,
    accepted_at_unix_ms: u128,
    payload: &[u8],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-replica-envelope-v1\0");
    for field in [
        writer.to_owned(),
        sequence.to_string(),
        previous_hash.unwrap_or_default().to_owned(),
        accepted_at_unix_ms.to_string(),
        hex::encode(Sha256::digest(payload)),
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    hex::encode(digest.finalize())
}

pub fn replica_record_ref(
    writer: &str,
    sequence: u64,
    envelope_hash: &str,
    position: u64,
) -> String {
    let digest = Sha256::digest(
        format!("st3-replica-record-v1\0{writer}\0{sequence}\0{envelope_hash}\0{position}")
            .as_bytes(),
    );
    format!("record/{}", hex::encode(digest))
}

pub fn verify_replica_batch_header(batch: &ReplicaBatch) -> Result<(), Error> {
    let expected_batch = batch_header_hash(
        &batch.origin,
        batch.replica_sequence,
        batch.previous_hash.as_deref(),
        batch.accepted_at_unix_ms,
    )
    .map_err(internal)?;
    if expected_batch != batch.hash
        || batch.id
            != format!(
                "batch/{}/{}/{}",
                batch.origin, batch.replica_sequence, batch.hash
            )
    {
        return Err(Error::new(
            "batch-hash-mismatch",
            format!("replicated batch `{}` failed verification", batch.id),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_hash_does_not_depend_on_json_object_insertion_order() {
        let mut left_fields = serde_json::Map::new();
        left_fields.insert("status".into(), Value::String("running".into()));
        left_fields.insert("pid".into(), Value::Number(42.into()));
        let mut left = serde_json::Map::new();
        left.insert("fields".into(), Value::Object(left_fields));
        left.insert("evidence".into(), Value::Array(Vec::new()));

        let mut right_fields = serde_json::Map::new();
        right_fields.insert("pid".into(), Value::Number(42.into()));
        right_fields.insert("status".into(), Value::String("running".into()));
        let mut right = serde_json::Map::new();
        right.insert("evidence".into(), Value::Array(Vec::new()));
        right.insert("fields".into(), Value::Object(right_fields));

        let left = claim_hash(
            "batch/node/1/hash",
            "daemon/node",
            "daemon.started",
            "node",
            None,
            &Value::Object(left),
            &[],
        )
        .expect("left claim hash");
        let right = claim_hash(
            "batch/node/1/hash",
            "daemon/node",
            "daemon.started",
            "node",
            None,
            &Value::Object(right),
            &[],
        )
        .expect("right claim hash");

        assert_eq!(left, right);
    }
}
