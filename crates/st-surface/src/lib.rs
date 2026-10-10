#![forbid(unsafe_code)]
//! Cards over st client resources. st owns the facts and their schema
//! (`docs/st3/client-v0/schemas`); this crate owns how facts become cards; each client owns
//! only layout. stui and Fractal draw cards with the `ratatui` feature; the web kit decodes the
//! same resources through the generated TypeScript client and must produce the cards in
//! `fixtures/clients/metric-cards.json`, which this crate's tests also check.

use serde::{Deserialize, Serialize};

pub mod metrics;
#[cfg(feature = "ratatui")]
pub mod terminal;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    AgentsRunning,
    HostLoad,
}

/// How a client should colour a card. Unknown is never drawn as a zero.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Normal,
    Busy,
    Stale,
    Unknown,
}

/// One st fact as every client shows it. `subject` is the st resource the card is about
/// (`machine/<name>`), so selecting a card opens the same resource everywhere.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub kind: CardKind,
    pub subject: String,
    pub label: String,
    pub value: String,
    pub tone: Tone,
}
