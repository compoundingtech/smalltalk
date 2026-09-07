# DELTA-006: the pre-ledger `delivery-state.json` boundary arm outlives its own necessity

Status: open

## Divergence

The canonical delivery ledger owns exactly one schema, `st2.delivery-ledger.v1`,
and `src/delivery_ledger.rs` is written as though only that format had ever
shipped. It nevertheless carries one statement that knows otherwise: the
`ErrorKind::NotFound` arm of `Ledger::open` calls
`crate::migrations::delivery_state::recover`, which reads the single-binding
`delivery-state.json` record every pre-ledger release wrote and carries an
in-flight attempt forward as an assertion instead of letting it be re-sent.

That arm is correct today and is dead weight the moment no such record exists
anywhere. Nothing in the code can observe that condition, so the divergence is
between an implementation that must still translate a retired format and a
design that owns one format — and it closes by deletion, on a fleet
observation, not by an amendment.

## VRS

No ratified requirement mentions either record. The rule the arm exists to
preserve is [`DING-R07`](../01-ding/requirements.md) staged ownership: once an
attempt has started, ambiguity about whether it landed is resolved by
inspection, never by pasting the same notice again. A release boundary is
exactly such an ambiguity — the old binary's record is the only evidence that
an attempt was made — so dropping that record on upgrade would resolve the
ambiguity by re-sending, which DING-R07 forbids and DING-T01 answers the other
way: every ambiguous case resolves toward deferral.

Requirements therefore need no change. What needs recording is that a piece of
the implementation is deliberately temporary, with the observation that ends
it.

## Implementation

`src/migrations/delivery_state/` owns the whole boundary: `mod.rs` (the entry
point, the ownership filter, the legacy filename), `codex_v1.rs`, and
`opencode_v1.rs` (one retired wire struct each, plus the meaning of its
labels — Codex's `accepted` was a typed in-turn receipt and grades to
`consumed`, OpenCode's was a storage read-back and grades to `persisted`).

Canonical code gains one version-free field, `Attestation{Observed, Asserted}`
on `Entry`: whether this build graded the phase or another authority asserted
it. It changes no decision — no `Retention`, `RetryDecision`, or transport
arm reads it. What holds a carried-forward attempt is its `Phase` measured
against the harness `Profile`: no profile proves `Attempted`, and `Ledger::seed`
refuses an asserted phase its profile cannot prove at all, so the safety
property is delivered by the phase, not by the label. The field is there so the
fleet can *see* an unobserved phase — it is clause 2 of the Resolution Signal
below — which makes it trigger instrumentation for this arm, not a permanent
concept. It is deleted **with** the arm.

Deletion is `git rm -r src/migrations`, replacing the seam arm with `Ok(())`,
and removing the instrumentation the trigger needed: `Entry.attestation` and
`Attestation`, `delivery_ledger::asserted_entries`,
`migrations::delivery_state::resolution_signal`, and the `st2 doctor` row that
prints it. What remains is byte-for-byte a first run on a fresh seat. Measured
cost of the boundary deletion itself: one compile error, at the seam.

## Direction

update implementation

## Resolution Signal

`st2 doctor` prints, per seat, a `pre-ledger delivery state (DELTA-006)`
advisory carrying `preLedgerRecords` and `assertedEntries`, and prints nothing
when both are zero. The trigger resolves when that line is absent from
`st2 doctor` on every admitted host for seven consecutive days. Producer:
`migrations::delivery_state::resolution_signal`, pinned by
`migrations::delivery_state::tests::the_resolution_signal_counts_each_clause_without_consuming_it`;
measuring is read-only, so a diagnostic cannot make the record it counts
disappear.

The two clauses, and the equivalent commands for a host with no `st2` on PATH:

```sh
state="${XDG_STATE_HOME:-$HOME/.local/state}/st2"

# 1. No pre-ledger record is left beside any per-harness state dir, so no
#    unread attempt can still need carrying forward.
find "$state/codex" "$state/opencode" -maxdepth 2 -name delivery-state.json -print

# 2. No ledger entry still holds a phase this fleet never observed. While one
#    exists, the translation that produced it is load-bearing.
find "$state/codex" "$state/opencode" -maxdepth 2 -name delivery-ledger.json -print0 \
  | xargs -0 -r jq -r 'select([.entries[].attestation] | any(. == "asserted")) | input_filename'
```

Clause 1 additionally requires a named artifact, not a recollection: a ratified
decision under `docs/vrs/.decisions/` recording that the pre-ledger release line
is no longer a supported rollback target. While no such record exists clause 1
stays open, because a rolled-back binary can write a new `delivery-state.json`
and the roll-forward window it opens is pinned by
`migrations::delivery_state::tests::a_rollback_then_roll_forward_does_not_see_the_record_written_in_between`.
That record's existence is what closes the clause; cite it by number here when
it lands.

The local half of the trigger — that with no old record present the module
contributes nothing and writes nothing, so removing it cannot change observed
behaviour — is asserted by
`migrations::delivery_state::tests::deletion_trigger_absent_old_record_makes_this_module_a_no_op`.
