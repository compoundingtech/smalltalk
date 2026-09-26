# Weird Git Setup eval for st3

This eval materializes a linked Git worktree before it starts one Codex worker.

The graph stores checkout discovery, failure reproduction, repair, verification, revision, and report state.

The held-out gates require a feature commit and reject changes to `main` or its sibling worktree.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI. During a development run, inspect the
exact run with `st3 missions show MISSION_RUN --follow`.

The `receipts/` directory contains the 2026-08-30 live graph proof.

The source fixture came from `compoundingtech/evals` commit `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.
