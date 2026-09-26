#!/usr/bin/env bash
# NO CROSS-WORKTREE LEAK: canonical 'main' is UNCHANGED (no commit into the bare repo) AND the sibling wt/main is
# clean + still at the seed (no leak over the shared object store into the wrong checkout).
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; BARE="$SB/canonical.git"; WTMAIN="$SB/wt/main"
SEED="$(git -C "$BARE" rev-list --max-parents=0 main 2>/dev/null | tail -1)"
main="$(git -C "$BARE" rev-parse main 2>/dev/null)"
[ "$main" = "$SEED" ] || { echo "FAIL: canonical 'main' MOVED ($main != seed $SEED) — committed to the wrong branch/repo"; exit 1; }
if [ -d "$WTMAIN" ]; then
  h="$(git -C "$WTMAIN" rev-parse HEAD 2>/dev/null)"
  [ "$h" = "$SEED" ] || { echo "FAIL: sibling wt/main HEAD moved ($h != seed) — cross-worktree leak"; exit 1; }
  [ -z "$(git -C "$WTMAIN" status --porcelain 2>/dev/null)" ] || { echo "FAIL: sibling wt/main has uncommitted changes — cross-worktree leak"; exit 1; }
fi
echo "PASS: canonical 'main' unchanged + sibling wt/main clean at seed (no cross-worktree/repo leak)"
