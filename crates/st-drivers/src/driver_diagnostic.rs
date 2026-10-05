//! Durable, bounded diagnostics published by native driver cores.
//!
//! A driver owns one current record per agent. Internally the publisher retains one failure per
//! stage and projects the earliest failing stage, so a later transport symptom cannot hide an
//! unresolved admission failure. Recovering a stage clears only that stage and immediately reveals
//! the next outstanding failure; recovering the final stage removes the record.

use std::array;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SCHEMA: &str = "st.driver-diagnostic.v1";
const RECOVERY: &str = "clearsOnStageRecovery";
const FUTURE_SKEW_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Launch,
    VersionGate,
    ApiGate,
    Sse,
    Seed,
    ProviderAuth,
    Turn,
    Delivery,
    ReadBack,
    #[serde(other)]
    Unknown,
}

impl Stage {
    pub const ALL: [Self; 9] = [
        Self::Launch,
        Self::VersionGate,
        Self::ApiGate,
        Self::Sse,
        Self::Seed,
        Self::ProviderAuth,
        Self::Turn,
        Self::Delivery,
        Self::ReadBack,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::VersionGate => "versionGate",
            Self::ApiGate => "apiGate",
            Self::Sse => "sse",
            Self::Seed => "seed",
            Self::ProviderAuth => "providerAuth",
            Self::Turn => "turn",
            Self::Delivery => "delivery",
            Self::ReadBack => "readBack",
            Self::Unknown => "unknown",
        }
    }

    /// Projection order, earliest boundary first. `ProviderAuth` sits between the four gates st2
    /// owns and the two it can only observe through them: the gates are st2↔producer contract
    /// facts that must hold before any provider-side reading means anything, while a rejected
    /// credential is the CAUSE whose symptoms are delivery and read-back failures — so it must
    /// outrank both rather than hide behind them.
    const fn index(self) -> Option<usize> {
        match self {
            Self::Launch => Some(0),
            Self::VersionGate => Some(1),
            Self::ApiGate => Some(2),
            Self::Sse => Some(3),
            Self::Seed => Some(4),
            Self::ProviderAuth => Some(5),
            Self::Turn => Some(6),
            Self::Delivery => Some(7),
            Self::ReadBack => Some(8),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Driver {
    #[serde(rename = "opencode")]
    OpenCode,
    Claude,
    Codex,
    Omp,
    #[serde(other)]
    Unknown,
}

impl Driver {
    pub const ALL: [Self; 4] = [Self::OpenCode, Self::Claude, Self::Codex, Self::Omp];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    LaunchConfigurationRejected,
    VersionProbeFailed,
    UnsupportedVersion,
    AdmissionExtensionLoad,
    AdmissionApiContract,
    AdmissionLifecycle,
    AdmissionIdleEdge,
    AdmissionApprovalCorrelation,
    AdmissionNativeConsumption,
    AdmissionIndeterminate,
    ApiUnavailable,
    IncompatibleApi,
    SseConnectFailed,
    SseDisconnected,
    UnknownEvent,
    StatusUnavailable,
    MalformedStatus,
    UnknownStatus,
    PermissionUnavailable,
    MalformedPermissions,
    QuestionUnavailable,
    MalformedQuestions,
    MissingAskId,
    DeliveryUnavailable,
    DeliveryRejected,
    ReadBackUnavailable,
    NotDurable,
    ProviderAuthRejected,
    TurnUsageLimit,
    TurnServerOverloaded,
    TurnContextWindow,
    TurnConnection,
    TurnPolicy,
    TurnAccount,
    TurnRejected,
    TurnInternal,
    TurnUnclassified,
    #[serde(other)]
    Unknown,
}

impl Reason {
    pub const ALL: [Self; 37] = [
        Self::LaunchConfigurationRejected,
        Self::VersionProbeFailed,
        Self::UnsupportedVersion,
        Self::AdmissionExtensionLoad,
        Self::AdmissionApiContract,
        Self::AdmissionLifecycle,
        Self::AdmissionIdleEdge,
        Self::AdmissionApprovalCorrelation,
        Self::AdmissionNativeConsumption,
        Self::AdmissionIndeterminate,
        Self::ApiUnavailable,
        Self::IncompatibleApi,
        Self::SseConnectFailed,
        Self::SseDisconnected,
        Self::UnknownEvent,
        Self::StatusUnavailable,
        Self::MalformedStatus,
        Self::UnknownStatus,
        Self::PermissionUnavailable,
        Self::MalformedPermissions,
        Self::QuestionUnavailable,
        Self::MalformedQuestions,
        Self::MissingAskId,
        Self::DeliveryUnavailable,
        Self::DeliveryRejected,
        Self::ReadBackUnavailable,
        Self::NotDurable,
        Self::ProviderAuthRejected,
        Self::TurnUsageLimit,
        Self::TurnServerOverloaded,
        Self::TurnContextWindow,
        Self::TurnConnection,
        Self::TurnPolicy,
        Self::TurnAccount,
        Self::TurnRejected,
        Self::TurnInternal,
        Self::TurnUnclassified,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchConfigurationRejected => "launchConfigurationRejected",
            Self::VersionProbeFailed => "versionProbeFailed",
            Self::UnsupportedVersion => "unsupportedVersion",
            Self::AdmissionExtensionLoad => "admissionExtensionLoad",
            Self::AdmissionApiContract => "admissionApiContract",
            Self::AdmissionLifecycle => "admissionLifecycle",
            Self::AdmissionIdleEdge => "admissionIdleEdge",
            Self::AdmissionApprovalCorrelation => "admissionApprovalCorrelation",
            Self::AdmissionNativeConsumption => "admissionNativeConsumption",
            Self::AdmissionIndeterminate => "admissionIndeterminate",
            Self::ApiUnavailable => "apiUnavailable",
            Self::IncompatibleApi => "incompatibleApi",
            Self::SseConnectFailed => "sseConnectFailed",
            Self::SseDisconnected => "sseDisconnected",
            Self::UnknownEvent => "unknownEvent",
            Self::StatusUnavailable => "statusUnavailable",
            Self::MalformedStatus => "malformedStatus",
            Self::UnknownStatus => "unknownStatus",
            Self::PermissionUnavailable => "permissionUnavailable",
            Self::MalformedPermissions => "malformedPermissions",
            Self::QuestionUnavailable => "questionUnavailable",
            Self::MalformedQuestions => "malformedQuestions",
            Self::MissingAskId => "missingAskId",
            Self::DeliveryUnavailable => "deliveryUnavailable",
            Self::DeliveryRejected => "deliveryRejected",
            Self::ReadBackUnavailable => "readBackUnavailable",
            Self::NotDurable => "notDurable",
            Self::ProviderAuthRejected => "providerAuthRejected",
            Self::TurnUsageLimit => "turnUsageLimit",
            Self::TurnServerOverloaded => "turnServerOverloaded",
            Self::TurnContextWindow => "turnContextWindow",
            Self::TurnConnection => "turnConnection",
            Self::TurnPolicy => "turnPolicy",
            Self::TurnAccount => "turnAccount",
            Self::TurnRejected => "turnRejected",
            Self::TurnInternal => "turnInternal",
            // The producer named a cause this build does not classify. Distinct from
            // [`Self::Unknown`], which means a READER met a word it cannot decode: this one is a
            // word st2 wrote on purpose, and it is evidence of a real failure.
            Self::TurnUnclassified => "turnUnclassified",
            Self::Unknown => "unknown",
        }
    }

    pub const fn stage(self) -> Stage {
        match self {
            Self::LaunchConfigurationRejected => Stage::Launch,
            Self::VersionProbeFailed | Self::UnsupportedVersion => Stage::VersionGate,
            Self::AdmissionExtensionLoad
            | Self::AdmissionApiContract
            | Self::AdmissionLifecycle
            | Self::AdmissionIdleEdge
            | Self::AdmissionApprovalCorrelation
            | Self::AdmissionNativeConsumption
            | Self::AdmissionIndeterminate => Stage::VersionGate,
            Self::ApiUnavailable | Self::IncompatibleApi => Stage::ApiGate,
            Self::SseConnectFailed | Self::SseDisconnected | Self::UnknownEvent => Stage::Sse,
            Self::StatusUnavailable
            | Self::MalformedStatus
            | Self::UnknownStatus
            | Self::PermissionUnavailable
            | Self::MalformedPermissions
            | Self::QuestionUnavailable
            | Self::MalformedQuestions
            | Self::MissingAskId => Stage::Seed,
            Self::ProviderAuthRejected => Stage::ProviderAuth,
            Self::TurnUsageLimit
            | Self::TurnServerOverloaded
            | Self::TurnContextWindow
            | Self::TurnConnection
            | Self::TurnPolicy
            | Self::TurnAccount
            | Self::TurnRejected
            | Self::TurnInternal
            | Self::TurnUnclassified => Stage::Turn,
            Self::DeliveryUnavailable | Self::DeliveryRejected => Stage::Delivery,
            Self::ReadBackUnavailable | Self::NotDurable => Stage::ReadBack,
            Self::Unknown => Stage::Unknown,
        }
    }

    const fn accepts_source(self, source: Source) -> bool {
        match self {
            Self::LaunchConfigurationRejected => matches!(source, Source::ProcessExit),
            Self::VersionProbeFailed | Self::UnsupportedVersion => {
                matches!(source, Source::VersionProbe)
            }
            Self::AdmissionExtensionLoad
            | Self::AdmissionApiContract
            | Self::AdmissionLifecycle
            | Self::AdmissionIdleEdge
            | Self::AdmissionApprovalCorrelation
            | Self::AdmissionNativeConsumption
            | Self::AdmissionIndeterminate => matches!(source, Source::AdmissionProbe),
            Self::ApiUnavailable | Self::IncompatibleApi => {
                matches!(source, Source::OpenApiDocument)
            }
            Self::SseConnectFailed | Self::SseDisconnected | Self::UnknownEvent => {
                matches!(source, Source::EventStream)
            }
            Self::StatusUnavailable | Self::MalformedStatus | Self::UnknownStatus => {
                matches!(source, Source::StatusSnapshot)
            }
            Self::PermissionUnavailable | Self::MalformedPermissions => {
                matches!(source, Source::PermissionSnapshot)
            }
            Self::QuestionUnavailable | Self::MalformedQuestions => {
                matches!(source, Source::QuestionSnapshot)
            }
            Self::MissingAskId => {
                matches!(
                    source,
                    Source::PermissionSnapshot | Source::QuestionSnapshot
                )
            }
            Self::ProviderAuthRejected => matches!(source, Source::TurnResult),
            Self::TurnUsageLimit
            | Self::TurnServerOverloaded
            | Self::TurnContextWindow
            | Self::TurnConnection
            | Self::TurnPolicy
            | Self::TurnAccount
            | Self::TurnRejected
            | Self::TurnInternal
            | Self::TurnUnclassified => matches!(source, Source::TurnError),
            Self::DeliveryUnavailable | Self::DeliveryRejected => {
                matches!(source, Source::PromptTransport)
            }
            Self::ReadBackUnavailable | Self::NotDurable => {
                matches!(source, Source::MessageReadBack)
            }
            Self::Unknown => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    ProcessExit,
    VersionProbe,
    AdmissionProbe,
    OpenApiDocument,
    EventStream,
    StatusSnapshot,
    PermissionSnapshot,
    QuestionSnapshot,
    PromptTransport,
    MessageReadBack,
    TurnResult,
    TurnError,
    #[serde(other)]
    Unknown,
}

impl Source {
    pub const ALL: [Self; 12] = [
        Self::ProcessExit,
        Self::VersionProbe,
        Self::AdmissionProbe,
        Self::OpenApiDocument,
        Self::EventStream,
        Self::StatusSnapshot,
        Self::PermissionSnapshot,
        Self::QuestionSnapshot,
        Self::PromptTransport,
        Self::MessageReadBack,
        Self::TurnResult,
        Self::TurnError,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessExit => "processExit",
            Self::VersionProbe => "versionProbe",
            Self::AdmissionProbe => "admissionProbe",
            Self::OpenApiDocument => "openApiDocument",
            Self::EventStream => "eventStream",
            Self::StatusSnapshot => "statusSnapshot",
            Self::PermissionSnapshot => "permissionSnapshot",
            Self::QuestionSnapshot => "questionSnapshot",
            Self::PromptTransport => "promptTransport",
            Self::MessageReadBack => "messageReadBack",
            // Turn results report completion and credential edges. Turn errors report failures
            // while a turn may still be retrying.
            Self::TurnResult => "turnResult",
            Self::TurnError => "turnError",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
    #[serde(other)]
    Unrecognized,
}

impl Support {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
            Self::Unrecognized => "unrecognized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    schema: String,
    driver: Driver,
    stage: Stage,
    reason: Reason,
    source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    producer_version: Option<String>,
    support: Support,
    observed_at: u64,
    recovery: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub driver: Driver,
    pub stage: Stage,
    pub reason: Reason,
    pub source: Source,
    pub producer_version: Option<String>,
    pub support: Support,
    pub observed_at: u64,
    pub evidence_age_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    MalformedRecord,
    UnsupportedSchema,
    UnknownVocabulary,
    FutureSkew,
}

impl InvalidReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MalformedRecord => "malformedRecord",
            Self::UnsupportedSchema => "unsupportedSchema",
            Self::UnknownVocabulary => "unknownVocabulary",
            Self::FutureSkew => "futureSkew",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Absent,
    Failure(Failure),
    Indeterminate(InvalidReason),
}

impl Observed {
    pub const fn status(&self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Failure(_) => "failure",
            Self::Indeterminate(_) => "indeterminate",
        }
    }
}

/// Stable operator guidance shared by Doctor and any future renderer. The text is driver-agnostic;
/// stage and source carry the typed native-driver boundary.
pub fn repair_text(observed: &Observed) -> &'static str {
    match observed {
        Observed::Absent => {
            "no diagnostic evidence exists — wait for the native driver to publish a boundary result or restart the seat"
        }
        Observed::Indeterminate(InvalidReason::MalformedRecord) => {
            "replace the malformed driver-diagnostic record by restarting the seat"
        }
        Observed::Indeterminate(InvalidReason::UnsupportedSchema) => {
            "upgrade this st reader or restart the seat with a compatible driver-diagnostic writer"
        }
        Observed::Indeterminate(InvalidReason::UnknownVocabulary) => {
            "upgrade this st reader; unknown diagnostic vocabulary is not healthy evidence"
        }
        Observed::Indeterminate(InvalidReason::FutureSkew) => {
            "correct the writer clock or restart the seat after clock recovery"
        }
        Observed::Failure(failure) => match failure.stage {
            Stage::Launch => {
                "repair the rejected declared launch arguments; the seat is running with a known-safe fallback"
            }
            Stage::VersionGate => match failure.driver {
                Driver::Omp => {
                    "repair the named producer admission check and remove its cache record to remeasure, or explicitly allow this build from a person's terminal with `st admission override omp --binary <installed-executable> --reason <reason>`; then restart the affected seat"
                }
                Driver::OpenCode => {
                    "repair the named producer admission check and remove its cache record to remeasure, or explicitly allow this build from a person's terminal with `st admission override opencode --binary <installed-executable> --reason <reason>`; then restart the affected seat"
                }
                _ => {
                    "repair the named producer admission check, remove its harness-admission cache record to repeat the isolated probe, and restart the seat"
                }
            },
            Stage::ApiGate => "restore the producer API contract, then restart the seat",
            Stage::Sse => "restore the producer event stream; recovery clears this advisory",
            Stage::Seed => {
                "restore readable producer state snapshots; recovery clears this advisory"
            }
            // The one boundary whose repair is neither an st-side nor a producer-side restore:
            // nothing in the seat is broken, the account's credential was refused. The text stays
            // generic on purpose — which client owns which credential home is declared outside
            // st2, and no credential knowledge enters this crate (Q12).
            Stage::ProviderAuth => {
                "the seat's provider credential was rejected; re-login with the account's own client and unpark"
            }
            Stage::Turn => {
                "the provider refused or dropped this seat's turns; read the reason word — recovery clears this advisory"
            }
            Stage::Delivery => {
                "restore the native prompt transport; the queued message remains retryable"
            }
            Stage::ReadBack => {
                "restore message read-back; st will reconcile without duplicating the prompt"
            }
            Stage::Unknown => "upgrade this st reader; an unknown stage is not healthy evidence",
        },
    }
}

pub fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("driver-diagnostic")
}

