//! Sekrets: a command gateway that runs any CLI with credentials no seat can read.
//!
//! Opt-in and Linux first. A `sekrets` Unix user owns the store and runs the gateway
//! (`st sekrets serve`). A caller asks the gateway to run a command as one of its profiles; the
//! gateway checks who is calling, the profile's policy and any grant, then runs the command in a
//! sandbox with the profile's home and environment and the caller's own standard streams. Only
//! the command's output and exit status reach the caller. See `docs/st3/sekrets.md`.

pub mod cli;
pub mod client;
pub mod daemon;
pub mod files;
pub mod gateway;
pub mod identity;
pub mod policy;
pub mod protocol;
pub mod sandbox;
pub mod store;

#[cfg(test)]
mod tests;
