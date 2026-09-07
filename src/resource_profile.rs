//! Observable Resource Profile runtime protocol, ownership, publication, and catch-up core.
//!
//! This module deliberately stops at the supervisor integration boundary: it does not spawn a
//! runtime or enqueue resync records. It validates and fences runtime output, publishes the one
//! canonical snapshot, and exposes level-triggered delivery work for the existing event ingress.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub use st2_resource_protocol::{
    BindingId, FactError, FactValue, HostMessage, MAX_FACT_KEY_BYTES, MAX_FACT_VALUE_BYTES,
    MAX_FACTS, MAX_HEALTH_DETAIL_BYTES, MAX_OBSERVATION_DIAGNOSTIC_BYTES, MAX_PROTOCOL_LINE_BYTES,
    MAX_SELECTOR_BYTES, MAX_SNAPSHOT_BYTES, ObservationResult, OpaqueIdError, OwnerClaim,
    ProposalCommit, ProposalFence, ProposalId, ProtocolError, Publication, PublicationCommit,
    RegistrationToken, ResourceFact, RuntimeHealthState, RuntimeIncarnation, RuntimeMessage,
    RuntimeOwner, SnapshotBytes, SnapshotDigest,
    SnapshotSizeError, decode_host_line, decode_runtime_line, encode_host_line,
    encode_runtime_line,
};

// Covers distinct latest-transition and retained-delivery envelopes with 32 maximally sized facts
// each after worst-case JSON escaping.
const MAX_CATCH_UP_FILE_BYTES: usize = 512 * 1024;
const CATCH_UP_FILE: &str = "resource-profile-catch-up.json";
const PUBLICATION_INTENT_FILE: &str = "resource-profile-publication-intent.json";
const PUBLICATION_LOCK_FILE: &str = "resource-profile-publication.lock";
const PROPOSAL_ID_DOMAIN: &[u8] = b"st2.resource-publication-proposal.v1\0";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSelection {
    topics: BTreeSet<String>,
}

impl TopicSelection {
    pub fn new(topics: impl IntoIterator<Item = String>) -> Result<Self, ContractError> {
        let mut normalized = BTreeSet::new();
        for topic in topics {
            if topic.is_empty() {
                return Err(ContractError::EmptyTopic);
            }
            if !normalized.insert(topic) {
                return Err(ContractError::DuplicateTopic);
            }
        }
        Ok(Self { topics: normalized })
    }

    pub fn contains(&self, topic: &str) -> bool {
        self.topics.contains(topic)
    }

    pub fn topics(&self) -> impl Iterator<Item = &str> {
        self.topics.iter().map(String::as_str)
    }

    fn select(&self, published: &[String]) -> Vec<String> {
        published
            .iter()
            .filter(|topic| self.contains(topic))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    EmptySchemaId,
    EmptyMediaType,
    EmptyTopic,
    DuplicateTopic,
    SelectedTopicNotPublished(String),
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchemaId => formatter.write_str("schema id must not be empty"),
            Self::EmptyMediaType => formatter.write_str("media type must not be empty"),
            Self::EmptyTopic => formatter.write_str("topic names must not be empty"),
            Self::DuplicateTopic => formatter.write_str("topic names must be unique"),
            Self::SelectedTopicNotPublished(topic) => {
                write!(formatter, "selected topic is not published: {topic}")
            }
        }
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationContract {
    schema_id: String,
    media_type: String,
    published_topics: BTreeSet<String>,
    selection: TopicSelection,
}

impl PublicationContract {
    pub fn new(
        schema_id: impl Into<String>,
        media_type: impl Into<String>,
        published_topics: impl IntoIterator<Item = String>,
        selection: TopicSelection,
    ) -> Result<Self, ContractError> {
        let schema_id = schema_id.into();
        if schema_id.is_empty() {
            return Err(ContractError::EmptySchemaId);
        }
        let media_type = media_type.into();
        if media_type.is_empty() {
            return Err(ContractError::EmptyMediaType);
        }
        let mut normalized = BTreeSet::new();
        for topic in published_topics {
            if topic.is_empty() {
                return Err(ContractError::EmptyTopic);
            }
            if !normalized.insert(topic) {
                return Err(ContractError::DuplicateTopic);
            }
        }
        for topic in selection.topics() {
            if !normalized.contains(topic) {
                return Err(ContractError::SelectedTopicNotPublished(topic.to_owned()));
            }
        }
        Ok(Self {
            schema_id,
            media_type,
            published_topics: normalized,
            selection,
        })
    }

    pub fn schema_id(&self) -> &str {
        &self.schema_id
    }

    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    pub fn published_topics(&self) -> impl Iterator<Item = &str> {
        self.published_topics.iter().map(String::as_str)
    }

    pub fn selection(&self) -> &TopicSelection {
        &self.selection
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotTarget {
    root: PathBuf,
    relative: PathBuf,
}

impl SnapshotTarget {
    pub fn new(
        root: impl Into<PathBuf>,
        carrier_path: impl AsRef<Path>,
    ) -> Result<Self, PathError> {
        let root = root.into();
        validate_absolute_path(&root).map_err(PathError::UnsafeRoot)?;
        let carrier_path = carrier_path.as_ref();
        let relative = if carrier_path.is_absolute() {
            carrier_path
                .strip_prefix(&root)
                .map_err(|_| PathError::EscapesRoot)?
                .to_path_buf()
        } else {
            carrier_path.to_path_buf()
        };
        validate_relative_path(&relative).map_err(PathError::UnsafeCarrier)?;
        Ok(Self { root, relative })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }

    /// Digest the currently published contained snapshot, if present.
    pub fn current_digest(&self) -> Result<Option<SnapshotDigest>, PublicationError> {
        let parent = self.relative.parent().unwrap_or_else(|| Path::new(""));
        let leaf = self.relative.file_name().ok_or_else(|| {
            PublicationError::UnsafeTarget(PathError::UnsafeCarrier("missing leaf"))
        })?;
        let directory = match open_absolute_dir_beneath(&self.root, parent) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PublicationError::Io(error)),
        };
        read_regular_optional_at(&directory, leaf, MAX_SNAPSHOT_BYTES)
            .map_err(|error| match error {
                BoundedReadError::TooLarge => PublicationError::ExistingSnapshotTooLarge,
                BoundedReadError::NotRegular => PublicationError::SnapshotNotRegular,
                BoundedReadError::Io(error) => PublicationError::Io(error),
            })
            .map(|bytes| bytes.map(|bytes| SnapshotDigest::of(&bytes)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    EscapesRoot,
    UnsafeRoot(&'static str),
    UnsafeCarrier(&'static str),
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EscapesRoot => formatter.write_str("snapshot path escapes its containment root"),
            Self::UnsafeRoot(reason) => write!(formatter, "unsafe containment root: {reason}"),
            Self::UnsafeCarrier(reason) => write!(formatter, "unsafe snapshot path: {reason}"),
        }
    }
}

impl std::error::Error for PathError {}

fn validate_absolute_path(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("path is not absolute");
    }
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err("path is not lexically normalized");
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), &'static str> {
    let mut any = false;
    for component in path.components() {
        let Component::Normal(_) = component else {
            return Err("path contains a non-normal component");
        };
        any = true;
    }
    if !any {
        return Err("path must name a file beneath the root");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRegistration {
    binding_id: BindingId,
    registration: RegistrationToken,
    target: SnapshotTarget,
    contract: PublicationContract,
}

impl BindingRegistration {
    pub fn new(
        binding_id: BindingId,
        registration: RegistrationToken,
        target: SnapshotTarget,
        contract: PublicationContract,
    ) -> Self {
        Self {
            binding_id,
            registration,
            target,
            contract,
        }
    }

    pub fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }

    pub fn registration(&self) -> &RegistrationToken {
        &self.registration
    }

    pub fn target(&self) -> &SnapshotTarget {
        &self.target
    }

    pub fn contract(&self) -> &PublicationContract {
        &self.contract
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationChange {
    Added,
    Replaced,
}

#[derive(Debug, Default)]
pub struct RuntimeLifecycle {
    owner: Option<RuntimeOwner>,
    bindings: HashMap<BindingId, BindingRegistration>,
}

impl RuntimeLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owner(&self) -> Option<&RuntimeOwner> {
        self.owner.as_ref()
    }

    pub fn claim(&mut self, owner: RuntimeOwner) -> bool {
        if self.owner.as_ref() == Some(&owner) {
            return false;
        }
        self.owner = Some(owner);
        self.bindings.clear();
        true
    }

    pub fn register(
        &mut self,
        owner: &RuntimeOwner,
        registration: BindingRegistration,
    ) -> Result<RegistrationChange, FenceError> {
        self.require_owner(owner)?;
        let change = if self
            .bindings
            .insert(registration.binding_id.clone(), registration)
            .is_some()
        {
            RegistrationChange::Replaced
        } else {
            RegistrationChange::Added
        };
        Ok(change)
    }

    pub fn unregister(
        &mut self,
        owner: &RuntimeOwner,
        binding_id: &BindingId,
        registration: &RegistrationToken,
    ) -> Result<BindingRegistration, FenceError> {
        self.require_registration(owner, binding_id, registration)?;
        Ok(self
            .bindings
            .remove(binding_id)
            .expect("registration was checked immediately before removal"))
    }

    pub fn accept_output<'a>(
        &'a self,
        message: &'a RuntimeMessage,
    ) -> Result<AcceptedOutput<'a>, FenceError> {
        match message {
            RuntimeMessage::Publish {
                owner,
                binding_id,
                registration,
                publication,
            } => {
                let binding = self.require_registration(owner, binding_id, registration)?;
                Ok(AcceptedOutput::Publication(
                    self.accept_publication(binding, publication)?,
                ))
            }
            RuntimeMessage::Health {
                owner,
                binding_id,
                registration,
                state,
                detail,
            } => {
                self.require_owner(owner)?;
                match (binding_id, registration) {
                    (Some(binding_id), Some(registration)) => {
                        self.require_registration(owner, binding_id, registration)?;
                    }
                    (None, None) => {}
                    _ => return Err(FenceError::InvalidHealthScope),
                }
                if detail
                    .as_ref()
                    .is_some_and(|detail| detail.len() > MAX_HEALTH_DETAIL_BYTES)
                {
                    return Err(FenceError::HealthDetailTooLarge);
                }
                Ok(AcceptedOutput::Health(AcceptedHealth {
                    binding_id: binding_id.as_ref(),
                    state: *state,
                    detail: detail.as_deref(),
                }))
            }
            RuntimeMessage::ObservationResult {
                owner,
                binding_id,
                registration,
                demand_watermark,
                result,
            } => {
                let binding = self.require_registration(owner, binding_id, registration)?;
                if *demand_watermark == 0 {
                    return Err(FenceError::InvalidDemandWatermark);
                }
                let result = match result {
                    ObservationResult::Unchanged => AcceptedObservation::Unchanged,
                    ObservationResult::Failed { diagnostic } => {
                        if diagnostic.as_ref().is_some_and(|diagnostic| {
                            diagnostic.len() > MAX_OBSERVATION_DIAGNOSTIC_BYTES
                        }) {
                            return Err(FenceError::ObservationDiagnosticTooLarge);
                        }
                        AcceptedObservation::Failed {
                            diagnostic: diagnostic.as_deref(),
                        }
                    }
                    ObservationResult::Published { publication } => AcceptedObservation::Published(
                        self.accept_publication(binding, publication)?,
                    ),
                };
                Ok(AcceptedOutput::ObservationResult(
                    AcceptedObservationResult {
                        binding_id,
                        demand_watermark: *demand_watermark,
                        result,
                    },
                ))
            }
        }
    }

    fn accept_publication<'a>(
        &'a self,
        binding: &'a BindingRegistration,
        publication: &'a Publication,
    ) -> Result<AcceptedPublication<'a>, FenceError> {
        if publication.schema_id != binding.contract.schema_id() {
            return Err(FenceError::ContractMismatch { field: "schemaId" });
        }
        if publication.media_type != binding.contract.media_type() {
            return Err(FenceError::ContractMismatch { field: "mediaType" });
        }
        let mut unique = BTreeSet::new();
        for topic in &publication.topics {
            if topic.is_empty() || !unique.insert(topic.as_str()) {
                return Err(FenceError::InvalidTopics);
            }
            if !binding.contract.published_topics.contains(topic) {
                return Err(FenceError::UnpublishedTopic(topic.clone()));
            }
        }
        Ok(AcceptedPublication {
            binding_id: &binding.binding_id,
            target: &binding.target,
            bytes: &publication.bytes,
            selected_topics: binding.contract.selection.select(&publication.topics),
            facts: publication.facts.as_deref().unwrap_or_default(),
        })
    }

