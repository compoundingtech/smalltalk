#!/usr/bin/env bash
# JUDGE: isolation / review-only — the reviewer edits NO code: no commit authored by prx.rev, and
# every tracked and untracked file is unchanged; the supervisor owns no repo.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; R="$ROOT/rev"; SUP="$ROOT/sup"; REVIEWER_ID="${REVIEWER_ID:-prx.rev}"
[ -d "$R/.git" ] || { echo "FAIL: no reviewer checkout at $R — did the run happen?"; exit 1; }
fail=0
badc=$(git -C "$R" log --all --format='%ae' 2>/dev/null | grep -iE "$REVIEWER_ID" | sort -u | tr '\n' ' ')
[ -z "$badc" ] && echo "PASS: no commit authored by the reviewer ($REVIEWER_ID)" || { echo "FAIL: the reviewer COMMITTED to the repo (review must not edit code): $badc"; fail=1; }
dirty=$(git -C "$R" status --porcelain 2>/dev/null)
[ -z "$dirty" ] && echo "PASS: the reviewer worktree is unchanged" || { echo "FAIL: the reviewer modified the worktree:"; echo "$dirty" | sed 's/^/      /'; fail=1; }
[ -d "$SUP/.git" ] && { echo "FAIL: supervisor dir IS a git repo (must own none)"; fail=1; } || echo "PASS: supervisor dir is not a git repo (structural isolation)"
exit "$fail"
