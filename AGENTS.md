# Repository instructions

## Fix causes; do not hide broken paths

Fix the cause of a slow or broken path. Do not add fallbacks, retries, caches or timeouts that hide it. A workaround that makes the symptom disappear while leaving the defect in place is not an acceptable fix.

Retries for peers and networks are valid because those connections can be unreachable by nature. Keep that distinction explicit; retries must not hide a defect in a local path.

Build reactive behavior instead of polling. Use incremental view maintenance when necessary to keep updates and reads efficient.

Nathan approved this policy on 2026-10-06. See issue #1566 and st document doc/fleet/smalltalk/no-workarounds/hunt-2026-10-06 for examples.

## Evidence: run the tests, they sign

`scripts/install-git-hooks` (run by the default `nix develop` shell outside CI, which prints one line the first time) installs a pre-push hook. When you push the branch you have checked out and its head tree has no signed evidence yet, the hook runs the required checks that have a local command (`scripts/st-test`) on this machine, the host's signer records them, the notes are pushed, and then the push goes. It never blocks a push: every failure is printed and the push goes. The hook and the runner are **not** read from the tree you push: the stub runs `scripts/evidence-prepush` and `scripts/st-test` as they are on `origin/main`. The checks they run (`nix develop`, `pnpm install`, the tests, in a clean worktree of the pushed commit) are the tree's code and run on your machine as you, with the host's signing key within reach: that is what testing it means here (our own machines are trusted; known defect 22).

**Agents:** run `scripts/st-test --push origin` before you push, so the first CI run already sees the notes. A push from an agent (`ST_AGENT` set, or no terminal) waits at most 120 seconds (`ST_EVIDENCE_AGENT_LIMIT`) for the hook, then goes, and signing continues in the background (log under `~/.local/state/st3/evidence-prepush/`); an interactive push waits up to 20 minutes (`ST_EVIDENCE_TIME_LIMIT`). `ST_SKIP_EVIDENCE=1 git push` skips it for work in progress (the pull request then simply runs in CI); `scripts/st-test --status` says whether your tree is covered. A new tree is always the slow path (the checks run once); a fixup that changes the tree is too; a push of a tree that already has notes is silent and instant. See [docs/st3/gate-evidence.md](docs/st3/gate-evidence.md), "Running the checks locally" and "Host keys".
