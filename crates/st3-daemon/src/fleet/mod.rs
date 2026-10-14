//! Fleet membership as smalltalk uses it. Keys, the membership fold, the join handshake, joining
//! from `fleet.toml` and the transports live in `smallclaims::fleet`. See `docs/fleet-join.md`
//! for the design.

pub use smallclaims::fleet::{
    Acceptance, FleetClaim, FleetView, Incarnation, MemberKey, MemberState, MemberView, Membership,
    Refusal, Sender, Window, accept, envelope_signature_message, verify_signature,
};
pub use smallclaims::fleet::{code, file, handshake, join, keys, membership, transport, view};
