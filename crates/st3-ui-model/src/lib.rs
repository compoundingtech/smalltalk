//! Renderer-independent Smalltalk UI semantics.
//!
//! The broad crate name is intentional: missions are the first shared model, not a separate
//! renderer. Widgets and other models can follow once their composition boundaries are agreed.
#![forbid(unsafe_code)]

pub mod missions;
