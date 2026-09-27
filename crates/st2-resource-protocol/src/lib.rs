//! Resource Profile runtime wire types, framing, and codecs.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const MAX_PROTOCOL_LINE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
pub const MAX_SELECTOR_BYTES: usize = 16 * 1024;
pub const MAX_HEALTH_DETAIL_BYTES: usize = 16 * 1024;
pub const MAX_OBSERVATION_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const MAX_OPAQUE_ID_BYTES: usize = 16 * 1024;

/// Fact bounds are deliberately small relative to the 2 MiB frame: even 32 facts whose strings
/// all require JSON escaping leave ample room beside a maximal base64-encoded 1 MiB snapshot.
pub const MAX_FACTS: usize = 32;
pub const MAX_FACT_KEY_BYTES: usize = 128;
pub const MAX_FACT_VALUE_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FactValue {
    #[default]
    Omitted,
    Null,
    Value(String),
}

impl FactValue {
    pub fn value(value: impl Into<String>) -> Self {
        Self::Value(value.into())
    }

    pub fn as_option(&self) -> Option<Option<&str>> {
        match self {
            Self::Omitted => None,
            Self::Null => Some(None),
            Self::Value(value) => Some(Some(value)),
        }
    }

    fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

impl Serialize for FactValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Omitted => serializer.serialize_unit(),
            Self::Null => serializer.serialize_none(),
            Self::Value(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for FactValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)
            .map(|value| value.map_or(Self::Null, Self::Value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceFact {
    key: String,
    #[serde(default, skip_serializing_if = "FactValue::is_omitted")]
    before: FactValue,
    #[serde(default, skip_serializing_if = "FactValue::is_omitted")]
    after: FactValue,
}

impl ResourceFact {
    pub fn new(
        key: impl Into<String>,
        before: FactValue,
        after: FactValue,
    ) -> Result<Self, FactError> {
        let fact = Self {
            key: key.into(),
            before,
            after,
        };
        fact.validate()?;
        Ok(fact)
    }

    pub fn current(key: impl Into<String>, value: impl Into<String>) -> Result<Self, FactError> {
        Self::new(key, FactValue::Omitted, FactValue::value(value))
    }

    pub fn transition(
        key: impl Into<String>,
        before: Option<impl Into<String>>,
        after: Option<impl Into<String>>,
    ) -> Result<Self, FactError> {
        Self::new(
            key,
            before.map_or(FactValue::Null, |value| FactValue::value(value)),
            after.map_or(FactValue::Null, |value| FactValue::value(value)),
        )
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn before(&self) -> Option<Option<&str>> {
        self.before.as_option()
    }

    pub fn after(&self) -> Option<Option<&str>> {
        self.after.as_option()
    }

    pub fn validate(&self) -> Result<(), FactError> {
        validate_fact_string("key", &self.key, MAX_FACT_KEY_BYTES, true)?;
        if self.before.is_omitted() && self.after.is_omitted() {
            return Err(FactError::MissingValue);
        }
        for (field, value) in [("before", &self.before), ("after", &self.after)] {
            if let FactValue::Value(value) = value {
                validate_fact_string(field, value, MAX_FACT_VALUE_BYTES, false)?;
            }
        }
        Ok(())
    }
}

fn validate_fact_string(
    field: &'static str,
    value: &str,
    maximum: usize,
    nonempty: bool,
) -> Result<(), FactError> {
    if nonempty && value.is_empty() {
        return Err(FactError::Empty { field });
    }
    if value.len() > maximum {
        return Err(FactError::TooLarge {
            field,
            actual: value.len(),
            maximum,
        });
    }
    if value
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(FactError::NotPrintable { field });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactError {
    TooMany {
        actual: usize,
    },
    Empty {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    NotPrintable {
        field: &'static str,
    },
    MissingValue,
}

impl fmt::Display for FactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany { actual } => {
                write!(
                    formatter,
                    "fact list has {actual} entries; maximum is {MAX_FACTS}"
                )
            }
            Self::Empty { field } => write!(formatter, "fact {field} must not be empty"),
            Self::TooLarge {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "fact {field} is {actual} bytes; maximum is {maximum}"
            ),
            Self::NotPrintable { field } => {
                write!(formatter, "fact {field} must be one printable line")
            }
            Self::MissingValue => formatter.write_str("fact must include before, after, or both"),
        }
    }
}

impl std::error::Error for FactError {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueIdError {
    kind: &'static str,
    reason: &'static str,
}

impl fmt::Display for OpaqueIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.kind, self.reason)
    }
}

impl std::error::Error for OpaqueIdError {}

macro_rules! opaque_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, OpaqueIdError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(OpaqueIdError {
                        kind: $kind,
                        reason: "must not be empty",
                    });
                }
                if value.len() > MAX_OPAQUE_ID_BYTES {
                    return Err(OpaqueIdError {
                        kind: $kind,
                        reason: "is too large",
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

opaque_id!(RuntimeIncarnation, "runtime incarnation");
opaque_id!(OwnerClaim, "owner claim");
opaque_id!(BindingId, "binding id");
opaque_id!(RegistrationToken, "registration token");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeOwner {
    incarnation: RuntimeIncarnation,
    claim: OwnerClaim,
}

impl RuntimeOwner {
    pub fn new(incarnation: RuntimeIncarnation, claim: OwnerClaim) -> Self {
        Self { incarnation, claim }
    }

    pub fn incarnation(&self) -> &RuntimeIncarnation {
        &self.incarnation
    }

    pub fn claim(&self) -> &OwnerClaim {
        &self.claim
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SnapshotBytes(Vec<u8>);

impl fmt::Debug for SnapshotBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotBytes")
            .field("len", &self.0.len())
            .finish()
    }
}

impl SnapshotBytes {
    pub fn new(bytes: Vec<u8>) -> Result<Self, SnapshotSizeError> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(SnapshotSizeError {
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSizeError {
    pub actual: usize,
}

impl fmt::Display for SnapshotSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "decoded snapshot is {} bytes; maximum is {MAX_SNAPSHOT_BYTES}",
            self.actual
        )
    }
}

impl std::error::Error for SnapshotSizeError {}

impl Serialize for SnapshotBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_base64(&self.0))
    }
}

