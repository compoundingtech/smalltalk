//! Fleet membership: member keys, signed envelopes, the membership fold, and the join handshake.
//!
//! See `docs/fleet-join.md` for the design. A node without a pinned anchor key treats every
//! writer as a legacy writer, so nothing here changes how a node that has not joined or
//! migrated replicates.

pub mod code;
pub mod handshake;
pub mod keys;
pub mod membership;
pub mod view;

pub use keys::{MemberKey, envelope_signature_message, verify_signature};
pub use membership::{FleetClaim, Incarnation, MemberState, Membership, Window};
pub use view::{Acceptance, FleetView, MemberView, Refusal, Sender, accept};
