//! `st2.codex-delivery-state.v1` — the Codex driver's own delivery record, written by every
//! release before `st2.delivery-ledger.v1`.
//!
//! Its loader `ensure!`d its schema and denied unknown fields, so it refuses to start on anything
//! it does not recognize. That is why the canonical record is a new filename rather than a bump of
//! this one, and why nothing here writes: this file only reads bytes an older binary left.

use anyhow::Result;
use serde::Deserialize;

use super::{Adopting, Version};
use crate::delivery_ledger::{self, Correlation, Entry, Phase};
use crate::message;

pub(super) const SCHEMA: &str = "st2.codex-delivery-state.v1";

/// The v1 wire struct. `runtimeId` is present in the bytes and deliberately not modelled: v1
/// compared it and hard-errored on drift, and this translation carries a drifted record forward
/// instead (see [`super::translate`]).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Record {
    schema: String,
    agent: String,
    #[serde(default)]
    runtime_incarnation: String,
    thread_id: String,
    filename: String,
    client_id: String,
    phase: Label,
}

/// v1's two-value phase. `Accepted` was written **only** from the typed
/// `item/completed{userMessage, clientId}` event inside a turn, so it means the model received
/// the message — Codex's true ceiling.
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
            message::is_message_filename(&record.filename) && !record.thread_id.is_empty(),
            "v1 Codex delivery state has an invalid binding or filename"
        );
        anyhow::ensure!(
            record.client_id == correlate(&record.thread_id, &record.filename),
            "v1 Codex delivery state correlation does not match its binding"
        );
        let phase = match record.phase {
            Label::Attempted => Phase::Attempted,
            Label::Accepted => Phase::Consumed,
        };
        Ok(delivery_ledger::asserted(
            record.filename,
            record.thread_id,
            Correlation::native(record.client_id),
            phase,
            // The typed receipt is a live frame, so an attempt may only be acknowledged by the
            // incarnation that made it; keeping the old one is what forces a resume sweep.
            Some(record.runtime_incarnation).filter(|value| !value.is_empty()),
        ))
    }
}
