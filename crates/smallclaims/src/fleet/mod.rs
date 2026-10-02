//! Fleet membership: member keys, signed envelopes, the membership fold, the join handshake,
//! founding and joining a fleet from `fleet.toml`, and the transports members reach each other by.
//!
//! See `docs/fleet-join.md` for the design. A node without a pinned anchor key treats every
//! writer as a legacy writer, so nothing here changes how a node that has not joined or
//! migrated replicates.

pub mod code;
pub mod file;
pub mod handshake;
pub mod join;
pub mod keys;
pub mod membership;
pub mod transport;
pub mod view;

pub use keys::{MemberKey, envelope_signature_message, verify_signature};
pub use membership::{FleetClaim, Incarnation, MemberState, Membership, Window};
pub use view::{Acceptance, FleetView, MemberView, Refusal, Sender, accept};

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

pub use file::{FleetFile, FleetMode, FleetRemoval, PeerConfig, RemovalNotice};

use crate::store::Store;

/// Bring a member's store in line with its `fleet.toml` before anything writes to it: bind it to
/// the fleet, pin the anchor, apply the writer floor, sign with the member key, and, on the anchor
/// itself, admit it once.
pub fn activate(store: &Store, state_dir: &Path, file: &FleetFile) -> Result<()> {
    store.bind_fleet(&file.fleet_id)?;
    if let Some(anchor) = &file.anchor_key {
        store.pin_fleet_anchor(anchor)?;
    }
    if let Some(floor) = file.writer_floor {
        store.set_writer_floor(floor)?;
    }
    let key = MemberKey::load(&file.node_key_path(state_dir))?;
    let public = key.public().to_owned();
    store.set_member_key(Some(Arc::new(key)))?;
    if file.anchor_key.as_deref() == Some(public.as_str()) {
        store.admit_fleet_anchor(&file.fleet_id, &public, file.mode.as_str())?;
    }
    Ok(())
}
