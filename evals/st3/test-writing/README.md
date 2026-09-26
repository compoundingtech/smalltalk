# Test Writing eval for st3

This eval tests a Codex supervisor and developer with a mutation-scored test task.

The graph stores the brief, the test mission, the revision, both reports, and verification state.

The developer can change tests only. A held-out battery requires at least 10 killed mutations from 12.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.

The source fixture came from `compoundingtech/evals` commit `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.
