#!/usr/bin/env bash
# Preserve the already-red seeded regression; do not require a redundant fourth spelling of the same case.
# The committed test must retain the exact seeded case, pass with HEAD, and fail when paired with the seed bug.
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; WT="$SB/wt/feature"; BARE="$SB/canonical.git"
SEED="$(git -C "$BARE" rev-list --max-parents=0 main 2>/dev/null | tail -1)"
scratch="$(mktemp -d)"
cleanup() {
  rm -rf -- "$scratch"
}
trap cleanup EXIT

current_test="$scratch/current-test"
git -C "$WT" show HEAD:test/clamp.test.js >"$current_test" 2>/dev/null || {
  echo "FAIL: committed test/clamp.test.js is missing" >&2
  exit 1
}

mkdir -p "$scratch/project/src" "$scratch/project/test"
git -C "$WT" show HEAD:package.json >"$scratch/project/package.json" 2>/dev/null || exit 1
git -C "$WT" show HEAD:src/clamp.js >"$scratch/project/src/clamp.js" 2>/dev/null || exit 1
cp "$current_test" "$scratch/project/test/clamp.test.js"
if ! (cd "$scratch/project" && node --test test/clamp.test.js) >"$scratch/head.out" 2>&1; then
  echo "FAIL: the preserved regression is not green against the committed fix" >&2
  exit 1
fi

git -C "$BARE" show "$SEED":src/clamp.js >"$scratch/project/src/clamp.js" 2>/dev/null || exit 1
if (cd "$scratch/project" && node --test test/clamp.test.js) >"$scratch/seed.out" 2>&1; then
  echo "FAIL: the committed test suite is not RED against the seeded above-range bug" >&2
  exit 1
fi

echo "PASS: seeded above-range regression is preserved, green on HEAD, and RED on the buggy seed"
