#!/usr/bin/env bash
# HELD-OUT sanity: wg.dev's checkout is a genuine LINKED WORKTREE (the layout it had to resolve) — its `.git` is
# a FILE (a gitdir: pointer, not a directory) and it is checked out on branch `feature`.
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; WT="$SB/wt/feature"
[ -f "$WT/.git" ] || { echo "FAIL: $WT/.git is not a file — not a linked worktree"; exit 1; }
br="$(git -C "$WT" rev-parse --abbrev-ref HEAD 2>/dev/null)"
[ "$br" = "feature" ] || { echo "FAIL: wt/feature is on '$br', expected 'feature'"; exit 1; }
echo "PASS: wt/feature is a linked worktree (.git is a FILE) on branch 'feature' — $(cat "$WT/.git")"
