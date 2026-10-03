#!/bin/bash
# Test the branch as it is on origin, not as a worker left it in a working tree.
# Usage: test-pushed-branch.sh BRANCH
# Gates run with a minimal environment, so every external command has an absolute path.
# An exec gate exits 0 to pass and 1 for not yet; any other status breaks it. A branch that is
# not pushed yet and tests that fail are not yet; a worktree git cannot make breaks the gate.
set -euo pipefail

branch="$1"
scratch="$(/usr/bin/mktemp -d)"
trap 'git worktree remove --force "$scratch/tree" >/dev/null 2>&1 || true; /bin/rm -rf "$scratch"' EXIT

git fetch --quiet origin "$branch" || exit 1
git worktree add --quiet --detach "$scratch/tree" FETCH_HEAD
cd "$scratch/tree"
/usr/bin/make test || exit 1
