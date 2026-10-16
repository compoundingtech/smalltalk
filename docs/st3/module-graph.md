# The st3 module graph and its layering rule

st3 is one crate of about a hundred top-level modules, so every change that touches it recompiles
and retests all of it, and the merge queue cannot tell which part of a combined tree two changes
could both affect. This page defines the module graph that the tooling uses, the rule it enforces,
and how to read the number it reports. The plan to split the crate is in the mission document
`doc/fleet/factory/crate-split/plan`; this page is what stays in the repository.

## The graph

A module is a top-level file or directory under `crates/st3/src` (and, once the library is split,
under the source directories of the crates it is split into). A reference from module A to module B
is a path in non-test code that starts `crate::B` or `st3::B`, or a name inside a `crate::{...}`
group. Left out: comments, `#[cfg(test)]` items, `*_tests.rs`, `tests/` and `test_support.rs`.
Items the library re-exports at its root (`parse_intent`, `St3Error`, the `cli_print` macros)
count toward the module that defines them. Binary-side modules (those `main.rs` declares) refer to
the library as `st3::X` and are part of the graph.

`scripts/module_layers.py` builds it; `scripts/check-module-layers` reports on it.

The parser is textual, not a compiler. It removes `#[cfg(test)]` items with a pattern for the item
shapes that occur in this tree (`mod`, `fn`, `use`, `impl`, `const`, `static`, `struct`, `enum`,
`type`, `trait`); a shape it does not list would count its test-only references as production
edges, so it errs toward extra edges and cannot hide a cycle. Before this check becomes required,
replace the pattern with a compiler-derived graph or keep the shape list tested.

## The rule

The graph minus the pairs in `.st/module-layers.json` is acyclic. Today 4 module pairs (6
references) point upward, mostly into `store`; they are the allowlist. The rule is enforced two
ways: a new edge that closes a cycle outside the allowlist fails, and an allowlisted pair that no
longer exists must be removed, so the list only shrinks. Layers are computed (longest path above
the leaves), not assigned: `scripts/check-module-layers --layers`.

A module that has to use a type or function from a module above it moves that definition down, or
takes a small trait. Do not add an allowlist entry for new code.

## What a change affects

The `protocol` class in `docs/st3/change-impact.md` lists `api.rs` whole, so nearly every daemon
change is a full run for now; narrowing it is a decision for the owners of the affected-set work.

`scripts/check-module-layers --affected PATH...` prints the modules whose dependency closure
contains the changed paths (the reverse closure), as JSON; paths outside `crates/st3` are listed
under `other_paths` for the caller to map to crates. `--metric` reads a JSON list of changed-path
lists and prints the distribution of the sizes.

Measured on the 719 merges of the four weeks to 2026-10-14 that touched a library module: the
median affected set was 68 of 96 modules, and 699 of the 719 affected 53 or more, because 53
modules form one cycle through `store`, `api` and `reconcile`. Removing the allowlisted pairs would
bring the median to 12. An affected set that large is why the merge queue retests everything; the
number to watch is the median, and it only falls when the allowlist shrinks.

## Related tools

- `scripts/classify-risk BASE HEAD` names the risky classes a change belongs to
  (`docs/st3/change-impact.md`); those always run in full whatever the graph says.
- `scripts/forecast-conflicts CANDIDATE --open-prs` forecasts which open pull requests a move
  would newly conflict with, with and without `merge.directoryRenames`.
