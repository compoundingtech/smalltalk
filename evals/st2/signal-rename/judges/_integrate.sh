#!/usr/bin/env bash
# Shared helper: integrate sig.sup's clone with origin/main (the agents pushed their lanes there), then export W
# (the integrated workspace) + BASE (the seed root commit). Sourced by every judge. Idempotent.
SB="${CATALOG:?CATALOG not set}"
W="$SB/sup"
[ -d "$W/.git" ] || { echo "FAIL: no integrated workspace at $W — did the run happen?"; exit 1; }
git -C "$W" fetch -q origin 2>/dev/null || true
git -C "$W" merge -q --ff-only origin/main 2>/dev/null || true
BASE="$(git -C "$W" rev-list --max-parents=0 HEAD 2>/dev/null | tail -1)"
# grep the COMMITTED product code — `git grep` searches only git-TRACKED files, so it excludes the gitignored eval
# persona overlay (AGENTS.md, which mentions the old product name in task text) and
# any untracked scratch. This grades what the agents actually committed, not the working-tree infra.
wgrep(){ ( cd "$W" && git grep -In "$@" 2>/dev/null ); }