/// Whether this declaration has a native driver that publishes this record at all.
pub fn expected_for(spec: &crate::AgentSpec) -> bool {
    matches!(
        spec.driver.as_ref(),
        Some(
            crate::Driver::OpenCode(_)
                | crate::Driver::Claude(_)
                | crate::Driver::Codex(_)
                | crate::Driver::Omp(_)
        )
    )
}

/// Whether a missing record is itself a fault for this declaration.
///
/// Only a driver that publishes a boundary result on EVERY launch can be missing one: OpenCode's
/// version gate publishes or clears before the provider spawns, so absence there means the native
/// driver never ran. Claude, Codex, and omp publish this record only when the provider's own typed
/// turn result names a rejected credential, so absence is their healthy steady state and advising
/// on it would put a warning under every seat in the fleet.
pub fn absence_is_a_fault(spec: &crate::AgentSpec) -> bool {
    matches!(spec.driver.as_ref(), Some(crate::Driver::OpenCode(_)))
}

pub fn read(path: &Path) -> Observed {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Observed::Absent,
        Err(_) => return Observed::Indeterminate(InvalidReason::MalformedRecord),
    };
    read_at(&raw, now_ms())
}

fn read_at(raw: &[u8], now: u64) -> Observed {
    let record = match serde_json::from_slice::<Record>(raw) {
        Ok(record) => record,
        Err(_) => return Observed::Indeterminate(InvalidReason::MalformedRecord),
    };
    if !crate::contracts::schema_matches(&record.schema, SCHEMA) || record.recovery != RECOVERY {
        return Observed::Indeterminate(InvalidReason::UnsupportedSchema);
    }
    if record.stage == Stage::Unknown
        || record.reason == Reason::Unknown
        || record.source == Source::Unknown
        || record.driver == Driver::Unknown
        || record.reason.stage() != record.stage
        || !record.reason.accepts_source(record.source)
        || record.support == Support::Unrecognized
    {
        return Observed::Indeterminate(InvalidReason::UnknownVocabulary);
    }
    if record.observed_at > now.saturating_add(FUTURE_SKEW_MS) {
        return Observed::Indeterminate(InvalidReason::FutureSkew);
    }
    Observed::Failure(Failure {
        driver: record.driver,
        stage: record.stage,
        reason: record.reason,
        source: record.source,
        producer_version: record.producer_version,
        support: record.support,
        observed_at: record.observed_at,
        evidence_age_ms: now.saturating_sub(record.observed_at),
    })
}

