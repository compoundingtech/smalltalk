# st-decision-fold

Pure decision record model, flat-frontmatter grammar, option-material validators,
and deterministic state fold. It does not read or write files, run commands,
register a manifest, or connect to a daemon.

## Provenance

Ported from the Axe Decision Tree implementation in dotfiles commit
`ae542137f3c74a852ab20b5e17503b78f0cdf545`:

- `flakes/axe/src/decision.rs`: record model, defects, handle resolution and allocation.
- `flakes/axe/src/decision/parsing.rs`: grammar and option validation, excluding filesystem reading.
- `flakes/axe/src/decision/evaluation.rs`: state fold, answer/assumption history, revival and defects.

The algorithms, ordering, accepted grammar, diagnostic strings and fold outcomes
are unchanged. Module paths/visibility and the error type are adapted to the
standalone crate; the error type has only the validation variant because I/O and
JSON serialization are outside this crate. The result alias exposes a defaulted
error parameter. Existing diagnostics mentioning `axe decision check` are kept
for parity, not as a claim that this crate supplies that command.

Two nested conditions use equivalent let-chain syntax to satisfy Clippy without
adding warnings to the workspace ratchet.

`parse_record` accepts an immutable record's ID, timestamp, and document bytes;
`Store::push` adds the parsed record to an in-memory store. `fold` computes the
four states or `Resolution::Undecidable`, with attributed defects. Assumptions
and promotions are records, never answers. Target existence and capture replay
policy belong to consumers; the fold does not access a filesystem or deduplicate
capture keys.

## Verification

Run `cargo test -p st-decision-fold` and
`cargo clippy -p st-decision-fold --all-targets -- -D warnings`.

The 28 pure source unit tests are ported intact, including assumption ordering
with equal timestamps and the exhaustive 3,375-store totality/determinism test.
Seven integration tests adapt the fold/parser assertions from
`flakes/axe/tests/decision_tree.rs`; each names its source test. They exercise
lifecycle/revival/reframe, assumption history, answer history, promotion links,
captured answers/manual supersession, and byte-exact material.

Filesystem storage, reserved-ID races/replay, seat discovery, CLI option/handle
normalization, target resolution, and capture retry/conflict/concurrency contracts
are outside the crate's boundary, not approximated here.