    fn require_owner(&self, owner: &RuntimeOwner) -> Result<(), FenceError> {
        match self.owner.as_ref() {
            None => Err(FenceError::NoOwner),
            Some(current) if current != owner => Err(FenceError::StaleOwner),
            Some(_) => Ok(()),
        }
    }

    fn require_registration(
        &self,
        owner: &RuntimeOwner,
        binding_id: &BindingId,
        registration: &RegistrationToken,
    ) -> Result<&BindingRegistration, FenceError> {
        self.require_owner(owner)?;
        let binding = self
            .bindings
            .get(binding_id)
            .ok_or(FenceError::UnknownBinding)?;
        if &binding.registration != registration {
            return Err(FenceError::StaleRegistration);
        }
        Ok(binding)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceError {
    NoOwner,
    StaleOwner,
    UnknownBinding,
    StaleRegistration,
    ContractMismatch { field: &'static str },
    UnpublishedTopic(String),
    InvalidTopics,
    InvalidHealthScope,
    InvalidDemandWatermark,
    HealthDetailTooLarge,
    ObservationDiagnosticTooLarge,
}

impl fmt::Display for FenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOwner => formatter.write_str("runtime has no current owner"),
            Self::StaleOwner => formatter.write_str("runtime output has a stale owner claim"),
            Self::UnknownBinding => formatter.write_str("runtime output names an unknown binding"),
            Self::StaleRegistration => {
                formatter.write_str("runtime output has a stale registration token")
            }
            Self::ContractMismatch { field } => {
                write!(formatter, "runtime output has a mismatched {field}")
            }
            Self::UnpublishedTopic(topic) => {
                write!(formatter, "runtime output names unpublished topic {topic}")
            }
            Self::InvalidTopics => formatter.write_str("runtime output has invalid topics"),
            Self::InvalidHealthScope => formatter.write_str("runtime health has an invalid scope"),
            Self::InvalidDemandWatermark => {
                formatter.write_str("runtime observation result has an invalid demand watermark")
            }
            Self::HealthDetailTooLarge => formatter.write_str("runtime health detail is too large"),
            Self::ObservationDiagnosticTooLarge => {
                formatter.write_str("runtime observation diagnostic is too large")
            }
        }
    }
}

impl std::error::Error for FenceError {}

#[derive(Debug)]
pub enum AcceptedOutput<'a> {
    Publication(AcceptedPublication<'a>),
    Health(AcceptedHealth<'a>),
    ObservationResult(AcceptedObservationResult<'a>),
}

#[derive(Debug)]
pub struct AcceptedPublication<'a> {
    binding_id: &'a BindingId,
    target: &'a SnapshotTarget,
    bytes: &'a SnapshotBytes,
    selected_topics: Vec<String>,
    facts: &'a [ResourceFact],
}

impl<'a> AcceptedPublication<'a> {
    pub fn binding_id(&self) -> &BindingId {
        self.binding_id
    }

    pub fn target(&self) -> &SnapshotTarget {
        self.target
    }

    pub fn selected_topics(&self) -> &[String] {
        &self.selected_topics
    }

    pub fn facts(&self) -> &[ResourceFact] {
        self.facts
    }

    fn prepare(self) -> Result<PreparedPublication<'a>, PublicationError> {
        prepare_snapshot(
            self.target,
            self.bytes.as_slice(),
            self.selected_topics,
            self.facts.to_vec(),
        )
    }
}

#[derive(Debug)]
pub struct AcceptedObservationResult<'a> {
    binding_id: &'a BindingId,
    demand_watermark: u64,
    result: AcceptedObservation<'a>,
}

impl<'a> AcceptedObservationResult<'a> {
    pub fn into_parts(self) -> (&'a BindingId, u64, AcceptedObservation<'a>) {
        (self.binding_id, self.demand_watermark, self.result)
    }
}

#[derive(Debug)]
pub enum AcceptedObservation<'a> {
    Unchanged,
    Failed { diagnostic: Option<&'a str> },
    Published(AcceptedPublication<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedHealth<'a> {
    binding_id: Option<&'a BindingId>,
    state: RuntimeHealthState,
    detail: Option<&'a str>,
}

impl<'a> AcceptedHealth<'a> {
    pub fn binding_id(&self) -> Option<&'a BindingId> {
        self.binding_id
    }

    pub fn state(&self) -> RuntimeHealthState {
        self.state
    }

    pub fn detail(&self) -> Option<&'a str> {
        self.detail
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotChange {
    First,
    Equal,
    Changed { previous: SnapshotDigest },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationOutcome {
    digest: SnapshotDigest,
    change: SnapshotChange,
    selected_topics: Vec<String>,
    facts: Vec<ResourceFact>,
}

impl PublicationOutcome {
    pub fn digest(&self) -> SnapshotDigest {
        self.digest
    }

    pub fn change(&self) -> SnapshotChange {
        self.change
    }

    pub fn selected_topics(&self) -> &[String] {
        &self.selected_topics
    }

    pub fn facts(&self) -> &[ResourceFact] {
        &self.facts
    }
    pub fn invalidating(&self) -> bool {
        self.change != SnapshotChange::Equal && !self.selected_topics.is_empty()
    }
}

#[derive(Debug)]
pub enum PublicationError {
    SnapshotTooLarge { actual: usize },
    UnsafeTarget(PathError),
    ExistingSnapshotTooLarge,
    SnapshotNotRegular,
    Io(io::Error),
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotTooLarge { actual } => write!(
                formatter,
                "snapshot is {actual} bytes; maximum is {MAX_SNAPSHOT_BYTES}"
            ),
            Self::UnsafeTarget(error) => write!(formatter, "unsafe snapshot target: {error}"),
            Self::ExistingSnapshotTooLarge => formatter.write_str("existing snapshot is too large"),
            Self::SnapshotNotRegular => {
                formatter.write_str("snapshot target is not a real regular file")
            }
            Self::Io(error) => write!(formatter, "snapshot publication failed: {error}"),
        }
    }
}

impl std::error::Error for PublicationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsafeTarget(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct PreparedPublication<'a> {
    target: &'a SnapshotTarget,
    bytes: &'a [u8],
    outcome: PublicationOutcome,
}

impl PreparedPublication<'_> {
    fn commit(&self) -> Result<(), PublicationError> {
        if self.outcome.change == SnapshotChange::Equal {
            return Ok(());
        }
        let parent = self
            .target
            .relative
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let leaf = self.target.relative.file_name().ok_or_else(|| {
            PublicationError::UnsafeTarget(PathError::UnsafeCarrier("missing leaf"))
        })?;
        let directory =
            open_absolute_dir_beneath(&self.target.root, parent).map_err(PublicationError::Io)?;
        atomic_replace_at(&directory, leaf, self.bytes).map_err(PublicationError::Io)
    }
}

