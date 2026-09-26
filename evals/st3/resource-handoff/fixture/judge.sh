#!/usr/bin/env bash
set -euo pipefail

diff -u <(cut -f1-3 events.tsv) <(printf '1\trh.a\tactive\n2\trh.a\trevoked\n3\trh.b\tactive\n')
resource="$(cut -f4 events.tsv | sort -u)"
test "$(printf '%s\n' "$resource" | wc -l)" -eq 1
st3 resource read "$resource" --json | jq -e '.actual.status == "active" and .actual.owner == "agent/rh.b"' >/dev/null
st3 trace "$resource" --json --limit 20 | jq -s -e '[.[] | select(.kind == "resource.observed") | .body.fields.status] | index("removed") != null' >/dev/null
