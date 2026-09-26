#!/usr/bin/env bash
set -euo pipefail

diff -u "$CATALOG/events.tsv" <(printf '1\ta\tactive\n2\ta\trevoked\n3\tb\tactive\n')
test "$(st2 resource ls local.a --catalog "$CATALOG/net" --host local --json | jq 'length')" -eq 0
st2 resource read local.b assignment --catalog "$CATALOG/net" --host local | grep -Fq work://eval/shared-handoff
