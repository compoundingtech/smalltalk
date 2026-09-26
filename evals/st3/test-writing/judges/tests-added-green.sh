#!/usr/bin/env bash
# JUDGE: a real test suite was added (>=4 tests across >=1 test file) AND it is GREEN on the ORIGINAL
# (unmutated) code (the tests themselves are correct).
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; W="$ROOT/worker"
[ -d "$W" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
fail=0
ntests=$(cd "$W" && node --test 2>&1 | grep -E "(# |ℹ )?tests [0-9]+" | grep -oE "[0-9]+" | head -1); ntests=${ntests:-0}
testfiles=$(cd "$W" && find . -path ./node_modules -prune -o -name '*.test.js' -print 2>/dev/null | wc -l | tr -d ' ')
{ [ "$ntests" -ge 4 ] && [ "$testfiles" -ge 1 ]; } && echo "PASS: a real test suite was added (tests=$ntests across $testfiles file(s))" || { echo "FAIL: no meaningful test suite added (tests=$ntests, files=$testfiles)"; fail=1; }
( cd "$W" && node --test >/dev/null 2>&1 ) && echo "PASS: suite is GREEN on the original (unmutated) code" || { echo "FAIL: suite is RED on the original code (the tests themselves are wrong)"; fail=1; }
exit "$fail"
