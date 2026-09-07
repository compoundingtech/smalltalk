//! Every translation from a format an earlier release wrote into a canonical type this release
//! owns.
//!
//! The rule this tree exists to enforce: **no module outside `src/migrations/` names a format it
//! does not itself write.** Canonical modules know exactly one schema — their own — and reach a
//! translation through a single, named seam. `src/delivery_ledger.rs`'s seam, for example, is one
//! statement in [`crate::delivery_ledger::Ledger::open`]'s "no ledger file" arm.
//!
//! Consequences, all deliberate:
//!
//! * A canonical module's complexity is what it would be if only this version had ever shipped.
//! * Deleting a boundary is a `git rm` plus the removal of its seam statement; nothing else in
//!   the tree references the retired format's field names, filename, or old semantics.
//! * Every translation is one directory with one entry point, so "what still reads old bytes?"
//!   is answered by `ls src/migrations/`.
//!
//! Each subdirectory MUST name, in its `mod.rs` doc comment, the delta record under
//! `docs/vrs/.delta/` that carries its deletion trigger, and its seam MUST carry a
//! `DELETION TRIGGER: DELTA-NNN` comment. The delta record's Resolution Signal is a live query,
//! so "may this go?" is answered by running a command rather than by reading an opinion. A
//! `deletion_trigger_*` test asserts the local half: that once no old record exists, the module
//! contributes nothing, so removing it cannot change behaviour.

pub mod delivery_state;
