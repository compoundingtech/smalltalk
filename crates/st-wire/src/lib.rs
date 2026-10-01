//! st-wire — the JSON shapes shared CLI producers emit.
//!
//! The producer prints machine-readable JSON that other programs read. Before this crate each reader
//! hand-typed its own struct to match, which is two transcriptions of one contract: nothing
//! asserted they agreed, either side could drift silently, and the failure surfaced somewhere else
//! entirely — a renderer reporting a parse error for a field the producer had legitimately changed.
//!
//! So the wire shape lives here, the CLI serializes *through* it, and readers deserialize *the same
//! type*. Optionality is then whatever the CLI says it is, checked by rustc rather than by whoever last
//! read the CLI source.
//!
//! This crate is deliberately small and dependency-light: it holds the shapes and nothing else. The
//! parsing, the filesystem, and the domain types stay in the CLI — a reader that wants to render a
//! message list should not thereby depend on the runner's mailbox I/O.
//!
//! - [`message`] — `message ls --json` and `message read --json`.
//!
//! # Readers are additive-tolerant on purpose
//!
//! No type here uses `deny_unknown_fields`. A reader is pinned to a revision of this crate and may
//! legitimately be older than the producer binary it shells out to, so an unknown field must be
//! ignored rather than fatal. Rejecting them would turn every future additive field into a broken
//! reader — the exact failure this crate exists to prevent.

pub mod message;