impl<'de> Deserialize<'de> for SnapshotBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SnapshotBytesVisitor;

        impl Visitor<'_> for SnapshotBytesVisitor {
            type Value = SnapshotBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an RFC 4648 padded base64 snapshot")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let bytes = decode_base64(value).map_err(E::custom)?;
                SnapshotBytes::new(bytes).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(SnapshotBytesVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Base64Error(&'static str);

impl fmt::Display for Base64Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, Base64Error> {
    if !encoded.len().is_multiple_of(4) {
        return Err(Base64Error("base64 length is not a multiple of four"));
    }
    let maximum_encoded = MAX_SNAPSHOT_BYTES.div_ceil(3) * 4;
    if encoded.len() > maximum_encoded {
        return Err(Base64Error("decoded snapshot exceeds the size limit"));
    }
    if encoded.is_empty() {
        return Ok(Vec::new());
    }

    fn value(byte: u8) -> Result<u8, Base64Error> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(Base64Error("base64 contains an invalid character")),
        }
    }

    let input = encoded.as_bytes();
    let padding =
        usize::from(input[input.len() - 1] == b'=') + usize::from(input[input.len() - 2] == b'=');
    let decoded_len = input.len() / 4 * 3 - padding;
    if decoded_len > MAX_SNAPSHOT_BYTES {
        return Err(Base64Error("decoded snapshot exceeds the size limit"));
    }
    let mut decoded = Vec::with_capacity(decoded_len);
    let chunks = input.chunks_exact(4);
    let chunk_count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let last = index + 1 == chunk_count;
        let a = value(chunk[0])?;
        let b = value(chunk[1])?;
        decoded.push((a << 2) | (b >> 4));
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if last => {
                if b & 0x0f != 0 {
                    return Err(Base64Error("base64 has non-canonical trailing bits"));
                }
            }
            (third, b'=') if last => {
                let c = value(third)?;
                if c & 0x03 != 0 {
                    return Err(Base64Error("base64 has non-canonical trailing bits"));
                }
                decoded.push((b << 4) | (c >> 2));
            }
            (b'=', _) => return Err(Base64Error("base64 padding is misplaced")),
            (third, fourth) => {
                let c = value(third)?;
                let d = value(fourth)?;
                decoded.push((b << 4) | (c >> 2));
                decoded.push((c << 6) | d);
            }
        }
    }
    Ok(decoded)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotDigest([u8; 32]);

impl SnapshotDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SnapshotDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for SnapshotDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for SnapshotDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SnapshotDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DigestVisitor;

        impl Visitor<'_> for DigestVisitor {
            type Value = SnapshotDigest;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a lowercase 64-character SHA-256 digest")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() != 64 || value.bytes().any(|byte| !byte.is_ascii_hexdigit()) {
                    return Err(E::custom("invalid SHA-256 digest"));
                }
                if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
                    return Err(E::custom("SHA-256 digest must use lowercase hex"));
                }
                let mut digest = [0_u8; 32];
                for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
                    let pair = std::str::from_utf8(pair).map_err(E::custom)?;
                    digest[index] = u8::from_str_radix(pair, 16).map_err(E::custom)?;
                }
                Ok(SnapshotDigest(digest))
            }
        }

        deserializer.deserialize_str(DigestVisitor)
    }
}

/// Content-derived identity of one host-validated publication proposal.
///
/// The host derives this after validating the publication. It is deliberately distinct from the
/// snapshot digest: the same bytes proposed against a different binding, generation, revision,
/// prior digest, selected-topic set, or ordered fact envelope are a different proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProposalId(SnapshotDigest);

