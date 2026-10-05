//! Record pointers preserve the original logical raw value, including its SQLite type.
use super::*;
use crate::claim::{ClaimRecord, EnvelopePayload};
use anyhow::{anyhow, bail, ensure};
use rusqlite::types::Value;
use std::{
    collections::BTreeMap,
    ops::Range,
    time::{Duration, Instant},
};

const CURSOR: &str = "replica_record_offset_cursor";
const PAGE: usize = 64;
const READ_BYTES: usize = 4 * 1024 * 1024;
const BUDGET: Duration = Duration::from_millis(50);

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let columns = connection
        .prepare("PRAGMA table_info(replica_records)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for name in ["raw_offset", "raw_length", "raw_mode"] {
        if !columns.iter().any(|column| column == name) {
            let kind = if name == "raw_mode" {
                "TEXT"
            } else {
                "INTEGER"
            };
            connection.execute_batch(&format!(
                "ALTER TABLE replica_records ADD COLUMN {name} {kind};"
            ))?;
        }
    }
    Ok(())
}

// This codec is a stored representation, not the evolving claim model. Keep its field
// order, defaults, and omissions unchanged: legacy raw bytes were serialized this way.
#[derive(serde::Deserialize, serde::Serialize)]
struct RawClaimV0 {
    id: String,
    store_index: u64,
    batch_id: String,
    subject: String,
    kind: String,
    origin: String,
    actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_digest: Option<String>,
    body: serde_json::Value,
    predecessors: Vec<String>,
    accepted_at_unix_ms: u128,
}

#[derive(Clone, Debug)]
struct Pointer {
    offset: usize,
    length: usize,
    mode: &'static str,
}
impl Pointer {
    fn range(range: Range<usize>, mode: &'static str) -> Self {
        Self {
            offset: range.start,
            length: range.end - range.start,
            mode,
        }
    }
    fn decode(&self, payload: &EnvelopePayload) -> Result<Value> {
        let bytes = payload.bytes()?;
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or_else(|| anyhow!("record range overflow"))?;
        let bytes = bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("record range outside envelope"))?;
        Ok(match self.mode {
            "claim-v0" => {
                let claim: RawClaimV0 = ciborium::from_reader(bytes)?;
                let mut raw = Vec::new();
                ciborium::into_writer(&claim, &mut raw)?;
                Value::Blob(raw)
            }
            "blob" => Value::Blob(ciborium::from_reader::<Vec<u8>, _>(bytes)?),
            "base64" => Value::Text(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                bytes,
            )),
            _ => bail!("unknown record representation {}", self.mode),
        })
    }
}

#[derive(Default)]
pub(super) struct Spans {
    claims: Vec<Range<usize>>,
    blobs: BTreeMap<String, Range<usize>>,
}
impl Spans {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Cbor { bytes, at: 0 };
        let mut count = reader.container(5)?;
        let mut spans = Self::default();
        while !reader.done(&mut count)? {
            match reader.text()?.as_str() {
                "batch" => {
                    let mut fields = reader.container(5)?;
                    while !reader.done(&mut fields)? {
                        if reader.text()? == "claims" {
                            let mut claims = reader.container(4)?;
                            while !reader.done(&mut claims)? {
                                spans.claims.push(reader.item()?);
                            }
                        } else {
                            reader.item()?;
                        }
                    }
                }
                "blobs" => {
                    let mut blobs = reader.container(5)?;
                    while !reader.done(&mut blobs)? {
                        let hash = reader.text()?;
                        spans.blobs.insert(hash, reader.item()?);
                    }
                }
                _ => {
                    reader.item()?;
                }
            }
        }
        ensure!(reader.at == bytes.len(), "trailing envelope CBOR");
        Ok(spans)
    }
    fn at(&self, position: usize) -> Option<Pointer> {
        if let Some(range) = self.claims.get(position) {
            Some(Pointer::range(range.clone(), "claim-v0"))
        } else {
            self.blobs
                .values()
                .nth(position.checked_sub(self.claims.len())?)
                .map(|range| Pointer::range(range.clone(), "blob"))
        }
    }
}