/// In-process stage set for one native driver session. Persistence failures stay diagnostic-only:
/// they are logged but never change launch, observation, delivery, retry, or archive semantics.
pub struct Publisher {
    path: PathBuf,
    driver: Driver,
    producer_version: Option<String>,
    support: Support,
    failures: [Option<Record>; 9],
}

impl Publisher {
    pub fn new(
        agent_dir: &Path,
        driver: Driver,
        producer_version: Option<String>,
        support: Support,
    ) -> Self {
        let path = path(agent_dir);
        if matches!(
            read(&path),
            Observed::Indeterminate(InvalidReason::MalformedRecord)
        ) && let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                "st driver diagnostic malformed predecessor cleanup failed: {error}"
            );
        }
        // Seed the stage set from whatever readable record this seat already carries for THIS
        // driver. Without it, a short-lived publisher — every Claude hook invocation is its own
        // process — would persist its own stage over a predecessor's EARLIER one, and stage
        // priority would hold only inside a process. Nothing is resurrected that was not already
        // on disk: `persist` writes the earliest stage it holds, and this only decides which of
        // two live failures a reader sees.
        let mut failures: [Option<Record>; 9] = array::from_fn(|_| None);
        if let Ok(raw) = fs::read(&path)
            && let Ok(record) = serde_json::from_slice::<Record>(&raw)
            && crate::contracts::schema_matches(&record.schema, SCHEMA)
            && record.recovery == RECOVERY
            && record.driver == driver
            && record.support != Support::Unrecognized
            && record.observed_at <= now_ms().saturating_add(FUTURE_SKEW_MS)
            && record.reason.stage() == record.stage
            && record.reason.accepts_source(record.source)
            && let Some(index) = record.stage.index()
        {
            failures[index] = Some(record);
        }
        Self {
            path,
            driver,
            producer_version,
            support,
            failures,
        }
    }

    pub fn publish(&mut self, stage: Stage, reason: Reason, source: Source) {
        let Some(index) = stage.index() else {
            return;
        };
        if reason.stage() != stage || !reason.accepts_source(source) {
            return;
        }
        if self.failures[index]
            .as_ref()
            .is_some_and(|failure| failure.reason == reason && failure.source == source)
        {
            return;
        }
        let record = Record {
            schema: crate::contracts::schema_for_agent(self.path.parent().unwrap(), SCHEMA),
            driver: self.driver,
            stage,
            reason,
            source,
            producer_version: self.producer_version.clone(),
            support: self.support,
            observed_at: now_ms(),
            recovery: RECOVERY.to_string(),
        };
        self.failures[index] = Some(record);
        crate::metrics::record_driver_diagnostic(
            self.driver,
            stage,
            reason,
            source,
            self.support,
            false,
        );
        emit(
            self.driver,
            stage,
            reason,
            source,
            self.support,
            "failure",
            self.producer_version.as_deref(),
        );
        self.persist();
    }

    pub fn clear(&mut self, stage: Stage) {
        let Some(index) = stage.index() else {
            return;
        };
        let cleared = self.failures[index].take().or_else(|| {
            let raw = fs::read(&self.path).ok()?;
            let record = serde_json::from_slice::<Record>(&raw).ok()?;
            (crate::contracts::schema_matches(&record.schema, SCHEMA)
                && record.recovery == RECOVERY
                && record.driver == self.driver
                && record.stage == stage)
                .then_some(record)
        });
        let Some(cleared) = cleared else {
            return;
        };
        crate::metrics::record_driver_diagnostic(
            self.driver,
            stage,
            cleared.reason,
            cleared.source,
            cleared.support,
            true,
        );
        emit(
            self.driver,
            stage,
            cleared.reason,
            cleared.source,
            cleared.support,
            "recovery",
            self.producer_version.as_deref(),
        );
        self.persist();
    }

    fn persist(&self) {
        let result = match self.failures.iter().flatten().next() {
            Some(record) => atomic_json(&self.path, record),
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = result {
            tracing::warn!(
                path = %self.path.display(),
                "st driver diagnostic persistence failed: {error}"
            );
        }
    }
}

