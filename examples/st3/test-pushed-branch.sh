#!/bin/bash
# Test the branch as it is on origin, not as a worker left it in a working tree.
# Usage: test-pushed-branch.sh BRANCH
# Gates run with a minimal environment, so every external command has an absolute path.
set -euo pipefail

branch="$1"
scratch="$(/usr/bin/mktemp -d)"
trap 'git worktree remove --force "$scratch/tree" >/dev/null 2>&1 || true; /bin/rm -rf "$scratch"' EXIT

git fetch --quiet origin "$branch"
git worktree add --quiet --detach "$scratch/tree" FETCH_HEAD
cd "$scratch/tree"
/usr/bin/make test
