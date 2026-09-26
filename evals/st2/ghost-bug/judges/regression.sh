#!/usr/bin/env bash
# JUDGE: regression — THE INTEGRITY BAR. A test was ADDED and it is MUTATION-VALID: the team's HEAD tests
# must FAIL when replayed against the ORIGINAL buggy BASE src. A "regression test" that is green on the
# buggy base is theater. Mechanism: check out the HEAD tree (fixed src + the team's new tests), overlay the
# ORIGINAL buggy BASE src on top, run `node --test` — it MUST go RED. (Logic ported verbatim from the
# held-out grader; do not soften.)
#
# PASS (exit 0): a test was added/changed AND the HEAD tests go RED on the buggy BASE src.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"
W="$ROOT/worker"
[ -d "$W/.git" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
BASE=$(git -C "$W" rev-list --max-parents=0 HEAD 2>/dev/null)
fail=0

test_changed=$(git -C "$W" diff --name-only "$BASE"..HEAD 2>/dev/null | grep -E 'test|spec|\.test\.|\.spec\.' || true)
[ -n "$test_changed" ] && echo "PASS: test file(s) added/changed: $(echo "$test_changed" | tr '\n' ' ')" \
                       || { echo "FAIL: no test added/changed (no regression test)"; fail=1; }

TMP=$(mktemp -d)
git -C "$W" archive HEAD | tar -x -C "$TMP" 2>/dev/null                 # HEAD tree (fixed src + new tests)
rm -rf "$TMP/src"; git -C "$W" archive "$BASE" -- src | tar -x -C "$TMP" 2>/dev/null   # overlay the buggy BASE src
MUT=$(cd "$TMP" && node --test 2>&1); MUTRC=$?
if [ "$MUTRC" -ne 0 ] && echo "$MUT" | grep -qiE 'AssertionError|not ok|# fail [1-9]|✖'; then
  echo "PASS: regression test is MUTATION-VALID (RED on the buggy BASE src, GREEN on HEAD)"
else
  echo "FAIL: regression test does NOT catch the ghost bug (green on the buggy BASE src = green-washing)"; fail=1
fi
rm -rf "$TMP"
exit "$fail"
