#!/usr/bin/env bash
# HARD GATE: the project suite is GREEN in the worktree (the planted above-range bug is fixed).
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; WT="$SB/wt/feature"
if ( cd "$WT" && node --test ) >/dev/null 2>&1; then
  echo "PASS: node --test GREEN in wt/feature (above-range bug fixed)"; exit 0
fi
echo "FAIL: node --test RED in wt/feature (bug not fixed / suite broken)"; exit 1
