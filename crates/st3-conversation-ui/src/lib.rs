//! Native conversation presentation, independent of application layout and transport.
//!
//! Apply `Frame`s to a `Timeline`, adapt its entries with caller-owned display names, then
//! render with a caller-owned `Theme`. `State` owns interaction preferences and emits
//! `PaneIntent`s; the embedding application executes sends, opens and history requests.
//! No terminal acquisition, daemon connection or application-wide shortcuts live here.

pub mod adapt;
pub mod ansi;
mod clean;
pub mod conversation;
pub mod doc;
mod entry;
pub mod pane;
pub mod style;
pub mod text;
pub mod theme;
pub mod timeline;

pub use clean::clean_message_text;
pub use conversation::{Cache, Density, bundle_id};
pub use entry::{Body, Entry, ToolState};
pub use pane::{PaneIntent, PaneState, Selection, State};
pub use theme::Theme;
pub use timeline::{Frame, Timeline};

#[cfg(test)]
mod tests;
