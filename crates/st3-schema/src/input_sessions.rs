//! Owner-authored, metadata-only ordered terminal input audit.
//!
//! A snapshot proves successful send-return totals, not delivery or replay. Open
//! and checkpoint snapshots are incomplete; interrupted snapshots retain only
//! the last proven counters. Only a closed snapshot without an uncertain handoff
//! can describe a complete session. Cross-snapshot fencing belongs to the store.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ValidationError, error};

/// Largest integer represented exactly by every supported JSON client.
pub const MAX_AUDIT_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputSessionEvent {
    Opened,
    Checkpoint,
    Closed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputSessionCloseReason {
    ClientClose,
    SocketDisconnected,
    Replaced,
    Gap,
    Rejected,
    Detached,
    IncarnationChanged,
    Revoked,
    AuditUnavailable,
    OwnerRestarted,
}

/// Fixed audit metadata. Nullable facts are emitted explicitly, never omitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputSessionRecord {
    pub version: u32,
    pub ordinal: u64,
    pub event: InputSessionEvent,
    pub session_id: String,
    pub owner: String,
    pub owner_epoch: String,
    pub terminal: String,
    pub incarnation: String,
    pub attachment: String,
    pub attachment_claim: String,
    pub device_id: Option<String>,
    pub device_actor: String,
    pub authority_actor: String,
    pub person: Option<String>,
    pub pairing_claim: Option<String>,
    pub opened_at_unix_ms: u64,
    pub observed_at_unix_ms: u64,
    pub successful_send_bytes: u64,
    pub successful_batches: u64,
    pub uncertain_handoff: bool,
    pub reason: Option<InputSessionCloseReason>,
}

impl InputSessionRecord {
    pub fn subject(&self) -> String {
        format!("input-session/{}/{}", self.owner, self.session_id)
    }