fn prepare_snapshot<'a>(
    target: &'a SnapshotTarget,
    bytes: &'a [u8],
    selected_topics: Vec<String>,
    facts: Vec<ResourceFact>,
) -> Result<PreparedPublication<'a>, PublicationError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(PublicationError::SnapshotTooLarge {
            actual: bytes.len(),
        });
    }
    let parent = target.relative.parent().unwrap_or_else(|| Path::new(""));
    ensure_absolute_dir_beneath(&target.root, parent).map_err(PublicationError::Io)?;
    let previous = target.current_digest()?;
    let digest = SnapshotDigest::of(bytes);
    let change = match previous {
        None => SnapshotChange::First,
        Some(previous) if previous == digest => SnapshotChange::Equal,
        Some(previous) => SnapshotChange::Changed { previous },
    };
    Ok(PreparedPublication {
        target,
        bytes,
        outcome: PublicationOutcome {
            digest,
            change,
            selected_topics,
            facts,
        },
    })
}
#[cfg(test)]
fn publish_snapshot(
    target: &SnapshotTarget,
    bytes: &[u8],
    selected_topics: Vec<String>,
    facts: Vec<ResourceFact>,
) -> Result<PublicationOutcome, PublicationError> {
    let prepared = prepare_snapshot(target, bytes, selected_topics, facts)?;
    prepared.commit()?;
    Ok(prepared.outcome)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatchUpState {
    current_snapshot_digest: Option<SnapshotDigest>,
    last_delivered_digest: Option<SnapshotDigest>,
    pending_relevant_change: bool,
    #[serde(default)]
    pending_from_last_intent: bool,
    #[serde(default)]
    pending_selected_topics: Vec<String>,
    #[serde(default)]
    pending_facts: Vec<ResourceFact>,
    deliverable: bool,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    last_intent: Option<PublicationIntent>,
}

impl CatchUpState {
    pub fn current_snapshot_digest(&self) -> Option<SnapshotDigest> {
        self.current_snapshot_digest
    }

    pub fn last_delivered_digest(&self) -> Option<SnapshotDigest> {
        self.last_delivered_digest
    }

    pub fn pending_relevant_change(&self) -> bool {
        self.pending_relevant_change
    }

    pub fn pending_selected_topics(&self) -> &[String] {
        if self.pending_from_last_intent {
            self.last_intent
                .as_ref()
                .map_or(&[], |intent| intent.selected_topics.as_slice())
        } else {
            &self.pending_selected_topics
        }
    }

    pub fn pending_facts(&self) -> &[ResourceFact] {
        if self.pending_from_last_intent {
            self.last_intent
                .as_ref()
                .map_or(&[], |intent| intent.facts.as_slice())
        } else {
            &self.pending_facts
        }
    }
    pub fn deliverable(&self) -> bool {
        self.deliverable
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn proposal_fence(&self) -> ProposalFence {
        ProposalFence::new(
            self.generation,
            self.revision,
            self.current_snapshot_digest,
        )
    }

    #[cfg(test)]
    pub fn last_commit(&self) -> Option<PublicationCommit> {
        self.last_intent
            .as_ref()
            .and_then(|intent| intent.commit().ok())
    }

    fn validate(&self) -> Result<(), CatchUpError> {
        if self.pending_relevant_change && self.current_snapshot_digest.is_none() {
            return Err(CatchUpError::InvalidState(
                "pending relevance requires a current snapshot digest",
            ));
        }
        if self.pending_from_last_intent {
            if !self.pending_relevant_change {
                return Err(CatchUpError::InvalidState(
                    "pending last intent requires pending relevance",
                ));
            }
            if !self.pending_selected_topics.is_empty() || !self.pending_facts.is_empty() {
                return Err(CatchUpError::InvalidState(
                    "pending last intent must not duplicate its semantic envelope",
                ));
            }
            if self
                .last_intent
                .as_ref()
                .is_none_or(|intent| intent.selected_topics.is_empty())
            {
                return Err(CatchUpError::InvalidState(
                    "pending last intent requires a relevant durable intent",
                ));
            }
        } else if self.pending_relevant_change != !self.pending_selected_topics.is_empty() {
            return Err(CatchUpError::InvalidState(
                "pending relevance and selected topics disagree",
            ));
        }
        validate_persisted_topics(self.pending_selected_topics())?;
        validate_persisted_facts(self.pending_facts())?;
        if !self.pending_relevant_change
            && (!self.pending_selected_topics.is_empty() || !self.pending_facts.is_empty())
        {
            return Err(CatchUpError::InvalidState(
                "pending semantic envelope requires a pending relevant change",
            ));
        }
        if let Some(intent) = self.last_intent.as_ref() {
            intent.validate()?;
            let commit = intent.commit()?;
            if commit.generation() > self.generation {
                return Err(CatchUpError::InvalidState(
                    "last intent generation is newer than catch-up state",
                ));
            }
            if commit.revision() > self.revision {
                return Err(CatchUpError::InvalidState(
                    "last intent revision is newer than catch-up state",
                ));
            }
            if commit.revision() == self.revision
                && self.current_snapshot_digest != Some(commit.digest())
            {
                return Err(CatchUpError::InvalidState(
                    "last intent digest differs from current snapshot",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRequest {
    digest: SnapshotDigest,
    selected_topics: Vec<String>,
    facts: Vec<ResourceFact>,
}

impl DeliveryRequest {
    pub fn digest(&self) -> SnapshotDigest {
        self.digest
    }

    pub fn selected_topics(&self) -> &[String] {
        &self.selected_topics
    }

    pub fn facts(&self) -> &[ResourceFact] {
        &self.facts
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationIntent {
    proposal_id: ProposalId,
    binding_id: BindingId,
    generation: u64,
    expected_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_digest: Option<SnapshotDigest>,
    digest: SnapshotDigest,
    selected_topics: Vec<String>,
    #[serde(default)]
    facts: Vec<ResourceFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyPublicationIntent {
    digest: SnapshotDigest,
    selected_topics: Vec<String>,
    #[serde(default)]
    facts: Vec<ResourceFact>,
}

impl LegacyPublicationIntent {
    fn validate(&self) -> Result<(), CatchUpError> {
        validate_persisted_topics(&self.selected_topics)?;
        validate_persisted_facts(&self.facts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredPublicationIntent {
    Current(PublicationIntent),
    Legacy(LegacyPublicationIntent),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposalIdentity<'a> {
    binding_id: &'a BindingId,
    generation: u64,
    expected_revision: u64,
    prior_digest: Option<SnapshotDigest>,
    digest: SnapshotDigest,
    selected_topics: &'a [String],
    facts: &'a [ResourceFact],
}

impl PublicationIntent {

    fn commit(&self) -> Result<PublicationCommit, CatchUpError> {
        let revision = self
            .expected_revision
            .checked_add(1)
            .ok_or(CatchUpError::InvalidState(
                "publication revision counter exhausted",
            ))?;
        Ok(PublicationCommit::new(
            self.proposal_id,
            self.generation,
            revision,
            self.digest,
        ))
    }

    fn validate(&self) -> Result<(), CatchUpError> {
        let identity = ProposalIdentity {
            binding_id: &self.binding_id,
            generation: self.generation,
            expected_revision: self.expected_revision,
            prior_digest: self.prior_digest,
            digest: self.digest,
            selected_topics: &self.selected_topics,
            facts: &self.facts,
        };
        if self.proposal_id != proposal_id(&identity) {
            return Err(CatchUpError::InvalidState(
                "publication intent proposal id is not deterministic",
            ));
        }
        validate_persisted_topics(&self.selected_topics)?;
        validate_persisted_facts(&self.facts)
    }
}

fn proposal_id(identity: &ProposalIdentity<'_>) -> ProposalId {
    let encoded = serde_json::to_vec(identity)
        .expect("proposal identity contains only infallibly serializable values");
    let mut bytes = Vec::with_capacity(PROPOSAL_ID_DOMAIN.len() + encoded.len());
    bytes.extend_from_slice(PROPOSAL_ID_DOMAIN);
    bytes.extend_from_slice(&encoded);
    ProposalId::of(&bytes)
}

fn validate_persisted_topics(topics: &[String]) -> Result<(), CatchUpError> {
    let mut unique = BTreeSet::new();
    for topic in topics {
        if topic.is_empty() || !unique.insert(topic.as_str()) {
            return Err(CatchUpError::InvalidState(
                "persisted selected topics must be non-empty and unique",
            ));
        }
    }
    Ok(())
}
fn validate_persisted_facts(facts: &[ResourceFact]) -> Result<(), CatchUpError> {
    if facts.len() > MAX_FACTS || facts.iter().any(|fact| fact.validate().is_err()) {
        return Err(CatchUpError::InvalidState("persisted facts are invalid"));
    }
    Ok(())
}

#[derive(Debug)]
pub struct CatchUp {
    directory: File,
    state: CatchUpState,
}

impl CatchUp {
    pub fn open(state_directory: &Path) -> Result<Self, CatchUpError> {
        validate_absolute_path(state_directory).map_err(CatchUpError::UnsafeStateDirectory)?;
        let directory = open_absolute_dir(state_directory).map_err(CatchUpError::Io)?;
        let state = read_catch_up_state(&directory)?;
        Ok(Self { directory, state })
    }

    pub fn open_for_snapshot(
        state_directory: &Path,
        target: &SnapshotTarget,
    ) -> Result<Self, CatchUpError> {
        let mut catch_up = Self::open(state_directory)?;
        catch_up.reconcile_snapshot(target)?;
        Ok(catch_up)
    }

    /// Open for an explicit binding replacement without first reconciling the previous generation.
    ///
    /// This is the supervisor hot-replacement entrypoint: it can recover a carrier that diverged
    /// out of band even when ordinary [`Self::open_for_snapshot`] correctly fails closed.
    pub fn open_for_generation_advance(
        state_directory: &Path,
        target: &SnapshotTarget,
    ) -> Result<Self, CatchUpError> {
        let mut catch_up = Self::open(state_directory)?;
        catch_up.advance_generation(target)?;
        Ok(catch_up)
    }

    pub fn state(&self) -> &CatchUpState {
        &self.state
    }

    /// Capture the compare-and-swap fence for provider work computed outside the host lock.
    pub fn proposal_fence(&self) -> ProposalFence {
        self.state.proposal_fence()
    }

    /// Advance the binding generation, fencing every proposal captured by the replaced binding.
    ///
    /// Unlike ordinary reconciliation, this explicit recovery transition treats the canonical
    /// carrier as authoritative: it adopts its current digest (or absence), clears delivery state
    /// whose semantic intent can no longer be proven, and invalidates the prior generation's
    /// durable intent. Call it on [`Self::open`] or use [`Self::open_for_generation_advance`] so a
    /// divergence cannot prevent the replacement that recovers it.
    pub fn advance_generation(
        &mut self,
        target: &SnapshotTarget,
    ) -> Result<ProposalFence, CatchUpError> {
        let _lock = lock_publication(&self.directory)?;
        self.reload()?;
        let observed = target.current_digest().map_err(CatchUpError::Publication)?;
        let intent = self.read_publication_intent()?;
        let mut next = self.state.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(CatchUpError::InvalidState(
                "binding generation counter exhausted",
            ))?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(CatchUpError::InvalidState(
                "publication revision counter exhausted",
            ))?;
        next.current_snapshot_digest = observed;
        next.last_intent = None;
        next.pending_relevant_change = false;
        next.pending_from_last_intent = false;
        next.pending_selected_topics.clear();
        next.pending_facts.clear();
        self.commit_state(next)?;
        if intent.is_some() {
            self.clear_publication_intent()?;
        }
        Ok(self.state.proposal_fence())
    }

    /// Preserve the PR #404 single-writer publication API.
    ///
    /// New provider boundaries should capture [`Self::proposal_fence`] before computing and use
    /// [`Self::commit_proposal`]. This method captures under the same lock for existing runtimes,
    /// which never claimed a provider-side compare-and-swap contract.
    pub fn publish(
        &mut self,
        publication: AcceptedPublication<'_>,
    ) -> Result<(PublicationOutcome, Option<DeliveryRequest>), PublicationTransactionError> {
        let _lock = lock_publication(&self.directory)
            .map_err(PublicationTransactionError::CatchUp)?;
        self.reload()
            .map_err(PublicationTransactionError::CatchUp)?;
        self.reconcile_snapshot_locked(publication.target)
            .map_err(PublicationTransactionError::CatchUp)?;
        let fence = self.state.proposal_fence();
        let (_, outcome) = self.commit_proposal_locked(fence, publication)?;
        let outcome = outcome.expect("a current fence cannot reject its own publication");
        Ok((outcome, self.pending_delivery()))
    }

    /// Validate and commit one provider proposal against its captured generation, revision, and
    /// prior digest.
    /// Delivery remains level-triggered and separate through [`Self::pending_delivery`] and
    /// [`Self::acknowledge_delivery`].
    ///
    /// The state directory uses one persistent advisory lock shared by every cooperating writer.
    /// Publication relies on same-directory rename atomicity and file-plus-parent-directory
    /// `fsync`. The durable intent is written first but is eligible only when its digest matches
    /// the canonical carrier; therefore a crash before the carrier rename exposes the old state,
    /// while a crash after it is replayed on restart. Filesystems that do not honor those POSIX
    /// rename, `fsync`, and `flock` semantics are unsupported.
    pub fn commit_proposal(
        &mut self,
        fence: ProposalFence,
        publication: AcceptedPublication<'_>,
    ) -> Result<ProposalCommit, PublicationTransactionError> {
        let _lock = lock_publication(&self.directory)
            .map_err(PublicationTransactionError::CatchUp)?;
        self.reload()
            .map_err(PublicationTransactionError::CatchUp)?;
        self.reconcile_snapshot_locked(publication.target)
            .map_err(PublicationTransactionError::CatchUp)?;
        self.commit_proposal_locked(fence, publication)
            .map(|(commit, _)| commit)
    }

    fn commit_proposal_locked(
        &mut self,
        fence: ProposalFence,
        publication: AcceptedPublication<'_>,
    ) -> Result<(ProposalCommit, Option<PublicationOutcome>), PublicationTransactionError> {
        let binding_id = publication.binding_id;
        let digest = SnapshotDigest::of(publication.bytes.as_slice());
        let identity = ProposalIdentity {
            binding_id,
            generation: fence.generation(),
            expected_revision: fence.revision(),
            prior_digest: fence.prior_digest(),
            digest,
            selected_topics: &publication.selected_topics,
            facts: publication.facts,
        };
        let proposal_id = proposal_id(&identity);

        if let Some(previous) = self.state.last_intent.as_ref()
            && previous.proposal_id == proposal_id
        {
            let commit = previous.commit().map_err(PublicationTransactionError::CatchUp)?;
            return Ok((ProposalCommit::AlreadyCommitted(commit), None));
        }
        if fence.generation() != self.state.generation {
            return Ok((
                ProposalCommit::StaleGeneration {
                    actual_generation: self.state.generation,
                    actual_revision: self.state.revision,
                },
                None,
            ));
        }
        if fence.revision() != self.state.revision
            || fence.prior_digest() != self.state.current_snapshot_digest
        {
            return Ok((
                ProposalCommit::StalePrior {
                    actual_generation: self.state.generation,
                    actual_revision: self.state.revision,
                    actual_digest: self.state.current_snapshot_digest,
                },
                None,
            ));
        }

        let prepared = publication
            .prepare()
            .map_err(PublicationTransactionError::Publication)?;
        let outcome = prepared.outcome.clone();
        debug_assert_eq!(digest, outcome.digest);

        if outcome.change == SnapshotChange::Equal {
            return Ok((
                ProposalCommit::Unchanged {
                    generation: self.state.generation,
                    revision: self.state.revision,
                    digest: outcome.digest,
                },
                Some(outcome),
            ));
        }
        let intent = PublicationIntent {
            proposal_id,
            binding_id: binding_id.clone(),
            generation: fence.generation(),
            expected_revision: fence.revision(),
            prior_digest: fence.prior_digest(),
            digest,
            selected_topics: outcome.selected_topics.clone(),
            facts: outcome.facts.clone(),
        };
        intent
            .commit()
            .map_err(PublicationTransactionError::CatchUp)?;

        self.write_publication_intent(&intent)
            .map_err(PublicationTransactionError::CatchUp)?;
        publication_checkpoint("after-intent-before-carrier");
        prepared
            .commit()
            .map_err(PublicationTransactionError::Publication)?;
        publication_checkpoint("after-carrier-before-state");
        let commit = self
            .record_intent(&intent)
            .map_err(PublicationTransactionError::CatchUp)?;
        self.clear_publication_intent()
            .map_err(PublicationTransactionError::CatchUp)?;
        publication_checkpoint("after-state-before-ack");
        Ok((ProposalCommit::Committed(commit), Some(outcome)))
    }

    pub fn reconcile_snapshot(
        &mut self,
        target: &SnapshotTarget,
    ) -> Result<Option<DeliveryRequest>, CatchUpError> {
        let _lock = lock_publication(&self.directory)?;
        self.reload()?;
        self.reconcile_snapshot_locked(target)?;
        Ok(self.pending_delivery())
    }

    fn reconcile_snapshot_locked(&mut self, target: &SnapshotTarget) -> Result<(), CatchUpError> {
        let observed = target.current_digest().map_err(CatchUpError::Publication)?;
        let intent = self.read_publication_intent()?;

        match intent.as_ref() {
            Some(StoredPublicationIntent::Current(intent)) => {
                if observed == Some(intent.digest) {
                    if self
                        .state
                        .last_intent
                        .as_ref()
                        .is_none_or(|committed| committed.proposal_id != intent.proposal_id)
                    {
                        if self.state.generation != intent.generation
                            || self.state.revision != intent.expected_revision
                            || self.state.current_snapshot_digest != intent.prior_digest
                        {
                            return Err(CatchUpError::InvalidState(
                                "published intent does not follow the authoritative fence",
                            ));
                        }
                        self.record_intent(intent)?;
                    }
                } else {
                    if self.state.revision > 0
                        && observed != self.state.current_snapshot_digest
                    {
                        return Err(CatchUpError::InvalidState(
                            "canonical snapshot differs from the authoritative committed digest",
                        ));
                    }
                    if observed.is_none() && self.state.pending_relevant_change {
                        return Err(CatchUpError::InvalidState(
                            "a pending invalidation has no readable canonical snapshot",
                        ));
                    }
                    if observed != self.state.current_snapshot_digest {
                        let mut next = self.state.clone();
                        next.current_snapshot_digest = observed;
                        self.commit_state(next)?;
                    }
                }
                self.clear_publication_intent()?;
            }
            Some(StoredPublicationIntent::Legacy(intent)) => {
                if self.state.revision > 0 {
                    return Err(CatchUpError::InvalidState(
                        "legacy publication intent conflicts with current authoritative state",
                    ));
                }
                let mut next = self.state.clone();
                if observed == Some(intent.digest) {
                    next.current_snapshot_digest = observed;
                    if !intent.selected_topics.is_empty() {
                        next.pending_relevant_change = true;
                        next.pending_from_last_intent = false;
                        next.pending_selected_topics = intent.selected_topics.clone();
                        next.pending_facts = intent.facts.clone();
                    }
                } else {
                    if observed.is_none() && next.pending_relevant_change {
                        return Err(CatchUpError::InvalidState(
                            "a pending invalidation has no readable canonical snapshot",
                        ));
                    }
                    next.current_snapshot_digest = observed;
                }
                if next != self.state {
                    self.commit_state(next)?;
                }
                self.clear_publication_intent()?;
            }
            None => {
                if observed != self.state.current_snapshot_digest {
                    if self.state.revision > 0 {
                        return Err(CatchUpError::InvalidState(
                            "canonical snapshot differs from the authoritative committed digest",
                        ));
                    }
                    if observed.is_none() && self.state.pending_relevant_change {
                        return Err(CatchUpError::InvalidState(
                            "a pending invalidation has no readable canonical snapshot",
                        ));
                    }
                    let mut next = self.state.clone();
                    next.current_snapshot_digest = observed;
                    self.commit_state(next)?;
                }
            }
        }
        Ok(())
    }

    pub fn set_deliverable(
        &mut self,
        deliverable: bool,
    ) -> Result<Option<DeliveryRequest>, CatchUpError> {
        let _lock = lock_publication(&self.directory)?;
        self.reload()?;
        if self.state.deliverable != deliverable {
            let mut next = self.state.clone();
            next.deliverable = deliverable;
            self.commit_state(next)?;
        }
        Ok(self.pending_delivery())
    }

    pub fn pending_delivery(&self) -> Option<DeliveryRequest> {
        if !self.state.deliverable || !self.state.pending_relevant_change {
            return None;
        }
        Some(DeliveryRequest {
            digest: self.state.current_snapshot_digest?,
            selected_topics: self.state.pending_selected_topics().to_vec(),
            facts: self.state.pending_facts().to_vec(),
        })
    }

    pub fn acknowledge_delivery(&mut self, digest: SnapshotDigest) -> Result<bool, CatchUpError> {
        let _lock = lock_publication(&self.directory)?;
        self.reload()?;
        if !self.state.pending_relevant_change || self.state.current_snapshot_digest != Some(digest)
        {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.last_delivered_digest = Some(digest);
        next.pending_relevant_change = false;
        next.pending_from_last_intent = false;
        next.pending_selected_topics.clear();
        next.pending_facts.clear();
        self.commit_state(next)?;
        Ok(true)
    }

    fn record_intent(
        &mut self,
        intent: &PublicationIntent,
    ) -> Result<PublicationCommit, CatchUpError> {
        let commit = intent.commit()?;
        let mut next = self.state.clone();
        next.current_snapshot_digest = Some(intent.digest);
        next.revision = commit.revision();
        if intent.selected_topics.is_empty() && next.pending_from_last_intent {
            let previous = next.last_intent.as_ref().ok_or(CatchUpError::InvalidState(
                "pending last intent is absent",
            ))?;
            let selected_topics = previous.selected_topics.clone();
            let facts = previous.facts.clone();
            next.pending_selected_topics = selected_topics;
            next.pending_facts = facts;
            next.pending_from_last_intent = false;
        }
        next.last_intent = Some(intent.clone());
        if !intent.selected_topics.is_empty() {
            next.pending_relevant_change = true;
            next.pending_from_last_intent = true;
            next.pending_selected_topics.clear();
            next.pending_facts.clear();
        }
        self.commit_state(next)?;
        Ok(commit)
    }

    #[cfg(test)]
    fn record_publication(
        &mut self,
        outcome: &PublicationOutcome,
    ) -> Result<Option<DeliveryRequest>, CatchUpError> {
        let mut next = self.state.clone();
        next.current_snapshot_digest = Some(outcome.digest);
        if outcome.invalidating() {
            next.pending_relevant_change = true;
            next.pending_selected_topics = outcome.selected_topics.clone();
            next.pending_facts = outcome.facts.clone();
        }
        self.commit_state(next)?;
        Ok(self.pending_delivery())
    }

    fn read_publication_intent(&self) -> Result<Option<StoredPublicationIntent>, CatchUpError> {
        match read_regular_optional_at(
            &self.directory,
            OsStr::new(PUBLICATION_INTENT_FILE),
            MAX_CATCH_UP_FILE_BYTES,
        ) {
            Ok(Some(bytes)) => match serde_json::from_slice::<PublicationIntent>(&bytes) {
                Ok(intent) => {
                    intent.validate()?;
                    Ok(Some(StoredPublicationIntent::Current(intent)))
                }
                Err(current_error) => {
                    let legacy = serde_json::from_slice::<LegacyPublicationIntent>(&bytes)
                        .map_err(|_| CatchUpError::Json(current_error))?;
                    legacy.validate()?;
                    Ok(Some(StoredPublicationIntent::Legacy(legacy)))
                }
            },
            Ok(None) => Ok(None),
            Err(BoundedReadError::TooLarge) => Err(CatchUpError::IntentTooLarge),
            Err(BoundedReadError::NotRegular) => Err(CatchUpError::IntentNotRegular),
            Err(BoundedReadError::Io(error)) => Err(CatchUpError::Io(error)),
        }
    }

    fn write_publication_intent(&self, intent: &PublicationIntent) -> Result<(), CatchUpError> {
        intent.validate()?;
        let mut bytes = serde_json::to_vec(intent).map_err(CatchUpError::Json)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CATCH_UP_FILE_BYTES {
            return Err(CatchUpError::IntentTooLarge);
        }
        atomic_replace_at(&self.directory, OsStr::new(PUBLICATION_INTENT_FILE), &bytes)
            .map_err(CatchUpError::Io)
    }

    fn clear_publication_intent(&self) -> Result<(), CatchUpError> {
        remove_optional_at(&self.directory, OsStr::new(PUBLICATION_INTENT_FILE))
            .map_err(CatchUpError::Io)
    }

    fn reload(&mut self) -> Result<(), CatchUpError> {
        self.state = read_catch_up_state(&self.directory)?;
        Ok(())
    }

    fn commit_state(&mut self, state: CatchUpState) -> Result<(), CatchUpError> {
        state.validate()?;
        let mut bytes = serde_json::to_vec(&state).map_err(CatchUpError::Json)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CATCH_UP_FILE_BYTES {
            return Err(CatchUpError::StateTooLarge);
        }
        atomic_replace_at(&self.directory, OsStr::new(CATCH_UP_FILE), &bytes)
            .map_err(CatchUpError::Io)?;
        self.state = state;
        Ok(())
    }
}

fn read_catch_up_state(directory: &File) -> Result<CatchUpState, CatchUpError> {
    let state = match read_regular_optional_at(
        directory,
        OsStr::new(CATCH_UP_FILE),
        MAX_CATCH_UP_FILE_BYTES,
    ) {
        Ok(Some(bytes)) => {
            serde_json::from_slice::<CatchUpState>(&bytes).map_err(CatchUpError::Json)?
        }
        Ok(None) => CatchUpState::default(),
        Err(BoundedReadError::TooLarge) => return Err(CatchUpError::StateTooLarge),
        Err(BoundedReadError::NotRegular) => return Err(CatchUpError::StateNotRegular),
        Err(BoundedReadError::Io(error)) => return Err(CatchUpError::Io(error)),
    };
    state.validate()?;
    Ok(state)
}

fn lock_publication(directory: &File) -> Result<File, CatchUpError> {
    let leaf = CString::new(PUBLICATION_LOCK_FILE).expect("lock filename contains no NUL");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(CatchUpError::Io(io::Error::last_os_error()));
    }
    let lock = unsafe { File::from_raw_fd(descriptor) };
    if !lock.metadata().map_err(CatchUpError::Io)?.is_file() {
        return Err(CatchUpError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication lock is not a regular file",
        )));
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } < 0 {
        return Err(CatchUpError::Io(io::Error::last_os_error()));
    }
    Ok(lock)
}

fn publication_checkpoint(stage: &str) {
    #[cfg(not(test))]
    let _ = stage;
    #[cfg(test)]
    if std::env::var_os("ST2_RESOURCE_PUBLICATION_CRASH_STAGE").as_deref()
        == Some(OsStr::new(stage))
    {
        std::process::exit(match stage {
            "after-intent-before-carrier" => 71,
            "after-carrier-before-state" => 72,
            "after-state-before-ack" => 73,
            _ => 74,
        });
    }
}

#[derive(Debug)]
pub enum PublicationTransactionError {
    Publication(PublicationError),
    CatchUp(CatchUpError),
}

impl fmt::Display for PublicationTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Publication(error) => fmt::Display::fmt(error, formatter),
            Self::CatchUp(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for PublicationTransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Publication(error) => Some(error),
            Self::CatchUp(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub enum CatchUpError {
    UnsafeStateDirectory(&'static str),
    StateTooLarge,
    StateNotRegular,
    IntentTooLarge,
    IntentNotRegular,
    InvalidState(&'static str),
    Publication(PublicationError),
    Json(serde_json::Error),
    Io(io::Error),
}

impl fmt::Display for CatchUpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeStateDirectory(reason) => {
                write!(formatter, "unsafe catch-up state directory: {reason}")
            }
            Self::StateTooLarge => formatter.write_str("catch-up state file is too large"),
            Self::StateNotRegular => {
                formatter.write_str("catch-up state path is not a real regular file")
            }
            Self::IntentTooLarge => formatter.write_str("publication intent file is too large"),
            Self::IntentNotRegular => {
                formatter.write_str("publication intent path is not a real regular file")
            }
            Self::InvalidState(reason) => write!(formatter, "invalid catch-up state: {reason}"),
            Self::Publication(error) => {
                write!(formatter, "snapshot reconciliation failed: {error}")
            }
            Self::Json(error) => write!(formatter, "invalid catch-up state JSON: {error}"),
            Self::Io(error) => write!(formatter, "catch-up state I/O failed: {error}"),
        }
    }
}

impl std::error::Error for CatchUpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Publication(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

enum BoundedReadError {
    TooLarge,
    NotRegular,
    Io(io::Error),
}

fn open_absolute_dir(path: &Path) -> io::Result<File> {
    open_absolute_dir_beneath(path, Path::new(""))
}

fn open_absolute_dir_beneath(root: &Path, relative: &Path) -> io::Result<File> {
    validate_absolute_path(root).map_err(invalid_input)?;
    validate_empty_or_relative_path(relative).map_err(invalid_input)?;
    let slash = c"/";
    let descriptor = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in root
        .components()
        .chain(relative.components())
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(name) => Some(name),
            _ => None,
        })
    {
        directory = openat_directory(&directory, component)?;
    }
    Ok(directory)
}

fn ensure_absolute_dir_beneath(root: &Path, relative: &Path) -> io::Result<File> {
    validate_absolute_path(root).map_err(invalid_input)?;
    validate_empty_or_relative_path(relative).map_err(invalid_input)?;
    let mut directory = open_absolute_dir(root)?;
    for component in relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
    {
        let name = c_string(component)?;
        let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        directory = openat_directory(&directory, component)?;
    }
    Ok(directory)
}

fn validate_empty_or_relative_path(path: &Path) -> Result<(), &'static str> {
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err("path contains a non-normal component");
        }
    }
    Ok(())
}

fn invalid_input(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn openat_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let name = c_string(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn read_regular_optional_at(
    directory: &File,
    name: &OsStr,
    maximum: usize,
) -> Result<Option<Vec<u8>>, BoundedReadError> {
    let name = c_string(name).map_err(BoundedReadError::Io)?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(BoundedReadError::Io(error));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(BoundedReadError::Io)?;
    if !metadata.is_file() {
        return Err(BoundedReadError::NotRegular);
    }
    if metadata.len() > maximum as u64 {
        return Err(BoundedReadError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() > maximum {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(Some(bytes))
}

fn atomic_replace_at(directory: &File, leaf: &OsStr, bytes: &[u8]) -> io::Result<()> {
    ensure_regular_or_absent_at(directory, leaf)?;
    let leaf = c_string(leaf)?;
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = CString::new(format!(
        ".resource-profile.tmp-{}-{sequence}",
        std::process::id()
    ))
    .expect("generated temporary name contains no NUL");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        let renamed = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                leaf.as_ptr(),
            )
        };
        if renamed < 0 {
            return Err(io::Error::last_os_error());
        }
        directory.sync_all()
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}
fn remove_optional_at(directory: &File, leaf: &OsStr) -> io::Result<()> {
    let leaf = c_string(leaf)?;
    let removed = unsafe { libc::unlinkat(directory.as_raw_fd(), leaf.as_ptr(), 0) };
    if removed < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error);
    }
    directory.sync_all()
}

fn ensure_regular_or_absent_at(directory: &File, leaf: &OsStr) -> io::Result<()> {
    match read_regular_optional_at(directory, leaf, 0) {
        Ok(None) | Ok(Some(_)) | Err(BoundedReadError::TooLarge) => Ok(()),
        Err(BoundedReadError::NotRegular) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target is not a regular file",
        )),
        Err(BoundedReadError::Io(error)) => Err(error),
    }
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::io::{BufRead as _, BufReader};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    fn id<T>(value: &str, make: impl FnOnce(String) -> Result<T, OpaqueIdError>) -> T {
        make(value.to_owned()).unwrap()
    }

    fn owner(value: &str) -> RuntimeOwner {
        RuntimeOwner::new(
            id(value, RuntimeIncarnation::new),
            id(&format!("claim-{value}"), OwnerClaim::new),
        )
    }

    fn binding_id(value: &str) -> BindingId {
        id(value, BindingId::new)
    }

    fn token(value: &str) -> RegistrationToken {
        id(value, RegistrationToken::new)
    }

    fn target(root: &Path) -> SnapshotTarget {
        SnapshotTarget::new(fs::canonicalize(root).unwrap(), "snapshot.json").unwrap()
    }

    fn contract(selected: &[&str]) -> PublicationContract {
        PublicationContract::new(
            "schema.v1",
            "application/json",
            ["selected", "ignored"].map(str::to_owned),
            TopicSelection::new(selected.iter().map(|topic| (*topic).to_owned())).unwrap(),
        )
        .unwrap()
    }

    fn registration(root: &Path, registration: &str) -> BindingRegistration {
        BindingRegistration::new(
            binding_id("binding"),
            token(registration),
            target(root),
            contract(&["selected"]),
        )
    }

    fn publication(
        owner: RuntimeOwner,
        registration: &str,
        bytes: &[u8],
        topics: &[&str],
    ) -> RuntimeMessage {
        RuntimeMessage::Publish {
            owner,
            binding_id: binding_id("binding"),
            registration: token(registration),
            publication: publication_payload(bytes, topics),
        }
    }

    fn publication_payload(bytes: &[u8], topics: &[&str]) -> Publication {
        Publication {
            schema_id: "schema.v1".to_owned(),
            media_type: "application/json".to_owned(),
            bytes: SnapshotBytes::new(bytes.to_vec()).unwrap(),
            topics: topics.iter().map(|topic| (*topic).to_owned()).collect(),
            facts: None,
        }
    }

    fn accepted_publication<'a>(
        lifecycle: &'a RuntimeLifecycle,
        message: &'a RuntimeMessage,
    ) -> AcceptedPublication<'a> {
        match lifecycle.accept_output(message).unwrap() {
            AcceptedOutput::Publication(publication) => publication,
            AcceptedOutput::Health(_) => panic!("expected publication"),
            AcceptedOutput::ObservationResult(_) => panic!("expected publication"),
        }
    }

    #[test]
    fn stale_owner_and_registration_are_fenced() {
        let directory = tempfile::tempdir().unwrap();
        let current = owner("current");
        let stale = owner("stale");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(current.clone());
        lifecycle
            .register(&current, registration(directory.path(), "current-token"))
            .unwrap();

        let stale_owner = publication(stale, "current-token", b"bytes", &["selected"]);
        assert!(matches!(
            lifecycle.accept_output(&stale_owner),
            Err(FenceError::StaleOwner)
        ));
        let stale_token = publication(current.clone(), "stale-token", b"bytes", &["selected"]);
        assert!(matches!(
            lifecycle.accept_output(&stale_token),
            Err(FenceError::StaleRegistration)
        ));
        let stale_owner_result = RuntimeMessage::ObservationResult {
            owner: owner("stale"),
            binding_id: binding_id("binding"),
            registration: token("current-token"),
            demand_watermark: 1,
            result: ObservationResult::Unchanged,
        };
        assert!(matches!(
            lifecycle.accept_output(&stale_owner_result),
            Err(FenceError::StaleOwner)
        ));
        let stale_registration_result = RuntimeMessage::ObservationResult {
            owner: current,
            binding_id: binding_id("binding"),
            registration: token("stale-token"),
            demand_watermark: 1,
            result: ObservationResult::Published {
                publication: publication_payload(b"demand", &["selected"]),
            },
        };
        assert!(matches!(
            lifecycle.accept_output(&stale_registration_result),
            Err(FenceError::StaleRegistration)
        ));
        assert!(!directory.path().join("snapshot.json").exists());
    }

    #[test]
    fn unregister_and_reregister_fence_every_old_token() {
        let directory = tempfile::tempdir().unwrap();
        let current = owner("current");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(current.clone());
        lifecycle
            .register(&current, registration(directory.path(), "first"))
            .unwrap();
        lifecycle
            .unregister(&current, &binding_id("binding"), &token("first"))
            .unwrap();

        let first = publication(current.clone(), "first", b"old", &["selected"]);
        assert!(matches!(
            lifecycle.accept_output(&first),
            Err(FenceError::UnknownBinding)
        ));
        lifecycle
            .register(&current, registration(directory.path(), "second"))
            .unwrap();
        assert!(matches!(
            lifecycle.accept_output(&first),
            Err(FenceError::StaleRegistration)
        ));
        let second = publication(current, "second", b"new", &["selected"]);
        assert!(matches!(
            lifecycle.accept_output(&second),
            Ok(AcceptedOutput::Publication(_))
        ));
    }

    #[test]
    fn new_owner_claim_clears_all_registrations() {
        let directory = tempfile::tempdir().unwrap();
        let first_owner = owner("first");
        let second_owner = owner("second");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(first_owner.clone());
        lifecycle
            .register(&first_owner, registration(directory.path(), "token"))
            .unwrap();
        assert!(lifecycle.claim(second_owner.clone()));

        let output = publication(second_owner, "token", b"bytes", &["selected"]);
        assert!(matches!(
            lifecycle.accept_output(&output),
            Err(FenceError::UnknownBinding)
        ));
    }

    #[test]
    fn first_publication_invalidates_but_equal_publication_is_silent() {
        let directory = tempfile::tempdir().unwrap();
        let current = owner("current");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(current.clone());
        lifecycle
            .register(&current, registration(directory.path(), "token"))
            .unwrap();
        let message = publication(current, "token", br#"{"state":1}"#, &["selected"]);
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        let mut catch_up = CatchUp::open(&state_directory).unwrap();

        let (first, _) = catch_up
            .publish(accepted_publication(&lifecycle, &message))
            .unwrap();
        assert_eq!(first.change(), SnapshotChange::First);
        assert!(first.invalidating());
        assert_eq!(
            fs::read(directory.path().join("snapshot.json")).unwrap(),
            br#"{"state":1}"#
        );

        let (equal, _) = catch_up
            .publish(accepted_publication(&lifecycle, &message))
            .unwrap();
        assert_eq!(equal.change(), SnapshotChange::Equal);
        assert!(!equal.invalidating());
        assert_eq!(equal.digest(), first.digest());
    }

    #[test]
    fn first_publication_creates_missing_contained_parent_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let target = SnapshotTarget::new(&root, "resources/github-pr/owner/repo/389.json").unwrap();
        assert_eq!(target.current_digest().unwrap(), None);

        let outcome =
            publish_snapshot(&target, b"bytes", vec!["selected".to_owned()], Vec::new()).unwrap();

        assert_eq!(outcome.change(), SnapshotChange::First);
        assert_eq!(
            fs::read(root.join("resources/github-pr/owner/repo/389.json")).unwrap(),
            b"bytes"
        );
    }

    #[test]
    fn topic_filtering_updates_snapshot_without_invalidating() {
        let directory = tempfile::tempdir().unwrap();
        let current = owner("current");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(current.clone());
        lifecycle
            .register(&current, registration(directory.path(), "token"))
            .unwrap();
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        let mut catch_up = CatchUp::open(&state_directory).unwrap();
        let ignored = publication(current.clone(), "token", b"ignored", &["ignored"]);
        let (outcome, _) = catch_up
            .publish(accepted_publication(&lifecycle, &ignored))
            .unwrap();
        assert_eq!(outcome.change(), SnapshotChange::First);
        assert!(outcome.selected_topics().is_empty());
        assert!(!outcome.invalidating());

        let selected = publication(current, "token", b"selected", &["ignored", "selected"]);
        let (outcome, _) = catch_up
            .publish(accepted_publication(&lifecycle, &selected))
            .unwrap();
        assert!(matches!(outcome.change(), SnapshotChange::Changed { .. }));
        assert_eq!(outcome.selected_topics(), ["selected"]);
        assert!(outcome.invalidating());
    }

    #[test]
    fn unavailable_delivery_catches_up_to_the_latest_digest_and_acks_by_level() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        let relevant = PublicationOutcome {
            digest: SnapshotDigest::of(b"relevant"),
            change: SnapshotChange::First,
            selected_topics: vec!["selected".to_owned()],
            facts: vec![ResourceFact::current("state", "ready").unwrap()],
        };
        let irrelevant = PublicationOutcome {
            digest: SnapshotDigest::of(b"later but irrelevant"),
            change: SnapshotChange::Changed {
                previous: relevant.digest(),
            },
            selected_topics: Vec::new(),
            facts: Vec::new(),
        };
        let mut catch_up = CatchUp::open(&state_directory).unwrap();
        assert_eq!(catch_up.record_publication(&relevant).unwrap(), None);
        assert_eq!(catch_up.record_publication(&irrelevant).unwrap(), None);
        assert!(catch_up.state().pending_relevant_change());

        let request = catch_up.set_deliverable(true).unwrap().unwrap();
        assert_eq!(request.digest(), irrelevant.digest());
        assert_eq!(request.selected_topics(), ["selected"]);
        assert_eq!(request.facts(), relevant.facts());
        assert!(!catch_up.acknowledge_delivery(relevant.digest()).unwrap());
        assert!(catch_up.state().pending_relevant_change());
        assert!(catch_up.acknowledge_delivery(irrelevant.digest()).unwrap());
        assert!(!catch_up.state().pending_relevant_change());
        assert_eq!(
            catch_up.state().last_delivered_digest(),
            Some(irrelevant.digest())
        );
    }

    #[test]
    fn catch_up_state_survives_reload_and_ignores_crash_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        let outcome = PublicationOutcome {
            digest: SnapshotDigest::of(b"snapshot"),
            change: SnapshotChange::First,
            selected_topics: vec!["selected".to_owned()],
            facts: vec![ResourceFact::current("state", "ready").unwrap()],
        };
        {
            let mut catch_up = CatchUp::open(&state_directory).unwrap();
            catch_up.record_publication(&outcome).unwrap();
        }
        fs::write(
            state_directory.join(".resource-profile.tmp-crash"),
            b"not committed",
        )
        .unwrap();

        let catch_up = CatchUp::open(&state_directory).unwrap();
        assert_eq!(
            catch_up.state().current_snapshot_digest(),
            Some(outcome.digest())
        );
        assert!(catch_up.state().pending_relevant_change());
        assert_eq!(catch_up.state().pending_selected_topics(), ["selected"]);
        assert_eq!(catch_up.state().pending_facts(), outcome.facts());
    }

    #[test]
    fn catch_up_persists_a_maximal_worst_case_escaped_fact_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        let facts = (0..MAX_FACTS)
            .map(|_| {
                ResourceFact::new(
                    "\"".repeat(MAX_FACT_KEY_BYTES),
                    FactValue::value("\"".repeat(MAX_FACT_VALUE_BYTES)),
                    FactValue::value("\\".repeat(MAX_FACT_VALUE_BYTES)),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let outcome = PublicationOutcome {
            digest: SnapshotDigest::of(b"snapshot"),
            change: SnapshotChange::First,
            selected_topics: vec!["selected".to_owned()],
            facts,
        };

        {
            let mut catch_up = CatchUp::open(&state_directory).unwrap();
            catch_up.record_publication(&outcome).unwrap();
        }
        let catch_up = CatchUp::open(&state_directory).unwrap();
        assert_eq!(catch_up.state().pending_facts(), outcome.facts());
    }

    fn commit_candidate(
        root: &Path,
        fence: ProposalFence,
        bytes: &[u8],
    ) -> ProposalCommit {
        let current = owner("current");
        let mut lifecycle = RuntimeLifecycle::new();
        lifecycle.claim(current.clone());
        lifecycle
            .register(&current, registration(root, "token"))
            .unwrap();
        let message = publication(current, "token", bytes, &["selected"]);
        let state_directory = fs::canonicalize(root).unwrap();
        let snapshot_target = target(root);
        let mut catch_up =
            CatchUp::open_for_snapshot(&state_directory, &snapshot_target).unwrap();
        catch_up
            .commit_proposal(fence, accepted_publication(&lifecycle, &message))
            .unwrap()
    }

    fn seed(root: &Path, bytes: &[u8]) -> PublicationCommit {
        let state_directory = fs::canonicalize(root).unwrap();
        let catch_up = CatchUp::open_for_snapshot(&state_directory, &target(root)).unwrap();
        match commit_candidate(root, catch_up.proposal_fence(), bytes) {
            ProposalCommit::Committed(commit) => commit,
            other => panic!("seed publication did not commit: {other:?}"),
        }
    }

    struct PublicationWorker {
        child: Child,
        input: Option<ChildStdin>,
        output: BufReader<ChildStdout>,
        transcript: String,
    }

    impl PublicationWorker {
        fn start(root: &Path, bytes: &str, crash_stage: Option<&str>) -> Self {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "resource_profile::tests::atomic_publication_process_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("ST2_RESOURCE_PUBLICATION_WORKER_ROOT", root)
                .env("ST2_RESOURCE_PUBLICATION_WORKER_BYTES", bytes)
                .env_remove("ST2_RESOURCE_PUBLICATION_CRASH_STAGE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            if let Some(stage) = crash_stage {
                command.env("ST2_RESOURCE_PUBLICATION_CRASH_STAGE", stage);
            }
            let mut child = command.spawn().unwrap();
            let input = child.stdin.take().unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut transcript = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(output.read_line(&mut line).unwrap(), 0, "{transcript}");
                transcript.push_str(&line);
                if line.contains("PUBLICATION-WORKER-READY") {
                    break;
                }
            }
            Self {
                child,
                input: Some(input),
                output,
                transcript,
            }
        }

        fn release(&mut self) {
            let mut input = self.input.take().unwrap();
            input.write_all(b"x").unwrap();
            drop(input);
        }

        fn finish(mut self) -> (std::process::ExitStatus, String) {
            if self.input.is_some() {
                self.release();
            }
            let status = self.child.wait().unwrap();
            self.output.read_to_string(&mut self.transcript).unwrap();
            (status, self.transcript)
        }
    }

    #[test]
    #[ignore = "subprocess entrypoint for atomic publication tests"]
    fn atomic_publication_process_worker() {
        let Some(root) = std::env::var_os("ST2_RESOURCE_PUBLICATION_WORKER_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let bytes = std::env::var("ST2_RESOURCE_PUBLICATION_WORKER_BYTES").unwrap();
        let state_directory = fs::canonicalize(&root).unwrap();
        let snapshot_target = target(&root);
        let catch_up = CatchUp::open_for_snapshot(&state_directory, &snapshot_target).unwrap();
        let fence = catch_up.proposal_fence();
        println!(
            "PUBLICATION-WORKER-READY {} {}",
            fence.generation(),
            fence.revision()
        );
        std::io::stdout().flush().unwrap();
        let mut release = [0_u8; 1];
        std::io::stdin().read_exact(&mut release).unwrap();
        assert_eq!(release, *b"x");
        let result = commit_candidate(&root, fence, bytes.as_bytes());
        println!("PUBLICATION-WORKER-RESULT {result:?}");
        std::io::stdout().flush().unwrap();
    }

    #[test]
    fn atomic_publication_fences_races_and_survives_crash_restarts() {
        let directory = tempfile::tempdir().unwrap();

        // POSIX advisory locks are process-scoped, so real child processes (not threads) prove that
        // two proposals captured from one prior cannot both pass the host CAS.
        let race = directory.path().join("same-prior-race");
        fs::create_dir(&race).unwrap();
        let mut racer_a = PublicationWorker::start(&race, "candidate-a", None);
        let mut racer_b = PublicationWorker::start(&race, "candidate-b", None);
        racer_a.release();
        racer_b.release();
        let (status_a, output_a) = racer_a.finish();
        let (status_b, output_b) = racer_b.finish();
        assert!(status_a.success(), "{output_a}");
        assert!(status_b.success(), "{output_b}");
        let outputs = format!("{output_a}\n{output_b}");
        assert_eq!(
            outputs
                .matches("PUBLICATION-WORKER-RESULT Committed(")
                .count(),
            1
        );
        assert_eq!(
            outputs
                .matches("PUBLICATION-WORKER-RESULT StalePrior")
                .count(),
            1
        );
        let raced = CatchUp::open_for_snapshot(
            &fs::canonicalize(&race).unwrap(),
            &target(&race),
        )
        .unwrap();
        assert_eq!(raced.state().revision(), 1);
        let raced_bytes = fs::read(race.join("snapshot.json")).unwrap();
        assert!(
            raced_bytes.as_slice() == b"candidate-a"
                || raced_bytes.as_slice() == b"candidate-b"
        );

        // Replacement advances the durable generation while the child is held at an explicit
        // pipe barrier; releasing it cannot revive the replaced binding.
        let replacement = directory.path().join("stale-generation");
        fs::create_dir(&replacement).unwrap();
        let stale = PublicationWorker::start(&replacement, "stale", None);
        let state_directory = fs::canonicalize(&replacement).unwrap();
        let mut host =
            CatchUp::open_for_snapshot(&state_directory, &target(&replacement)).unwrap();
        let replacement_fence = host.advance_generation(&target(&replacement)).unwrap();
        assert_eq!(replacement_fence.generation(), 1);
        let (status, output) = stale.finish();
        assert!(status.success(), "{output}");
        assert!(output.contains("StaleGeneration"), "{output}");
        assert!(!replacement.join("snapshot.json").exists());

        // A crash after the durable intent but before carrier rename leaves the old publication
        // visible. Restart discards the ineligible intent without manufacturing an outbox item.
        let before = directory.path().join("crash-before");
        fs::create_dir(&before).unwrap();
        let seed_commit = seed(&before, b"old");
        let before_revision = seed_commit.revision();
        let worker = PublicationWorker::start(
            &before,
            "must-not-appear",
            Some("after-intent-before-carrier"),
        );
        let (status, output) = worker.finish();
        assert_eq!(status.code(), Some(71), "{output}");
        let restarted =
            CatchUp::open_for_snapshot(&fs::canonicalize(&before).unwrap(), &target(&before))
                .unwrap();
        assert_eq!(fs::read(before.join("snapshot.json")).unwrap(), b"old");
        assert_eq!(restarted.state().revision(), before_revision);
        assert!(!before.join(PUBLICATION_INTENT_FILE).exists());

        // A crash after carrier rename is caught up from the exact matching intent on restart.
        // Current digest, deterministic commit receipt, and delivery envelope then coexist in the
        // one authoritative catch-up state; the WAL is no longer a second source of truth.
        let catch_up = directory.path().join("restart-catch-up");
        fs::create_dir(&catch_up).unwrap();
        seed(&catch_up, b"old");
        let worker = PublicationWorker::start(
            &catch_up,
            "recovered",
            Some("after-carrier-before-state"),
        );
        let (status, output) = worker.finish();
        assert_eq!(status.code(), Some(72), "{output}");
        let recovered = CatchUp::open_for_snapshot(
            &fs::canonicalize(&catch_up).unwrap(),
            &target(&catch_up),
        )
        .unwrap();
        let recovered_digest = SnapshotDigest::of(b"recovered");
        assert_eq!(
            recovered.state().current_snapshot_digest(),
            Some(recovered_digest)
        );
        let receipt = recovered.state().last_commit().unwrap();
        assert_eq!(receipt.digest(), recovered_digest);
        let durable_intent = recovered.state().last_intent.as_ref().unwrap();
        assert_eq!(durable_intent.proposal_id, receipt.proposal_id());
        assert_eq!(durable_intent.digest, recovered_digest);
        assert_eq!(durable_intent.selected_topics, ["selected"]);
        assert!(recovered.state().pending_from_last_intent);
        assert!(recovered.state().pending_selected_topics.is_empty());
        assert!(recovered.state().pending_facts.is_empty());
        assert_eq!(recovered.state().pending_selected_topics(), ["selected"]);
        assert!(!catch_up.join(PUBLICATION_INTENT_FILE).exists());
        let restarted = CatchUp::open_for_snapshot(
            &fs::canonicalize(&catch_up).unwrap(),
            &target(&catch_up),
        )
        .unwrap();
        assert_eq!(restarted.state(), recovered.state());

        // The state rename can land even when the acknowledgement is lost. Replaying the exact
        // proposal returns its durable receipt and does not advance revision or duplicate intent.
        let lost_ack = directory.path().join("lost-ack");
        fs::create_dir(&lost_ack).unwrap();
        seed(&lost_ack, b"old");
        let state_directory = fs::canonicalize(&lost_ack).unwrap();
        let prior =
            CatchUp::open_for_snapshot(&state_directory, &target(&lost_ack)).unwrap();
        let retry_fence = prior.proposal_fence();
        let worker = PublicationWorker::start(
            &lost_ack,
            "committed",
            Some("after-state-before-ack"),
        );
        let (status, output) = worker.finish();
        assert_eq!(status.code(), Some(73), "{output}");
        let committed =
            CatchUp::open_for_snapshot(&state_directory, &target(&lost_ack)).unwrap();
        let committed_revision = committed.state().revision();
        let committed_receipt = committed.state().last_commit().unwrap();
        drop(committed);
        assert_eq!(
            commit_candidate(&lost_ack, retry_fence, b"committed"),
            ProposalCommit::AlreadyCommitted(committed_receipt)
        );
        let after_retry =
            CatchUp::open_for_snapshot(&state_directory, &target(&lost_ack)).unwrap();
        assert_eq!(after_retry.state().revision(), committed_revision);
        assert_eq!(fs::read(lost_ack.join("snapshot.json")).unwrap(), b"committed");
    }

    #[test]
    fn generation_advance_explicitly_recovers_diverged_or_missing_carrier() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = fs::canonicalize(directory.path()).unwrap();
        seed(directory.path(), b"committed");
        let before =
            CatchUp::open_for_snapshot(&state_directory, &target(directory.path())).unwrap();
        let stale_fence = before.proposal_fence();
        drop(before);

        let worker = PublicationWorker::start(
            directory.path(),
            "interrupted",
            Some("after-intent-before-carrier"),
        );
        let (status, output) = worker.finish();
        assert_eq!(status.code(), Some(71), "{output}");
        fs::write(directory.path().join("snapshot.json"), b"operator-recovery").unwrap();
        assert!(matches!(
            CatchUp::open_for_snapshot(&state_directory, &target(directory.path())),
            Err(CatchUpError::InvalidState(
                "canonical snapshot differs from the authoritative committed digest"
            ))
        ));
        assert!(directory.path().join(PUBLICATION_INTENT_FILE).exists());

        let recovered = CatchUp::open_for_generation_advance(
            &state_directory,
            &target(directory.path()),
        )
        .unwrap();
        assert_eq!(recovered.state().generation(), stale_fence.generation() + 1);
        assert_eq!(recovered.state().revision(), stale_fence.revision() + 1);
        assert_eq!(
            recovered.state().current_snapshot_digest(),
            Some(SnapshotDigest::of(b"operator-recovery"))
        );
        assert!(recovered.state().last_commit().is_none());
        assert!(!recovered.state().pending_relevant_change());
        assert!(!directory.path().join(PUBLICATION_INTENT_FILE).exists());
        drop(recovered);
        assert!(matches!(
            commit_candidate(directory.path(), stale_fence, b"late"),
            ProposalCommit::StaleGeneration { .. }
        ));
        assert_eq!(
            fs::read(directory.path().join("snapshot.json")).unwrap(),
            b"operator-recovery"
        );
        fs::write(directory.path().join("snapshot.json"), b"second-divergence").unwrap();
        assert!(matches!(
            CatchUp::open_for_snapshot(&state_directory, &target(directory.path())),
            Err(CatchUpError::InvalidState(
                "canonical snapshot differs from the authoritative committed digest"
            ))
        ));

        let missing = tempfile::tempdir().unwrap();
        let missing_state = fs::canonicalize(missing.path()).unwrap();
        seed(missing.path(), b"present");
        fs::remove_file(missing.path().join("snapshot.json")).unwrap();
        assert!(matches!(
            CatchUp::open_for_snapshot(&missing_state, &target(missing.path())),
            Err(CatchUpError::InvalidState(
                "canonical snapshot differs from the authoritative committed digest"
            ))
        ));
        let recovered_missing =
            CatchUp::open_for_generation_advance(&missing_state, &target(missing.path())).unwrap();
        assert_eq!(recovered_missing.state().current_snapshot_digest(), None);
        assert!(recovered_missing.state().last_commit().is_none());
        assert!(!recovered_missing.state().pending_relevant_change());
    }

    #[test]
    fn predecessor_publication_intent_migrates_by_digest_and_unknown_shapes_fail_closed() {
        let matching = tempfile::tempdir().unwrap();
        let matching_state = fs::canonicalize(matching.path()).unwrap();
        fs::write(matching.path().join("snapshot.json"), b"legacy-carrier").unwrap();
        let legacy = serde_json::json!({
            "digest": SnapshotDigest::of(b"legacy-carrier").to_string(),
            "selectedTopics": ["selected"],
            "facts": [{"key": "state", "after": "ready"}],
        });
        fs::write(
            matching.path().join(PUBLICATION_INTENT_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let migrated =
            CatchUp::open_for_snapshot(&matching_state, &target(matching.path())).unwrap();
        assert_eq!(
            migrated.state().current_snapshot_digest(),
            Some(SnapshotDigest::of(b"legacy-carrier"))
        );
        assert_eq!(migrated.state().pending_selected_topics(), ["selected"]);
        assert_eq!(
            migrated.state().pending_facts(),
            [ResourceFact::current("state", "ready").unwrap()]
        );
        assert!(migrated.state().last_commit().is_none());
        assert!(!matching.path().join(PUBLICATION_INTENT_FILE).exists());

        let mismatched = tempfile::tempdir().unwrap();
        let mismatched_state = fs::canonicalize(mismatched.path()).unwrap();
        fs::write(mismatched.path().join("snapshot.json"), b"old-carrier").unwrap();
        let legacy = serde_json::json!({
            "digest": SnapshotDigest::of(b"never-published").to_string(),
            "selectedTopics": ["selected"],
            "facts": [],
        });
        fs::write(
            mismatched.path().join(PUBLICATION_INTENT_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let migrated =
            CatchUp::open_for_snapshot(&mismatched_state, &target(mismatched.path())).unwrap();
        assert_eq!(
            migrated.state().current_snapshot_digest(),
            Some(SnapshotDigest::of(b"old-carrier"))
        );
        assert!(!migrated.state().pending_relevant_change());
        assert!(!mismatched.path().join(PUBLICATION_INTENT_FILE).exists());

        let malformed = tempfile::tempdir().unwrap();
        let malformed_state = fs::canonicalize(malformed.path()).unwrap();
        fs::write(malformed.path().join("snapshot.json"), b"carrier").unwrap();
        let unknown = serde_json::json!({
            "digest": SnapshotDigest::of(b"carrier").to_string(),
            "selectedTopics": [],
            "facts": [],
            "unknown": true,
        });
        fs::write(
            malformed.path().join(PUBLICATION_INTENT_FILE),
            serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            CatchUp::open_for_snapshot(&malformed_state, &target(malformed.path())),
            Err(CatchUpError::Json(_))
        ));
        assert!(malformed.path().join(PUBLICATION_INTENT_FILE).exists());
    }

    #[test]
    fn snapshot_and_state_paths_reject_escape_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        assert!(matches!(
            SnapshotTarget::new(&root, "../escape"),
            Err(PathError::UnsafeCarrier(_))
        ));
        assert!(matches!(
            SnapshotTarget::new(&root, root.parent().unwrap().join("escape")),
            Err(PathError::EscapesRoot)
        ));

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.join("linked-parent")).unwrap();
        let target = SnapshotTarget::new(&root, "linked-parent/snapshot.json").unwrap();
        assert!(matches!(
            publish_snapshot(&target, b"bytes", vec!["selected".to_owned()], Vec::new()),
            Err(PublicationError::Io(_))
        ));

        symlink(outside.path().join("state.json"), root.join(CATCH_UP_FILE)).unwrap();
        assert!(matches!(
            CatchUp::open(&root),
            Err(CatchUpError::Io(_)) | Err(CatchUpError::StateNotRegular)
        ));
    }
}
