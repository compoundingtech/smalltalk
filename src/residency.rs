//! Durable, provider-neutral runtime residency state.
//!
//! This module owns only the on-demand residency transition. Agent desired state, process
//! observation, provider-native session bindings, and message delivery remain authoritative in
//! their existing subsystems.

use std::fs;
use std::path::{Path, PathBuf};

use agent_spec::{AgentDesiredState, ResidencyPolicy, SessionDriver};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const LEDGER_SCHEMA: &str = "st2.residency-ledger.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimePresence {
    Present,
    Absent,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopStep {
    StopOwnedGroup,
    VerifyAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartStep {
    PrepareNativeResume,
    LaunchOwnedGroup,
    VerifyNativeSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RefusalReason {
    UnsupportedNativeResume,
    InvalidFence,
    RuntimeIndeterminate,
    GenerationExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum RuntimeResidency {
    Active,
    Quiescing {
        resume: Generation,
    },
    Stopping {
        step: StopStep,
        resume: Generation,
    },
    Cold {
        resume: Generation,
    },
    Starting {
        step: StartStep,
        source: Generation,
    },
    Refused {
        reason: RefusalReason,
        presence: RuntimePresence,
    },
}

impl RuntimeResidency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Quiescing { .. } => "quiescing",
            Self::Stopping { .. } => "stopping",
            Self::Cold { .. } => "cold",
            Self::Starting { .. } => "starting",
            Self::Refused { .. } => "refused",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryProjection {
    state: &'static str,
    generation: u64,
    wake_pending: bool,
    refusal_reason: Option<RefusalReason>,
    presence: Option<RuntimePresence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ledger {
    schema: String,
    agent_id: String,
    host: String,
    driver: SessionDriver,
    generation: Generation,
    runtime_residency: RuntimeResidency,
    wake_pending: bool,
}

impl Ledger {
    pub fn active(
        agent_id: impl Into<String>,
        host: impl Into<String>,
        driver: SessionDriver,
        generation: Generation,
    ) -> Result<Self, ValidationError> {
        let ledger = Self {
            schema: LEDGER_SCHEMA.to_owned(),
            agent_id: agent_id.into(),
            host: host.into(),
            driver,
            generation,
            runtime_residency: RuntimeResidency::Active,
            wake_pending: false,
        };
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn driver(&self) -> SessionDriver {
        self.driver
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn runtime_residency(&self) -> &RuntimeResidency {
        &self.runtime_residency
    }

    pub fn wake_pending(&self) -> bool {
        self.wake_pending
    }

    pub fn inventory_projection(&self) -> InventoryProjection {
        let (refusal_reason, presence) = match self.runtime_residency {
            RuntimeResidency::Refused { reason, presence } => (Some(reason), Some(presence)),
            _ => (None, None),
        };
        InventoryProjection {
            state: self.runtime_residency.as_str(),
            generation: self.generation.0,
            wake_pending: self.wake_pending,
            refusal_reason,
            presence,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema != LEDGER_SCHEMA {
            return Err(ValidationError::UnsupportedSchema(self.schema.clone()));
        }
        if self.agent_id.is_empty() {
            return Err(ValidationError::Invalid("agent ID is empty"));
        }
        if self.host.is_empty() {
            return Err(ValidationError::Invalid("host is empty"));
        }
        if self.generation.0 == 0 {
            return Err(ValidationError::Invalid("generation must be positive"));
        }
        if matches!(self.runtime_residency, RuntimeResidency::Cold { .. }) && self.wake_pending {
            return Err(ValidationError::Invalid(
                "cold residency cannot have pending wake demand",
            ));
        }

        match self.runtime_residency {
            RuntimeResidency::Quiescing { resume }
            | RuntimeResidency::Stopping { resume, .. }
            | RuntimeResidency::Cold { resume } => {
                if self.generation.0.checked_add(1) != Some(resume.0) {
                    return Err(ValidationError::Invalid("invalid resume generation fence"));
                }
            }
            RuntimeResidency::Starting { source, .. } => {
                if source.0.checked_add(1) != Some(self.generation.0) {
                    return Err(ValidationError::Invalid(
                        "invalid starting generation fence",
                    ));
                }
            }
            RuntimeResidency::Active | RuntimeResidency::Refused { .. } => {}
        }
        Ok(())
    }

    pub fn next_action(&self) -> Option<Action> {
        match self.runtime_residency {
            RuntimeResidency::Quiescing { resume } => Some(Action::CheckpointNativeSession {
                source: self.generation,
                resume,
            }),
            RuntimeResidency::Stopping {
                step: StopStep::StopOwnedGroup,
                ..
            } => Some(Action::StopOwnedGroup {
                generation: self.generation,
            }),
            RuntimeResidency::Stopping {
                step: StopStep::VerifyAbsent,
                ..
            } => Some(Action::VerifyAbsent {
                generation: self.generation,
            }),
            RuntimeResidency::Starting {
                step: StartStep::PrepareNativeResume,
                ..
            } => Some(Action::PrepareNativeResume {
                generation: self.generation,
            }),
            RuntimeResidency::Starting {
                step: StartStep::LaunchOwnedGroup,
                ..
            } => Some(Action::LaunchOwnedGroup {
                generation: self.generation,
            }),
            RuntimeResidency::Starting {
                step: StartStep::VerifyNativeSession,
                ..
            } => Some(Action::VerifyNativeSession {
                generation: self.generation,
            }),
            RuntimeResidency::Active
            | RuntimeResidency::Cold { .. }
            | RuntimeResidency::Refused { .. } => None,
        }
    }

    pub fn apply(&mut self, event: Event) -> Transition {
        match event {
            Event::WakeDemandObserved => {
                if self.runtime_residency == RuntimeResidency::Active {
                    return Transition::without_action(Outcome::Ignored);
                }
                if self.wake_pending {
                    return Transition::without_action(Outcome::Duplicate);
                }
                self.wake_pending = true;
                if let RuntimeResidency::Cold { resume } = self.runtime_residency {
                    let source = self.generation;
                    self.generation = resume;
                    self.runtime_residency = RuntimeResidency::Starting {
                        step: StartStep::PrepareNativeResume,
                        source,
                    };
                    return Transition::applied(self.next_action());
                }
                Transition::applied(None)
            }
            Event::IdleConfirmed { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                if self.runtime_residency != RuntimeResidency::Active || self.wake_pending {
                    return Transition::without_action(Outcome::Ignored);
                }
                let Some(resume) = generation.0.checked_add(1).map(Generation) else {
                    self.runtime_residency = RuntimeResidency::Refused {
                        reason: RefusalReason::GenerationExhausted,
                        presence: RuntimePresence::Present,
                    };
                    return Transition::without_action(Outcome::Refused(
                        RefusalReason::GenerationExhausted,
                    ));
                };
                self.runtime_residency = RuntimeResidency::Quiescing { resume };
                Transition::applied(self.next_action())
            }
            Event::CheckpointStored { source, resume } => {
                if source != self.generation {
                    return self.inert(source);
                }
                if self.runtime_residency != (RuntimeResidency::Quiescing { resume }) {
                    return Transition::without_action(Outcome::Ignored);
                }
                self.runtime_residency = RuntimeResidency::Stopping {
                    step: StopStep::StopOwnedGroup,
                    resume,
                };
                Transition::applied(self.next_action())
            }
            Event::OwnedGroupStopped { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                let RuntimeResidency::Stopping {
                    step: StopStep::StopOwnedGroup,
                    resume,
                } = self.runtime_residency
                else {
                    return Transition::without_action(Outcome::Ignored);
                };
                self.runtime_residency = RuntimeResidency::Stopping {
                    step: StopStep::VerifyAbsent,
                    resume,
                };
                Transition::applied(self.next_action())
            }
            Event::AbsenceVerified { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                let RuntimeResidency::Stopping {
                    step: StopStep::VerifyAbsent,
                    resume,
                } = self.runtime_residency
                else {
                    return Transition::without_action(Outcome::Ignored);
                };
                if self.wake_pending {
                    let source = self.generation;
                    self.generation = resume;
                    self.runtime_residency = RuntimeResidency::Starting {
                        step: StartStep::PrepareNativeResume,
                        source,
                    };
                    Transition::applied(self.next_action())
                } else {
                    self.runtime_residency = RuntimeResidency::Cold { resume };
                    Transition::applied(None)
                }
            }
            Event::NativeResumePrepared { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                let RuntimeResidency::Starting {
                    step: StartStep::PrepareNativeResume,
                    source,
                } = self.runtime_residency
                else {
                    return Transition::without_action(Outcome::Ignored);
                };
                self.runtime_residency = RuntimeResidency::Starting {
                    step: StartStep::LaunchOwnedGroup,
                    source,
                };
                Transition::applied(self.next_action())
            }
            Event::OwnedGroupLaunched { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                let RuntimeResidency::Starting {
                    step: StartStep::LaunchOwnedGroup,
                    source,
                } = self.runtime_residency
                else {
                    return Transition::without_action(Outcome::Ignored);
                };
                self.runtime_residency = RuntimeResidency::Starting {
                    step: StartStep::VerifyNativeSession,
                    source,
                };
                Transition::applied(self.next_action())
            }
            Event::NativeSessionVerified { generation } => {
                if generation != self.generation {
                    return self.inert(generation);
                }
                if !matches!(
                    self.runtime_residency,
                    RuntimeResidency::Starting {
                        step: StartStep::VerifyNativeSession,
                        ..
                    }
                ) {
                    return Transition::without_action(Outcome::Ignored);
                }
                self.runtime_residency = RuntimeResidency::Active;
                self.wake_pending = false;
                Transition::applied(None)
            }
            Event::Refused {
                generation,
                reason,
                presence,
            } => {
                if generation != self.generation || self.next_action().is_none() {
                    return self.inert(generation);
                }
                self.runtime_residency = RuntimeResidency::Refused { reason, presence };
                Transition::without_action(Outcome::Refused(reason))
            }
        }
    }

    fn inert(&self, generation: Generation) -> Transition {
        Transition::without_action(if generation == self.generation {
            Outcome::Ignored
        } else {
            Outcome::Stale
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    WakeDemandObserved,
    IdleConfirmed {
        generation: Generation,
    },
    CheckpointStored {
        source: Generation,
        resume: Generation,
    },
    OwnedGroupStopped {
        generation: Generation,
    },
    AbsenceVerified {
        generation: Generation,
    },
    NativeResumePrepared {
        generation: Generation,
    },
    OwnedGroupLaunched {
        generation: Generation,
    },
    NativeSessionVerified {
        generation: Generation,
    },
    Refused {
        generation: Generation,
        reason: RefusalReason,
        presence: RuntimePresence,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CheckpointNativeSession {
        source: Generation,
        resume: Generation,
    },
    StopOwnedGroup {
        generation: Generation,
    },
    VerifyAbsent {
        generation: Generation,
    },
    PrepareNativeResume {
        generation: Generation,
    },
    LaunchOwnedGroup {
        generation: Generation,
    },
    VerifyNativeSession {
        generation: Generation,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Duplicate,
    Ignored,
    Stale,
    Refused(RefusalReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub outcome: Outcome,
    pub action: Option<Action>,
}

impl Transition {
    fn applied(action: Option<Action>) -> Self {
        Self {
            outcome: Outcome::Applied,
            action,
        }
    }

    fn without_action(outcome: Outcome) -> Self {
        Self {
            outcome,
            action: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    UnsupportedSchema(String),
    Invalid(&'static str),
    OwnershipMismatch,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(schema) => write!(formatter, "unsupported schema {schema:?}"),
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::OwnershipMismatch => formatter.write_str("residency ledger ownership mismatch"),
        }
    }
}

impl std::error::Error for ValidationError {}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Malformed(serde_json::Error),
    Invalid(ValidationError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "read residency ledger: {error}"),
            Self::Malformed(error) => write!(formatter, "decode residency ledger: {error}"),
            Self::Invalid(error) => write!(formatter, "validate residency ledger: {error}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// On-demand residency applies only while the independent desired lifecycle remains running.
pub fn enabled(desired: &AgentDesiredState, policy: ResidencyPolicy) -> bool {
    desired.is_running() && policy == ResidencyPolicy::OnDemand
}

/// Stable host-local ledger path keyed by the immutable agent ID rather than its mutable address.
pub fn ledger_path(catalog: &Path, host: &str, agent_id: &str) -> PathBuf {
    let mut hash = Sha256::new();
    for value in [host.as_bytes(), agent_id.as_bytes()] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    catalog
        .join(crate::catalog_lock::CONTROL_DIR)
        .join("residency")
        .join(format!("{:x}.json", hash.finalize()))
}

pub fn load(
    path: &Path,
    expected_agent_id: &str,
    expected_host: &str,
    expected_driver: SessionDriver,
) -> Result<Option<Ledger>, LoadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(LoadError::Io(error)),
    };
    let ledger: Ledger = serde_json::from_slice(&bytes).map_err(LoadError::Malformed)?;
    ledger.validate().map_err(LoadError::Invalid)?;
    if ledger.agent_id != expected_agent_id
        || ledger.host != expected_host
        || ledger.driver != expected_driver
    {
        return Err(LoadError::Invalid(ValidationError::OwnershipMismatch));
    }
    Ok(Some(ledger))
}

pub fn store(path: &Path, ledger: &Ledger) -> Result<(), LoadError> {
    ledger.validate().map_err(LoadError::Invalid)?;
    atomic_json(path, ledger).map_err(LoadError::Io)
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ledger path has no parent",
        )
    })?;
    create_dir_all_durable(parent)?;
    crate::fsatomic::replace(
        path,
        &bytes,
        crate::fsatomic::Staging::new(".residency-ledger"),
        crate::fsatomic::Durability::FsyncFileAndDir,
    )
}

pub(crate) fn create_dir_all_durable(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.try_exists()? {
        missing.push(cursor.to_path_buf());
        cursor = cursor.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger directory has no existing ancestor",
            )
        })?;
    }
    fs::create_dir_all(path)?;
    for directory in missing.into_iter().rev() {
        let parent = directory.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "created ledger directory has no parent",
            )
        })?;
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> Ledger {
        Ledger::active("agent-1", "host-a", SessionDriver::Codex, Generation(1)).unwrap()
    }

    fn apply(ledger: &mut Ledger, event: Event, action: Action) {
        let transition = ledger.apply(event);
        assert_eq!(transition.outcome, Outcome::Applied);
        assert_eq!(transition.action, Some(action));
        ledger.validate().unwrap();
    }

    fn make_cold(ledger: &mut Ledger) {
        apply(
            ledger,
            Event::IdleConfirmed {
                generation: Generation(1),
            },
            Action::CheckpointNativeSession {
                source: Generation(1),
                resume: Generation(2),
            },
        );
        apply(
            ledger,
            Event::CheckpointStored {
                source: Generation(1),
                resume: Generation(2),
            },
            Action::StopOwnedGroup {
                generation: Generation(1),
            },
        );
        apply(
            ledger,
            Event::OwnedGroupStopped {
                generation: Generation(1),
            },
            Action::VerifyAbsent {
                generation: Generation(1),
            },
        );
        assert_eq!(
            ledger
                .apply(Event::AbsenceVerified {
                    generation: Generation(1),
                })
                .action,
            None
        );
        assert!(matches!(
            ledger.runtime_residency(),
            RuntimeResidency::Cold {
                resume: Generation(2)
            }
        ));
    }

    #[test]
    fn demand_is_already_fulfilled_while_active() {
        let mut ledger = active();

        assert_eq!(
            ledger.apply(Event::WakeDemandObserved),
            Transition::without_action(Outcome::Ignored)
        );
        assert!(!ledger.wake_pending());
        assert_eq!(
            ledger
                .apply(Event::IdleConfirmed {
                    generation: Generation(1),
                })
                .action,
            Some(Action::CheckpointNativeSession {
                source: Generation(1),
                resume: Generation(2),
            })
        );
    }

    #[test]
    fn cold_wake_requires_the_complete_monotonic_sequence() {
        let mut ledger = active();
        make_cold(&mut ledger);
        apply(
            &mut ledger,
            Event::WakeDemandObserved,
            Action::PrepareNativeResume {
                generation: Generation(2),
            },
        );
        apply(
            &mut ledger,
            Event::NativeResumePrepared {
                generation: Generation(2),
            },
            Action::LaunchOwnedGroup {
                generation: Generation(2),
            },
        );
        apply(
            &mut ledger,
            Event::OwnedGroupLaunched {
                generation: Generation(2),
            },
            Action::VerifyNativeSession {
                generation: Generation(2),
            },
        );
        assert_eq!(
            ledger
                .apply(Event::NativeSessionVerified {
                    generation: Generation(2),
                })
                .outcome,
            Outcome::Applied
        );
        assert_eq!(ledger.runtime_residency(), &RuntimeResidency::Active);
        assert!(!ledger.wake_pending());
    }

    #[test]
    fn demand_after_checkpoint_waits_for_verified_absence_before_starting() {
        let mut ledger = active();
        apply(
            &mut ledger,
            Event::IdleConfirmed {
                generation: Generation(1),
            },
            Action::CheckpointNativeSession {
                source: Generation(1),
                resume: Generation(2),
            },
        );
        assert_eq!(
            ledger.apply(Event::WakeDemandObserved),
            Transition::applied(None)
        );
        apply(
            &mut ledger,
            Event::CheckpointStored {
                source: Generation(1),
                resume: Generation(2),
            },
            Action::StopOwnedGroup {
                generation: Generation(1),
            },
        );
        apply(
            &mut ledger,
            Event::OwnedGroupStopped {
                generation: Generation(1),
            },
            Action::VerifyAbsent {
                generation: Generation(1),
            },
        );
        apply(
            &mut ledger,
            Event::AbsenceVerified {
                generation: Generation(1),
            },
            Action::PrepareNativeResume {
                generation: Generation(2),
            },
        );
    }

    #[test]
    fn every_in_flight_phase_restores_one_idempotent_next_action() {
        let mut ledger = active();
        let events = [
            Event::IdleConfirmed {
                generation: Generation(1),
            },
            Event::CheckpointStored {
                source: Generation(1),
                resume: Generation(2),
            },
            Event::OwnedGroupStopped {
                generation: Generation(1),
            },
            Event::AbsenceVerified {
                generation: Generation(1),
            },
        ];
        ledger.apply(Event::WakeDemandObserved);
        for event in events {
            ledger.apply(event);
            let bytes = serde_json::to_vec(&ledger).unwrap();
            let restored: Ledger = serde_json::from_slice(&bytes).unwrap();
            restored.validate().unwrap();
            assert_eq!(restored, ledger);
            assert_eq!(
                restored.next_action().is_some(),
                !matches!(
                    restored.runtime_residency(),
                    RuntimeResidency::Active
                        | RuntimeResidency::Cold { .. }
                        | RuntimeResidency::Refused { .. }
                )
            );
        }
    }

    #[test]
    fn stale_and_duplicate_events_are_inert() {
        let mut ledger = active();
        assert_eq!(
            ledger
                .apply(Event::IdleConfirmed {
                    generation: Generation(0),
                })
                .outcome,
            Outcome::Stale
        );
        assert_eq!(
            ledger
                .apply(Event::IdleConfirmed {
                    generation: Generation(1),
                })
                .outcome,
            Outcome::Applied
        );
        assert_eq!(
            ledger.apply(Event::WakeDemandObserved).outcome,
            Outcome::Applied
        );
        assert_eq!(
            ledger.apply(Event::WakeDemandObserved).outcome,
            Outcome::Duplicate
        );
    }

    #[test]
    fn invalid_schema_fence_and_owner_fail_closed() {
        let mut ledger = active();
        ledger.schema = "st2.residency-ledger.v2".into();
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::UnsupportedSchema(_))
        ));

        let mut fence = active();
        fence.runtime_residency = RuntimeResidency::Cold {
            resume: Generation(9),
        };
        assert_eq!(
            fence.validate(),
            Err(ValidationError::Invalid("invalid resume generation fence"))
        );

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.json");
        store(&path, &active()).unwrap();
        assert!(matches!(
            load(&path, "another-agent", "host-a", SessionDriver::Codex),
            Err(LoadError::Invalid(ValidationError::OwnershipMismatch))
        ));
    }

    #[test]
    fn cold_with_pending_wake_fails_closed_on_load() {
        let mut ledger = active();
        make_cold(&mut ledger);
        ledger.wake_pending = true;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.json");
        fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();

        assert!(matches!(
            load(&path, "agent-1", "host-a", SessionDriver::Codex),
            Err(LoadError::Invalid(ValidationError::Invalid(
                "cold residency cannot have pending wake demand"
            )))
        ));
    }

    #[test]
    fn durable_store_replaces_atomically_at_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/residency.json");
        let mut ledger = active();
        store(&path, &ledger).unwrap();
        ledger.apply(Event::WakeDemandObserved);
        store(&path, &ledger).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            load(&path, "agent-1", "host-a", SessionDriver::Codex)
                .unwrap()
                .unwrap(),
            ledger
        );
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn desired_lifecycle_and_residency_policy_are_independent() {
        assert!(enabled(
            &AgentDesiredState::Running,
            ResidencyPolicy::OnDemand
        ));
        assert!(!enabled(
            &AgentDesiredState::Running,
            ResidencyPolicy::Always
        ));
        assert!(!enabled(
            &AgentDesiredState::Suspended {
                reason: "operator hold".into(),
            },
            ResidencyPolicy::OnDemand
        ));
        assert!(!enabled(
            &AgentDesiredState::Retired { reason: None },
            ResidencyPolicy::OnDemand
        ));
    }

    #[test]
    fn ledger_path_is_stable_and_separates_host_and_agent_components() {
        let catalog = Path::new("/catalog");
        assert_eq!(
            ledger_path(catalog, "a", "bc"),
            ledger_path(catalog, "a", "bc")
        );
        assert_ne!(
            ledger_path(catalog, "a", "bc"),
            ledger_path(catalog, "ab", "c")
        );
    }
}
