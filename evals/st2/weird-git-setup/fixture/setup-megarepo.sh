#!/usr/bin/env bash
# Materialize the weird-git-setup MEGAREPO inside the eval catalog ($CATALOG): a BARE canonical repo + two
# LINKED WORKTREES (not a plain clone) + a planted bug, then drop the worker instructions into wt/feature. This runs
# as the eval's `run { step "materialize" }` — BEFORE the team boots. Worktrees hold ABSOLUTE `gitdir:` paths, so
# they must be built IN PLACE at run time; a static `copy "./fixture"` (with _git→.git) cannot preserve the
# bare↔worktree linkage. Deterministic, offline, public-safe (no real repo/operator tokens).
set -euo pipefail
SB="${CATALOG:?CATALOG must be set — st2 eval provides it to run steps}"
cd "$SB"
SEED="$SB/.seed"
WORKTREE_ACTOR_NAME="Eval Worktree Actor"
WORKTREE_ACTOR_EMAIL="worktree-actor@eval.invalid"

echo "== seed clampkit (a tiny Node lib with a PLANTED above-range bug + a RED test) =="
mkdir -p "$SEED/src" "$SEED/test"
cat > "$SEED/package.json" <<'JSON'
{
  "name": "clampkit",
  "version": "1.0.0",
  "private": true,
  "type": "module",
  "description": "Clamp numbers into an inclusive range.",
  "scripts": { "test": "node --test" }
}
JSON
# BUG: the above-range branch returns `lo` instead of `hi` (a real off-by-meaning bug).
cat > "$SEED/src/clamp.js" <<'JS'
// Clamp `n` into the inclusive range [lo, hi].
export function clamp(n, lo, hi) {
  if (n < lo) return lo;
  if (n > hi) return lo; // BUG: an above-range value should clamp to `hi`, not `lo`.
  return n;
}
JS
cat > "$SEED/test/clamp.test.js" <<'JS'
import { test } from "node:test";
import assert from "node:assert/strict";
import { clamp } from "../src/clamp.js";

test("in-range values pass through", () => { assert.equal(clamp(5, 0, 10), 5); });
test("below-range values clamp to lo", () => { assert.equal(clamp(-3, 0, 10), 0); });
test("above-range values clamp to hi", () => { assert.equal(clamp(15, 0, 10), 10); }); // RED on the planted bug
JS
cat > "$SEED/README.md" <<'MD'
# clampkit
`clamp(n, lo, hi)` clamps a number into the inclusive range `[lo, hi]`. Run `node --test`.
MD

echo "== git init seed -> bare canonical.git (the shared object store + refs) =="
git -C "$SEED" init -q -b main
git -C "$SEED" add -A
# Seed history is fixture data; plumbing keeps agent commit hooks out of immutable setup.
seed_tree="$(git -C "$SEED" write-tree)"
seed_commit="$(
  printf '%s\n' "clampkit: initial (has a planted above-range bug)" |
    GIT_AUTHOR_NAME="eval-seed" GIT_AUTHOR_EMAIL="seed@eval.local" \
    GIT_COMMITTER_NAME="eval-seed" GIT_COMMITTER_EMAIL="seed@eval.local" \
    GIT_AUTHOR_DATE="2026-07-30T00:00:00Z" GIT_COMMITTER_DATE="2026-07-30T00:00:00Z" \
    git -C "$SEED" commit-tree "$seed_tree"
)"
git -C "$SEED" update-ref refs/heads/main "$seed_commit"
git clone -q --bare "$SEED" "$SB/canonical.git"

echo "== add TWO linked worktrees off the bare canonical (the megarepo shape) =="
# wt/main tracks main; wt/feature is a new branch off main — the agent works here.
git -C "$SB/canonical.git" worktree add -q "$SB/wt/main" main
git -C "$SB/canonical.git" worktree add -q -b feature "$SB/wt/feature" main

echo "== pin PER-WORKTREE authors (worktrees SHARE config; worktreeConfig makes each author distinct) =="
git -C "$SB/canonical.git" config extensions.worktreeConfig true
# Worktrees of a bare repo inherit core.bare=true from the SHARED config; with worktreeConfig on that makes git
# refuse commit ("must be run in a work tree"). Override core.bare=false per-worktree so the checkouts work.
git -C "$SB/wt/feature" config --worktree core.bare false
git -C "$SB/wt/main"    config --worktree core.bare false
# This fixture owns the declared actor policy for ordinary agent-authored product commits. It is intentionally
# distinct from the deterministic plumbing identity used above to construct immutable seed history.
git -C "$SB/wt/feature" config --worktree user.name  "$WORKTREE_ACTOR_NAME"
git -C "$SB/wt/feature" config --worktree user.email "$WORKTREE_ACTOR_EMAIL"
git -C "$SB/wt/main"    config --worktree user.name  "$WORKTREE_ACTOR_NAME"
git -C "$SB/wt/main"    config --worktree user.email "$WORKTREE_ACTOR_EMAIL"

echo "== drop the worker instructions into wt/feature (git-excluded so they never show in status) =="
cp "$SB/persona/AGENTS.md" "$SB/wt/feature/"
excl="$(git -C "$SB/wt/feature" rev-parse --git-path info/exclude)"
printf 'AGENTS.md\n.codex/\n' >> "$excl"

echo
echo "MEGAREPO READY under $SB:"
echo "  canonical.git (BARE object store + refs) — its 'main' must stay UNCHANGED"
echo "  wt/main    (branch main)    — sibling worktree, must stay untouched"
echo "  wt/feature (branch feature) — the agent's worktree; .git is a FILE -> $(cat "$SB/wt/feature/.git")"
echo "  planted bug: src/clamp.js above-range branch returns lo (should be hi); test/clamp.test.js is RED."