// Skip nested values without materializing the envelope, its blobs, or unknown fields.
struct Cbor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Cbor<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| anyhow!("CBOR length overflow"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| anyhow!("truncated CBOR"))?;
        self.at = end;
        Ok(bytes)
    }
    fn head(&mut self) -> Result<(u8, Option<u64>)> {
        let byte = self.take(1)?[0];
        let value = match byte & 31 {
            n @ 0..=23 => Some(n as u64),
            n @ 24..=27 => {
                let bytes = self.take(1 << (n - 24))?;
                Some(bytes.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)))
            }
            31 => None,
            _ => bail!("reserved CBOR header"),
        };
        Ok((byte >> 5, value))
    }
    fn container(&mut self, expected: u8) -> Result<Option<u64>> {
        let (major, count) = self.head()?;
        ensure!(major == expected, "unexpected CBOR container");
        Ok(count)
    }
    fn done(&mut self, count: &mut Option<u64>) -> Result<bool> {
        if let Some(count) = count {
            if *count == 0 {
                return Ok(true);
            }
            *count -= 1;
        } else if self.bytes.get(self.at) == Some(&255) {
            self.at += 1;
            return Ok(true);
        } else {
            ensure!(self.at < self.bytes.len(), "unterminated CBOR container");
        }
        Ok(false)
    }
    fn item(&mut self) -> Result<Range<usize>> {
        let start = self.at;
        self.skip(0)?;
        Ok(start..self.at)
    }
    fn text(&mut self) -> Result<String> {
        let range = self.item()?;
        Ok(ciborium::from_reader(&self.bytes[range])?)
    }
    fn skip(&mut self, depth: usize) -> Result<()> {
        ensure!(depth < 256, "CBOR nesting limit");
        let (major, mut count) = self.head()?;
        match major {
            0 | 1 => ensure!(count.is_some(), "indefinite CBOR integer"),
            2 | 3 => {
                if let Some(length) = count {
                    self.take(length.try_into()?)?;
                } else {
                    while !self.done(&mut count)? {
                        let (chunk_major, length) = self.head()?;
                        ensure!(chunk_major == major, "invalid CBOR string chunk");
                        self.take(
                            length
                                .ok_or_else(|| anyhow!("nested indefinite string"))?
                                .try_into()?,
                        )?;
                    }
                }
            }
            4 | 5 => {
                while !self.done(&mut count)? {
                    self.skip(depth + 1)?;
                    if major == 5 {
                        self.skip(depth + 1)?;
                    }
                }
            }
            6 => {
                ensure!(count.is_some(), "indefinite CBOR tag");
                self.skip(depth + 1)?;
            }
            7 => ensure!(count.is_some(), "unexpected CBOR break"),
            _ => unreachable!(),
        }
        Ok(())
    }
}

pub(super) struct RecordRaw {
    pub value: Value,
    pub offset: Option<i64>,
    pub length: Option<i64>,
    pub mode: Option<&'static str>,
}
impl RecordRaw {
    fn pointer(pointer: Pointer) -> Self {
        Self {
            value: Value::Blob(Vec::new()),
            offset: Some(pointer.offset as i64),
            length: Some(pointer.length as i64),
            mode: Some(pointer.mode),
        }
    }
    fn inline(value: Value) -> Self {
        Self {
            value,
            offset: None,
            length: None,
            mode: None,
        }
    }
    pub(super) fn claim(
        spans: Option<&Spans>,
        position: usize,
        claim: &ClaimRecord,
    ) -> Result<Self> {
        if let Some(pointer) = spans
            .and_then(|s| s.at(position))
            .filter(|p| p.mode == "claim-v0")
        {
            return Ok(Self::pointer(pointer));
        }
        let mut raw = Vec::new();
        ciborium::into_writer(claim, &mut raw)?;
        Ok(Self::inline(Value::Blob(raw)))
    }
    pub(super) fn blob(spans: Option<&Spans>, position: usize, bytes: &[u8]) -> Self {
        if let Some(pointer) = spans
            .and_then(|s| s.at(position))
            .filter(|p| p.mode == "blob")
        {
            return Self::pointer(pointer);
        }
        Self::inline(Value::Blob(bytes.to_vec()))
    }
    pub(super) fn invalid(connection: &Connection, envelope: &ReplicaEnvelope) -> Result<Self> {
        let verified = envelope.payload.bytes().is_ok_and(|bytes| {
            replica_envelope_hash(
                &envelope.writer,
                envelope.sequence,
                envelope.previous_hash.as_deref(),
                envelope.accepted_at_unix_ms,
                bytes,
            ) == envelope.hash
        });
        if verified
            && matches_payload(
                connection,
                &envelope.writer,
                envelope.sequence,
                &envelope.hash,
                &envelope.payload,
            )?
        {
            return Ok(Self::pointer(
                whole(&envelope.payload).expect("verified payload has bytes"),
            ));
        }
        Ok(Self::inline(Value::Text(envelope.payload.base64())))
    }
}

