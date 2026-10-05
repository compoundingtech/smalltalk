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

The standalone module paths and validation-only error type are adapted from that
source. Existing diagnostics mentioning `axe decision check` are retained, not
as a claim that this crate supplies that command. Defensive divergences from the
source are listed below; importer comparisons must account for them.

`parse_record` accepts an immutable record's ID, timestamp, and document bytes;
`Store::push` adds the parsed record to an in-memory store. `fold` computes the
four states or `Resolution::Undecidable`, with attributed defects. Assumptions
and promotions are records, never answers. Target existence and capture replay
policy belong to consumers; the fold does not access a filesystem or deduplicate
capture keys.

## Trust boundary

There are no tree, owner, seat, recipient, or claim-ID types in this file-store
model. `answered_by` is unused by the fold: it is retained as historical data, not
evidence of authority. `parent` is unvalidated structural metadata. This crate
cannot authenticate an actor, verify a claim's kind or establish same-tree
membership.

The PR6 custom-subject adapter must check authority, kind and same-tree references
before accepting records, and must ignore records that fail those checks. Its
contract requires squatter, wrong-actor, cross-tree-reference and wrong-kind-
reference tests. The adapter/importer, not untrusted frontmatter, must assign and
write provenance explicitly: `Imported` for imported answers and `Native` for
authorized live answers. Missing provenance parses as `Unknown` and cannot settle
a guard. Provenance is not an authentication decision.

## Divergences from dotfiles ae542137

- Ambiguous, dangling, cyclic or disconnected answer chains have no usable
  current answer/history and make their request and guard dependents undecidable;
  no root or successor is chosen from vector order. Raw records remain in
  `Store::answers`. Valid chains are still ordered solely by supersession.
- Guard evaluation uses iterative postorder traversal, never call-stack recursion.
  At most 65,536 total records (including parse defects), 64 guard terms per
  request, and 64 dependency edges to a leaf are accepted. Limit overflow reports
  `limit-exceeded` and resolves to undecidable, not gated or a panic. Global record
  overflow yields an empty resolution map and one store-wide defect;
  `Fold::resolution` returns undecidable for every identifier.
- Duplicate immutable IDs across all record kinds report `duplicate-id`.
  Ambiguous requests and owners of ambiguous answers are undecidable. Request
  lookup refuses ambiguous IDs, including cross-kind collisions.
- `next_handle` returns `Result<u64>` and errors at `u64::MAX` rather than wrapping,
  panicking or reusing a handle.
- Dangling assumption supersession is reported, including a lone assumption or a
  predecessor belonging to a different request.
- Answer choices not offered by the referenced request report `unknown-option`,
  including historical answers. An invalid current choice makes the owner and
  guard dependents undecidable; a valid later answer may supersede it.
- `Answer` carries `AnswerProvenance::{Native, Imported, Unknown}`. The parser
  accepts `provenance: native|imported|unknown`, defaults omission to unknown and
  rejects other values. Imported and unknown answers remain current/history and
  can make their own request answered, but every guard term on either is invalid
  with `imported-answer` or `unknown-answer-provenance` (even if its owner is moot).
  An explicitly native superseding answer can settle the guard. Neither imported
  nor unknown answers can manufacture revival through a false guard during prefix
  replay.
- Request traversal and diagnostic lists use deterministic ID order. Valid
  outcomes and defects/revival are invariant under input record permutations;
  cycle/fork diagnostics are correspondingly deterministic.

The public API is explicitly exported: record/model types, parsing and validation
entrypoints, fold/history accessors and the three limit constants. Frontmatter,
option-building and evaluator implementation helpers are not public API.

Revival detection replays each answer prefix. For a valid chain of length L, it
evaluates the whole store L times, rebuilding indexes, parsing option material
and walking the dependency graph each time. Its work is therefore approximately
L times one store evaluation, not linear in chain length alone. The total-record
cap bounds L but is not a latency budget; consumers must budget replay cost when
accepting large histories.

## Verification

Run `cargo test -p st-decision-fold` and
`cargo clippy -p st-decision-fold --all-targets -- -D warnings`.

The 43 unit tests include all 28 pure source tests, with the exhaustive 3,375-store property
extended to all request/answer permutations (59,250 comparisons of resolutions,
defects and revival). The assumption ordering test still covers equal timestamps.
Additional regressions cover ambiguous chains/IDs, overflow boundaries, provenance,
invalid choices and a 20,000-node chain on a two-mebibyte stack.
Eight integration tests adapt the fold/parser assertions from
`flakes/axe/tests/decision_tree.rs`; each names its source test. They exercise
lifecycle/revival/reframe, assumption history, answer history, promotion links,
captured answers/manual supersession, and byte-exact material.

The required Linux tests lane runs workspace nextest with the CI profile, which
includes this crate independently of st2. The st2 workspace check excludes this
crate; a separate Nix check would duplicate the required nextest coverage.

Filesystem storage, reserved-ID races/replay, seat discovery, CLI option/handle
normalization, target resolution, and capture retry/conflict/concurrency contracts
are outside the crate's boundary, not approximated here.
