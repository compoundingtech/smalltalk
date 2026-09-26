#!/usr/bin/env bash
set -euo pipefail

test -f RESULT.md
grep -Fx 'Services: 3' RESULT.md >/dev/null
grep -Fx 'Total replicas: 7' RESULT.md >/dev/null
grep -Fx 'Public services: api' RESULT.md >/dev/null
grep -Fx 'Owners: core, edge' RESULT.md >/dev/null