/// What one observation — a Claude hook event, a pi-family typed turn result — proves about the
/// seat's provider credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderAuthEdge {
    Rejected,
    Accepted,
}

/// Record one credential edge on the seat's native-driver diagnostic.
///
/// A fresh publisher per edge on purpose, and every producer of these edges is short-lived: each
/// Claude hook invocation is its own process, so the publisher's stage set starts empty and its
/// on-disk fallback is what lets a later `Stop` clear a rejection an earlier `StopFailure` wrote
/// from a different process; a channel that restarted mid-session inherits the predecessor's
/// record the same way rather than silently starting clean. Fail-open like every other
/// observation: the publisher only warns on a write it cannot land, and neither delivery nor
/// launch depends on it.
pub(crate) fn publish_provider_auth(agent_dir: &Path, driver: Driver, edge: ProviderAuthEdge) {
    let mut publisher = Publisher::new(
        agent_dir,
        driver,
        // No producer version and no support verdict is knowable at either edge. A Claude hook
        // payload carries no version — the common hook input is session id, transcript path, cwd,
        // prompt id, permission mode, agent identity and effort, and nothing else (2.1.259) — and
        // st2 gates no Claude version at all. On the pi family the WRAPPER, not the channel, owns
        // the version gate and refuses the launch on an unadmitted MINOR (OMP-R05), so a running
        // channel has no version fact of its own to publish and no verdict to restate.
        None,
        Support::Unknown,
    );
    match edge {
        ProviderAuthEdge::Rejected => publisher.publish(
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
        ),
        ProviderAuthEdge::Accepted => publisher.clear(Stage::ProviderAuth),
    }
}

