#!/usr/bin/env bash
# JUDGE: visible suite — the labelkit unit suite (`node --test`) is GREEN on HEAD. The team must keep the
# existing tests passing while fixing the bug (no deleting/weakening tests to go green).
#
# PASS (exit 0): `node --test` exits 0 in the worker repo.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"
W="$ROOT/worker"
[ -d "$W" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
if ( cd "$W" && node --test >/dev/null 2>&1 ); then
  echo "PASS: npm test suite is GREEN on HEAD"; exit 0
else
  echo "FAIL: suite is RED on HEAD"; exit 1
fi
