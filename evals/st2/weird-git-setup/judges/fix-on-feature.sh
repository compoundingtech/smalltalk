#!/usr/bin/env bash
# THE DISCRIMINATOR: the fix is committed ON `feature` (ahead of the seed) AND its author matches the identity
# pinned in that worktree — proving the agent resolved the worktree layout and committed HERE, not nowhere useful.
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; WT="$SB/wt/feature"; BARE="$SB/canonical.git"
SEED="$(git -C "$BARE" rev-list --max-parents=0 main 2>/dev/null | tail -1)"
ahead="$(git -C "$WT" rev-list --count "$SEED"..HEAD 2>/dev/null || echo 0)"
[ "${ahead:-0}" -ge 1 ] || { echo "FAIL: no commit on 'feature' beyond the seed — nothing landed on this branch"; exit 1; }
branch="$(git -C "$WT" branch --show-current 2>/dev/null)"
[ "$branch" = "feature" ] || { echo "FAIL: fix landed on '$branch', expected 'feature'"; exit 1; }
expected_name="$(git -C "$WT" config --worktree --get user.name 2>/dev/null)"
expected_email="$(git -C "$WT" config --worktree --get user.email 2>/dev/null)"
an="$(git -C "$WT" show -s --format='%an' HEAD 2>/dev/null)"
ae="$(git -C "$WT" show -s --format='%ae' HEAD 2>/dev/null)"
if [ -n "$expected_name" ] && [ -n "$expected_email" ] &&
   [ "$an" = "$expected_name" ] && [ "$ae" = "$expected_email" ]; then
  echo "PASS: fix committed on 'feature' ($ahead beyond seed), authored by its pinned worktree identity ($an <$ae>)"
  exit 0
fi
echo "FAIL: feature tip author '$an <$ae>' does not match its pinned worktree identity '$expected_name <$expected_email>'"
exit 1