    /// Validate one snapshot. Every audit integer fits the exact JSON number
    /// domain shared by Rust, Swift and TypeScript clients.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.version != 1
            || self.ordinal > MAX_AUDIT_INTEGER
            || self.opened_at_unix_ms > MAX_AUDIT_INTEGER
            || self.observed_at_unix_ms > MAX_AUDIT_INTEGER
            || self.successful_send_bytes > MAX_AUDIT_INTEGER
            || self.successful_batches > MAX_AUDIT_INTEGER
        {
            return Err(invalid("unsupported version or out-of-range audit integer"));
        }
        if !valid_owner(&self.owner)
            || "input-session/".len() + self.owner.len() + 1 + self.session_id.len() > 512
            || !super::glasses::valid_uuid(&self.session_id)
            || !super::glasses::valid_uuid(&self.owner_epoch)
        {
            return Err(invalid("invalid owner, session UUID or process epoch UUID"));
        }
        for value in [
            self.incarnation.as_str(),
            self.attachment_claim.as_str(),
            self.device_actor.as_str(),
        ] {
            if !valid_identifier(value) {
                return Err(invalid(
                    "audit identifiers must be bounded without whitespace",
                ));
            }
        }
        for value in [self.device_id.as_deref(), self.pairing_claim.as_deref()]
            .into_iter()
            .flatten()
        {
            if !valid_identifier(value) {
                return Err(invalid(
                    "nullable identifiers must be null or a bounded identifier",
                ));
            }
        }
        for subject in [&self.terminal, &self.attachment, &self.authority_actor] {
            super::registry().validate_subject(subject)?;
        }
        let person = self.authority_actor.strip_prefix("person/");
        if let Some(name) = person {
            if name.contains('/') || self.person.as_deref() != Some(self.authority_actor.as_str()) {
                return Err(invalid(
                    "a concrete person authority requires the exact delegated person",
                ));
            }
        } else if !self.authority_actor.starts_with("agent/") || self.person.is_some() {
            return Err(invalid(
                "a local agent authority must not attribute a delegated person",
            ));
        }
        if self.observed_at_unix_ms < self.opened_at_unix_ms
            || self.successful_batches > self.successful_send_bytes
            || (self.successful_batches == 0) != (self.successful_send_bytes == 0)
        {
            return Err(invalid(
                "invalid observation time or successful-send counters",
            ));
        }
        let valid_phase = match self.event {
            InputSessionEvent::Opened => {
                self.ordinal == 0
                    && self.opened_at_unix_ms == self.observed_at_unix_ms
                    && self.successful_send_bytes == 0
                    && self.successful_batches == 0
                    && !self.uncertain_handoff
                    && self.reason.is_none()
            }
            InputSessionEvent::Checkpoint => self.ordinal > 0 && self.reason.is_none(),
            InputSessionEvent::Closed => {
                self.ordinal > 0
                    && self.reason.is_some()
                    && self.reason != Some(InputSessionCloseReason::OwnerRestarted)
            }
            InputSessionEvent::Interrupted => {
                self.ordinal > 0 && self.reason == Some(InputSessionCloseReason::OwnerRestarted)
            }
        };
        if !valid_phase {
            return Err(invalid(
                "event, ordinal, counters and close reason do not form a valid phase",
            ));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> ValidationError {
    error("invalid-input-session", message)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
}

fn valid_owner(value: &str) -> bool {
    valid_identifier(value) && !value.contains('/')
}

pub(crate) fn validate_subject(subject: &str) -> Result<(), ValidationError> {
    let Some((owner, id)) = subject
        .strip_prefix("input-session/")
        .and_then(|suffix| suffix.split_once('/'))
    else {
        return Err(invalid(
            "an input session subject requires input-session/OWNER/UUID",
        ));
    };
    if subject.len() > 512 || !valid_owner(owner) || !super::glasses::valid_uuid(id) {
        return Err(invalid(
            "an input session subject requires a bounded owner and canonical UUID",
        ));
    }
    Ok(())
}

pub(crate) fn validate_fields(
    subject: &str,
    fields: &BTreeMap<String, Value>,
) -> Result<InputSessionRecord, ValidationError> {
    validate_subject(subject)?;
    // Option fields deserialize missing values as None; durable snapshots must
    // nevertheless carry every nullable fact explicitly.
    for name in ["device_id", "person", "pairing_claim", "reason"] {
        if !fields.contains_key(name) {
            return Err(invalid(
                "audit snapshots must include nullable metadata fields",
            ));
        }
    }
    let record = InputSessionRecord::deserialize(serde::de::value::MapDeserializer::new(
        fields.iter().map(|(key, value)| (key.as_str(), value)),
    ))
    .map_err(|_| invalid("audit fields must match the fixed metadata-only record schema"))?;
    record.validate()?;
    if subject
        .strip_prefix("input-session/")
        .and_then(|suffix| suffix.split_once('/'))
        != Some((record.owner.as_str(), record.session_id.as_str()))
    {
        return Err(invalid(
            "audit subject does not match its owner and session UUID",
        ));
    }
    Ok(record)
}

/// Only the node named by both the reserved subject and metadata may issue an
/// audit record. System-only policy alone is not an owner-origin proof.
pub fn validate_origin(
    subject: &str,
    origin: &str,
    actor: Option<&str>,
    fields: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    let record = validate_fields(subject, fields)?;
    if actor.is_some() || origin != record.owner {
        return Err(error(
            "input-session-owner-forbidden",
            "only the terminal owner daemon may issue an unattributed input audit record",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opened() -> InputSessionRecord {
        InputSessionRecord {
            version: 1,
            ordinal: 0,
            event: InputSessionEvent::Opened,
            session_id: "019a0000-0000-7000-8000-000000000001".into(),
            owner: "owner-a".into(),
            owner_epoch: "019a0000-0000-7000-8000-000000000002".into(),
            terminal: "pty/run/shell".into(),
            incarnation: "incarnation-1".into(),
            attachment: "custom/client/terminal-attachment-viewer".into(),
            attachment_claim: "claim-viewer".into(),
            device_id: Some("device/phone".into()),
            device_actor: "person/ada/session/phone".into(),
            authority_actor: "person/ada".into(),
            person: Some("person/ada".into()),
            pairing_claim: Some("claim-pairing".into()),
            opened_at_unix_ms: 1_000,
            observed_at_unix_ms: 1_000,
            successful_send_bytes: 0,
            successful_batches: 0,
            uncertain_handoff: false,
            reason: None,
        }
    }

    fn fields(record: &InputSessionRecord) -> BTreeMap<String, Value> {
        serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap()
    }

    fn validate(record: &InputSessionRecord) -> Result<(), ValidationError> {
        super::super::registry()
            .validate_claim(&record.subject(), "terminal.input-session", &fields(record))
            .map(|_| ())
    }

    #[test]
    fn owner_origin_and_public_write_boundaries_cannot_be_forged() {
        let record = opened();
        let fields = fields(&record);
        validate_origin(&record.subject(), "owner-a", None, &fields).unwrap();
        for (origin, actor) in [
            ("owner-b", None),
            ("owner-a", Some("person/ada")),
            ("owner-a", Some("daemon/owner-a")),
        ] {
            assert_eq!(
                validate_origin(&record.subject(), origin, actor, &fields)
                    .unwrap_err()
                    .code,
                "input-session-owner-forbidden"
            );
        }
        let forged_subject = record.subject().replace("owner-a", "owner-b");
        assert!(validate_origin(&forged_subject, "owner-b", None, &fields).is_err());
        let subject = record.subject();
        for actor in [
            None,
            Some("person/ada"),
            Some("daemon/owner-a"),
            Some(subject.as_str()),
        ] {
            assert_eq!(
                super::super::registry()
                    .validate_public_claim(
                        &record.subject(),
                        "terminal.input-session",
                        &fields,
                        actor
                    )
                    .unwrap_err()
                    .code,
                "claim-write-forbidden"
            );
        }
        assert!(
            super::super::registry()
                .validate_claim(&record.subject(), "custom.audit.input", &fields)
                .is_err()
        );
        assert!(
            super::super::registry()
                .validate_claim(&record.subject(), "intent.desired", &BTreeMap::new())
                .is_err()
        );
    }

    #[test]
    fn audit_integers_are_exact_at_the_shared_json_boundary() {
        let mut record = opened();
        record.event = InputSessionEvent::Checkpoint;
        record.ordinal = MAX_AUDIT_INTEGER;
        record.opened_at_unix_ms = MAX_AUDIT_INTEGER;
        record.observed_at_unix_ms = MAX_AUDIT_INTEGER;
        record.successful_send_bytes = MAX_AUDIT_INTEGER;
        record.successful_batches = MAX_AUDIT_INTEGER;
        validate(&record).unwrap();
        for name in [
            "ordinal",
            "opened_at_unix_ms",
            "observed_at_unix_ms",
            "successful_send_bytes",
            "successful_batches",
        ] {
            let mut overflow = fields(&record);
            overflow.insert(name.into(), json!(MAX_AUDIT_INTEGER + 1));
            assert_eq!(
                super::super::registry()
                    .validate_claim(&record.subject(), "terminal.input-session", &overflow)
                    .unwrap_err()
                    .code,
                "invalid-input-session",
                "accepted inexact {name}"
            );
        }
    }

    #[test]
    fn lifecycle_phases_and_send_counts_are_semantic_constraints() {
        let record = opened();
        validate(&record).unwrap();
        let mut checkpoint = record.clone();
        checkpoint.event = InputSessionEvent::Checkpoint;
        checkpoint.ordinal = 1;
        checkpoint.observed_at_unix_ms = 1_100;
        checkpoint.successful_send_bytes = 7;
        checkpoint.successful_batches = 2;
        validate(&checkpoint).unwrap();
        let mut closed = checkpoint.clone();
        closed.event = InputSessionEvent::Closed;
        closed.ordinal = 2;
        closed.reason = Some(InputSessionCloseReason::ClientClose);
        validate(&closed).unwrap();
        closed.uncertain_handoff = true;
        validate(&closed).unwrap();
        let mut interrupted = checkpoint.clone();
        interrupted.event = InputSessionEvent::Interrupted;
        interrupted.reason = Some(InputSessionCloseReason::OwnerRestarted);
        validate(&interrupted).unwrap();

        let mut invalid_records = Vec::new();
        let mut changed = record.clone();
        changed.ordinal = 1;
        invalid_records.push(changed);
        let mut changed = record.clone();
        changed.successful_send_bytes = 1;
        changed.successful_batches = 1;
        invalid_records.push(changed);
        let mut changed = record.clone();
        changed.uncertain_handoff = true;
        invalid_records.push(changed);
        let mut changed = record.clone();
        changed.observed_at_unix_ms += 1;
        invalid_records.push(changed);
        let mut changed = checkpoint.clone();
        changed.ordinal = 0;
        invalid_records.push(changed);
        let mut changed = checkpoint.clone();
        changed.reason = Some(InputSessionCloseReason::Gap);
        invalid_records.push(changed);
        let mut changed = checkpoint.clone();
        changed.successful_batches = 8;
        invalid_records.push(changed);
        let mut changed = checkpoint.clone();
        changed.successful_batches = 0;
        invalid_records.push(changed);
        let mut changed = checkpoint.clone();
        changed.observed_at_unix_ms = 999;
        invalid_records.push(changed);
        let mut changed = closed.clone();
        changed.reason = None;
        invalid_records.push(changed);
        let mut changed = closed;
        changed.reason = Some(InputSessionCloseReason::OwnerRestarted);
        invalid_records.push(changed);
        let mut changed = interrupted;
        changed.reason = Some(InputSessionCloseReason::SocketDisconnected);
        invalid_records.push(changed);
        for changed in invalid_records {
            assert!(
                validate(&changed).is_err(),
                "accepted invalid record: {changed:?}"
            );
        }
    }

    #[test]
    fn fixed_schema_rejects_payloads_unknown_versions_and_invalid_identifiers() {
        let record = opened();
        for name in ["text", "bytes", "digest", "payload", "raw_input"] {
            let mut forged = fields(&record);
            forged.insert(name.into(), json!("secret"));
            assert!(
                super::super::registry()
                    .validate_claim(&record.subject(), "terminal.input-session", &forged)
                    .is_err()
            );
            assert!(validate_origin(&record.subject(), "owner-a", None, &forged).is_err());
        }
        for (name, value) in [
            ("version", json!(2)),
            ("ordinal", json!(-1)),
            ("ordinal", json!(u64::MAX)),
            ("opened_at_unix_ms", json!(u64::MAX)),
            ("successful_send_bytes", json!(-1)),
            ("session_id", json!("not-a-uuid")),
            ("owner_epoch", json!("019A0000-0000-7000-8000-000000000002")),
            ("owner", json!("other/owner")),
            ("incarnation", json!("x".repeat(513))),
            ("attachment_claim", json!("claim\nsecret")),
            ("device_id", json!("")),
            ("person", json!("person/other")),
            ("authority_actor", json!("person/ada/session/phone")),
            ("reason", json!("unknown")),
            ("event", json!("replayed")),
        ] {
            let mut forged = fields(&record);
            forged.insert(name.into(), value);
            assert!(
                super::super::registry()
                    .validate_claim(&record.subject(), "terminal.input-session", &forged)
                    .is_err(),
                "accepted {name}"
            );
        }
        for name in ["person", "pairing_claim", "device_id", "reason"] {
            let mut missing = fields(&record);
            missing.remove(name);
            assert!(validate_origin(&record.subject(), "owner-a", None, &missing).is_err());
        }
    }

    #[test]
    fn local_agent_authority_does_not_fabricate_a_person() {
        let mut record = opened();
        record.authority_actor = "agent/run/worker".into();
        record.device_actor = record.authority_actor.clone();
        record.person = None;
        record.device_id = None;
        record.pairing_claim = None;
        validate(&record).unwrap();
        let serialized = serde_json::to_value(&record).unwrap();
        assert_eq!(serialized["person"], Value::Null);
        record.person = Some("person/ada".into());
        assert!(validate(&record).is_err());
        record.person = None;
        record.authority_actor = "person/ada".into();
        assert!(validate(&record).is_err());
    }
}
