//! `st2.opencode-delivery-state.v1` — the OpenCode driver's own delivery record, written by every
//! release before `st2.delivery-ledger.v1`.
//!
//! Its loader silently discarded a record it could not validate and then re-POSTed the same
//! message id — which on OpenCode 1.18.19 appends the message's parts a second time. That is why
//! the canonical record is a new filename rather than a bump of this one.

use anyhow::Result;
use serde::Deserialize;

use super::{Adopting, Version};
use crate::delivery_ledger::{self, Correlation, Entry, Phase};
use crate::message;

pub(super) const SCHEMA: &str = "st2.opencode-delivery-state.v1";

/// The v1 wire struct. `runtimeId` is present in the bytes and deliberately not modelled: v1's own
/// load filter never looked at it, so comparing it here would drop a record the old binary would
/// have acted on.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Record {
    schema: String,
    agent: String,
    session_id: String,
    filename: String,
    message_id: String,
    phase: Label,
}

/// v1's two-value phase. `Accepted` was written from `GET /session/{s}/message/{m}` returning 200,
/// which proves the server **stored** the exact client message. It is not scheduling and not
/// consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Label {
    Attempted,
    Accepted,
}

impl Version for Record {
    const SCHEMA: &'static str = SCHEMA;

    fn schema(&self) -> &str {
        &self.schema
    }

    fn agent(&self) -> &str {
        &self.agent
    }
}

impl TryFrom<Adopting<'_, Record>> for Entry {
    type Error = anyhow::Error;

    fn try_from(adopting: Adopting<'_, Record>) -> Result<Self> {
        let Adopting { record, correlate } = adopting;
        anyhow::ensure!(
            message::is_message_filename(&record.filename) && !record.session_id.is_empty(),
            "v1 OpenCode delivery state has an invalid binding or filename"
        );
        anyhow::ensure!(
            record.message_id == correlate(&record.session_id, &record.filename),
            "v1 OpenCode delivery state correlation does not match its binding"
        );
        let phase = match record.phase {
            Label::Attempted => Phase::Attempted,
            // Storage, never consumption: mapping this to a released phase would make the
            // stored-but-never-admitted class permanently unretryable.
            Label::Accepted => Phase::Persisted,
        };
        Ok(delivery_ledger::asserted(
            record.filename,
            record.session_id,
            Correlation::native(record.message_id),
            phase,
            // v1 carried no incarnation, and the read-back is a durable query rather than a live
            // frame, so a pre-crash attempt is reconcilable without one.
            None,
        ))
    }
}