fn whole(payload: &EnvelopePayload) -> Option<Pointer> {
    payload.bytes().ok().map(|bytes| Pointer {
        offset: 0,
        length: bytes.len(),
        mode: "base64",
    })
}

// Healing may replace an unverified stored payload with the bytes its identity commits to.
// Such forensic values remain inline; only hash-verified bytes can back an immutable pointer.
struct StoredPayload {
    payload: EnvelopePayload,
    previous_hash: Option<String>,
    accepted_at: Option<u128>,
}
impl StoredPayload {
    fn verified(&self, key: &(String, u64, String)) -> bool {
        self.accepted_at
            .zip(self.payload.bytes().ok())
            .is_some_and(|(at, bytes)| {
                replica_envelope_hash(&key.0, key.1, self.previous_hash.as_deref(), at, bytes)
                    == key.2
            })
    }
}

pub(super) fn matches_payload(
    connection: &Connection,
    writer: &str,
    sequence: u64,
    hash: &str,
    payload: &impl rusqlite::ToSql,
) -> Result<bool> {
    Ok(connection.prepare_cached(
        "SELECT payload=?4 FROM replica_envelopes WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3"
    )?.query_row(params![writer, sequence, hash, payload], |r| r.get(0)).optional()?.unwrap_or(false))
}