/// What one observation proves about the seat's current turn, beside the credential axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnFailureEdge {
    /// The producer said this turn failed, and named a cause this build classified.
    Failed(Reason),
    /// A turn reached its ordinary end. Positive proof, and the only thing that clears a standing
    /// failure — silence never does, because silence is what a stuck seat produces.
    Recovered,
}

/// Record one turn edge on the seat's native-driver diagnostic.
///
/// A fresh publisher per edge, exactly like [`publish_provider_auth`] and for the same reason: the
/// producers of these edges are short-lived. The publisher seeds itself from the record on disk,
/// so a turn failure published from one hook process cannot hide a credential rejection published
/// from another.
pub(crate) fn publish_turn_failure(agent_dir: &Path, driver: Driver, edge: TurnFailureEdge) {
    let mut publisher = Publisher::new(agent_dir, driver, None, Support::Unknown);
    match edge {
        TurnFailureEdge::Failed(reason) => {
            publisher.publish(Stage::Turn, reason, Source::TurnError)
        }
        TurnFailureEdge::Recovered => publisher.clear(Stage::Turn),
    }
}

fn emit(
    driver: Driver,
    stage: Stage,
    reason: Reason,
    source: Source,
    support: Support,
    outcome: &'static str,
    producer_version: Option<&str>,
) {
    let span = crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st.driver.diagnostic",
            "span.label" = stage.as_str(),
            "st.driver.name" = driver.as_str(),
            "st.driver.stage" = stage.as_str(),
            "st.driver.reason" = reason.as_str(),
            "st.driver.source" = source.as_str(),
            "st.driver.support" = support.as_str(),
            "st.outcome" = outcome,
            "st.driver.producer_version" = producer_version,
        )
    });
    let _guard = span.as_ref().map(tracing::Span::enter);
    tracing::info!(
        driver = driver.as_str(),
        stage = stage.as_str(),
        reason = reason.as_str(),
        source = source.as_str(),
        support = support.as_str(),
        outcome,
        producer_version,
        "st native driver diagnostic transition"
    );
}

