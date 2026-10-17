//! The logical backup archive format (`crate::backup`): its version, records and digests.
use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{ReplicaEnvelope, ReplicaEnvelopeSignature};
use super::CheckpointManifestPage;

pub const FORMAT: &str = "st3.claim-backup";
pub const VERSION: u32 = 1;
/// The version of an archive that also carries message bodies. An archive without any keeps
/// [`VERSION`], so a build that predates bodies restores it; one with bodies it refuses cleanly.
pub const VERSION_WITH_BODIES: u32 = 2;
pub const PAGE: usize = 512;
// One envelope can contain document bytes; reject unbounded lines before allocating them.
pub const MAX_RECORD_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TableDigest {
    pub digest: String,
    pub count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Header {
    pub format: String,
    pub version: u32,
    pub source_build: String,
    pub source_schema: u32,
    pub registry_digest: String,
    pub fleet_id: Option<String>,
    pub fleet_anchor: Option<String>,
    pub graph_digest: String,
    pub log_digest: String,
    pub tables: BTreeMap<String, TableDigest>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "record", content = "data", rename_all = "kebab-case")]
pub enum Record {
    Header(Header),
    Envelope(ReplicaEnvelope),
    Checkpoint(CheckpointManifestPage),
    Signatures(Vec<ReplicaEnvelopeSignature>),
    /// A long message body the exporting member holds as its owner, with the hash it is kept by.
    /// Restore writes it back beside the database. See [`crate::message_body`].
    MessageBody(MessageBodyRecord),
    End { envelopes: u64, sha256: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MessageBodyRecord {
    /// The message whose text this is, and the sender whose store held it.
    pub message: String,
    pub actor: String,
    pub sha256: String,
    /// The text's bytes, base64.
    pub data: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RestoreReport {
    pub writer: String,
    pub envelopes: u64,
    pub graph_digest: String,
    pub source_graph_digest: String,
    pub log_digest: String,
    pub projections_match: bool,
    pub tables: BTreeMap<String, TableDigest>,
    /// Long message bodies the archive held and the restored claims name, verified by hash.
    #[serde(default)]
    pub message_bodies_restored: u64,
    /// Bodies in the archive that no restored claim names with that hash; not restored.
    #[serde(default)]
    pub message_bodies_unmatched: u64,
    /// Long messages the restored claims name, including those whose text another member owns.
    #[serde(default)]
    pub message_bodies_referenced: u64,
}

pub fn write_record(
    output: &mut impl Write,
    hash: &mut Sha256,
    record: &Record,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "backup record exceeds the format size limit"
    );
    output.write_all(&bytes)?;
    hash.update(&bytes);
    Ok(())
}
