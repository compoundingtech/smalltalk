//! Fleet membership as smalltalk uses it: joining from a config file and reaching peers.
//!
//! Keys, the membership fold and the join handshake live in `smallclaims::fleet`. See
//! `docs/fleet-join.md` for the design.

pub mod join;
pub mod transport;

pub use smallclaims::fleet::{code, handshake, keys, membership, view};
pub use smallclaims::fleet::{
    Acceptance, FleetClaim, FleetView, Incarnation, MemberKey, MemberState, MemberView, Membership,
    Refusal, Sender, Window, accept, envelope_signature_message, verify_signature,
};

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::store::Store;

/// Bring a member's store in line with its `fleet.toml` before the daemon writes anything:
/// pin the anchor, apply the writer floor, sign with the member key, and, on the anchor
/// itself, admit it once.
pub fn activate(store: &Store, config: &Config) -> Result<()> {
    let Some(file) = &config.fleet else {
        return Ok(());
    };
    if let Some(anchor) = &file.anchor_key {
        store.pin_fleet_anchor(anchor)?;
    }
    if let Some(floor) = file.writer_floor {
        store.set_writer_floor(floor)?;
    }
    let key = MemberKey::load(&file.node_key_path(&config.state_dir))?;
    let public = key.public().to_owned();
    store.set_member_key(Some(Arc::new(key)))?;
    if file.anchor_key.as_deref() == Some(public.as_str()) {
        store.admit_fleet_anchor(&file.fleet_id, &public, file.mode.as_str())?;
    }
    Ok(())
}
