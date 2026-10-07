//! Native conversation model, independent of rendering, application layout and transport.
//!
//! Apply `Frame`s to a `Timeline` and adapt its entries with caller-owned display names.
//! The default build has no ratatui dependency. Enable the `ratatui` feature to draw entries
//! with `Cache` and a caller-owned `Theme`. `State` owns interaction preferences and emits
//! `PaneIntent`s; the embedding application executes sends, opens and history requests.
//! No terminal acquisition, daemon connection or application-wide shortcuts live here.

pub mod adapt;
#[cfg(feature = "ratatui")]
pub mod ansi;
mod clean;
#[cfg(feature = "ratatui")]
pub mod conversation;
#[cfg(feature = "ratatui")]
pub mod doc;
mod entry;
pub mod pane;
pub mod style;
#[cfg(feature = "ratatui")]
pub mod text;
#[cfg(feature = "ratatui")]
pub mod theme;
pub mod timeline;

pub use clean::{clean_message_text, clean_message_text_with_filters};

/// UI preferences only; neither the owner read nor the normalized data applies these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayFilter {
    HarnessMarkup,
    ContextBlocks,
    ControlCharacters,
    InternalBlocks,
    Excerpts,
}
pub const DEFAULT_FILTERS: &[DisplayFilter] = &[
    DisplayFilter::HarnessMarkup,
    DisplayFilter::ContextBlocks,
    DisplayFilter::ControlCharacters,
    DisplayFilter::InternalBlocks,
    DisplayFilter::Excerpts,
];
/// Empty filters render every entry as reversible JSON, including metadata and raw byte blocks.
pub const SHOW_EVERYTHING: &[DisplayFilter] = &[];
#[cfg(feature = "ratatui")]
pub use conversation::Cache;
pub use entry::{Body, Density, Entry, MailImage, ToolState, bundle_id, display_rows, folds};
pub use pane::{PaneIntent, PaneState, Selection, SelectionLine, State};
#[cfg(feature = "ratatui")]
pub use theme::Theme;
pub use timeline::{Frame, Older, Timeline};

#[cfg(test)]
mod tests;
