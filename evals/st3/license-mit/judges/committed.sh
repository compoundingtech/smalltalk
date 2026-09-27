#!/usr/bin/env bash
# JUDGE: committed + clean — the license change is really COMMITTED (present in base..HEAD) and the worker's
# worktree is CLEAN (no dangling uncommitted edits). A half-done job that leaves the change staged/dirty, or
# never commits it, fails here even if the working-tree LICENSE looks right.
#
# PASS (exit 0): a LICENSE path is in base..HEAD AND `git status --porcelain` is empty.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"
W="$ROOT/worker"
[ -d "$W/.git" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
BASE=$(git -C "$W" rev-list --max-parents=0 HEAD 2>/dev/null)
CHANGED=$(git -C "$W" diff --name-only "$BASE"..HEAD 2>/dev/null | tr '\n' ' ')

fail=0
if echo " $CHANGED " | grep -qE ' LICENSE(\.md|\.txt)? '; then
  echo "PASS: the LICENSE change is COMMITTED (present in base..HEAD)"
else
  echo "FAIL: LICENSE is not in base..HEAD (the change was not committed) — changed: ${CHANGED:-<none>}"; fail=1
fi
dirty=$(git -C "$W" status --porcelain 2>/dev/null)
if [ -z "$dirty" ]; then
  echo "PASS: worker worktree is clean"
else
  echo "FAIL: worker worktree is DIRTY (uncommitted changes remain):"; echo "$dirty" | sed 's/^/      /'; fail=1
fi
exit "$fail"
