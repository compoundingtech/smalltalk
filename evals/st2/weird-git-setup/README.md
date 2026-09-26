# Weird Git Setup eval for st2

This eval starts one Codex agent inside a linked Git worktree.

The agent must discover the checkout shape, fix a failing test, and commit on `feature`.

The held-out judges reject a change on `main`, a dirty sibling worktree, or a removed regression test.

Run it with `st2 eval ./evals/st2/weird-git-setup`.

The source fixture came from `compoundingtech/evals` commit `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.
