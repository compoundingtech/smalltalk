//! Capture generated claim ranges during the existing envelope serialization.
//! Received envelopes still use the bounded CBOR parser; these ranges describe only claims
//! in a locally generated envelope, whose blob records are not seeded by the caller.
use super::Spans;
use crate::claim::{ClaimRecord, ReplicaBatch, ReplicaEnvelopePayload};
use crate::principal::ClaimSignature;
use anyhow::Result;
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    io::{self, Write},
    ops::Range,
};

struct PositionedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    position: &'a Cell<usize>,
}

impl Write for PositionedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        self.position.set(self.bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Claims<'a> {
    records: &'a [ClaimRecord],
    position: &'a Cell<usize>,
    ranges: &'a RefCell<Vec<Range<usize>>>,
}

impl Serialize for Claims<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct PositionedClaim<'a> {
            record: &'a ClaimRecord,
            position: &'a Cell<usize>,
            ranges: &'a RefCell<Vec<Range<usize>>>,
        }
        impl Serialize for PositionedClaim<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let start = self.position.get();
                let result = self.record.serialize(serializer)?;
                self.ranges.borrow_mut().push(start..self.position.get());
                Ok(result)
            }
        }
        let mut sequence = serializer.serialize_seq(Some(self.records.len()))?;
        for record in self.records {
            sequence.serialize_element(&PositionedClaim {
                record,
                position: self.position,
                ranges: self.ranges,
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct Batch<'a> {
    id: &'a String,
    origin: &'a String,
    replica_sequence: u64,
    previous_hash: &'a Option<String>,
    hash: &'a String,
    accepted_at_unix_ms: u128,
    claims: Claims<'a>,
}

#[derive(Serialize)]
struct Payload<'a> {
    batch: Batch<'a>,
    blobs: &'a BTreeMap<String, Vec<u8>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    claim_signatures: &'a BTreeMap<String, ClaimSignature>,
}

pub(in crate::store) fn encode_claim_spans(
    payload: &ReplicaEnvelopePayload,
) -> Result<(Vec<u8>, Spans)> {
    // Exhaustive patterns make new wire fields a compile-time review obligation. The byte
    // parity tests also cover ordering and signature omission against the original types.
    let ReplicaEnvelopePayload {
        batch,
        blobs,
        claim_signatures,
    } = payload;
    let ReplicaBatch {
        id,
        origin,
        replica_sequence,
        previous_hash,
        hash,
        accepted_at_unix_ms,
        claims,
    } = batch;
    let position = Cell::new(0);
    let ranges = RefCell::new(Vec::with_capacity(claims.len()));
    let mut bytes = Vec::new();
    ciborium::into_writer(
        &Payload {
            batch: Batch {
                id,
                origin,
                replica_sequence: *replica_sequence,
                previous_hash,
                hash,
                accepted_at_unix_ms: *accepted_at_unix_ms,
                claims: Claims {
                    records: claims,
                    position: &position,
                    ranges: &ranges,
                },
            },
            blobs,
            claim_signatures,
        },
        PositionedWriter {
            bytes: &mut bytes,
            position: &position,
        },
    )?;
    Ok((
        bytes,
        Spans {
            claims: ranges.into_inner(),
            blobs: BTreeMap::new(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rusqlite::types::Value;

    fn check(count: usize, text: &str, time: u128, signed: bool) {
        let claims = (0..count)
            .map(|n| ClaimRecord {
                id: format!("claim/{n}"),
                store_index: n as u64,
                batch_id: "batch/sample".into(),
                subject: "note/sample".into(),
                kind: "example.note".into(),
                origin: "alder".into(),
                actor: (n % 2 == 0).then(|| "person/sample".into()),
                operation_id: (n % 3 == 0).then(|| format!("operation/{n}")),
                request_digest: (n % 4 == 0).then(|| "digest/sample".into()),
                body: serde_json::json!({"text":text,"nested":[null,true,1.25,{"n":n}]}),
                predecessors: (n != 0)
                    .then(|| format!("claim/{}", n - 1))
                    .into_iter()
                    .collect(),
                accepted_at_unix_ms: time,
            })
            .collect::<Vec<_>>();
        let claim_signatures = if signed {
            BTreeMap::from([(
                "claim/0".into(),
                ClaimSignature {
                    signer: "person/sample".into(),
                    on_behalf: None,
                    key: "key/sample".into(),
                    chain: vec!["grant/sample".into()],
                    nonce: "sample".into(),
                    signed_at_unix_ms: 42,
                    signature: "sample-signature".into(),
                    format: Some("fields-v1".into()),
                    signed_fields: vec!["text".into()],
                },
            )])
        } else {
            BTreeMap::new()
        };
        let payload = ReplicaEnvelopePayload {
            batch: ReplicaBatch {
                id: "batch/sample".into(),
                origin: "alder".into(),
                replica_sequence: 24,
                previous_hash: Some("previous/sample".into()),
                hash: "hash/sample".into(),
                accepted_at_unix_ms: time,
                claims,
            },
            blobs: BTreeMap::from([("blob/sample".into(), vec![0, 23, 24, 255])]),
            claim_signatures,
        };
        let mut original = Vec::new();
        ciborium::into_writer(&payload, &mut original).unwrap();
        let (encoded, spans) = encode_claim_spans(&payload).unwrap();
        assert_eq!(
            encoded, original,
            "signed envelope payload bytes must not change"
        );
        let parsed = Spans::parse(&encoded).unwrap();
        assert_eq!(spans.claims, parsed.claims);
        let bytes = encoded.into();
        for (n, claim) in payload.batch.claims.iter().enumerate() {
            let mut raw = Vec::new();
            ciborium::into_writer(claim, &mut raw).unwrap();
            assert_eq!(
                spans.at(n).unwrap().decode(&bytes).unwrap(),
                Value::Blob(raw)
            );
        }
    }

    #[test]
    fn generated_wire_ranges_and_raw_match_across_container_widths_and_signatures() {
        for count in [0, 1, 24, 40, 256] {
            for signed in [false, true] {
                check(count, "sample 🌳\n\"text\"", u128::MAX - 1, signed);
            }
        }
    }

    proptest! {
        #[test]
        fn varied_generated_claims_keep_exact_wire_and_raw(
            count in 0usize..32, text in "(?s).{0,128}", time in any::<u128>(), signed in any::<bool>(),
        ) {
            check(count, &text, time, signed);
        }
    }
}