impl ProposalId {
    pub fn of(bytes: &[u8]) -> Self {
        Self(SnapshotDigest::of(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl fmt::Display for ProposalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

/// Compare-and-swap fence captured before a provider computes a publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalFence {
    generation: u64,
    revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_digest: Option<SnapshotDigest>,
}

impl ProposalFence {
    pub fn new(generation: u64, revision: u64, prior_digest: Option<SnapshotDigest>) -> Self {
        Self {
            generation,
            revision,
            prior_digest,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn prior_digest(&self) -> Option<SnapshotDigest> {
        self.prior_digest
    }
}

/// Durable receipt for one changed publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationCommit {
    proposal_id: ProposalId,
    generation: u64,
    revision: u64,
    digest: SnapshotDigest,
}

impl PublicationCommit {
    pub fn new(
        proposal_id: ProposalId,
        generation: u64,
        revision: u64,
        digest: SnapshotDigest,
    ) -> Self {
        Self {
            proposal_id,
            generation,
            revision,
            digest,
        }
    }

    pub fn proposal_id(&self) -> ProposalId {
        self.proposal_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn digest(&self) -> SnapshotDigest {
        self.digest
    }
}

/// Result of the host's one authoritative compare-and-swap transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalCommit {
    Committed(PublicationCommit),
    AlreadyCommitted(PublicationCommit),
    Unchanged {
        generation: u64,
        revision: u64,
        digest: SnapshotDigest,
    },
    StaleGeneration {
        actual_generation: u64,
        actual_revision: u64,
    },
    StalePrior {
        actual_generation: u64,
        actual_revision: u64,
        actual_digest: Option<SnapshotDigest>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum HostMessage {
    Register {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        uri: String,
        selector: Value,
        carrier_path: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_digest: Option<SnapshotDigest>,
    },
    Unregister {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
    },
    Observe {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        demand_watermark: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeHealthState {
    Starting,
    Ready,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Publication {
    pub schema_id: String,
    pub media_type: String,
    pub bytes: SnapshotBytes,
    pub topics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<Vec<ResourceFact>>,
}

impl Publication {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_topics(&self.topics)?;
        validate_facts(self.facts.as_deref().unwrap_or_default())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ObservationResult {
    Unchanged,
    Failed {
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    },
    Published {
        publication: Publication,
    },
}

impl ObservationResult {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Unchanged => Ok(()),
            Self::Failed { diagnostic } => {
                if let Some(diagnostic) = diagnostic
                    && diagnostic.len() > MAX_OBSERVATION_DIAGNOSTIC_BYTES
                {
                    return Err(ProtocolError::ObservationDiagnosticTooLarge {
                        actual: diagnostic.len(),
                    });
                }
                Ok(())
            }
            Self::Published { publication } => publication.validate(),
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ObservationResultWire {
    Unchanged {},
    Failed { diagnostic: Option<String> },
    Published { publication: Publication },
}

impl<'de> Deserialize<'de> for ObservationResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ObservationResultWire::deserialize(deserializer)? {
            ObservationResultWire::Unchanged {} => Self::Unchanged,
            ObservationResultWire::Failed { diagnostic } => Self::Failed { diagnostic },
            ObservationResultWire::Published { publication } => Self::Published { publication },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RuntimeMessage {
    Publish {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        #[serde(flatten)]
        publication: Publication,
    },
    Health {
        owner: RuntimeOwner,
        #[serde(skip_serializing_if = "Option::is_none")]
        binding_id: Option<BindingId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        registration: Option<RegistrationToken>,
        state: RuntimeHealthState,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ObservationResult {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        demand_watermark: u64,
        result: ObservationResult,
    },
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum RuntimeMessageWire {
    Publish {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        /// ABI-3 runtimes may still send this retired producer timestamp. The host never trusted it,
        /// so decode it only to preserve that ABI and discard it at this boundary.
        observed_at: Option<String>,
        #[serde(flatten)]
        publication: Publication,
    },
    Health {
        owner: RuntimeOwner,
        binding_id: Option<BindingId>,
        registration: Option<RegistrationToken>,
        state: RuntimeHealthState,
        detail: Option<String>,
    },
    ObservationResult {
        owner: RuntimeOwner,
        binding_id: BindingId,
        registration: RegistrationToken,
        demand_watermark: u64,
        result: ObservationResult,
    },
}

impl<'de> Deserialize<'de> for RuntimeMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match RuntimeMessageWire::deserialize(deserializer)? {
            RuntimeMessageWire::Publish {
                owner,
                binding_id,
                registration,
                observed_at,
                publication,
            } => {
                drop(observed_at);
                Self::Publish {
                    owner,
                    binding_id,
                    registration,
                    publication,
                }
            }
            RuntimeMessageWire::Health {
                owner,
                binding_id,
                registration,
                state,
                detail,
            } => Self::Health {
                owner,
                binding_id,
                registration,
                state,
                detail,
            },
            RuntimeMessageWire::ObservationResult {
                owner,
                binding_id,
                registration,
                demand_watermark,
                result,
            } => Self::ObservationResult {
                owner,
                binding_id,
                registration,
                demand_watermark,
                result,
            },
        })
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    MissingNewline,
    MultipleLines,
    EmptyLine,
    LineTooLarge { actual: usize },
    SelectorTooLarge { actual: usize },
    HealthDetailTooLarge { actual: usize },
    ObservationDiagnosticTooLarge { actual: usize },
    InvalidDemandWatermark,
    InvalidTopics(&'static str),
    InvalidFacts(FactError),
    InvalidHealthScope,
    Json(serde_json::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNewline => formatter.write_str("protocol frame is missing its newline"),
            Self::MultipleLines => formatter.write_str("protocol frame contains multiple lines"),
            Self::EmptyLine => formatter.write_str("protocol frame is empty"),
            Self::LineTooLarge { actual } => write!(
                formatter,
                "protocol line is {actual} bytes; maximum is {MAX_PROTOCOL_LINE_BYTES}"
            ),
            Self::SelectorTooLarge { actual } => write!(
                formatter,
                "selector is {actual} bytes; maximum is {MAX_SELECTOR_BYTES}"
            ),
            Self::HealthDetailTooLarge { actual } => write!(
                formatter,
                "health detail is {actual} bytes; maximum is {MAX_HEALTH_DETAIL_BYTES}"
            ),
            Self::ObservationDiagnosticTooLarge { actual } => write!(
                formatter,
                "observation diagnostic is {actual} bytes; maximum is {MAX_OBSERVATION_DIAGNOSTIC_BYTES}"
            ),
            Self::InvalidDemandWatermark => {
                formatter.write_str("observation demand watermark must be positive")
            }
            Self::InvalidTopics(reason) => write!(formatter, "invalid topics: {reason}"),
            Self::InvalidFacts(error) => write!(formatter, "invalid facts: {error}"),
            Self::InvalidHealthScope => formatter
                .write_str("binding-scoped health must carry both bindingId and registration"),
            Self::Json(error) => write!(formatter, "invalid protocol JSON: {error}"),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::InvalidFacts(error) => Some(error),
            _ => None,
        }
    }
}

pub fn decode_host_line(line: &[u8]) -> Result<HostMessage, ProtocolError> {
    let payload = protocol_payload(line)?;
    let message = serde_json::from_slice(payload).map_err(ProtocolError::Json)?;
    validate_host_message(&message)?;
    Ok(message)
}

pub fn decode_runtime_line(line: &[u8]) -> Result<RuntimeMessage, ProtocolError> {
    let payload = protocol_payload(line)?;
    let message = serde_json::from_slice(payload).map_err(ProtocolError::Json)?;
    validate_runtime_message(&message)?;
    Ok(message)
}

pub fn encode_host_line(message: &HostMessage) -> Result<Vec<u8>, ProtocolError> {
    validate_host_message(message)?;
    encode_protocol_line(message)
}

pub fn encode_runtime_line(message: &RuntimeMessage) -> Result<Vec<u8>, ProtocolError> {
    validate_runtime_message(message)?;
    encode_protocol_line(message)
}

fn protocol_payload(line: &[u8]) -> Result<&[u8], ProtocolError> {
    if line.len() > MAX_PROTOCOL_LINE_BYTES {
        return Err(ProtocolError::LineTooLarge { actual: line.len() });
    }
    let Some(payload) = line.strip_suffix(b"\n") else {
        return Err(ProtocolError::MissingNewline);
    };
    if payload.is_empty() {
        return Err(ProtocolError::EmptyLine);
    }
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(ProtocolError::MultipleLines);
    }
    Ok(payload)
}

fn encode_protocol_line(message: &impl Serialize) -> Result<Vec<u8>, ProtocolError> {
    let mut line = serde_json::to_vec(message).map_err(ProtocolError::Json)?;
    line.push(b'\n');
    if line.len() > MAX_PROTOCOL_LINE_BYTES {
        return Err(ProtocolError::LineTooLarge { actual: line.len() });
    }
    Ok(line)
}

fn validate_host_message(message: &HostMessage) -> Result<(), ProtocolError> {
    match message {
        HostMessage::Register { selector, .. } => {
            let actual = serde_json::to_vec(selector)
                .map_err(ProtocolError::Json)?
                .len();
            if actual > MAX_SELECTOR_BYTES {
                return Err(ProtocolError::SelectorTooLarge { actual });
            }
            Ok(())
        }
        HostMessage::Observe {
            demand_watermark, ..
        } if *demand_watermark == 0 => Err(ProtocolError::InvalidDemandWatermark),
        HostMessage::Observe { .. } | HostMessage::Unregister { .. } => Ok(()),
    }
}

fn validate_runtime_message(message: &RuntimeMessage) -> Result<(), ProtocolError> {
    match message {
        RuntimeMessage::Publish { publication, .. } => publication.validate(),
        RuntimeMessage::Health {
            binding_id,
            registration,
            detail,
            ..
        } => {
            if binding_id.is_some() != registration.is_some() {
                return Err(ProtocolError::InvalidHealthScope);
            }
            if let Some(detail) = detail
                && detail.len() > MAX_HEALTH_DETAIL_BYTES
            {
                return Err(ProtocolError::HealthDetailTooLarge {
                    actual: detail.len(),
                });
            }
            Ok(())
        }
        RuntimeMessage::ObservationResult {
            demand_watermark,
            result,
            ..
        } => {
            if *demand_watermark == 0 {
                return Err(ProtocolError::InvalidDemandWatermark);
            }
            result.validate()
        }
    }
}
fn validate_facts(facts: &[ResourceFact]) -> Result<(), ProtocolError> {
    if facts.len() > MAX_FACTS {
        return Err(ProtocolError::InvalidFacts(FactError::TooMany {
            actual: facts.len(),
        }));
    }
    facts
        .iter()
        .try_for_each(ResourceFact::validate)
        .map_err(ProtocolError::InvalidFacts)
}

pub fn validate_topics(topics: &[String]) -> Result<(), ProtocolError> {
    let mut unique = BTreeSet::new();
    for topic in topics {
        if topic.is_empty() {
            return Err(ProtocolError::InvalidTopics(
                "topic names must not be empty",
            ));
        }
        if !unique.insert(topic.as_str()) {
            return Err(ProtocolError::InvalidTopics("topic names must be unique"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owner() -> RuntimeOwner {
        RuntimeOwner::new(
            RuntimeIncarnation::new("incarnation").unwrap(),
            OwnerClaim::new("claim").unwrap(),
        )
    }

    fn publication(bytes: &[u8]) -> Publication {
        Publication {
            schema_id: "schema.v1".to_owned(),
            media_type: "application/json".to_owned(),
            bytes: SnapshotBytes::new(bytes.to_vec()).unwrap(),
            topics: vec!["selected".to_owned()],
            facts: None,
        }
    }

    fn publish(bytes: &[u8]) -> RuntimeMessage {
        RuntimeMessage::Publish {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            publication: publication(bytes),
        }
    }

    #[test]
    fn proposal_fence_and_commit_receipt_are_domain_typed() {
        let prior = SnapshotDigest::of(b"prior");
        let fence = ProposalFence::new(7, 11, Some(prior));
        assert_eq!(
            serde_json::to_value(fence).unwrap(),
            json!({
                "generation": 7,
                "revision": 11,
                "priorDigest": prior.to_string(),
            })
        );
        assert_eq!(
            serde_json::from_value::<ProposalFence>(serde_json::to_value(fence).unwrap()).unwrap(),
            fence
        );

        let proposal_id = ProposalId::of(b"proposal identity");
        let digest = SnapshotDigest::of(b"carrier");
        let commit = PublicationCommit::new(proposal_id, 7, 12, digest);
        assert_eq!(commit.proposal_id(), proposal_id);
        assert_eq!(commit.generation(), 7);
        assert_eq!(commit.revision(), 12);
        assert_eq!(commit.digest(), digest);
        assert_ne!(proposal_id.as_bytes(), digest.as_bytes());
    }

    #[test]
    fn host_frames_have_exact_json_shape_and_newline() {
        let register = HostMessage::Register {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            uri: "resource://one".to_owned(),
            selector: json!({"kind": "one"}),
            carrier_path: PathBuf::from("resources/one.json"),
            previous_digest: Some(SnapshotDigest::of(b"previous")),
        };
        assert_eq!(
            encode_host_line(&register).unwrap(),
            b"{\"type\":\"register\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"uri\":\"resource://one\",\"selector\":{\"kind\":\"one\"},\"carrierPath\":\"resources/one.json\",\"previousDigest\":\"6da0633528deaa0144e7b058315f0b753ec0b945163a72bf96a0d18180f9de0d\"}\n"
        );
        let unregister = HostMessage::Unregister {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
        };
        assert_eq!(
            encode_host_line(&unregister).unwrap(),
            b"{\"type\":\"unregister\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\"}\n"
        );
        let observe = HostMessage::Observe {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 7,
        };
        assert_eq!(
            encode_host_line(&observe).unwrap(),
            b"{\"type\":\"observe\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"demandWatermark\":7}\n"
        );
    }

    #[test]
    fn periodic_publish_has_exact_flat_json_shape_and_padded_base64() {
        assert_eq!(
            encode_runtime_line(&publish(b"one byte")).unwrap(),
            b"{\"type\":\"publish\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"schemaId\":\"schema.v1\",\"mediaType\":\"application/json\",\"bytes\":\"b25lIGJ5dGU=\",\"topics\":[\"selected\"]}\n"
        );
        let health = RuntimeMessage::Health {
            owner: owner(),
            binding_id: None,
            registration: None,
            state: RuntimeHealthState::Ready,
            detail: None,
        };
        assert_eq!(
            encode_runtime_line(&health).unwrap(),
            b"{\"type\":\"health\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"state\":\"ready\"}\n"
        );
        assert_eq!(
            decode_runtime_line(&encode_runtime_line(&publish(b"one byte")).unwrap()).unwrap(),
            publish(b"one byte")
        );
    }

    #[test]
    fn abi_3_publish_decodes_with_or_without_deprecated_observed_at_only() {
        let without_observed_at = b"{\"type\":\"publish\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"schemaId\":\"schema.v1\",\"mediaType\":\"application/json\",\"bytes\":\"b25lIGJ5dGU=\",\"topics\":[\"selected\"]}\n";
        let with_observed_at = b"{\"type\":\"publish\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"schemaId\":\"schema.v1\",\"mediaType\":\"application/json\",\"bytes\":\"b25lIGJ5dGU=\",\"topics\":[\"selected\"],\"observedAt\":\"2026-08-30T00:00:00Z\"}\n";
        let unknown_field = b"{\"type\":\"publish\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"schemaId\":\"schema.v1\",\"mediaType\":\"application/json\",\"bytes\":\"b25lIGJ5dGU=\",\"topics\":[\"selected\"],\"extra\":true}\n";

        assert_eq!(
            decode_runtime_line(without_observed_at).unwrap(),
            publish(b"one byte")
        );
        assert_eq!(
            decode_runtime_line(with_observed_at).unwrap(),
            publish(b"one byte")
        );
        assert!(matches!(
            decode_runtime_line(unknown_field),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn observation_results_have_one_atomic_tagged_wire_shape() {
        let unchanged = RuntimeMessage::ObservationResult {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 7,
            result: ObservationResult::Unchanged,
        };
        assert_eq!(
            encode_runtime_line(&unchanged).unwrap(),
            b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"demandWatermark\":7,\"result\":{\"status\":\"unchanged\"}}\n"
        );

        let failed = RuntimeMessage::ObservationResult {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 8,
            result: ObservationResult::Failed {
                diagnostic: Some("provider unavailable".to_owned()),
            },
        };
        assert_eq!(
            encode_runtime_line(&failed).unwrap(),
            b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"demandWatermark\":8,\"result\":{\"status\":\"failed\",\"diagnostic\":\"provider unavailable\"}}\n"
        );

        let mut published = publication(b"one byte");
        published.facts = Some(vec![ResourceFact::current("state", "ready").unwrap()]);
        let published = RuntimeMessage::ObservationResult {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 9,
            result: ObservationResult::Published {
                publication: published,
            },
        };
        assert_eq!(
            encode_runtime_line(&published).unwrap(),
            b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"incarnation\",\"claim\":\"claim\"},\"bindingId\":\"binding\",\"registration\":\"registration\",\"demandWatermark\":9,\"result\":{\"status\":\"published\",\"publication\":{\"schemaId\":\"schema.v1\",\"mediaType\":\"application/json\",\"bytes\":\"b25lIGJ5dGU=\",\"topics\":[\"selected\"],\"facts\":[{\"key\":\"state\",\"after\":\"ready\"}]}}}\n"
        );
        assert_eq!(
            decode_runtime_line(&encode_runtime_line(&published).unwrap()).unwrap(),
            published
        );
    }

    #[test]
    fn observation_result_validation_is_reusable_outside_line_framing() {
        assert!(ObservationResult::Unchanged.validate().is_ok());
        assert!(matches!(
            ObservationResult::Failed {
                diagnostic: Some("x".repeat(MAX_OBSERVATION_DIAGNOSTIC_BYTES + 1)),
            }
            .validate(),
            Err(ProtocolError::ObservationDiagnosticTooLarge { .. })
        ));
        let mut invalid = publication(b"duplicate topic");
        invalid.topics = vec!["same".to_owned(), "same".to_owned()];
        assert!(matches!(
            ObservationResult::Published {
                publication: invalid,
            }
            .validate(),
            Err(ProtocolError::InvalidTopics(_))
        ));
    }

    #[test]
    fn fact_wire_shape_distinguishes_omission_from_explicit_null() {
        let mut message = publish(b"fact");
        let RuntimeMessage::Publish { publication, .. } = &mut message else {
            unreachable!();
        };
        publication.facts = Some(vec![
            ResourceFact::current("state", "ready").unwrap(),
            ResourceFact::transition("label", None::<String>, Some("added")).unwrap(),
            ResourceFact::transition("removed", Some("old"), None::<String>).unwrap(),
        ]);
        let encoded = encode_runtime_line(&message).unwrap();
        let json: Value = serde_json::from_slice(encoded.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(
            json["facts"],
            json!([
                {"key": "state", "after": "ready"},
                {"key": "label", "before": null, "after": "added"},
                {"key": "removed", "before": "old", "after": null}
            ])
        );
        assert_eq!(decode_runtime_line(&encoded).unwrap(), message);
    }

    #[test]
    fn invalid_fact_shapes_and_bounds_are_rejected_for_shared_publications() {
        let missing_values = b"{\"type\":\"publish\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"schemaId\":\"s\",\"mediaType\":\"m\",\"bytes\":\"\",\"topics\":[],\"facts\":[{\"key\":\"state\"}]}\n";
        assert!(matches!(
            decode_runtime_line(missing_values),
            Err(ProtocolError::InvalidFacts(FactError::MissingValue))
        ));
        let demanded_missing_values = b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":1,\"result\":{\"status\":\"published\",\"publication\":{\"schemaId\":\"s\",\"mediaType\":\"m\",\"bytes\":\"\",\"topics\":[],\"facts\":[{\"key\":\"state\"}]}}}\n";
        assert!(matches!(
            decode_runtime_line(demanded_missing_values),
            Err(ProtocolError::InvalidFacts(FactError::MissingValue))
        ));
        assert!(ResourceFact::current("", "value").is_err());
        assert!(ResourceFact::current("state", "two\nlines").is_err());
        assert!(ResourceFact::current("x".repeat(MAX_FACT_KEY_BYTES + 1), "value").is_err());
        assert!(ResourceFact::current("state", "x".repeat(MAX_FACT_VALUE_BYTES + 1)).is_err());

        let too_many = (0..=MAX_FACTS)
            .map(|index| ResourceFact::current(format!("key-{index}"), "value").unwrap())
            .collect();
        let mut message = publish(b"facts");
        let RuntimeMessage::Publish { publication, .. } = &mut message else {
            unreachable!();
        };
        publication.facts = Some(too_many);
        assert!(matches!(
            encode_runtime_line(&message),
            Err(ProtocolError::InvalidFacts(FactError::TooMany { .. }))
        ));
    }

    #[test]
    fn decoding_is_strict_about_fields_ids_digest_and_obsolete_protocol() {
        let unknown_message_field = b"{\"type\":\"health\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"state\":\"ready\",\"extra\":true}\n";
        assert!(matches!(
            decode_runtime_line(unknown_message_field),
            Err(ProtocolError::Json(_))
        ));
        let unknown_owner_field = b"{\"type\":\"health\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\",\"extra\":true},\"state\":\"ready\"}\n";
        assert!(matches!(
            decode_runtime_line(unknown_owner_field),
            Err(ProtocolError::Json(_))
        ));
        let empty_id = b"{\"type\":\"unregister\",\"owner\":{\"incarnation\":\"\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\"}\n";
        assert!(matches!(
            decode_host_line(empty_id),
            Err(ProtocolError::Json(_))
        ));
        let uppercase_digest = b"{\"type\":\"register\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"uri\":\"u\",\"selector\":{},\"carrierPath\":\"p\",\"previousDigest\":\"AFA459DEEC028BA69A538AB3DF3ED61A63F9C8383C2FDAFA95ED544547FB675D\"}\n";
        assert!(matches!(
            decode_host_line(uppercase_digest),
            Err(ProtocolError::Json(_))
        ));
        let unknown_result_field = b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":1,\"result\":{\"status\":\"unchanged\",\"extra\":true}}\n";
        assert!(matches!(
            decode_runtime_line(unknown_result_field),
            Err(ProtocolError::Json(_))
        ));
        let unknown_publication_field = b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":1,\"result\":{\"status\":\"published\",\"publication\":{\"schemaId\":\"s\",\"mediaType\":\"m\",\"bytes\":\"\",\"topics\":[],\"extra\":true}}}\n";
        assert!(matches!(
            decode_runtime_line(unknown_publication_field),
            Err(ProtocolError::Json(_))
        ));

        let obsolete_settlement = b"{\"type\":\"observationSettled\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":1,\"outcome\":\"unchanged\"}\n";
        assert!(matches!(
            decode_runtime_line(obsolete_settlement),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn observation_watermarks_must_be_positive() {
        let zero_observe = b"{\"type\":\"observe\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":0}\n";
        assert!(matches!(
            decode_host_line(zero_observe),
            Err(ProtocolError::InvalidDemandWatermark)
        ));
        let zero_result = b"{\"type\":\"observationResult\",\"owner\":{\"incarnation\":\"i\",\"claim\":\"c\"},\"bindingId\":\"b\",\"registration\":\"r\",\"demandWatermark\":0,\"result\":{\"status\":\"unchanged\"}}\n";
        assert!(matches!(
            decode_runtime_line(zero_result),
            Err(ProtocolError::InvalidDemandWatermark)
        ));
        let observe = HostMessage::Observe {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 0,
        };
        assert!(matches!(
            encode_host_line(&observe),
            Err(ProtocolError::InvalidDemandWatermark)
        ));
    }

    #[test]
    fn decoding_rejects_malformed_frames() {
        assert!(matches!(
            decode_runtime_line(b"{}"),
            Err(ProtocolError::MissingNewline)
        ));
        assert!(matches!(
            decode_runtime_line(b"\n"),
            Err(ProtocolError::EmptyLine)
        ));
        assert!(matches!(
            decode_runtime_line(b"{}\n{}\n"),
            Err(ProtocolError::MultipleLines)
        ));
        assert!(matches!(
            decode_runtime_line(b"{}\r\n"),
            Err(ProtocolError::MultipleLines)
        ));
        assert!(matches!(
            decode_runtime_line(b"not json\n"),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn snapshot_base64_rejects_malformed_and_noncanonical_values() {
        for encoded in ["A", "!!!!", "A===", "AB==", "AAB=", "AA=A", "AA==AAAA"] {
            let line = format!(
                "{{\"type\":\"publish\",\"owner\":{{\"incarnation\":\"i\",\"claim\":\"c\"}},\"bindingId\":\"b\",\"registration\":\"r\",\"schemaId\":\"s\",\"mediaType\":\"m\",\"bytes\":\"{encoded}\",\"topics\":[]}}\n"
            );
            assert!(
                matches!(
                    decode_runtime_line(line.as_bytes()),
                    Err(ProtocolError::Json(_))
                ),
                "accepted {encoded}"
            );
        }
    }

    #[test]
    fn protocol_enforces_all_byte_limits() {
        assert!(matches!(
            decode_runtime_line(&vec![b'x'; MAX_PROTOCOL_LINE_BYTES + 1]),
            Err(ProtocolError::LineTooLarge { actual }) if actual == MAX_PROTOCOL_LINE_BYTES + 1
        ));
        assert_eq!(
            SnapshotBytes::new(vec![0; MAX_SNAPSHOT_BYTES + 1]).unwrap_err(),
            SnapshotSizeError {
                actual: MAX_SNAPSHOT_BYTES + 1
            }
        );
        let oversized_snapshot = encode_base64(&vec![0; MAX_SNAPSHOT_BYTES + 1]);
        let snapshot_line = format!(
            "{{\"type\":\"publish\",\"owner\":{{\"incarnation\":\"i\",\"claim\":\"c\"}},\"bindingId\":\"b\",\"registration\":\"r\",\"schemaId\":\"s\",\"mediaType\":\"m\",\"bytes\":\"{oversized_snapshot}\",\"topics\":[]}}\n"
        );
        assert!(matches!(
            decode_runtime_line(snapshot_line.as_bytes()),
            Err(ProtocolError::Json(_))
        ));

        let register = HostMessage::Register {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            uri: "u".to_owned(),
            selector: Value::String("x".repeat(MAX_SELECTOR_BYTES)),
            carrier_path: PathBuf::from("p"),
            previous_digest: None,
        };
        assert!(matches!(
            encode_host_line(&register),
            Err(ProtocolError::SelectorTooLarge { .. })
        ));
        let mut register_line = serde_json::to_vec(&register).unwrap();
        register_line.push(b'\n');
        assert!(matches!(
            decode_host_line(&register_line),
            Err(ProtocolError::SelectorTooLarge { .. })
        ));

        let health = RuntimeMessage::Health {
            owner: owner(),
            binding_id: None,
            registration: None,
            state: RuntimeHealthState::Degraded,
            detail: Some("x".repeat(MAX_HEALTH_DETAIL_BYTES + 1)),
        };
        assert!(matches!(
            encode_runtime_line(&health),
            Err(ProtocolError::HealthDetailTooLarge { .. })
        ));
        let mut health_line = serde_json::to_vec(&health).unwrap();
        health_line.push(b'\n');
        assert!(matches!(
            decode_runtime_line(&health_line),
            Err(ProtocolError::HealthDetailTooLarge { .. })
        ));

        let oversized_diagnostic = RuntimeMessage::ObservationResult {
            owner: owner(),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
            demand_watermark: 1,
            result: ObservationResult::Failed {
                diagnostic: Some("x".repeat(MAX_OBSERVATION_DIAGNOSTIC_BYTES + 1)),
            },
        };
        assert!(matches!(
            encode_runtime_line(&oversized_diagnostic),
            Err(ProtocolError::ObservationDiagnosticTooLarge { .. })
        ));
        let mut diagnostic_line = serde_json::to_vec(&oversized_diagnostic).unwrap();
        diagnostic_line.push(b'\n');
        assert!(matches!(
            decode_runtime_line(&diagnostic_line),
            Err(ProtocolError::ObservationDiagnosticTooLarge { .. })
        ));
        assert!(RuntimeIncarnation::new("x".repeat(MAX_OPAQUE_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn runtime_semantic_validation_applies_on_encode_and_decode() {
        let invalid_health = RuntimeMessage::Health {
            owner: owner(),
            binding_id: Some(BindingId::new("binding").unwrap()),
            registration: None,
            state: RuntimeHealthState::Failed,
            detail: None,
        };
        assert!(matches!(
            encode_runtime_line(&invalid_health),
            Err(ProtocolError::InvalidHealthScope)
        ));
        let mut duplicate_topics = publish(b"bytes");
        let RuntimeMessage::Publish { publication, .. } = &mut duplicate_topics else {
            unreachable!();
        };
        publication.topics = vec!["same".to_owned(), "same".to_owned()];
        assert!(matches!(
            encode_runtime_line(&duplicate_topics),
            Err(ProtocolError::InvalidTopics(_))
        ));
    }
}