/// Durable replacement: the record's bytes reach disk before the rename and the directory entry is
/// synced after it.
///
/// The directory sync is now STRICT — a parent that cannot be opened for it makes this fail, where
/// it used to be swallowed. [`Publisher::persist`] already logs a failed publication and carries
/// on, so the visible consequence is one warning line, and the alternative was keeping a
/// durability level nothing can be made to fail.
fn atomic_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::fsatomic::replace(
        path,
        &bytes,
        crate::fsatomic::Staging::new(".driver-diagnostic"),
        crate::fsatomic::Durability::FsyncFileAndDir,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_reason_and_source_has_bounded_wire_vocabulary() {
        for stage in Stage::ALL {
            let wire = serde_json::to_value(stage).unwrap();
            assert_eq!(wire, serde_json::Value::String(stage.as_str().to_string()));
            assert_ne!(stage.as_str(), "unknown");
        }
        for reason in Reason::ALL {
            let wire = serde_json::to_value(reason).unwrap();
            assert_eq!(wire, serde_json::Value::String(reason.as_str().to_string()));
            assert_ne!(reason.as_str(), "unknown");
            assert!(Stage::ALL.contains(&reason.stage()));
        }
        for driver in Driver::ALL {
            let wire = serde_json::to_value(driver).unwrap();
            assert_eq!(wire, serde_json::Value::String(driver.as_str().to_string()));
        }
        for source in Source::ALL {
            let wire = serde_json::to_value(source).unwrap();
            assert_eq!(wire, serde_json::Value::String(source.as_str().to_string()));
            assert_ne!(source.as_str(), "unknown");
        }
    }

    #[test]
    fn degraded_launch_is_visible_until_an_exact_launch_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut publisher = Publisher::new(
            tmp.path(),
            Driver::Codex,
            Some("0.156.0".into()),
            Support::Supported,
        );
        publisher.publish(
            Stage::Launch,
            Reason::LaunchConfigurationRejected,
            Source::ProcessExit,
        );

        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!("degraded launch did not publish a visible diagnostic")
        };
        assert_eq!(failure.stage, Stage::Launch);
        assert_eq!(failure.reason, Reason::LaunchConfigurationRejected);
        assert_eq!(failure.source, Source::ProcessExit);
        assert!(repair_text(&Observed::Failure(failure)).contains("known-safe fallback"));

        publisher.clear(Stage::Launch);
        assert_eq!(read(&path(tmp.path())), Observed::Absent);
    }

    #[test]
    fn additive_fields_decode_but_malformed_foreign_and_unknown_records_are_indeterminate() {
        let valid = br#"{
          "schema":"st2.driver-diagnostic.v1","driver":"opencode","stage":"seed",
          "reason":"unknownStatus","source":"statusSnapshot","producerVersion":"1.18.19",
          "support":"supported","observedAt":100,"recovery":"clearsOnStageRecovery",
          "futureField":{"ignored":true}
        }"#;
        let Observed::Failure(failure) = read_at(valid, 125) else {
            panic!("valid additive record must remain readable")
        };
        assert_eq!(failure.evidence_age_ms, 25);
        assert_eq!(failure.stage, Stage::Seed);
        assert_eq!(
            read_at(b"not json", 0),
            Observed::Indeterminate(InvalidReason::MalformedRecord)
        );
        assert_eq!(
            read_at(
                &valid.replace(b"st2.driver-diagnostic.v1", b"st2.driver-diagnostic.v9"),
                0
            ),
            Observed::Indeterminate(InvalidReason::UnsupportedSchema)
        );
        assert_eq!(
            read_at(&valid.replace(b"unknownStatus", b"futureReason"), 0),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary)
        );
        assert_eq!(
            read_at(&valid.replace(b"opencode", b"futureDriver"), 0),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary)
        );
        assert_eq!(
            read_at(&valid.replace(b"supported", b"futureSupport"), 0),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary)
        );
        assert_eq!(
            read_at(&valid.replace(b"unknownStatus", b"notDurable"), 0),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary),
            "a known reason on the wrong stage is not valid evidence"
        );
        assert_eq!(
            read_at(
                &valid.replace(b"\"observedAt\":100", b"\"observedAt\":70000"),
                0,
            ),
            Observed::Indeterminate(InvalidReason::FutureSkew)
        );
    }

    /// The credential boundary is the one record a Claude hook, a Codex control pump, or an omp
    /// channel writes, so its wire pairing is pinned on its own: a rejection is evidence only when
    /// it came from the harness's typed turn result, and only on the stage whose repair text says
    /// "re-login".
    #[test]
    fn a_credential_rejection_is_evidence_only_from_a_typed_turn_result() {
        let valid = br#"{
          "schema":"st2.driver-diagnostic.v1","driver":"claude","stage":"providerAuth",
          "reason":"providerAuthRejected","source":"turnResult","support":"unknown",
          "observedAt":100,"recovery":"clearsOnStageRecovery"
        }"#;
        let observed = read_at(valid, 100);
        let Observed::Failure(failure) = &observed else {
            panic!("a credential rejection must read as a failure")
        };
        assert_eq!(failure.driver, Driver::Claude);
        assert_eq!(failure.support, Support::Unknown);
        assert!(failure.producer_version.is_none());
        assert!(
            repair_text(&observed).contains("re-login"),
            "{}",
            repair_text(&observed)
        );
        assert_eq!(
            read_at(&valid.replace(b"turnResult", b"eventStream"), 100),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary),
            "a rejection attributed to a channel that cannot carry a turn result is not evidence"
        );
        assert_eq!(
            read_at(&valid.replace(b"\"providerAuth\"", b"\"delivery\""), 100),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary),
            "the credential reason belongs to exactly one stage"
        );
        for (word, driver) in [
            (&b"\"codex\""[..], Driver::Codex),
            (b"\"omp\"", Driver::Omp),
        ] {
            let Observed::Failure(other) = read_at(&valid.replace(b"\"claude\"", word), 100) else {
                panic!("{driver:?} is an admitted driver word")
            };
            assert_eq!(other.driver, driver);
        }
    }

    /// Stage priority has to hold ACROSS processes, not only inside one. Every Claude hook
    /// invocation is its own process, so a publisher that started with an empty stage set would
    /// persist its own boundary over an earlier one somebody else published — and an operator
    /// would stop being told the real cause the moment a later symptom appeared.
    #[test]
    fn a_fresh_publisher_inherits_the_record_it_finds_so_stage_priority_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let record = path(tmp.path());
        Publisher::new(tmp.path(), Driver::Claude, None, Support::Unknown).publish(
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
        );

        // A later process publishes a LATER boundary. The earlier one must still be projected.
        Publisher::new(tmp.path(), Driver::Claude, None, Support::Unknown).publish(
            Stage::Turn,
            Reason::TurnUsageLimit,
            Source::TurnError,
        );
        let Observed::Failure(failure) = read(&record) else {
            panic!("the earlier boundary must still be the projected failure")
        };
        assert_eq!(failure.stage, Stage::ProviderAuth);

        // Seeding is scoped to this driver's own records: another driver's record is not this
        // publisher's history and must not be inherited.
        let foreign = tempfile::tempdir().unwrap();
        Publisher::new(foreign.path(), Driver::OpenCode, None, Support::Unknown).publish(
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
        );
        Publisher::new(foreign.path(), Driver::Claude, None, Support::Unknown).publish(
            Stage::Turn,
            Reason::TurnUsageLimit,
            Source::TurnError,
        );
        let Observed::Failure(failure) = read(&path(foreign.path())) else {
            panic!("this driver's own failure must be readable")
        };
        assert_eq!(failure.stage, Stage::Turn);
        assert_eq!(failure.driver, Driver::Claude);

        // And a record this reader cannot trust seeds nothing — inheriting a wrongly paired
        // record would launder it into evidence.
        let damaged = tempfile::tempdir().unwrap();
        fs::write(
            path(damaged.path()),
            br#"{"schema":"st2.driver-diagnostic.v1","driver":"claude","stage":"providerAuth",
                "reason":"notDurable","source":"turnResult","support":"unknown",
                "observedAt":1,"recovery":"clearsOnStageRecovery"}"#,
        )
        .unwrap();
        Publisher::new(damaged.path(), Driver::Claude, None, Support::Unknown).publish(
            Stage::Turn,
            Reason::TurnUsageLimit,
            Source::TurnError,
        );
        let Observed::Failure(failure) = read(&path(damaged.path())) else {
            panic!("the real failure must be readable")
        };
        assert_eq!(failure.stage, Stage::Turn);
    }

    /// The turn boundary is the one a RUNNING seat meets, so it is the one whose absence is most
    /// easily misread as health. Its wire pairing is pinned like the credential's: a turn failure
    /// is evidence only when it came from the producer's own error notification.
    #[test]
    fn a_turn_failure_is_evidence_only_from_the_producers_error_notification() {
        let valid = br#"{
          "schema":"st2.driver-diagnostic.v1","driver":"codex","stage":"turn",
          "reason":"turnServerOverloaded","source":"turnError","support":"supported",
          "producerVersion":"codex-cli 0.153.0","observedAt":100,
          "recovery":"clearsOnStageRecovery"
        }"#;
        let observed = read_at(valid, 100);
        let Observed::Failure(failure) = &observed else {
            panic!("a refused turn must read as a failure")
        };
        assert_eq!(failure.stage, Stage::Turn);
        assert_eq!(failure.reason, Reason::TurnServerOverloaded);
        assert_eq!(failure.source, Source::TurnError);
        assert!(
            !repair_text(&observed).contains("re-login"),
            "a refused turn is not a credential problem: {}",
            repair_text(&observed)
        );
        assert_eq!(
            read_at(&valid.replace(b"turnError", b"turnResult"), 100),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary),
            "the turn result names a completed turn; only the error notification carries a cause"
        );
        assert_eq!(
            read_at(&valid.replace(b"\"turn\"", b"\"delivery\""), 100),
            Observed::Indeterminate(InvalidReason::UnknownVocabulary),
            "every turn reason belongs to exactly one stage"
        );
        // Every turn reason must be readable off the wire on its own stage and source. A word
        // that decodes only because a sibling does is not a closed vocabulary.
        for reason in Reason::ALL.into_iter().filter(|r| r.stage() == Stage::Turn) {
            let bytes = valid.replace(b"turnServerOverloaded", reason.as_str().as_bytes());
            let Observed::Failure(failure) = read_at(&bytes, 100) else {
                panic!("{} must read as a failure", reason.as_str())
            };
            assert_eq!(failure.reason, reason);
        }
    }

    /// Projection order is load-bearing: a rejected credential is the CAUSE of the delivery and
    /// read-back failures it produces, so it must outrank them — while the gates that prove st2
    /// can read the producer at all still outrank it.
    #[test]
    fn a_rejected_credential_outranks_its_symptoms_but_not_the_producer_gates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut publisher = Publisher::new(
            tmp.path(),
            Driver::Codex,
            Some("codex-cli 0.153.0".to_string()),
            Support::Supported,
        );
        publisher.publish(
            Stage::ReadBack,
            Reason::ReadBackUnavailable,
            Source::MessageReadBack,
        );
        publisher.publish(
            Stage::Delivery,
            Reason::DeliveryUnavailable,
            Source::PromptTransport,
        );
        publisher.publish(
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
        );
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!("the credential boundary must be the projected failure")
        };
        assert_eq!(failure.stage, Stage::ProviderAuth);
        assert_eq!(failure.driver, Driver::Codex);
        assert_eq!(
            failure.producer_version.as_deref(),
            Some("codex-cli 0.153.0")
        );

        publisher.publish(Stage::Sse, Reason::SseDisconnected, Source::EventStream);
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!()
        };
        assert_eq!(
            failure.stage,
            Stage::Sse,
            "an unreadable producer stream makes any credential reading untrustworthy"
        );

        publisher.clear(Stage::Sse);
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!()
        };
        assert_eq!(failure.stage, Stage::ProviderAuth);

        publisher.clear(Stage::ProviderAuth);
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!()
        };
        assert_eq!(
            failure.stage,
            Stage::Delivery,
            "clearing the cause reveals the symptom it was hiding"
        );
    }

    #[test]
    fn recovery_clears_only_its_stage_and_reveals_the_next_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut publisher = Publisher::new(
            tmp.path(),
            Driver::OpenCode,
            Some("1.18.19".to_string()),
            Support::Supported,
        );
        publisher.publish(Stage::ReadBack, Reason::NotDurable, Source::MessageReadBack);
        publisher.publish(Stage::Sse, Reason::SseDisconnected, Source::EventStream);
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!()
        };
        assert_eq!(failure.stage, Stage::Sse, "earliest boundary wins");

        publisher.clear(Stage::ReadBack);
        let Observed::Failure(failure) = read(&path(tmp.path())) else {
            panic!()
        };
        assert_eq!(
            failure.stage,
            Stage::Sse,
            "unrelated recovery cannot clear SSE"
        );

        publisher.clear(Stage::Sse);
        assert_eq!(read(&path(tmp.path())), Observed::Absent);

        fs::write(path(tmp.path()), b"{bad").unwrap();
        assert_eq!(
            read(&path(tmp.path())),
            Observed::Indeterminate(InvalidReason::MalformedRecord)
        );
        let _successor = Publisher::new(
            tmp.path(),
            Driver::OpenCode,
            Some("1.18.19".to_string()),
            Support::Supported,
        );
        assert_eq!(
            read(&path(tmp.path())),
            Observed::Absent,
            "a replacement writer removes an unreadable predecessor snapshot"
        );
    }

    trait ReplaceBytes {
        fn replace(&self, from: &[u8], to: &[u8]) -> Vec<u8>;
    }

    impl ReplaceBytes for [u8] {
        fn replace(&self, from: &[u8], to: &[u8]) -> Vec<u8> {
            let at = self
                .windows(from.len())
                .position(|window| window == from)
                .unwrap();
            let mut out = Vec::with_capacity(self.len() - from.len() + to.len());
            out.extend_from_slice(&self[..at]);
            out.extend_from_slice(to);
            out.extend_from_slice(&self[at + from.len()..]);
            out
        }
    }

    /// The publication path writes into an agent-writable directory, so its staging file is the
    /// one place an agent could aim st2's own privilege at a file it does not own. Refusing an
    /// existing path is what stops that, and `0600` is what stops the diagnostic being readable
    /// by anyone who can reach the directory.
    #[test]
    fn a_planted_staging_symlink_is_refused_and_the_record_is_owner_only() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agents/h/worker");
        fs::create_dir_all(&agent).unwrap();

        let victim = tmp.path().join("authored");
        fs::write(&victim, b"authored bytes").unwrap();
        let planted = agent.join(".driver-diagnostic.tmp-planted");
        symlink(&victim, &planted).unwrap();

        let refused = crate::fsatomic::create_staging(&planted).unwrap_err();
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::AlreadyExists,
            "a planted symlink at the staging path must be refused, not followed"
        );
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"authored bytes",
            "the planted symlink was followed and its target was truncated"
        );

        let record = Record {
            schema: SCHEMA.to_owned(),
            driver: Driver::OpenCode,
            stage: Stage::Seed,
            reason: Reason::UnknownStatus,
            source: Source::StatusSnapshot,
            producer_version: None,
            support: Support::Supported,
            observed_at: 100,
            recovery: RECOVERY.to_owned(),
        };
        let path = agent.join("driver-diagnostic");
        atomic_json(&path, &record).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the diagnostic is readable by anyone who can reach the agent directory"
        );
        let residue = fs::read_dir(&agent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.starts_with(".driver-diagnostic.tmp-")
                    && name != ".driver-diagnostic.tmp-planted"
            })
            .collect::<Vec<_>>();
        assert!(
            residue.is_empty(),
            "staging residue left behind: {residue:?}"
        );
    }

    /// The directory sync is strict since the fold onto `fsatomic`: a parent that cannot be opened
    /// for it fails the publication, where it used to be swallowed. This is the deliberate
    /// behaviour change of that fold on this caller — [`Publisher::persist`] already logs a failed
    /// publication and carries on, so the visible consequence is one warning line for a record
    /// whose bytes did land.
    ///
    /// Real only for a non-root uid; the hermetic gate runs as the sandbox's unprivileged build
    /// user, and a local root run skips the edge instead of asserting what root cannot observe.
    #[test]
    fn a_directory_that_cannot_be_synced_fails_the_publication() {
        use std::os::unix::fs::PermissionsExt as _;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agents/h/worker");
        fs::create_dir_all(&agent).unwrap();
        let path = path(&agent);
        let record = Record {
            schema: SCHEMA.to_owned(),
            driver: Driver::OpenCode,
            stage: Stage::Seed,
            reason: Reason::UnknownStatus,
            source: Source::StatusSnapshot,
            producer_version: None,
            support: Support::Supported,
            observed_at: 100,
            recovery: RECOVERY.to_owned(),
        };

        // Write and traverse, but not read: staging and renaming still work, opening the
        // directory to sync it does not.
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o300)).unwrap();
        let published = atomic_json(&path, &record);
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            published.is_err(),
            "the diagnostic's directory sync is strict: {published:?}"
        );
        // The bytes did land — the rename happens before the sync — so the failure is a report
        // about durability, not about the record's contents.
        assert!(path.exists(), "the record still landed");
    }
}