#[derive(Debug, Default)]
pub struct RecordOffsetConversion {
    pub scanned: usize,
    pub converted: usize,
    pub retained: usize,
    pub done: bool,
    /// Time waiting for and executing the writer transaction, including its commit.
    pub queue_ms: f64,
}
struct LegacyRecord {
    rowid: i64,
    position: usize,
    raw: Value,
    pointed: bool,
    envelope: (String, u64, String),
}
struct PlannedRecord {
    rowid: i64,
    pointer: Option<Pointer>,
    retained: bool,
}
impl Store {
    /// Read and validate representations away from the writer queue. Only the bounded
    /// pointer updates and their resumable cursor share the daemon's normal write queue.
    pub fn convert_record_offsets(&self) -> Result<RecordOffsetConversion> {
        let (cursor, rows, mut payloads) = self.read_snapshot(|_| {
            let connection = self.readers.get();
            let cursor: Option<String> = connection.query_row(
                "SELECT value FROM meta WHERE key=?1", [CURSOR], |r| r.get(0)
            ).optional()?;
            if cursor.as_deref() == Some("done") { return Ok((cursor, Vec::new(), BTreeMap::new())); }
            let after: i64 = cursor.as_deref().unwrap_or("0").parse()?;
            let mut statement = connection.prepare_cached(
                "SELECT rowid,writer,sequence,envelope_hash,position,raw,raw_mode IS NOT NULL FROM replica_records
                 WHERE rowid>?1 ORDER BY rowid LIMIT ?2"
            )?;
            let mut source = statement.query(params![after, PAGE])?;
            let mut rows = Vec::new();
            let mut payloads = BTreeMap::new();
            let mut total_bytes = 0;
            let start = Instant::now();
            while let Some(row) = source.next()? {
                let envelope = (row.get::<_,String>(1)?, row.get::<_,u64>(2)?, row.get::<_,String>(3)?);
                let raw: Value = row.get(5)?;
                total_bytes += match &raw { Value::Blob(bytes) => bytes.len(), Value::Text(text) => text.len(), _ => 0 };
                let pointed: bool = row.get(6)?;
                if !pointed && let std::collections::btree_map::Entry::Vacant(entry) = payloads.entry(envelope.clone()) {
                    let payload: Option<StoredPayload> = connection.prepare_cached(
                        "SELECT payload,previous_hash,accepted_at_unix_ms FROM replica_envelopes WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3"
                    )?.query_row(params![envelope.0,envelope.1,envelope.2], |r| Ok(StoredPayload {
                        payload:r.get(0)?, previous_hash:r.get(1)?, accepted_at:r.get::<_,String>(2)?.parse().ok(),
                    })).optional()?;
                    total_bytes += payload.as_ref().map(|p| p.payload.bytes().map(|b| b.len()).unwrap_or(0)).unwrap_or(0);
                    entry.insert(payload);
                }
                rows.push(LegacyRecord { rowid: row.get(0)?, position: row.get(4)?, raw, pointed, envelope });
                if total_bytes >= READ_BYTES || start.elapsed() >= BUDGET { break; }
            }
            Ok((cursor, rows, payloads))
        })?;
        if cursor.as_deref() == Some("done") {
            return Ok(RecordOffsetConversion {
                done: true,
                ..Default::default()
            });
        }
        for (key, payload) in &mut payloads {
            if payload.as_ref().is_some_and(|p| !p.verified(key)) {
                *payload = None;
            }
        }
        let mut spans = BTreeMap::new();
        for (key, payload) in &payloads {
            let parsed = payload
                .as_ref()
                .and_then(|p| p.payload.bytes().ok())
                .and_then(|bytes| Spans::parse(bytes).ok());
            spans.insert(key.clone(), parsed);
        }
        let plans = rows
            .into_iter()
            .map(|record| {
                let pointer = payloads
                    .get(&record.envelope)
                    .and_then(Option::as_ref)
                    .and_then(|payload| {
                        let candidate = match &record.raw {
                            Value::Text(_) => whole(&payload.payload),
                            Value::Blob(_) => spans
                                .get(&record.envelope)
                                .and_then(Option::as_ref)
                                .and_then(|s| s.at(record.position)),
                            _ => None,
                        };
                        candidate.filter(|p| {
                            p.decode(&payload.payload).ok().as_ref() == Some(&record.raw)
                        })
                    });
                let retained = !record.pointed && pointer.is_none();
                PlannedRecord {
                    rowid: record.rowid,
                    pointer,
                    retained,
                }
            })
            .collect::<Vec<_>>();
        let queued = Instant::now();
        let mut report = self.connection.batched(|tx| -> Result<RecordOffsetConversion> {
            let current: Option<String> = tx.query_row("SELECT value FROM meta WHERE key=?1", [CURSOR], |r| r.get(0)).optional()?;
            if current != cursor { return Ok(RecordOffsetConversion::default()); }
            let mut report = RecordOffsetConversion { done: plans.is_empty(), ..Default::default() };
            let start = Instant::now();
            let mut after = cursor.as_deref().unwrap_or("0").parse::<i64>()?;
            for record in plans {
                if let Some(pointer) = record.pointer {
                    report.converted += tx.prepare_cached(
                        "UPDATE replica_records SET raw=X'',raw_offset=?2,raw_length=?3,raw_mode=?4
                         WHERE rowid=?1 AND raw_mode IS NULL"
                    )?.execute(params![record.rowid,pointer.offset as i64,pointer.length as i64,pointer.mode])?;
                } else if record.retained { report.retained += 1; }
                report.scanned += 1;
                after = record.rowid;
                if start.elapsed() >= BUDGET { break; }
            }
            tx.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![CURSOR, if report.done { "done".to_string() } else { after.to_string() }],
            )?;
            Ok(report)
        }).map_err(anyhow::Error::msg)??;
        report.queue_ms = queued.elapsed().as_secs_f64() * 1000.0;
        Ok(report)
    }

    /// The original raw value of a record. Its type and bytes remain unchanged by conversion.
    pub fn replica_record_raw(&self, record_ref: &str) -> Result<Option<Value>> {
        let row = {
            let connection = self.readers.get();
            connection
                .prepare_cached(
                    "SELECT records.raw,records.raw_offset,records.raw_length,records.raw_mode,
                        CASE WHEN records.raw_mode IS NULL THEN NULL ELSE envelopes.payload END
                 FROM replica_records records LEFT JOIN replica_envelopes envelopes
                 ON envelopes.writer=records.writer AND envelopes.sequence=records.sequence
                    AND envelopes.envelope_hash=records.envelope_hash WHERE records.record_ref=?1",
                )?
                .query_row([record_ref], |row| {
                    Ok((
                        row.get::<_, Value>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<EnvelopePayload>>(4)?,
                    ))
                })
                .optional()?
        };
        let Some((raw, offset, length, mode, payload)) = row else {
            return Ok(None);
        };
        let Some(mode) = mode else {
            return Ok(Some(raw));
        };
        let mode = match mode.as_str() {
            "claim-v0" => "claim-v0",
            "blob" => "blob",
            "base64" => "base64",
            _ => bail!("unknown record raw mode"),
        };
        let pointer = Pointer {
            offset: offset
                .ok_or_else(|| anyhow!("missing record offset"))?
                .try_into()?,
            length: length
                .ok_or_else(|| anyhow!("missing record length"))?
                .try_into()?,
            mode,
        };
        Ok(Some(pointer.decode(&payload.ok_or_else(|| {
            anyhow!("record's envelope is missing")
        })?)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn frozen_claim_codec_preserves_legacy_raw_with_reordered_unknown_wire_fields() {
        let claim = ClaimRecord {
            id: "claim/sample".into(),
            store_index: 7,
            batch_id: "batch/sample".into(),
            subject: "note/sample".into(),
            kind: "example.note".into(),
            origin: "alder".into(),
            actor: None,
            operation_id: Some("operation/sample".into()),
            request_digest: None,
            body: serde_json::json!({"number":1.5,"text":"sample"}),
            predecessors: vec![],
            accepted_at_unix_ms: 42,
        };
        let mut original = Vec::new();
        ciborium::into_writer(&claim, &mut original).unwrap();
        let mut value: ciborium::Value = ciborium::from_reader(original.as_slice()).unwrap();
        let fields = value.as_map_mut().unwrap();
        fields.reverse();
        fields.push((
            "future".into(),
            ciborium::Value::Tag(42, Box::new("sample".into())),
        ));
        let mut wire = Vec::new();
        ciborium::into_writer(&value, &mut wire).unwrap();
        assert_ne!(wire, original);
        let pointer = Pointer {
            offset: 0,
            length: wire.len(),
            mode: "claim-v0",
        };
        assert_eq!(pointer.decode(&wire.into()).unwrap(), Value::Blob(original));
    }

    #[test]
    fn indefinite_containers_chunks_and_unknown_values_keep_exact_spans() {
        // Indefinite root, batch, and claims; the claim itself is a scalar for span testing.
        let mut wire = vec![0xbf];
        ciborium::into_writer(&"batch", &mut wire).unwrap();
        wire.push(0xbf);
        ciborium::into_writer(&"claims", &mut wire).unwrap();
        wire.extend([0x9f, 0x18, 0x2a, 0xff, 0xff]);
        ciborium::into_writer(&"blobs", &mut wire).unwrap();
        wire.push(0xbf);
        ciborium::into_writer(&"hash", &mut wire).unwrap();
        wire.extend([0x9f, 0, 0x18, 255, 0xff, 0xff]);
        ciborium::into_writer(&"future", &mut wire).unwrap();
        wire.extend([0xc1, 0x7f, 0x61, b'a', 0x61, b'b', 0xff, 0xff]);
        let spans = Spans::parse(&wire).unwrap();
        assert_eq!(&wire[spans.claims[0].clone()], &[0x18, 0x2a]);
        assert_eq!(
            spans.at(1).unwrap().decode(&wire.into()).unwrap(),
            Value::Blob(vec![0, 255])
        );
    }

    proptest! {
        #[test]
        fn arbitrary_payloads_are_bounded_and_never_panic(bytes in prop::collection::vec(any::<u8>(),0..1024)) {
            let _ = Spans::parse(&bytes);
        }
    }
}
